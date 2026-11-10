//! GitHub REST client for one repository's self-hosted runners and queued jobs.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::boat;
use crate::model::{JobKey, QueuedJob, RunnerState, UnixSecs};
use crate::run::Transport;

pub const API: &str = "https://api.github.com";
/// Every runner this tool registers has a name starting with this.
pub const PREFIX: &str = "sbr-";
/// Requests stop for the rest of the rate-limit window once fewer than this many remain,
/// so a human with the same token keeps some budget.
pub const RATE_FLOOR: u64 = 200;
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
    Network(String),
    Status {
        status: u16,
        message: String,
    },
    Parse(String),
}

impl Error {
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::Unauthorized { .. })
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
            Error::Network(e) => write!(f, "GitHub network error: {e}"),
            Error::Status { status, message } => write!(f, "GitHub error {status}: {message}"),
            Error::Parse(e) => write!(f, "GitHub response not understood: {e}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GhRunner {
    pub id: u64,
    pub name: String,
    /// `online` or `offline`.
    pub status: String,
    pub busy: bool,
}

/// A just-in-time runner config. It is a credential: no `Debug`, no `Display`, and the
/// only way out is `expose`.
pub struct JitConfig(String);

impl JitConfig {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// `sbr-<pool>-<machine id>-<slot>-<registered unix time>`.
pub fn runner_name(pool: &str, machine: &str, slot: u32, registered_at: UnixSecs) -> String {
    format!("{PREFIX}{pool}-{machine}-{slot}-{registered_at}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerName {
    pub pool: String,
    pub machine: String,
    pub slot: u32,
    pub registered_at: UnixSecs,
}

/// Inverse of `runner_name`. Splits from the right, because a pool name may contain `-`
/// and a boat id never does.
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
pub fn runner_state(online: bool, busy: bool, seen_online: bool) -> RunnerState {
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

struct Cached {
    etag: String,
    body: Vec<u8>,
    used: bool,
}

pub struct Client<'t> {
    t: &'t dyn Transport,
    token: String,
    repo: String,
    now: UnixSecs,
    /// Bodies by URL with their ETag. A 304 answer to `If-None-Match` does not count
    /// against the rate limit.
    cache: HashMap<String, Cached>,
    /// Set once `X-RateLimit-Remaining` drops under `RATE_FLOOR`: (remaining, reset).
    limited: Option<(u64, UnixSecs)>,
}

impl<'t> Client<'t> {
    pub fn new(t: &'t dyn Transport, token: String, repo: String) -> Self {
        Client {
            t,
            token,
            repo,
            now: 0,
            cache: HashMap::new(),
            limited: None,
        }
    }

    /// Sets the clock and drops cached bodies the previous tick did not use, so the
    /// cache holds at most one tick's URLs.
    pub fn begin_tick(&mut self, now: UnixSecs) {
        self.now = now;
        self.cache.retain(|_, c| c.used);
        for c in self.cache.values_mut() {
            c.used = false;
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
        let auth = format!("Bearer {}", self.token);
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "sw-boat-runner"),
        ];
        let etag = self
            .cache
            .get(&url)
            .filter(|_| cacheable)
            .map(|c| c.etag.clone());
        if let Some(etag) = &etag {
            headers.push(("If-None-Match", etag.as_str()));
        }
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let bytes = body.map(|b| b.to_string().into_bytes());
        let resp = self
            .t
            .send(method, &url, &headers, bytes.as_deref())
            .map_err(|e| Error::Network(e.0))?;

        let num = |name: &str| resp.header(name).and_then(|v| v.trim().parse::<u64>().ok());
        let remaining = num("x-ratelimit-remaining");
        let reset = num("x-ratelimit-reset").unwrap_or(self.now.saturating_add(60));
        if let Some(r) = remaining.filter(|&r| r < RATE_FLOOR) {
            self.limited = Some((r, reset));
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
            403 | 429 if remaining == Some(0) || resp.header("retry-after").is_some() => {
                let reset = num("retry-after").map_or(reset, |s| self.now.saturating_add(s));
                self.limited = Some((0, reset));
                Err(Error::RateLimited {
                    remaining: 0,
                    reset,
                })
            }
            401 | 403 => Err(Error::Unauthorized {
                status: resp.status,
            }),
            404 => Err(Error::NotFound),
            status => {
                let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                let message = v.get("message").and_then(Value::as_str).unwrap_or("");
                Err(Error::Status {
                    status,
                    message: message.to_string(),
                })
            }
        }
    }

    fn get<T: for<'de> Deserialize<'de>>(&mut self, path: &str) -> Result<T, Error> {
        let body = self.request("GET", path, None)?;
        serde_json::from_slice(&body).map_err(|e| Error::Parse(e.to_string()))
    }

    /// The repository's runners whose names start with `sbr-`.
    pub fn runners(&mut self) -> Result<Vec<GhRunner>, Error> {
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
    pub fn generate_jitconfig(
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

    pub fn delete_runner(&mut self, id: u64) -> Result<(), Error> {
        self.request("DELETE", &format!("/actions/runners/{id}"), None)
            .map(drop)
    }

    /// Queued jobs of the first 100 queued and the first 100 in-progress runs; an
    /// in-progress run still queues its later jobs.
    pub fn queued_jobs(&mut self) -> Result<Vec<QueuedJob>, Error> {
        #[derive(Deserialize)]
        struct Runs {
            workflow_runs: Vec<Run>,
        }
        #[derive(Deserialize)]
        struct Run {
            id: u64,
        }
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
        let mut out = BTreeMap::new();
        for status in ["queued", "in_progress"] {
            let runs: Runs = self.get(&format!(
                "/actions/runs?status={status}&per_page={PER_PAGE}"
            ))?;
            for run in runs.workflow_runs {
                let jobs: Jobs = self.get(&format!(
                    "/actions/runs/{}/jobs?per_page={PER_PAGE}",
                    run.id
                ))?;
                for job in jobs.jobs.into_iter().filter(|j| j.status == "queued") {
                    let key = JobKey {
                        id: job.id,
                        attempt: job.run_attempt,
                    };
                    out.insert(key, job.labels);
                }
            }
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
    use crate::run::fake::{Fake, respond};

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
        let mut gh = Client::new(&fake, "t".into(), "o/r".into());
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
        let mut gh = Client::new(&fake, "t".into(), "o/r".into());
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
        let mut gh = Client::new(&fake, "t".into(), "o/r".into());
        gh.begin_tick(1000);
        assert!(gh.runners().unwrap().is_empty());
        assert_eq!(
            gh.queued_jobs(),
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
        let mut gh = Client::new(&fake, "t".into(), "o/r".into());
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
        let mut gh = Client::new(&fake, "t".into(), "o/r".into());
        let jobs = gh.queued_jobs().unwrap();
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
        let mut gh = Client::new(&fake, "t".into(), "o/r".into());
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
}
