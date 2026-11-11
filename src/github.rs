//! GitHub REST client for one repository's self-hosted runners and queued jobs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::boat;
use crate::github_app::App;
use crate::model::{JobKey, QueuedJob, RunnerState, UnixSecs};
use crate::transport::{Response, Transport};

pub(crate) const API: &str = "https://api.github.com";
/// Every runner this tool registers has a name starting with this.
pub(crate) const PREFIX: &str = "sbr-";
/// Requests stop for the rest of the rate-limit window once fewer than this many remain,
/// so a human with the same token keeps some budget.
pub(crate) const RATE_FLOOR: u64 = 200;
/// Under this many requests left the daemon observes only once every `PACE_MAX_S`, so
/// the requests between here and `RATE_FLOOR` stay for runner registrations and deletes,
/// `drain`, and whoever else uses the token.
pub(crate) const RESERVE: u64 = 500;
/// The share of the token's hourly limit (`X-RateLimit-Limit`) the daemon aims to use.
const SHARE: f64 = 0.25;
/// Assumed until GitHub reports the limit: a personal access token's 5,000 an hour.
const DEFAULT_LIMIT: u64 = 5_000;
/// The budget saves up at most this many seconds of its rate for a busy tick.
const BURST_S: f64 = 60.0;
/// While the budget allows, an in-progress run's jobs are read at least this often, even
/// when the run looks unchanged.
pub(crate) const RUN_MAX_AGE_S: u64 = 60;
/// However low the budget, the daemon observes at least this often while GitHub still
/// answers, so cleanup goes on.
pub(crate) const PACE_MAX_S: u64 = 300;
const PER_PAGE: usize = 100;
const MAX_PAGES: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Under `RATE_FLOOR` requests left, or GitHub's own limit hit; wait for `reset`.
    RateLimited {
        remaining: u64,
        reset: UnixSecs,
    },
    /// 401, or 403 that is not a rate limit: bad token or missing permission.
    Unauthorized {
        status: u16,
    },
    NotFound,
    /// The GitHub App's JWT or installation was refused: wrong app id or key, the app
    /// not installed on the repository, or a missing permission.
    App(String),
    Network(String),
    Status {
        status: u16,
        message: String,
    },
    Parse(String),
}

impl Error {
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(self, Error::Unauthorized { .. } | Error::App(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::RateLimited { remaining, reset } => {
                write!(f, "GitHub rate limit: {remaining} left until {reset}")
            }
            Error::Unauthorized { status } => write!(f, "GitHub unauthorized ({status})"),
            Error::NotFound => write!(f, "GitHub: not found"),
            Error::App(e) => write!(f, "GitHub App: {e}"),
            Error::Network(e) => write!(f, "GitHub network error: {e}"),
            Error::Status { status, message } => write!(f, "GitHub error {status}: {message}"),
            Error::Parse(e) => write!(f, "GitHub response not understood: {e}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct GhRunner {
    pub id: u64,
    pub name: String,
    /// `online` or `offline`.
    pub status: String,
    pub busy: bool,
}

/// A just-in-time runner config. It is a credential: no `Debug`, no `Display`, and the
/// only way out is `expose`.
pub(crate) struct JitConfig(String);

impl JitConfig {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// `sbr-<pool>-<machine id>-<slot>-<registered unix time>`.
pub(crate) fn runner_name(pool: &str, machine: &str, slot: u32, registered_at: UnixSecs) -> String {
    format!("{PREFIX}{pool}-{machine}-{slot}-{registered_at}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerName {
    pub pool: String,
    pub machine: String,
    pub slot: u32,
    pub registered_at: UnixSecs,
}

/// Inverse of `runner_name`. Splits from the right: the time, slot and boat id never
/// contain `-`, so the rest is the pool name. `policy::validate` keeps `-` out of pool
/// names anyway.
pub fn parse_runner_name(name: &str) -> Option<RunnerName> {
    let mut parts = name.strip_prefix(PREFIX)?.rsplitn(4, '-');
    let registered_at = parts.next()?.parse().ok()?;
    let slot = parts.next()?.parse().ok()?;
    let machine = parts.next()?;
    let pool = parts.next()?;
    if pool.is_empty() || !boat::is_sandbox_id(machine) {
        return None;
    }
    Some(RunnerName {
        pool: pool.to_string(),
        machine: machine.to_string(),
        slot,
        registered_at,
    })
}

/// GitHub reports only `online`/`offline` and `busy`, so "never came online" is what this
/// process has not seen online. After a restart a runner that went offline earlier is
/// `Registered`, and `decide` unregisters it once `register_timeout_s` has passed since its
/// `registered_at`.
pub(crate) fn runner_state(online: bool, busy: bool, seen_online: bool) -> RunnerState {
    if busy {
        RunnerState::Busy
    } else if online {
        RunnerState::Idle
    } else if seen_online {
        RunnerState::Offline
    } else {
        RunnerState::Registered
    }
}

/// A token bucket over GitHub requests. It fills at `SHARE` of the hourly limit, or
/// slower when the requests left above `RESERVE` would not last until the reset at that
/// rate, and every request GitHub counts (anything but a 304) takes one token. Required
/// reads and writes always go, down to `RATE_FLOOR`, and may take it below zero; optional
/// reads wait for a token, and a tick observes only with a token in hand.
struct Budget {
    tokens: f64,
    refilled: Option<UnixSecs>,
    limit: u64,
    /// The last `X-RateLimit-Remaining` and `X-RateLimit-Reset`.
    left: Option<(u64, UnixSecs)>,
    observed: Option<UnixSecs>,
}

impl Budget {
    /// Tokens per second at `now`.
    fn rate(&self, now: UnixSecs) -> f64 {
        let share = self.limit as f64 * SHARE / 3_600.0;
        match self.left {
            Some((left, reset)) if now < reset => {
                let spare = left.saturating_sub(RESERVE) as f64;
                share.min(spare / (reset - now) as f64)
            }
            _ => share,
        }
    }

    fn refill(&mut self, now: UnixSecs) {
        let rate = self.rate(now);
        let elapsed = self
            .refilled
            .map_or(BURST_S, |t| now.saturating_sub(t) as f64);
        self.tokens = (self.tokens + rate * elapsed).min(rate * BURST_S);
        self.refilled = Some(now);
    }

    fn has_token(&self) -> bool {
        self.tokens >= 1.0
    }
}

fn jobs_path(run: u64) -> String {
    format!("/actions/runs/{run}/jobs?per_page={PER_PAGE}")
}

/// What the last read of an in-progress run's jobs found.
struct RunJobs {
    attempt: u32,
    updated_at: String,
    fetched: UnixSecs,
    queued: Vec<(JobKey, Vec<String>)>,
    /// Some queued job matched a pool, so the run is read again every tick.
    wanted: bool,
}

struct Cached {
    etag: String,
    body: Vec<u8>,
    used: bool,
}

/// How the client authenticates to GitHub.
pub enum Auth {
    /// A personal access token, sent as is.
    Token(String),
    /// A GitHub App, whose installation token the client mints and renews itself.
    App(Box<App>),
}

pub(crate) struct Client<'t> {
    t: &'t dyn Transport,
    auth: Auth,
    repo: String,
    now: UnixSecs,
    /// Bodies by URL with their ETag. A 304 answer to `If-None-Match` does not count
    /// against the rate limit.
    cache: HashMap<String, Cached>,
    /// Set once `X-RateLimit-Remaining` drops under `RATE_FLOOR`: (remaining, reset).
    limited: Option<(u64, UnixSecs)>,
    budget: Budget,
    /// In-progress runs by id.
    runs: HashMap<u64, RunJobs>,
}

impl<'t> Client<'t> {
    pub(crate) fn new(t: &'t dyn Transport, auth: Auth, repo: String) -> Self {
        Client {
            t,
            auth,
            repo,
            now: 0,
            cache: HashMap::new(),
            limited: None,
            budget: Budget {
                tokens: 0.0,
                refilled: None,
                limit: DEFAULT_LIMIT,
                left: None,
                observed: None,
            },
            runs: HashMap::new(),
        }
    }

    /// Sets the clock and drops cached bodies the previous tick did not use, so the
    /// cache holds at most one tick's URLs.
    pub(crate) fn begin_tick(&mut self, now: UnixSecs) {
        self.now = now;
        self.cache.retain(|_, c| c.used);
        for c in self.cache.values_mut() {
            c.used = false;
        }
        self.budget.refill(now);
    }

    /// Whether this tick should observe: with a token in the budget, or when the last
    /// observe was `PACE_MAX_S` ago. A tick that does not observe sends nothing to GitHub.
    pub(crate) fn may_observe(&mut self) -> bool {
        let due = self
            .budget
            .observed
            .is_none_or(|t| self.now >= t.saturating_add(PACE_MAX_S));
        let ok = self.budget.has_token() || due;
        if ok {
            self.budget.observed = Some(self.now);
        }
        ok
    }

    /// For the log line of a tick that does not observe.
    pub(crate) fn budget_note(&self) -> String {
        match self.budget.left {
            Some((left, reset)) if self.now < reset => {
                format!("GitHub request budget spent ({left} left until {reset})")
            }
            _ => "GitHub request budget spent".into(),
        }
    }

    fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Vec<u8>, Error> {
        if let Some((remaining, reset)) = self.limited {
            if self.now < reset {
                return Err(Error::RateLimited { remaining, reset });
            }
            self.limited = None;
        }
        let url = format!("{API}/repos/{}{path}", self.repo);
        let cacheable = method == "GET";
        let mut resp = self.send(method, &url, body)?;
        // An installation token GitHub no longer takes, revoked or outlived by a clock
        // that ran slow, is minted again once.
        if resp.status == 401 {
            if let Auth::App(app) = &mut self.auth {
                app.forget_token();
                self.budget.tokens -= 1.0;
                resp = self.send(method, &url, body)?;
            }
        }

        let num = |name: &str| resp.header(name).and_then(|v| v.trim().parse::<u64>().ok());
        let remaining = num("x-ratelimit-remaining");
        let retry_after = num("retry-after");
        let a_minute = self.now.saturating_add(60);
        let reset = num("x-ratelimit-reset").unwrap_or(a_minute);
        if resp.status != 304 {
            self.budget.tokens -= 1.0;
        }
        if let Some(limit) = num("x-ratelimit-limit").filter(|&l| l > 0) {
            self.budget.limit = limit;
        }
        if let Some(r) = remaining {
            self.budget.left = Some((r, reset));
        }
        if let Some(r) = remaining.filter(|&r| r < RATE_FLOOR) {
            self.limited = Some((r, reset));
        }
        let message = || {
            let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
            v.get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        // GitHub signals a secondary rate limit with 403 or 429 and a message, sometimes
        // with neither header; it then asks for a wait of at least a minute.
        let rate_limited = matches!(resp.status, 403 | 429)
            && (resp.status == 429
                || remaining == Some(0)
                || retry_after.is_some()
                || message().to_ascii_lowercase().contains("rate limit"));
        if rate_limited {
            let reset = match (retry_after, remaining) {
                (Some(s), _) => self.now.saturating_add(s),
                (None, Some(0)) => reset,
                (None, _) => a_minute,
            };
            self.limited = Some((0, reset));
            return Err(Error::RateLimited {
                remaining: 0,
                reset,
            });
        }
        match resp.status {
            304 => match self.cache.get_mut(&url) {
                Some(c) => {
                    c.used = true;
                    Ok(c.body.clone())
                }
                None => Err(Error::Status {
                    status: 304,
                    message: "not modified without a cached body".into(),
                }),
            },
            200..=299 => {
                if let Some(etag) = resp.header("etag").filter(|_| cacheable) {
                    let cached = Cached {
                        etag: etag.to_string(),
                        body: resp.body.clone(),
                        used: true,
                    };
                    self.cache.insert(url, cached);
                }
                Ok(resp.body)
            }
            401 | 403 => Err(Error::Unauthorized {
                status: resp.status,
            }),
            404 => Err(Error::NotFound),
            status => Err(Error::Status {
                status,
                message: message(),
            }),
        }
    }

    /// One request with the current credential and, for a cached GET, its ETag.
    fn send(&mut self, method: &str, url: &str, body: Option<&Value>) -> Result<Response, Error> {
        let token = match &mut self.auth {
            Auth::Token(token) => token.clone(),
            Auth::App(app) => app.token(self.t, &self.repo, self.now)?,
        };
        let auth = format!("Bearer {token}");
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "sw-boat-runner"),
        ];
        let etag = self
            .cache
            .get(url)
            .filter(|_| method == "GET")
            .map(|c| c.etag.clone());
        if let Some(etag) = &etag {
            headers.push(("If-None-Match", etag.as_str()));
        }
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let bytes = body.map(|b| b.to_string().into_bytes());
        self.t
            .send(method, url, &headers, bytes.as_deref())
            .map_err(|e| Error::Network(e.0))
    }

    fn get<T: for<'de> Deserialize<'de>>(&mut self, path: &str) -> Result<T, Error> {
        let body = self.request("GET", path, None)?;
        serde_json::from_slice(&body).map_err(|e| Error::Parse(e.to_string()))
    }

    /// The repository's runners whose names start with `sbr-`.
    pub(crate) fn runners(&mut self) -> Result<Vec<GhRunner>, Error> {
        #[derive(Deserialize)]
        struct Page {
            runners: Vec<GhRunner>,
        }
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let p: Page = self.get(&format!("/actions/runners?per_page={PER_PAGE}&page={page}"))?;
            let last = p.runners.len() < PER_PAGE;
            out.extend(p.runners.into_iter().filter(|r| r.name.starts_with(PREFIX)));
            if last {
                return Ok(out);
            }
        }
        Err(Error::Parse(format!(
            "more than {MAX_PAGES} pages of runners"
        )))
    }

    /// Registers an ephemeral runner (one job) in the default group 1.
    pub(crate) fn generate_jitconfig(
        &mut self,
        name: &str,
        labels: &[String],
    ) -> Result<JitConfig, Error> {
        #[derive(Deserialize)]
        struct Out {
            encoded_jit_config: String,
        }
        let body = json!({ "name": name, "runner_group_id": 1, "labels": labels });
        let bytes = self.request("POST", "/actions/runners/generate-jitconfig", Some(&body))?;
        // serde errors can quote the input; this input holds a credential.
        let out: Out = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Parse("generate-jitconfig response".into()))?;
        Ok(JitConfig(out.encoded_jit_config))
    }

    pub(crate) fn delete_runner(&mut self, id: u64) -> Result<(), Error> {
        self.request("DELETE", &format!("/actions/runners/{id}"), None)
            .map(drop)
    }

    /// The queued jobs of one run's first 100 jobs.
    fn run_jobs(&mut self, run: u64) -> Result<Vec<(JobKey, Vec<String>)>, Error> {
        #[derive(Deserialize)]
        struct Jobs {
            jobs: Vec<Job>,
        }
        #[derive(Deserialize)]
        struct Job {
            id: u64,
            run_attempt: u32,
            status: String,
            labels: Vec<String>,
        }
        let jobs: Jobs = self.get(&jobs_path(run))?;
        Ok(jobs
            .jobs
            .into_iter()
            .filter(|j| j.status == "queued")
            .map(|j| {
                let key = JobKey {
                    id: j.id,
                    attempt: j.run_attempt,
                };
                (key, j.labels)
            })
            .collect())
    }

    /// Queued jobs of the first 100 queued and the first 100 in-progress runs; an
    /// in-progress run still queues its later jobs. `wanted` tells whether a pool serves a
    /// job's labels.
    ///
    /// A queued run's jobs are read every call; they rarely change, so most reads are
    /// free 304s. An in-progress run's jobs change with every step, so they are read when
    /// they can matter:
    /// - every call, while the run is new or re-run, or its last read found a queued job
    ///   a pool serves, so a job another runner took drops out at once;
    /// - with a budget token, when the run's `updated_at` changed since its last read, or
    ///   that read is `RUN_MAX_AGE_S` old, most changed and oldest first.
    ///
    /// Any other run keeps its last read, whose queued jobs no pool serves, so a stale read
    /// never puts a taken job in front of `decide`. A job a run queued since its last read
    /// is found by the next read, `RUN_MAX_AGE_S` later at most while the budget lasts.
    pub(crate) fn queued_jobs(
        &mut self,
        wanted: &dyn Fn(&[String]) -> bool,
    ) -> Result<Vec<QueuedJob>, Error> {
        #[derive(Deserialize)]
        struct Runs {
            workflow_runs: Vec<Run>,
        }
        #[derive(Deserialize)]
        struct Run {
            id: u64,
            #[serde(default)]
            run_attempt: u32,
            #[serde(default)]
            updated_at: String,
        }
        let mut out = BTreeMap::new();
        let queued: Runs = self.get(&format!("/actions/runs?status=queued&per_page={PER_PAGE}"))?;
        for run in queued.workflow_runs {
            out.extend(self.run_jobs(run.id)?);
        }

        let runs: Runs = self.get(&format!(
            "/actions/runs?status=in_progress&per_page={PER_PAGE}"
        ))?;
        let listed: HashSet<u64> = runs.workflow_runs.iter().map(|r| r.id).collect();
        self.runs.retain(|id, _| listed.contains(id));
        // (0 required, 1 changed, 2 old; last read; run)
        let mut todo: Vec<(u8, UnixSecs, &Run)> = Vec::new();
        for run in &runs.workflow_runs {
            let rank = match self.runs.get(&run.id) {
                None => (0, 0),
                Some(c) if c.attempt != run.run_attempt || c.wanted => (0, c.fetched),
                Some(c) if c.updated_at != run.updated_at => (1, c.fetched),
                Some(c) if self.now >= c.fetched.saturating_add(RUN_MAX_AGE_S) => (2, c.fetched),
                Some(_) => {
                    // Keep its ETag, so its next read may be a free 304.
                    let url = format!("{API}/repos/{}{}", self.repo, jobs_path(run.id));
                    if let Some(c) = self.cache.get_mut(&url) {
                        c.used = true;
                    }
                    continue;
                }
            };
            todo.push((rank.0, rank.1, run));
        }
        todo.sort_by_key(|&(rank, fetched, run)| (rank, fetched, run.id));
        for (rank, _, run) in todo {
            if rank > 0 && !self.budget.has_token() {
                break;
            }
            let queued = self.run_jobs(run.id)?;
            let read = RunJobs {
                attempt: run.run_attempt,
                updated_at: run.updated_at.clone(),
                fetched: self.now,
                wanted: queued.iter().any(|(_, labels)| wanted(labels)),
                queued,
            };
            self.runs.insert(run.id, read);
        }
        for read in self.runs.values() {
            out.extend(read.queued.iter().cloned());
        }
        Ok(out
            .into_iter()
            .map(|(key, labels)| QueuedJob { key, labels })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake::{Fake, respond};

    const RUNNERS: &str = "https://api.github.com/repos/o/r/actions/runners?per_page=100&page=1";

    #[test]
    fn runner_name_round_trip() {
        let name = runner_name("ci-big", "bx_22222222", 3, 1_700_000_000);
        assert_eq!(name, "sbr-ci-big-bx_22222222-3-1700000000");
        assert_eq!(
            parse_runner_name(&name),
            Some(RunnerName {
                pool: "ci-big".into(),
                machine: "bx_22222222".into(),
                slot: 3,
                registered_at: 1_700_000_000
            })
        );
        assert_eq!(parse_runner_name("sbr--bx_22222222-1-2"), None);
        assert_eq!(parse_runner_name("sbr-ci-bx_2222-1-2"), None);
        assert_eq!(parse_runner_name("sbr-ci-bx_22222222-x-2"), None);
        assert_eq!(parse_runner_name("ci-bx_22222222-1-2"), None);
        assert_eq!(parse_runner_name("sbr-bx_22222222-1-2"), None);
    }

    #[test]
    fn runner_state_rule() {
        assert_eq!(runner_state(true, true, true), RunnerState::Busy);
        assert_eq!(runner_state(true, false, false), RunnerState::Idle);
        assert_eq!(runner_state(false, false, true), RunnerState::Offline);
        assert_eq!(runner_state(false, false, false), RunnerState::Registered);
    }

    fn runner_json(id: u64, name: &str) -> Value {
        json!({"id": id, "name": name, "status": "online", "busy": false, "labels": []})
    }

    #[test]
    fn runners_paginate_and_keep_sbr() {
        let fake = Fake::new(|s| {
            let runners: Vec<Value> = if s.url.ends_with("page=1") {
                (0..100)
                    .map(|i| match i {
                        0 => runner_json(0, "someone-else"),
                        i => runner_json(i, &format!("sbr-ci-bx_22222222-{i}-1")),
                    })
                    .collect()
            } else {
                vec![runner_json(500, "sbr-ci-bx_33333333-0-1")]
            };
            respond(
                200,
                &[],
                &json!({"total_count": 101, "runners": runners}).to_string(),
            )
        });
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        let got = gh.runners().unwrap();
        assert_eq!(got.len(), 100);
        assert_eq!(got.last().unwrap().id, 500);
        let sent = fake.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].url, RUNNERS);
        assert!(sent[1].url.ends_with("page=2"));
        assert_eq!(sent[0].header("authorization"), Some("Bearer t"));
        assert_eq!(sent[0].header("x-github-api-version"), Some("2022-11-28"));
    }

    #[test]
    fn etag_304_reuses_body() {
        let fake = Fake::new(|s| match s.header("if-none-match") {
            Some("\"v1\"") => respond(304, &[], ""),
            _ => respond(
                200,
                &[("ETag", "\"v1\"")],
                &json!({"runners": [runner_json(1, "sbr-ci-bx_22222222-0-1")]}).to_string(),
            ),
        });
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        gh.begin_tick(1);
        let first = gh.runners().unwrap();
        gh.begin_tick(2);
        let second = gh.runners().unwrap();
        assert_eq!(first, second);
        assert_eq!(second.len(), 1);
        let sent = fake.sent();
        assert_eq!(sent[0].header("if-none-match"), None);
        assert_eq!(sent[1].header("if-none-match"), Some("\"v1\""));

        // A tick that does not use the URL drops it from the cache.
        gh.begin_tick(3);
        gh.begin_tick(4);
        gh.runners().unwrap();
        assert_eq!(fake.sent()[2].header("if-none-match"), None);
    }

    #[test]
    fn rate_floor_stops_requests_until_reset() {
        let fake = Fake::new(|_| {
            respond(
                200,
                &[
                    ("X-RateLimit-Remaining", "150"),
                    ("X-RateLimit-Reset", "2000"),
                ],
                r#"{"runners":[]}"#,
            )
        });
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        gh.begin_tick(1000);
        assert!(gh.runners().unwrap().is_empty());
        assert_eq!(
            gh.queued_jobs(&|_| true),
            Err(Error::RateLimited {
                remaining: 150,
                reset: 2000
            })
        );
        assert_eq!(fake.sent().len(), 1);
        gh.begin_tick(2000);
        gh.runners().unwrap();
        assert_eq!(fake.sent().len(), 2);
    }

    #[test]
    fn maps_errors() {
        let fake = Fake::new(|s| {
            if s.url.contains("page=1") {
                respond(
                    403,
                    &[("X-RateLimit-Remaining", "0"), ("X-RateLimit-Reset", "77")],
                    "{}",
                )
            } else if s.method == "DELETE" {
                respond(404, &[], r#"{"message":"Not Found"}"#)
            } else {
                respond(401, &[], r#"{"message":"Bad credentials"}"#)
            }
        });
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        assert_eq!(
            gh.runners(),
            Err(Error::RateLimited {
                remaining: 0,
                reset: 77
            })
        );
        gh.begin_tick(77);
        assert_eq!(gh.delete_runner(5), Err(Error::NotFound));
        let err = gh.generate_jitconfig("n", &[]).err().unwrap();
        assert!(err.is_fatal());
    }

    /// A secondary rate limit without headers waits a minute instead of ending the daemon.
    #[test]
    fn secondary_rate_limit_is_not_unauthorized() {
        let fake = Fake::new(|_| {
            respond(
                403,
                &[("X-RateLimit-Remaining", "4000")],
                r#"{"message":"You have exceeded a secondary rate limit."}"#,
            )
        });
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        gh.begin_tick(100);
        let limited = Error::RateLimited {
            remaining: 0,
            reset: 160,
        };
        assert_eq!(gh.runners().unwrap_err(), limited);
        assert_eq!(gh.queued_jobs(&|_| true).unwrap_err(), limited);
        assert_eq!(fake.sent().len(), 1);

        let fake = Fake::new(|_| respond(429, &[], "{}"));
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        assert!(matches!(gh.runners(), Err(Error::RateLimited { .. })));
    }

    #[test]
    fn queued_jobs_from_runs() {
        let fake = Fake::new(|s| {
            let body = if s.url.contains("status=queued") {
                json!({"workflow_runs": [{"id": 5, "run_attempt": 2}]})
            } else if s.url.contains("status=in_progress") {
                json!({"workflow_runs": [{"id": 6, "run_attempt": 1}]})
            } else if s.url.contains("/runs/5/jobs") {
                json!({"jobs": [
                    {"id": 50, "run_attempt": 2, "status": "queued", "labels": ["self-hosted", "large"]},
                    {"id": 51, "run_attempt": 2, "status": "completed", "labels": ["self-hosted"]}
                ]})
            } else {
                json!({"jobs": [
                    {"id": 60, "run_attempt": 1, "status": "in_progress", "labels": ["self-hosted"]},
                    {"id": 61, "run_attempt": 1, "status": "queued", "labels": ["self-hosted"]}
                ]})
            };
            respond(200, &[], &body.to_string())
        });
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        let jobs = gh.queued_jobs(&|_| true).unwrap();
        assert_eq!(
            jobs,
            vec![
                QueuedJob {
                    key: JobKey { id: 50, attempt: 2 },
                    labels: vec!["self-hosted".into(), "large".into()]
                },
                QueuedJob {
                    key: JobKey { id: 61, attempt: 1 },
                    labels: vec!["self-hosted".into()]
                },
            ]
        );
    }

    #[test]
    fn jitconfig_request() {
        let fake = Fake::new(|_| {
            respond(
                201,
                &[],
                r#"{"runner":{"id":9,"name":"n"},"encoded_jit_config":"SECRET"}"#,
            )
        });
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        let jit = gh
            .generate_jitconfig("sbr-ci-bx_22222222-0-1", &["self-hosted".into()])
            .unwrap();
        assert_eq!(jit.expose(), "SECRET");
        let sent = fake.sent();
        assert_eq!(
            sent[0].url,
            "https://api.github.com/repos/o/r/actions/runners/generate-jitconfig"
        );
        let body: Value = serde_json::from_slice(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"name": "sbr-ci-bx_22222222-0-1", "runner_group_id": 1, "labels": ["self-hosted"]})
        );
    }

    /// An installation token GitHub answers with 401 is minted again and the request sent
    /// once more; a second 401 is fatal.
    #[test]
    fn an_app_token_is_renewed_after_a_401() {
        let fake = |revoked: &'static [&'static str]| {
            let minted = std::cell::Cell::new(0);
            Fake::new(move |s| {
                if s.url.ends_with("/app/installations/77/access_tokens") {
                    minted.set(minted.get() + 1);
                    let body = format!(
                        r#"{{"token":"ghs_{}","expires_at":"2099-01-01T00:00:00Z"}}"#,
                        minted.get()
                    );
                    return respond(201, &[], &body);
                }
                let token = s.header("authorization").unwrap_or_default();
                if revoked.iter().any(|r| token.ends_with(r)) {
                    respond(401, &[], r#"{"message":"Bad credentials"}"#)
                } else {
                    respond(200, &[], r#"{"runners":[]}"#)
                }
            })
        };
        let app = |fake| {
            let app = crate::github_app::tests::test_app(Some(77));
            Client::new(fake, Auth::App(Box::new(app)), "o/r".into())
        };
        let renewed = fake(&["ghs_1"]);
        let mut gh = app(&renewed);
        gh.begin_tick(T0);
        gh.runners().unwrap();
        gh.runners().unwrap();
        let sent: Vec<(String, String)> = renewed
            .sent()
            .iter()
            .map(|s| {
                let path = s.url.trim_start_matches(API).to_string();
                let auth = s.header("authorization").unwrap().to_string();
                (
                    path,
                    if auth.contains("ghs_") {
                        auth
                    } else {
                        "jwt".into()
                    },
                )
            })
            .collect();
        let runners = "/repos/o/r/actions/runners?per_page=100&page=1";
        let tokens = "/app/installations/77/access_tokens";
        let want = [
            (tokens, "jwt"),
            (runners, "Bearer ghs_1"),
            (tokens, "jwt"),
            (runners, "Bearer ghs_2"),
            (runners, "Bearer ghs_2"),
        ];
        let want: Vec<(String, String)> = want
            .iter()
            .map(|(p, a)| (p.to_string(), a.to_string()))
            .collect();
        assert_eq!(sent, want);

        let refused = fake(&["ghs_1", "ghs_2"]);
        let mut gh = app(&refused);
        gh.begin_tick(T0);
        let err = gh.runners().unwrap_err();
        assert_eq!(err, Error::Unauthorized { status: 401 });
        assert!(err.is_fatal());
        assert_eq!(refused.sent().len(), 4);
    }

    /// A repository with `runs` in-progress runs, each with one running job. `queued_in`
    /// adds a queued job a pool serves to a run, `touched` changes a run's `updated_at`.
    struct Busy {
        fake: Fake,
        state: std::rc::Rc<std::cell::RefCell<BusyState>>,
    }

    #[derive(Default)]
    struct BusyState {
        queued_in: Option<u64>,
        touched: Option<u64>,
    }

    const T0: UnixSecs = 1_000_000;
    const TICK_S: UnixSecs = 15;

    impl Busy {
        fn new(runs: u64, left: u64) -> Self {
            let state = std::rc::Rc::new(std::cell::RefCell::new(BusyState::default()));
            let st = state.clone();
            let fake = Fake::new(move |s| {
                let st = st.borrow();
                let body = if s.url.contains("status=queued") {
                    json!({"workflow_runs": []})
                } else if s.url.contains("status=in_progress") {
                    let list: Vec<Value> = (1..=runs)
                        .map(|id| {
                            let at = if st.touched == Some(id) { "t2" } else { "t1" };
                            json!({"id": id, "run_attempt": 1, "updated_at": at})
                        })
                        .collect();
                    json!({ "workflow_runs": list })
                } else {
                    let id: u64 = s.url.split('/').nth(8).unwrap().parse().unwrap();
                    let mut jobs = vec![json!({"id": id * 10, "run_attempt": 1,
                        "status": "in_progress", "labels": ["self-hosted", "large"]})];
                    if st.queued_in == Some(id) {
                        jobs.push(json!({"id": id * 10 + 1, "run_attempt": 1,
                            "status": "queued", "labels": ["self-hosted", "large"]}));
                    }
                    json!({ "jobs": jobs })
                };
                let left = left.to_string();
                let reset = (T0 + 7_200).to_string();
                let headers = [
                    ("X-RateLimit-Limit", "5000"),
                    ("X-RateLimit-Remaining", left.as_str()),
                    ("X-RateLimit-Reset", reset.as_str()),
                ];
                respond(200, &headers, &body.to_string())
            });
            Busy { fake, state }
        }

        /// One tick as the loop runs it: the requests it sent and the queued job ids found,
        /// or `None` when the budget skipped the observe.
        fn tick(&self, gh: &mut Client, t: u64) -> (usize, Option<Vec<u64>>) {
            let before = self.fake.sent().len();
            gh.begin_tick(T0 + t * TICK_S);
            let found = gh.may_observe().then(|| {
                let large = |l: &[String]| l.iter().any(|l| l == "large");
                let jobs = gh.queued_jobs(&large).unwrap();
                jobs.iter().map(|j| j.key.id).collect()
            });
            (self.fake.sent().len() - before, found)
        }
    }

    /// The old loop read all 30 runs' jobs every tick: 32 requests a tick, 7,680 an hour at
    /// `tick_s` 15. Now the first tick reads them all once, and after that the budget holds
    /// the hour to a quarter of the 5,000 limit.
    #[test]
    fn thirty_in_progress_runs_stay_in_budget() {
        let busy = Busy::new(30, 4_000);
        let mut gh = Client::new(&busy.fake, Auth::Token("t".into()), "o/r".into());
        let per_tick: Vec<usize> = (0..240).map(|t| busy.tick(&mut gh, t).0).collect();
        assert_eq!(per_tick[0], 32);
        let hour: usize = per_tick.iter().sum();
        eprintln!("first ticks {:?}, hour {hour}", &per_tick[..12]);
        // 1,250 an hour, the 60 s the bucket starts with, and the first tick's required reads.
        assert!(hour <= 1_250 + 21 + 32, "{hour} requests in an hour");
        assert!(hour >= 1_100, "{hour} requests in an hour");
    }

    /// Ticks until a job queued in `run` after `settle` ticks is found, for a run that
    /// looks unchanged.
    fn ticks_to_find(runs: u64, run: u64, settle: u64) -> u64 {
        let busy = Busy::new(runs, 4_000);
        let mut gh = Client::new(&busy.fake, Auth::Token("t".into()), "o/r".into());
        for t in 0..settle {
            busy.tick(&mut gh, t);
        }
        busy.state.borrow_mut().queued_in = Some(run);
        let found = (settle..settle + 100)
            .find(|&t| busy.tick(&mut gh, t).1.is_some_and(|f| f == [run * 10 + 1]))
            .expect("found");
        found - settle + 1
    }

    /// The most ticks over every run and every phase of the budget.
    fn worst_ticks_to_find(runs: u64) -> u64 {
        (1..=runs)
            .flat_map(|run| (40..52).map(move |settle| ticks_to_find(runs, run, settle)))
            .max()
            .unwrap()
    }

    /// An in-progress run whose `updated_at` does not move still has its newly queued job
    /// found: on the first tick `RUN_MAX_AGE_S` after its last read when few runs share the budget, and within
    /// the time the budget takes to read every run when 30 do.
    #[test]
    fn a_newly_queued_job_in_a_seen_run_is_found() {
        let one = worst_ticks_to_find(1);
        assert_eq!(one, RUN_MAX_AGE_S / TICK_S);
        let thirty = worst_ticks_to_find(30);
        eprintln!("worst ticks to find: 1 run {one}, 30 runs {thirty}");
        assert!(thirty <= 12, "{thirty} ticks");
    }

    /// A changed `updated_at` is read on the next tick, and a run with a queued job a pool
    /// serves is read every tick, so the job drops out once it is taken.
    #[test]
    fn changed_and_wanted_runs_are_read_at_once() {
        let busy = Busy::new(30, 4_000);
        let mut gh = Client::new(&busy.fake, Auth::Token("t".into()), "o/r".into());
        for t in 0..40 {
            busy.tick(&mut gh, t);
        }
        busy.state.borrow_mut().queued_in = Some(7);
        busy.state.borrow_mut().touched = Some(7);
        let mut t = 40;
        while busy.tick(&mut gh, t).1.is_none() {
            t += 1;
        }
        assert_eq!(t, 40);
        let reads = |busy: &Busy| {
            busy.fake
                .sent()
                .iter()
                .filter(|s| s.url.contains("/runs/7/jobs"))
                .count()
        };
        for t in 41..45 {
            let before = reads(&busy);
            let (_, found) = busy.tick(&mut gh, t);
            if found.is_some() {
                assert_eq!(reads(&busy), before + 1);
            }
        }
        busy.state.borrow_mut().queued_in = None;
        let found = (45..60).find_map(|t| busy.tick(&mut gh, t).1).unwrap();
        assert!(found.is_empty());
    }

    /// Under `RESERVE` requests left the daemon observes once every `PACE_MAX_S`, so
    /// cleanup goes on, until `RATE_FLOOR` stops every request.
    #[test]
    fn a_low_budget_stretches_observation() {
        let busy = Busy::new(30, 450);
        let mut gh = Client::new(&busy.fake, Auth::Token("t".into()), "o/r".into());
        let observed: Vec<u64> = (0..80)
            .filter(|&t| busy.tick(&mut gh, t).1.is_some())
            .collect();
        let pace = PACE_MAX_S / TICK_S;
        assert_eq!(observed, [0, pace, 2 * pace, 3 * pace]);
    }

    /// A run left unread keeps its ETag, so its next read is a free 304 when nothing changed.
    #[test]
    fn an_unread_run_keeps_its_etag() {
        let fake = Fake::new(|s| {
            if s.url.contains("/jobs") && s.header("if-none-match") == Some("\"j\"") {
                return respond(304, &[], "");
            }
            let body = if s.url.contains("status=in_progress") {
                json!({"workflow_runs": [{"id": 1, "run_attempt": 1, "updated_at": "t1"}]})
            } else if s.url.contains("status=queued") {
                json!({"workflow_runs": []})
            } else {
                json!({"jobs": [{"id": 10, "run_attempt": 1, "status": "in_progress", "labels": []}]})
            };
            respond(200, &[("ETag", "\"j\"")], &body.to_string())
        });
        let mut gh = Client::new(&fake, Auth::Token("t".into()), "o/r".into());
        for t in 0..=RUN_MAX_AGE_S / TICK_S {
            gh.begin_tick(T0 + t * TICK_S);
            gh.queued_jobs(&|_| true).unwrap();
        }
        let reads: Vec<_> = fake
            .sent()
            .into_iter()
            .filter(|s| s.url.contains("/runs/1/jobs"))
            .map(|s| s.header("if-none-match").map(str::to_string))
            .collect();
        assert_eq!(reads, [None, Some("\"j\"".to_string())]);
    }
}
