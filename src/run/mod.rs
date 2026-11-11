//! The tick loop. Each tick is three steps over plain data:
//!
//! 1. `observe` (observe.rs) reads boat and GitHub into a `Snapshot`;
//! 2. `plan` asks `decide` for each pool's actions;
//! 3. `act` (act.rs) carries them out, cheapest first.
//!
//! What a tick leaves for the next has two homes, each written by one module only:
//! `observe::Memory` holds the facts about machines and runners that boat and GitHub do not
//! report (docs/design.md#observed-state), and `act::Ledger` the start budget and back-off
//! (docs/design.md#start-budget).

mod act;
mod observe;
mod script;
#[cfg(test)]
mod testkit;

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::boat;
use crate::github;
use crate::github_app;
use crate::model::{Action, Cents, Config, MachineState, Pool, Snapshot, UnixSecs};
use crate::transport::{Transport, Ureq};

pub(crate) use script::HOME_WAIT_S;

/// boat's file write and command calls wait up to 60 s for a machine that is starting. Also
/// how long after a restart a create sent before the crash may still be in flight.
const HTTP_TIMEOUT: Duration = Duration::from_secs(90);
/// The "pacing" line is logged at most once in this many seconds: pacing is normal,
/// so a minute-by-minute line would only be noise.
const PACED_LOG_S: UnixSecs = 600;

#[derive(Debug)]
pub enum Error {
    MissingEnv(&'static str),
    /// Credentials set but unusable: both GitHub modes, or an App key that does not load.
    Config(String),
    Boat(boat::Error),
    GitHub(github::Error),
    /// A start after starts closed for this tick.
    StartsClosed,
    /// An action naming a pool or machine the snapshot does not have.
    Invalid(String),
}

impl Error {
    fn is_fatal(&self) -> bool {
        match self {
            Error::MissingEnv(_) | Error::Config(_) => true,
            Error::Boat(e) => e.is_fatal(),
            Error::GitHub(e) => e.is_fatal(),
            Error::StartsClosed | Error::Invalid(_) => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::MissingEnv(name) => write!(f, "{name} is not set"),
            Error::Config(m) => write!(f, "{m}"),
            Error::Boat(e) => write!(f, "{e}"),
            Error::GitHub(e) => write!(f, "{e}"),
            Error::StartsClosed => write!(f, "starts closed for this tick"),
            Error::Invalid(m) => write!(f, "invalid action: {m}"),
        }
    }
}

impl From<boat::Error> for Error {
    fn from(e: boat::Error) -> Self {
        Error::Boat(e)
    }
}

impl From<github::Error> for Error {
    fn from(e: github::Error) -> Self {
        Error::GitHub(e)
    }
}

pub fn run(cfg: &Config, decide: impl Fn(&Snapshot, &Pool) -> Vec<Action>) -> Result<(), Error> {
    let transport = Ureq::new(HTTP_TIMEOUT);
    let mut tick_loop = Loop::connect(cfg, &transport, unix_now())?;
    // Once, and only advice: a failed read changes nothing.
    match tick_loop.boat.saved_snapshots(None) {
        Ok(saved) => {
            for warning in crate::snapshot::oversized(&cfg.pools, &saved) {
                eprintln!("warning: {warning}");
            }
        }
        Err(e) => eprintln!("saved snapshots: {e}"),
    }
    let period = Duration::from_secs(cfg.tick_s.max(1));
    loop {
        let started = Instant::now();
        tick_loop.tick(unix_now(), &decide)?;
        std::thread::sleep(period.saturating_sub(started.elapsed()));
    }
}

/// One observe with `run`'s credentials and no writes, for `plan` without a snapshot file.
pub fn observe_once(cfg: &Config) -> Result<Snapshot, Error> {
    let transport = Ureq::new(HTTP_TIMEOUT);
    let now = unix_now();
    // Started a request timeout ago, so the snapshot shows the starts `run` makes once its
    // restart wait is over.
    let since = now.saturating_sub(HTTP_TIMEOUT.as_secs());
    Ok(Loop::connect(cfg, &transport, since)?
        .observe(now)?
        .snapshot)
}

/// Deletes every machine and runner of the policy's pools, for taking a pool out of use.
/// A machine whose runner is running a job is left alone. Returns the machines still
/// there, so a caller can repeat it until it returns 0.
pub fn drain(cfg: &Config) -> Result<usize, Error> {
    let transport = Ureq::new(HTTP_TIMEOUT);
    let now = unix_now();
    Loop::connect(cfg, &transport, now)?.drain(now)
}

/// `drain`'s stand-in for `decide`.
fn delete_everything(snap: &Snapshot, pool: &Pool) -> Vec<Action> {
    let runners = snap.runners.iter().filter(|r| r.pool == pool.name);
    let machines = snap
        .machines
        .iter()
        .filter(|m| m.pool == pool.name && m.state != MachineState::Gone);
    runners
        .map(|r| Action::Unregister {
            runner: r.name.clone(),
        })
        .chain(machines.map(|m| Action::Delete {
            machine: m.id.clone(),
        }))
        .collect()
}

const APP_VARS: [&str; 3] = [
    "GITHUB_APP_ID",
    "GITHUB_APP_INSTALLATION_ID",
    "GITHUB_APP_PRIVATE_KEY_FILE",
];

/// `GITHUB_TOKEN`, or a GitHub App from `GITHUB_APP_ID`, `GITHUB_APP_PRIVATE_KEY_FILE` and,
/// when set, `GITHUB_APP_INSTALLATION_ID`. Both at once is an error, so the daemon never
/// quietly runs on a token someone meant to retire.
fn github_auth(var: &dyn Fn(&str) -> Option<String>) -> Result<github::Auth, Error> {
    let app = APP_VARS.iter().any(|name| var(name).is_some());
    match (var("GITHUB_TOKEN"), app) {
        (Some(_), true) => Err(Error::Config(
            "GITHUB_TOKEN and GITHUB_APP_* are both set; set one of them".into(),
        )),
        (Some(token), false) => Ok(github::Auth::Token(token)),
        (None, false) => Err(Error::Config(
            "GITHUB_TOKEN is not set (or set GITHUB_APP_ID and GITHUB_APP_PRIVATE_KEY_FILE)".into(),
        )),
        (None, true) => {
            let id = var("GITHUB_APP_ID").ok_or(Error::MissingEnv("GITHUB_APP_ID"))?;
            let key_file = var("GITHUB_APP_PRIVATE_KEY_FILE")
                .ok_or(Error::MissingEnv("GITHUB_APP_PRIVATE_KEY_FILE"))?;
            let installation = var("GITHUB_APP_INSTALLATION_ID")
                .map(|v| {
                    v.trim().parse::<u64>().map_err(|_| {
                        Error::Config(format!("GITHUB_APP_INSTALLATION_ID {v:?} is not a number"))
                    })
                })
                .transpose()?;
            let app = github_app::App::from_file(id, installation, Path::new(&key_file))
                .map_err(|e| Error::Config(format!("GITHUB_APP_PRIVATE_KEY_FILE {e}")))?;
            Ok(github::Auth::App(Box::new(app)))
        }
    }
}

fn env(name: &'static str) -> Result<String, Error> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or(Error::MissingEnv(name))
}

fn unix_now() -> UnixSecs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// For each queued job, the index of the first pool in the file that serves it.
fn route(cfg: &Config, snap: &Snapshot) -> Vec<Option<usize>> {
    snap.queued
        .iter()
        .map(|j| cfg.pools.iter().position(|p| p.serves(&j.labels)))
        .collect()
}

/// The actions `decide` asks for across all pools, in file order. Each pool sees only the
/// queued jobs it is the first in the file to serve, so two pools never fork for one job,
/// and sees the spend and included starts of the starts earlier pools plan, so no pool
/// overruns its cap.
pub fn plan<'c>(
    cfg: &'c Config,
    snap: &Snapshot,
    decide: impl Fn(&Snapshot, &Pool) -> Vec<Action>,
) -> Vec<(&'c Pool, Action)> {
    let owners = route(cfg, snap);
    // One copy for every pool: only `queued` and `limits` differ between them.
    let mut routed = snap.clone();
    let mut out = Vec::new();
    for (i, pool) in cfg.pools.iter().enumerate() {
        routed.queued = snap
            .queued
            .iter()
            .zip(&owners)
            .filter(|(_, owner)| **owner == Some(i))
            .map(|(job, _)| job.clone())
            .collect();
        let actions = decide(&routed, pool);
        let starts = actions
            .iter()
            .filter(|a| matches!(a, Action::Resume { .. } | Action::Fork { .. }))
            .count();
        let starts = Cents::try_from(starts).unwrap_or(Cents::MAX);
        let limits = &mut routed.limits;
        limits.spent_today_cents = limits
            .spent_today_cents
            .saturating_add(starts.saturating_mul(snap.limits.next_start_cents));
        limits.free_starts_left = limits.free_starts_left.map(|n| n.saturating_sub(starts));
        out.extend(actions.into_iter().map(|a| (pool, a)));
    }
    out
}

pub struct Loop<'a> {
    cfg: &'a Config,
    boat: boat::Client<'a>,
    gh: github::Client<'a>,
    memory: observe::Memory,
    ledger: act::Ledger,
    /// False for `drain`, which needs no queued jobs.
    read_jobs: bool,
    /// When a tick last logged that it skipped its observe to stay in the GitHub request
    /// budget. Ticks that skip and ticks that observe can alternate, so the line is logged
    /// at most once every `PACED_LOG_S`.
    paced_logged: Option<UnixSecs>,
}

impl<'a> Loop<'a> {
    /// A loop for a process that started at `now`.
    pub fn new(
        cfg: &'a Config,
        t: &'a dyn Transport,
        boat_key: String,
        github: github::Auth,
        now: UnixSecs,
    ) -> Self {
        Loop {
            cfg,
            boat: boat::Client::new(t, boat_key),
            gh: github::Client::new(t, github, cfg.repo.clone()),
            memory: observe::Memory::default(),
            ledger: act::Ledger::new(cfg, now),
            read_jobs: true,
            paced_logged: None,
        }
    }

    /// A loop with the credentials in `BOAT_API_KEY`, and `GITHUB_TOKEN` or the
    /// `GITHUB_APP_*` variables.
    fn connect(cfg: &'a Config, t: &'a dyn Transport, now: UnixSecs) -> Result<Self, Error> {
        let boat_key = env("BOAT_API_KEY")?;
        let github = github_auth(&|name| std::env::var(name).ok().filter(|v| !v.is_empty()))?;
        Ok(Loop::new(cfg, t, boat_key, github, now))
    }

    /// One observe-decide-act pass. Returns `Err` only for errors no later tick can fix.
    pub fn tick(
        &mut self,
        now: UnixSecs,
        decide: impl Fn(&Snapshot, &Pool) -> Vec<Action>,
    ) -> Result<(), Error> {
        self.ledger.begin_tick(now);
        self.gh.begin_tick(now);
        // A tick without budget acts on nothing rather than on a stale snapshot; see
        // docs/design.md#github-requests.
        if !self.gh.may_observe() {
            if self
                .paced_logged
                .is_none_or(|t| now >= t.saturating_add(PACED_LOG_S))
            {
                eprintln!("observe: {}; skipping this tick", self.gh.budget_note());
                self.paced_logged = Some(now);
            }
            return Ok(());
        }
        let observed = match self.read(now) {
            Ok(v) => v,
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => {
                eprintln!("observe: {e}");
                return Ok(());
            }
        };
        let actions = plan(self.cfg, &observed.snapshot, decide)
            .into_iter()
            .map(|(_, a)| a)
            .collect();
        self.act(now, observed, actions)
    }

    /// `drain`'s one tick, then the machines left.
    fn drain(&mut self, now: UnixSecs) -> Result<usize, Error> {
        // `delete_everything` ignores queued jobs, so drain does not spend requests on them.
        self.read_jobs = false;
        self.tick(now, delete_everything)?;
        let after = self.observe(now)?.snapshot;
        let left = after
            .machines
            .iter()
            .filter(|m| m.state != MachineState::Gone);
        Ok(left.count())
    }

    fn pool(&self, name: &str) -> Result<&'a Pool, Error> {
        self.cfg
            .pools
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| Error::Invalid(format!("unknown pool {name}")))
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::decide::decide;
    use crate::model::{JobKey, Limits, QueuedJob, RunnerState};
    use crate::transport::fake::{Fake, respond};

    /// Returns every kind of action, out of order, and checks the snapshot it was given.
    fn decide_all(s: &Snapshot, p: &Pool) -> Vec<Action> {
        assert_eq!(p.name, "ci");
        assert_eq!(s.now, NOW);
        let states: Vec<_> = s
            .machines
            .iter()
            .map(|m| (m.id.as_str(), m.state))
            .collect();
        assert_eq!(
            states,
            [
                ("bx_22222222", MachineState::Up),
                ("bx_33333333", MachineState::Stopped),
                ("bx_44444444", MachineState::Up)
            ]
        );
        assert_eq!(s.machines[2].job, Some(JobKey { id: 7, attempt: 2 }));
        assert!(s.machines.iter().all(|m| m.since == NOW));
        // Only bx_44444444 has a runner that may still pick up a job.
        let busy: Vec<_> = s.machines.iter().map(|m| m.last_busy).collect();
        assert_eq!(busy, [NOW - 1, NOW - 1, NOW]);
        let runners: Vec<_> = s
            .runners
            .iter()
            .map(|r| (r.name.as_str(), r.state))
            .collect();
        assert_eq!(
            runners,
            [
                ("sbr-ci-bx_22222222-0-900", RunnerState::Idle),
                ("sbr-ci-bx_44444444-0-900", RunnerState::Registered)
            ]
        );
        assert_eq!(s.queued[0].key, JobKey { id: 70, attempt: 1 });
        assert_eq!(
            s.limits,
            Limits {
                can_start: true,
                next_start_cents: 25,
                // A fresh process assumes every pool's cap is spent.
                spent_today_cents: 100,
                free_starts_left: None,
            }
        );
        vec![
            fork(Some(70)),
            resume("bx_33333333"),
            register("bx_22222222", 1),
            stop("bx_22222222"),
            delete("bx_44444444"),
            unregister("sbr-ci-bx_44444444-0-900"),
        ]
    }

    /// The reads every tick makes for the one pool of `config`, with one queued run.
    fn observe_requests() -> [String; 6] {
        [
            format!("GET {BOAT}/sandboxes?label=sw-boat-runner%3Dci&limit=200"),
            format!("GET {GH}/runners?per_page=100&page=1"),
            format!("GET {GH}/runs?status=queued&per_page=100"),
            format!("GET {GH}/runs/5/jobs?per_page=100"),
            format!("GET {GH}/runs?status=in_progress&per_page=100"),
            format!("GET {BOAT}/limits"),
        ]
    }

    #[test]
    fn tick_executes_in_order() {
        let cfg = config();
        let fake = Fake::new(reply);
        let mut tick_loop = Loop::new(&cfg, &fake, BOAT_KEY.into(), gh_auth(), STARTED);
        tick_loop.tick(NOW, decide_all).unwrap();

        let mut expected = observe_requests().to_vec();
        expected.extend([
            format!("DELETE {GH}/runners/12"),
            // Runner 12 is already gone, so only the machine is deleted.
            format!("DELETE {BOAT}/sandboxes/bx_44444444"),
            // Stop deletes the machine's runner first.
            format!("DELETE {GH}/runners/11"),
            format!("POST {BOAT}/sandboxes/bx_22222222/stop"),
            // A runner goes only on a machine that holds none of its creator's logins.
            format!("GET {BOAT}/sandboxes/bx_22222222"),
            format!("POST {GH}/runners/generate-jitconfig"),
            format!("PUT {BOAT}/sandboxes/bx_22222222/files"),
            format!("POST {BOAT}/sandboxes/bx_22222222/commands"),
            format!("POST {BOAT}/sandboxes/bx_33333333/resume"),
            // The quote is read again before the next start, not after the last one.
            format!("GET {BOAT}/limits"),
            format!("POST {BOAT}/sandboxes"),
        ]);
        assert_eq!(requests(&fake), expected);

        let sent = fake.sent();
        let fork = &sent[16];
        assert!(fork.body_text().contains(r#""sw-boat-runner-job":"70-1""#));
        assert!(fork.body_text().contains(r#""ttlSeconds":3600"#));
        let jit = &sent[11];
        assert!(
            jit.body_text()
                .contains(r#""name":"sbr-ci-bx_22222222-1-1000""#)
        );
        assert!(
            sent[12]
                .body_text()
                .contains(r#""path":"/home/user/.sbr-jit-1""#)
        );
        assert!(sent[13].body_text().contains("/home/user/.sbr-jit-1"));
        // The jit config goes into the file write only: never a URL, a command or a header.
        for (i, s) in sent.iter().enumerate() {
            let leaks = s.url.contains("JITSECRET")
                || s.headers.iter().any(|(_, v)| v.contains("JITSECRET"))
                || s.body_text().contains("JITSECRET");
            assert_eq!(leaks, i == 12, "request {i}: {}", s.url);
        }
        // Two accepted starts at the 25-cent quote, on top of the assumed 100.
        assert_eq!(tick_loop.ledger.spent_cents(), 150);

        tick_loop.tick(act::DAY_S, |_, _| Vec::new()).unwrap();
        assert_eq!(tick_loop.ledger.spent_cents(), 0);
    }

    /// `plan` without a snapshot file observes as `run` does and sends nothing else, even
    /// when the plan has every kind of action.
    #[test]
    fn a_live_plan_only_reads() {
        let cfg = config();
        let fake = Fake::new(reply);
        // `observe_once` reads as a loop that started a request timeout ago.
        let snap = started(&cfg, &fake).observe(NOW).unwrap().snapshot;
        assert_eq!(plan(&cfg, &snap, decide_all).len(), 6);
        assert_eq!(requests(&fake), observe_requests());
    }

    /// An idle tick reads each list once and boat's quote once; on GitHub, a list that has
    /// not changed is asked for with its ETag, and GitHub does not count a 304 against the
    /// rate limit.
    #[test]
    fn an_idle_tick_makes_five_reads_and_spends_no_github_rate_limit_once_warm() {
        let cfg = config();
        let fake = Fake::new(|s| {
            let gh = s.url.starts_with(GH);
            if gh && s.header("if-none-match") == Some("\"v\"") {
                return respond(304, &[], "");
            }
            let body = if s.url.contains("/runners?") {
                r#"{"runners":[]}"#
            } else if s.url.contains("/runs?") {
                r#"{"workflow_runs":[]}"#
            } else if s.url.ends_with("/limits") {
                r#"{"canStart":true,"nextStartDollars":0}"#
            } else {
                r#"{"sandboxes":[],"pageInfo":{"nextCursor":null,"hasMore":false}}"#
            };
            respond(200, if gh { &[("ETag", "\"v\"")] } else { &[] }, body)
        });
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.tick(NOW, decide).unwrap();
        tick_loop.tick(NOW + 15, decide).unwrap();
        let sent = fake.sent();
        assert_eq!(sent.len(), 10, "{:?}", requests(&fake));
        let conditional: Vec<bool> = sent[5..]
            .iter()
            .map(|s| s.header("if-none-match").is_some())
            .collect();
        // sandboxes, runners, queued runs, in-progress runs, limits
        assert_eq!(conditional, [false, true, true, true, false]);
    }

    #[test]
    fn drain_deletes_the_pools_runners_and_machines() {
        let cfg = config();
        let fake = Fake::new(reply);
        // The fake lists the same three machines afterwards.
        let left = Loop::new(&cfg, &fake, BOAT_KEY.into(), gh_auth(), NOW)
            .drain(NOW)
            .unwrap();
        assert_eq!(left, 3);
        // Queued jobs do not matter to drain, so it spends no requests on them.
        assert!(!requests(&fake).iter().any(|r| r.contains("/runs")));
        let writes: Vec<String> = requests(&fake)
            .into_iter()
            .filter(|r| !r.starts_with("GET"))
            .collect();
        assert_eq!(
            writes,
            [
                format!("DELETE {GH}/runners/11"),
                format!("DELETE {GH}/runners/12"),
                format!("DELETE {BOAT}/sandboxes/bx_22222222"),
                format!("DELETE {BOAT}/sandboxes/bx_33333333"),
                format!("DELETE {BOAT}/sandboxes/bx_44444444"),
            ]
        );
    }

    fn job(id: u64, labels: &[&str]) -> QueuedJob {
        QueuedJob {
            key: JobKey { id, attempt: 1 },
            labels: labels.iter().map(|l| l.to_string()).collect(),
        }
    }

    fn fork_each_job(s: &Snapshot, p: &Pool) -> Vec<Action> {
        // Pool "b" comes second, so it sees the 25-cent start pool "ci" planned.
        let spent = if p.name == "b" { 35 } else { 10 };
        assert_eq!(s.limits.spent_today_cents, spent, "pool {}", p.name);
        s.queued
            .iter()
            .map(|j| Action::Fork {
                pool: p.name.clone(),
                job: Some(j.key),
            })
            .collect()
    }

    #[test]
    fn plan_routes_each_job_to_the_first_pool_that_serves_it() {
        let mut cfg = config();
        let mut b = cfg.pools[0].clone();
        b.name = "b".into();
        b.labels.push("gpu".into());
        cfg.pools.push(b);
        let snap = Snapshot {
            now: NOW,
            machines: Vec::new(),
            runners: Vec::new(),
            queued: vec![
                job(1, &["self-hosted", "large"]),
                job(2, &["self-hosted", "GPU"]),
                job(3, &["self-hosted", "linux"]),
            ],
            limits: Limits {
                can_start: true,
                next_start_cents: 25,
                spent_today_cents: 10,
                free_starts_left: None,
            },
        };
        let forks = |snap: &Snapshot, decide: fn(&Snapshot, &Pool) -> Vec<Action>| {
            plan(&cfg, snap, decide)
                .into_iter()
                .map(|(p, a)| match a {
                    Action::Fork { job: Some(j), .. } => (p.name.clone(), j.id),
                    other => panic!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };
        let both = [("ci".to_string(), 1), ("b".to_string(), 2)];
        assert_eq!(forks(&snap, fork_each_job), both);

        // The pools share boat's free starts: the second sees what the first took.
        let mut free = snap.clone();
        free.limits.next_start_cents = 0;
        free.limits.free_starts_left = Some(1);
        assert_eq!(forks(&free, decide), both[..1]);
    }

    #[test]
    fn fatal_errors_end_the_loop() {
        let cfg = config();
        let fake = Fake::new(|s| {
            if s.url.ends_with("/limits") {
                respond(402, &[], r#"{"code":"billing_required"}"#)
            } else {
                reply(s)
            }
        });
        let mut tick_loop = started(&cfg, &fake);
        let err = tick_loop.tick(NOW, |_, _| Vec::new()).unwrap_err();
        assert!(matches!(err, Error::Boat(boat::Error::BillingRequired)));
    }

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn github_credentials_from_the_environment() {
        let token = github_auth(&vars(&[("GITHUB_TOKEN", "ghp_x")])).unwrap();
        assert!(matches!(token, github::Auth::Token(t) if t == "ghp_x"));
        let err = |pairs: &[(&str, &str)]| github_auth(&vars(pairs)).err().unwrap().to_string();
        assert_eq!(
            err(&[("GITHUB_TOKEN", "ghp_x"), ("GITHUB_APP_ID", "1")]),
            "GITHUB_TOKEN and GITHUB_APP_* are both set; set one of them"
        );
        assert!(err(&[]).starts_with("GITHUB_TOKEN is not set"));
        assert_eq!(
            err(&[("GITHUB_APP_ID", "1")]),
            "GITHUB_APP_PRIVATE_KEY_FILE is not set"
        );
        assert_eq!(
            err(&[("GITHUB_APP_PRIVATE_KEY_FILE", "/k.pem")]),
            "GITHUB_APP_ID is not set"
        );
        assert_eq!(
            err(&[
                ("GITHUB_APP_ID", "1"),
                ("GITHUB_APP_PRIVATE_KEY_FILE", "/k.pem"),
                ("GITHUB_APP_INSTALLATION_ID", "abc"),
            ]),
            r#"GITHUB_APP_INSTALLATION_ID "abc" is not a number"#
        );
        assert!(
            err(&[
                ("GITHUB_APP_ID", "1"),
                ("GITHUB_APP_PRIVATE_KEY_FILE", "/nonexistent/k.pem")
            ])
            .starts_with("GITHUB_APP_PRIVATE_KEY_FILE /nonexistent/k.pem: ")
        );

        let path = std::env::temp_dir().join(format!("sbr-app-key-{}.pem", std::process::id()));
        std::fs::write(&path, crate::github_app::tests::test_pem()).unwrap();
        let app = github_auth(&vars(&[
            ("GITHUB_APP_ID", "12345"),
            ("GITHUB_APP_PRIVATE_KEY_FILE", path.to_str().unwrap()),
            ("GITHUB_APP_INSTALLATION_ID", "9"),
        ]));
        std::fs::remove_file(&path).unwrap();
        match app.unwrap() {
            github::Auth::App(app) => assert_eq!(
                format!("{app:?}"),
                r#"App { id: "12345", installation: Some(9), .. }"#
            ),
            github::Auth::Token(_) => panic!("a token"),
        }
    }
}
