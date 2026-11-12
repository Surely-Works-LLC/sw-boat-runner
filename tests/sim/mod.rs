//! A stateful fake boat and GitHub over virtual time, and a harness that drives the real
//! `run::Loop` against them. Each integration test that uses it says `mod sim;`.
//!
//! - `World` is boat and GitHub: machines move through Creating, Up, Stopping, Stopped
//!   and Deleted, with a listing that lags by `Knobs::list_lag_s`, boat's TTL, a start
//!   price and start windows; GitHub has runners that come online only after their JIT
//!   config is written and started on an Up machine, and runs whose jobs are queued,
//!   dispatched to matching idle runners, finished or cancelled.
//! - `Net` is the `Transport` the loops send through. It applies random `Faults` and
//!   targeted `Rule`s: errors before boat or GitHub act, and answers lost after they act.
//! - `Sim` ticks one or more daemons (`Loop`s) over one `World`, restarts them (dropping
//!   their memory), kills them, and drains them.
//! - The world checks the invariants as it goes and records each break as a `Violation`;
//!   `Sim::end_checks` adds the end-of-run ones.
//!
//! A new scenario builds a `World` (or `World::new` with `Knobs`), pushes jobs with
//! `World::add_run`, optionally adds `Rule`s, and drives a `Sim`. See tests/sim_*.rs.

#![allow(dead_code)]

use std::cell::{Ref, RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::rc::Rc;

use serde_json::{Value, json};
use sw_boat_runner::model::{Action, Config, JobKey, Pool, Snapshot, UnixSecs};
use sw_boat_runner::run::Loop;
use sw_boat_runner::transport::{NetworkError, Response, Transport};
use sw_boat_runner::{decide, github, policy};

pub const BOAT: &str = "https://boat.dev/api/v1";
pub const GH_REPOS: &str = "https://api.github.com/repos/";
pub const POOL_LABEL: &str = "sw-boat-runner";
pub const JOB_LABEL: &str = "sw-boat-runner-job";
pub const DAY_S: u64 = 86_400;
/// boat's longest TTL.
pub const MAX_TTL_S: u64 = 2_592_000;
/// The daemon's request timeout (`run::HTTP_TIMEOUT`), which its restart wait and its
/// memory of unlisted starts are built on.
pub const HTTP_TIMEOUT_S: u64 = 90;

// ---------------------------------------------------------------------------------------
// Randomness

/// splitmix64, as in bench/main.rs, so a seed replays a run exactly.
#[derive(Clone, Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

// ---------------------------------------------------------------------------------------
// Configuration of the world

/// boat's start price: the first `included_per_hour` starts of each clock hour are free,
/// the next `paid_per_hour` cost `paid_cents` each, and boat refuses any more with a 429.
#[derive(Clone, Copy, Debug)]
pub struct Pricing {
    pub included_per_hour: u32,
    pub paid_per_hour: u32,
    pub paid_cents: u32,
    /// boat's `canStart`.
    pub can_start: bool,
}

impl Pricing {
    pub fn free() -> Self {
        Pricing {
            included_per_hour: 1_000_000,
            paid_per_hour: 0,
            paid_cents: 0,
            can_start: true,
        }
    }
}

/// How boat and GitHub behave, apart from faults.
#[derive(Clone, Debug)]
pub struct Knobs {
    /// A create or resume is Up this long after boat accepts it.
    pub boot_s: u64,
    /// boat's listing shows each machine as it was this long ago.
    pub list_lag_s: u64,
    /// Each listing (all its pages) lags a further random 0 to this many seconds, so one
    /// listing can be older than the one before it.
    pub list_jitter_s: u64,
    /// GitHub lists a runner only this long after its registration.
    pub runner_list_lag_s: u64,
    /// A JIT runner whose job ended stays listed offline this long before GitHub removes it.
    pub runner_linger_s: u64,
    /// Chance per tick that an Idle runner's process dies (it goes offline for good).
    pub runner_death: f64,
    /// A machine stays listed as `deleted` this long after its delete is listed.
    pub deleted_listed_s: u64,
    /// A stop leaves the machine `archiving` this long.
    pub stop_s: u64,
    /// A resume within this long of the stop is free (boat's keep window).
    pub keep_s: u64,
    /// boat deletes the disk of a machine stopped this long; a resume then gets 409
    /// `snapshot_data_expired`.
    pub disk_keep_s: u64,
    /// A started runner is online this long after its start command.
    pub runner_online_s: u64,
    /// boat's page size for `GET /sandboxes`, at most the `limit` asked for.
    pub page_size: usize,
    /// Every new machine reports `holdsCreatorLogins: true`.
    pub logins: bool,
    /// GitHub's hourly request limit per token, sent as `X-RateLimit-Limit`.
    pub gh_rate_limit: u64,
    /// GitHub hands queued jobs to idle runners before every request, not only between
    /// ticks, so a job can be taken between the daemon's observe and its act.
    pub dispatch_every_request: bool,
    /// Each request moves the world clock on by this many seconds. The daemon's `now`
    /// stays the tick's.
    pub latency_s: u64,
    pub pricing: Pricing,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            boot_s: 20,
            list_lag_s: 0,
            list_jitter_s: 0,
            runner_list_lag_s: 0,
            runner_linger_s: 0,
            runner_death: 0.0,
            deleted_listed_s: 60,
            stop_s: 15,
            keep_s: 600,
            disk_keep_s: 14 * DAY_S,
            runner_online_s: 10,
            page_size: 200,
            logins: false,
            gh_rate_limit: 1_000_000_000,
            dispatch_every_request: true,
            latency_s: 0,
            pricing: Pricing::free(),
        }
    }
}

/// Random faults, each a probability per request it can apply to.
#[derive(Clone, Debug, Default)]
pub struct Faults {
    /// A boat call fails without an answer before boat acts (a timeout).
    pub boat_net: f64,
    /// boat acts, then the answer is lost.
    pub boat_lost: f64,
    /// boat answers 500 without acting.
    pub boat_5xx: f64,
    /// boat refuses a create or resume with 429.
    pub boat_429: f64,
    /// boat answers 401 or 403 (fatal for the daemon).
    pub boat_auth: f64,
    pub gh_net: f64,
    /// A GitHub write acts, then its answer is lost.
    pub gh_lost: f64,
    pub gh_5xx: f64,
    /// GitHub's secondary rate limit: 403 with `Retry-After`.
    pub gh_429: f64,
    /// GitHub answers 401 (fatal for the daemon).
    pub gh_auth: f64,
    /// A new machine boots into boat's `error` state instead of Up.
    pub boot_error: f64,
    /// The daemon process dies right after a write it sent was applied: the answer is
    /// lost, every later request of the tick fails, and `Sim` restarts it.
    pub crash: f64,
}

impl Faults {
    pub fn none() -> Self {
        Faults::default()
    }
}

/// What a `Rule` does to a matching request.
#[derive(Clone)]
pub enum Effect {
    /// Change the world first (another user of the boat account starts a machine, a job
    /// is cancelled...), then handle the request as usual.
    Then(Rc<dyn Fn(&mut World)>),
    /// Answer this status and body without acting.
    Status(u16, Value),
    /// Fail without an answer, without acting.
    Network,
    /// Act, then fail without an answer.
    Lost,
    /// Act, then answer this status instead of the real answer.
    LostWith(u16),
}

impl fmt::Debug for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Effect::Then(_) => write!(f, "Then(..)"),
            Effect::Status(s, b) => write!(f, "Status({s}, {b})"),
            Effect::Network => write!(f, "Network"),
            Effect::Lost => write!(f, "Lost"),
            Effect::LostWith(s) => write!(f, "LostWith({s})"),
        }
    }
}

pub type Matcher = Rc<dyn Fn(&Req) -> bool>;

/// A targeted fault: `effect` for the next `times` requests that `when` matches.
#[derive(Clone)]
pub struct Rule {
    pub when: Matcher,
    pub effect: Effect,
    pub times: u32,
}

impl Rule {
    pub fn new(when: impl Fn(&Req) -> bool + 'static, effect: Effect, times: u32) -> Self {
        Rule {
            when: Rc::new(when),
            effect,
            times,
        }
    }
}

/// One request as the world sees it.
#[derive(Clone, Debug)]
pub struct Req {
    pub at: UnixSecs,
    pub method: String,
    pub url: String,
    /// The daemon that sent it, from its credential; `usize::MAX` when unknown.
    pub who: usize,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_boat(&self) -> bool {
        self.url.starts_with(BOAT)
    }

    /// `METHOD path`, with the API base cut off, for matching and messages.
    pub fn line(&self) -> String {
        let path = self
            .url
            .strip_prefix(BOAT)
            .or_else(|| self.url.strip_prefix("https://api.github.com"))
            .unwrap_or(&self.url);
        format!("{} {path}", self.method)
    }
}

// ---------------------------------------------------------------------------------------
// boat

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxState {
    Creating {
        up_at: UnixSecs,
    },
    Up,
    Stopping {
        done_at: UnixSecs,
    },
    Stopped {
        since: UnixSecs,
    },
    /// boat could not make it (`error`).
    Error,
    Deleted {
        at: UnixSecs,
    },
}

impl BoxState {
    fn listed(self) -> &'static str {
        match self {
            BoxState::Creating { .. } => "provisioning",
            BoxState::Up => "running",
            BoxState::Stopping { .. } => "archiving",
            BoxState::Stopped { .. } => "archived",
            BoxState::Error => "error",
            BoxState::Deleted { .. } => "deleted",
        }
    }

    /// Holds a `max_boxes` box (spec `Up + Starting`).
    pub fn holds_box(self) -> bool {
        matches!(self, BoxState::Creating { .. } | BoxState::Up)
    }

    pub fn live(self) -> bool {
        !matches!(self, BoxState::Error | BoxState::Deleted { .. })
    }
}

#[derive(Clone, Debug)]
pub struct Sandbox {
    pub id: String,
    pub labels: BTreeMap<String, String>,
    /// The daemon whose create made it.
    pub owner: usize,
    pub state: BoxState,
    pub ttl_s: u64,
    /// When boat archives it if it is still Up.
    pub expires_at: Option<UnixSecs>,
    /// (time, listed state), oldest first: what a lagging listing replays.
    pub history: Vec<(UnixSecs, &'static str)>,
    pub holds_logins: bool,
    pub files: BTreeMap<String, String>,
    pub created_at: UnixSecs,
    /// Boots into `error` instead of Up.
    pub boot_fails: bool,
}

impl Sandbox {
    pub fn pool(&self) -> Option<&str> {
        self.labels.get(POOL_LABEL).map(String::as_str)
    }

    pub fn job(&self) -> Option<JobKey> {
        let (id, attempt) = self.labels.get(JOB_LABEL)?.split_once('-')?;
        Some(JobKey {
            id: id.parse().ok()?,
            attempt: attempt.parse().ok()?,
        })
    }

    fn set(&mut self, now: UnixSecs, state: BoxState) {
        self.state = state;
        self.history.push((now, state.listed()));
    }

    /// What a listing at `now` shows, `lag_s` behind; `None` when it does not list it.
    fn listed_at(&self, now: UnixSecs, lag_s: u64, deleted_listed_s: u64) -> Option<&'static str> {
        let seen = now.checked_sub(lag_s)?;
        let (t, state) = self.history.iter().rev().find(|(t, _)| *t <= seen)?;
        if *state == "deleted" && seen >= t + deleted_listed_s {
            return None;
        }
        Some(state)
    }
}

// ---------------------------------------------------------------------------------------
// GitHub

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerProc {
    /// JIT config issued, never started.
    Registered,
    /// Started; online at this time if its machine stays Up.
    Starting {
        online_at: UnixSecs,
    },
    Idle,
    Busy {
        job: usize,
    },
    /// Its process is gone; a JIT runner never comes back.
    Offline,
}

#[derive(Clone, Debug)]
pub struct GhRunner {
    pub id: u64,
    pub repo: String,
    pub name: String,
    pub labels: Vec<String>,
    pub machine: String,
    pub slot: u32,
    pub proc: RunnerProc,
    /// The JIT config was used once already.
    pub started: bool,
    pub created_at: UnixSecs,
    pub online_since: Option<UnixSecs>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    /// Not created yet.
    Future,
    Queued,
    InProgress {
        runner: u64,
        until: UnixSecs,
    },
    Done,
    /// Its runner died under it.
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub id: u64,
    pub attempt: u32,
    pub run: u64,
    pub repo: String,
    pub labels: Vec<String>,
    pub created_at: UnixSecs,
    pub run_s: u64,
    pub cancel_at: Option<UnixSecs>,
    pub status: JobStatus,
    pub started_at: Option<UnixSecs>,
    /// GitHub runner ids that ran it.
    pub ran_on: Vec<u64>,
}

impl Job {
    pub fn key(&self) -> JobKey {
        JobKey {
            id: self.id,
            attempt: self.attempt,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Run {
    pub id: u64,
    pub repo: String,
    pub attempt: u32,
    pub jobs: Vec<usize>,
    pub created_at: UnixSecs,
    pub changes: u64,
}

// ---------------------------------------------------------------------------------------
// Invariants

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A stop or delete of a machine with a Busy runner (`NoBusyStop`).
    BusyStop,
    /// A runner start that killed a Busy runner on the same slot.
    BusyKill,
    /// More machines Creating or Up than `max_boxes` (`MaxBoxes`).
    MaxBoxes,
    /// More paid start cents in a UTC day than the pool's cap (`StartBudget`).
    Spend,
    /// Two live forks for one job (`NoDoubleFork`).
    DoubleFork,
    /// A daemon stopped, deleted, resumed or started a runner on another's machine.
    Foreign,
    /// A queued job a pool serves was never run although starts were free (`NoStarvation`).
    Starved,
    /// A job fork still exists after its job settled (`NoLeak`).
    Leak,
    /// `drain` reported 0 but boat still has machines of the pool, or never got to 0.
    Drain,
    /// A machine still Up long after every daemon stopped.
    UpForever,
    /// A daemon's tick returned an error (it would exit).
    Exit,
    /// The daemon sent a request the fake does not know.
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Violation {
    pub kind: Kind,
    pub at: UnixSecs,
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?} at {}] {}", self.kind, self.at, self.detail)
    }
}

/// One pool of one daemon, as the world checks it.
#[derive(Clone, Debug)]
pub struct Owned {
    pub owner: usize,
    pub repo: String,
    pub pool: Pool,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub creates: u64,
    pub resumes: u64,
    pub stops: u64,
    pub deletes: u64,
    pub registers: u64,
    pub runner_deletes: u64,
    pub paid_cents: u64,
    pub ttl_archives: u64,
    pub ttl_kills: u64,
    pub requests: u64,
    pub faults: u64,
    pub exits: u64,
    pub restarts: u64,
}

// ---------------------------------------------------------------------------------------
// The world

#[derive(Clone)]
pub struct World {
    pub now: UnixSecs,
    pub knobs: Knobs,
    pub faults: Faults,
    pub rules: Vec<Rule>,
    pub rng: Rng,
    pub sandboxes: Vec<Sandbox>,
    pub runners: Vec<GhRunner>,
    pub jobs: Vec<Job>,
    pub runs: Vec<Run>,
    /// Pools of every daemon, for the invariants.
    pub pools: Vec<Owned>,
    pub violations: Vec<Violation>,
    pub stats: Stats,
    /// Every request, in order.
    pub log: Vec<Req>,
    /// Keep `log` (it costs memory in long property runs).
    pub keep_log: bool,
    /// The sending daemon died during this tick; see `Faults::crash`.
    pub crashed: bool,
    /// The extra lag of the listing being paged through.
    jitter_now: u64,
    /// Runners GitHub removed after their job, still listed offline until the time.
    lingering: Vec<(GhRunner, UnixSecs)>,
    next_id: u64,
    /// Idempotency key → sandbox id, as boat keeps them for 24 h.
    idempotency: HashMap<String, (String, UnixSecs)>,
    /// (clock hour, starts in it).
    hour_starts: (u64, u32),
    /// Paid cents by (owner, pool, UTC day).
    spend: BTreeMap<(usize, String, u64), u64>,
    /// GitHub requests counted by (token, clock hour).
    gh_used: HashMap<(String, u64), u64>,
}

impl World {
    pub fn new(now: UnixSecs, knobs: Knobs, seed: u64) -> Self {
        World {
            now,
            knobs,
            faults: Faults::none(),
            rules: Vec::new(),
            rng: Rng(seed),
            sandboxes: Vec::new(),
            runners: Vec::new(),
            jobs: Vec::new(),
            runs: Vec::new(),
            pools: Vec::new(),
            violations: Vec::new(),
            stats: Stats::default(),
            log: Vec::new(),
            keep_log: true,
            crashed: false,
            jitter_now: 0,
            lingering: Vec::new(),
            next_id: seed % 10_000,
            idempotency: HashMap::new(),
            hour_starts: (0, 0),
            spend: BTreeMap::new(),
            gh_used: HashMap::new(),
        }
    }

    fn violate(&mut self, kind: Kind, detail: String) {
        self.violations.push(Violation {
            kind,
            at: self.now,
            detail,
        });
    }

    pub fn violations_of(&self, kind: Kind) -> Vec<&Violation> {
        self.violations.iter().filter(|v| v.kind == kind).collect()
    }

    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// A fresh id in boat's format (`bx_` and 8 characters of its alphabet).
    fn new_sandbox_id(&mut self) -> String {
        const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
        let mut n = self.fresh_id().wrapping_mul(0x9E37_79B9) ^ 0x5bd1;
        let mut id = String::from("bx_");
        for _ in 0..8 {
            id.push(ALPHABET[(n % ALPHABET.len() as u64) as usize] as char);
            n /= ALPHABET.len() as u64;
        }
        if self.sandboxes.iter().any(|s| s.id == id) {
            return self.new_sandbox_id();
        }
        id
    }

    pub fn sandbox(&self, id: &str) -> Option<&Sandbox> {
        self.sandboxes.iter().find(|s| s.id == id)
    }

    fn sandbox_mut(&mut self, id: &str) -> Option<&mut Sandbox> {
        self.sandboxes.iter_mut().find(|s| s.id == id)
    }

    /// Puts a machine straight into boat, as if a daemon `owner` had made it earlier.
    pub fn seed_machine(
        &mut self,
        owner: usize,
        pool: &str,
        job: Option<JobKey>,
        state: BoxState,
    ) -> String {
        let id = self.new_sandbox_id();
        let mut labels = BTreeMap::from([(POOL_LABEL.to_string(), pool.to_string())]);
        if let Some(j) = job {
            labels.insert(JOB_LABEL.into(), format!("{}-{}", j.id, j.attempt));
        }
        let ttl_s = if job.is_some() { 3_600 } else { MAX_TTL_S };
        let since = self.now.saturating_sub(DAY_S);
        let mut sb = Sandbox {
            id: id.clone(),
            labels,
            owner,
            state,
            ttl_s,
            expires_at: Some(self.now + ttl_s),
            history: Vec::new(),
            holds_logins: self.knobs.logins,
            files: BTreeMap::new(),
            created_at: since,
            boot_fails: false,
        };
        sb.history.push((since, state.listed()));
        self.sandboxes.push(sb);
        id
    }

    /// Queues a run of `jobs` (labels, seconds each runs) in `repo` at `at`. Returns the
    /// jobs' indexes in `World::jobs`.
    pub fn add_run(&mut self, repo: &str, at: UnixSecs, jobs: &[(Vec<String>, u64)]) -> Vec<usize> {
        let run = self.fresh_id();
        let mut idx = Vec::new();
        for (labels, run_s) in jobs {
            let id = self.fresh_id();
            idx.push(self.jobs.len());
            self.jobs.push(Job {
                id,
                attempt: 1,
                run,
                repo: repo.to_string(),
                labels: labels.clone(),
                created_at: at,
                run_s: *run_s,
                cancel_at: None,
                status: JobStatus::Future,
                started_at: None,
                ran_on: Vec::new(),
            });
        }
        self.runs.push(Run {
            id: run,
            repo: repo.to_string(),
            attempt: 1,
            jobs: idx.clone(),
            created_at: at,
            changes: 0,
        });
        idx
    }

    fn touch_run(&mut self, run: u64) {
        if let Some(r) = self.runs.iter_mut().find(|r| r.id == run) {
            r.changes += 1;
        }
    }

    // --- time ------------------------------------------------------------------------

    /// Moves the clock to `now` and lets boat and GitHub do what they do on their own.
    pub fn advance(&mut self, now: UnixSecs) {
        self.now = self.now.max(now);
        let now = self.now;
        // Machines: boot, finish stopping, expire. A coarse step may need several turns.
        for _ in 0..4 {
            let mut died = Vec::new();
            for i in 0..self.sandboxes.len() {
                let sb = &mut self.sandboxes[i];
                match sb.state {
                    BoxState::Creating { up_at } if up_at <= now => {
                        if sb.boot_fails {
                            sb.set(up_at, BoxState::Error);
                        } else {
                            sb.set(up_at, BoxState::Up);
                            sb.expires_at = Some(up_at.saturating_add(sb.ttl_s));
                        }
                    }
                    BoxState::Up if sb.expires_at.is_some_and(|t| t <= now) => {
                        let at = sb.expires_at.unwrap_or(now);
                        sb.set(at, BoxState::Stopping { done_at: at + 15 });
                        died.push(sb.id.clone());
                        self.stats.ttl_archives += 1;
                    }
                    BoxState::Stopping { done_at } if done_at <= now => {
                        sb.set(done_at, BoxState::Stopped { since: done_at });
                    }
                    _ => {}
                }
            }
            for id in died {
                let killed = self.kill_runners_on(&id, None);
                self.stats.ttl_kills += killed as u64;
            }
        }
        self.github_step();
    }

    /// Jobs arrive, finish or are cancelled; runners come online; queued jobs go to idle
    /// runners, oldest job first, to the runner that has been online longest.
    fn github_step(&mut self) {
        let now = self.now;
        for j in 0..self.jobs.len() {
            let job = &mut self.jobs[j];
            match job.status {
                JobStatus::Future if job.created_at <= now => {
                    job.status = JobStatus::Queued;
                    let run = job.run;
                    self.touch_run(run);
                }
                JobStatus::InProgress { runner, until } if until <= now => {
                    job.status = JobStatus::Done;
                    let run = job.run;
                    self.end_runner(runner);
                    self.touch_run(run);
                }
                _ => {}
            }
            let job = &mut self.jobs[j];
            if job.cancel_at.is_some_and(|t| t <= now) {
                match job.status {
                    JobStatus::Queued | JobStatus::Future => {
                        job.status = JobStatus::Cancelled;
                        let run = job.run;
                        self.touch_run(run);
                    }
                    JobStatus::InProgress { runner, .. } => {
                        job.status = JobStatus::Cancelled;
                        let run = job.run;
                        self.end_runner(runner);
                        self.touch_run(run);
                    }
                    _ => {}
                }
            }
        }
        for i in 0..self.runners.len() {
            if let RunnerProc::Starting { online_at } = self.runners[i].proc {
                let up = self
                    .sandbox(&self.runners[i].machine)
                    .is_some_and(|s| s.state == BoxState::Up);
                if online_at <= now && up {
                    self.runners[i].proc = RunnerProc::Idle;
                    self.runners[i].online_since = Some(online_at);
                }
            }
        }
        let now = self.now;
        self.lingering.retain(|(_, until)| *until > now);
        self.dispatch();
    }

    /// Once a tick: Idle runner processes may die.
    pub fn tick_faults(&mut self) {
        let p = self.knobs.runner_death;
        if p <= 0.0 {
            return;
        }
        for i in 0..self.runners.len() {
            if self.runners[i].proc == RunnerProc::Idle && self.rng.chance(p) {
                self.runners[i].proc = RunnerProc::Offline;
            }
        }
    }

    fn dispatch(&mut self) {
        let now = self.now;
        let mut order: Vec<usize> = (0..self.jobs.len())
            .filter(|&j| self.jobs[j].status == JobStatus::Queued)
            .collect();
        order.sort_by_key(|&j| (self.jobs[j].created_at, self.jobs[j].id));
        for j in order {
            let job = &self.jobs[j];
            let fits = |r: &GhRunner| {
                r.repo == job.repo
                    && r.proc == RunnerProc::Idle
                    && job
                        .labels
                        .iter()
                        .all(|l| r.labels.iter().any(|x| x.eq_ignore_ascii_case(l)))
            };
            let pick = (0..self.runners.len())
                .filter(|&i| fits(&self.runners[i]))
                .min_by_key(|&i| (self.runners[i].online_since, self.runners[i].id));
            if let Some(i) = pick {
                let job = &mut self.jobs[j];
                job.status = JobStatus::InProgress {
                    runner: self.runners[i].id,
                    until: now + job.run_s,
                };
                job.started_at.get_or_insert(now);
                job.ran_on.push(self.runners[i].id);
                self.runners[i].proc = RunnerProc::Busy { job: j };
                let run = job.run;
                self.touch_run(run);
            }
        }
    }

    /// A JIT runner whose job ended: GitHub removes it.
    fn end_runner(&mut self, id: u64) {
        if let Some(i) = self.runners.iter().position(|r| r.id == id) {
            let mut r = self.runners.remove(i);
            if self.knobs.runner_linger_s > 0 {
                r.proc = RunnerProc::Offline;
                let until = self.now + self.knobs.runner_linger_s;
                self.lingering.push((r, until));
            }
        }
    }

    /// Every runner process on `machine` dies. A Busy runner's job fails. Returns how many
    /// Busy runners died.
    fn kill_runners_on(&mut self, machine: &str, slot: Option<u32>) -> usize {
        let mut killed = 0;
        let mut failed = Vec::new();
        for r in &mut self.runners {
            if r.machine != machine || slot.is_some_and(|s| s != r.slot) {
                continue;
            }
            match r.proc {
                RunnerProc::Busy { job } => {
                    killed += 1;
                    failed.push(job);
                    r.proc = RunnerProc::Offline;
                }
                RunnerProc::Idle | RunnerProc::Starting { .. } => r.proc = RunnerProc::Offline,
                RunnerProc::Registered | RunnerProc::Offline => {}
            }
        }
        for j in failed {
            self.jobs[j].status = JobStatus::Failed;
            let run = self.jobs[j].run;
            self.touch_run(run);
        }
        killed
    }

    fn busy_on(&self, machine: &str) -> Vec<String> {
        self.runners
            .iter()
            .filter(|r| r.machine == machine && matches!(r.proc, RunnerProc::Busy { .. }))
            .map(|r| r.name.clone())
            .collect()
    }

    // --- invariants checked on the way -------------------------------------------------

    fn check_boxes(&mut self, owner: usize, pool: &str) {
        let Some(max) = self
            .pools
            .iter()
            .find(|o| o.owner == owner && o.pool.name == pool)
            .map(|o| o.pool.max_boxes)
        else {
            return;
        };
        let held: Vec<&str> = self
            .sandboxes
            .iter()
            .filter(|s| s.owner == owner && s.pool() == Some(pool) && s.state.holds_box())
            .map(|s| s.id.as_str())
            .collect();
        if held.len() > max as usize {
            let detail = format!(
                "daemon {owner} pool {pool}: {} machines Creating or Up, max_boxes {max}: {held:?}",
                held.len()
            );
            self.violate(Kind::MaxBoxes, detail);
        }
    }

    fn check_double_fork(&mut self, job: JobKey) {
        let live: Vec<&str> = self
            .sandboxes
            .iter()
            .filter(|s| s.state.live() && s.job() == Some(job))
            .map(|s| s.id.as_str())
            .collect();
        if live.len() > 1 {
            let detail = format!("job {}-{}: live forks {live:?}", job.id, job.attempt);
            self.violate(Kind::DoubleFork, detail);
        }
    }

    fn charge(&mut self, owner: usize, pool: &str, cents: u32) {
        if cents == 0 {
            return;
        }
        self.stats.paid_cents += u64::from(cents);
        let day = self.now / DAY_S;
        let total = self
            .spend
            .entry((owner, pool.to_string(), day))
            .or_default();
        *total += u64::from(cents);
        let total = *total;
        let cap = self
            .pools
            .iter()
            .find(|o| o.owner == owner && o.pool.name == pool)
            .map(|o| o.pool.paid_start_cents_per_day);
        if let Some(cap) = cap {
            if total > u64::from(cap) {
                let detail = format!(
                    "daemon {owner} pool {pool}: {total} cents of paid starts on day {day}, cap {cap}"
                );
                self.violate(Kind::Spend, detail);
            }
        }
    }

    fn check_foreign(&mut self, req: &Req, machine: &str) {
        let Some(sb) = self.sandbox(machine) else {
            return;
        };
        if req.who != usize::MAX && sb.owner != req.who {
            let detail = format!(
                "daemon {} sent {} to {machine}, made by daemon {}",
                req.who,
                req.line(),
                sb.owner
            );
            self.violate(Kind::Foreign, detail);
        }
    }

    // --- boat's start price --------------------------------------------------------------

    fn hour_count(&mut self) -> u32 {
        let hour = self.now / 3_600;
        if self.hour_starts.0 != hour {
            self.hour_starts = (hour, 0);
        }
        self.hour_starts.1
    }

    /// The price of the next start, `None` when boat would refuse it.
    fn next_price(&mut self) -> Option<u32> {
        let n = self.hour_count();
        let p = self.knobs.pricing;
        if !p.can_start {
            None
        } else if n < p.included_per_hour {
            Some(0)
        } else if n < p.included_per_hour.saturating_add(p.paid_per_hour) {
            Some(p.paid_cents)
        } else {
            None
        }
    }

    /// Counts and charges a start, or `None` when boat refuses it.
    pub fn take_start(&mut self, owner: usize, pool: &str) -> Option<u32> {
        let price = self.next_price()?;
        self.hour_starts.1 += 1;
        self.charge(owner, pool, price);
        Some(price)
    }

    // --- request handling ----------------------------------------------------------------

    fn who(&self, req_headers: &[(String, String)]) -> usize {
        let auth = req_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        auth.rsplit('-')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(usize::MAX)
    }

    /// The random fault for `req`, if any. `Lost` faults apply only to writes.
    fn random_fault(&mut self, req: &Req) -> Option<Effect> {
        let f = self.faults.clone();
        let write = req.method != "GET";
        let start = req.is_boat()
            && req.method == "POST"
            && (req.url == format!("{BOAT}/sandboxes") || req.url.ends_with("/resume"));
        let r = &mut self.rng;
        if req.is_boat() {
            if r.chance(f.boat_auth) {
                return Some(Effect::Status(403, json!({"code": "forbidden"})));
            }
            if r.chance(f.boat_net) {
                return Some(Effect::Network);
            }
            if r.chance(f.boat_5xx) {
                return Some(Effect::Status(500, json!({"code": "internal"})));
            }
            if start && r.chance(f.boat_429) {
                return Some(Effect::Status(
                    429,
                    json!({"code": "rate_limited", "retryAfterSeconds": 30}),
                ));
            }
            if write && r.chance(f.boat_lost) {
                return Some(Effect::Lost);
            }
        } else {
            if r.chance(f.gh_auth) {
                return Some(Effect::Status(401, json!({"message": "Bad credentials"})));
            }
            if r.chance(f.gh_net) {
                return Some(Effect::Network);
            }
            if r.chance(f.gh_5xx) {
                return Some(Effect::Status(502, json!({"message": "Server Error"})));
            }
            if r.chance(f.gh_429) {
                return Some(Effect::Status(
                    403,
                    json!({"message": "You have exceeded a secondary rate limit"}),
                ));
            }
            if write && r.chance(f.gh_lost) {
                return Some(Effect::Lost);
            }
        }
        None
    }

    fn rule_fault(&mut self, req: &Req) -> Option<Effect> {
        let rule = self
            .rules
            .iter_mut()
            .find(|r| r.times > 0 && (r.when)(req))?;
        rule.times -= 1;
        Some(rule.effect.clone())
    }

    pub fn handle(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, NetworkError> {
        self.stats.requests += 1;
        if self.knobs.latency_s > 0 {
            let t = self.now + self.knobs.latency_s;
            self.advance(t);
        } else if self.knobs.dispatch_every_request {
            self.github_step();
        }
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let body = body
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null);
        let req = Req {
            at: self.now,
            method: method.to_string(),
            url: url.to_string(),
            who: self.who(&headers),
            headers,
            body,
        };
        if self.crashed {
            return Err(NetworkError("sim: the daemon is dead".into()));
        }
        if self.keep_log {
            self.log.push(req.clone());
        }
        let mut fault = self.rule_fault(&req).or_else(|| self.random_fault(&req));
        if fault.is_none() && req.method != "GET" && self.rng.chance(self.faults.crash) {
            self.crashed = true;
            fault = Some(Effect::Lost);
        }
        if let Some(Effect::Then(f)) = &fault {
            let f = f.clone();
            f(self);
            fault = None;
        }
        if fault.is_some() {
            self.stats.faults += 1;
        }
        match fault {
            Some(Effect::Status(status, body)) => {
                let mut headers = vec![];
                if status == 403 && !req.is_boat() {
                    headers.push(("retry-after".to_string(), "60".to_string()));
                }
                if status == 429 {
                    headers.push(("retry-after".to_string(), "30".to_string()));
                }
                return Ok(Response {
                    status,
                    headers,
                    body: body.to_string().into_bytes(),
                });
            }
            Some(Effect::Network) => return Err(NetworkError("sim: timed out".into())),
            _ => {}
        }
        let resp = if req.is_boat() {
            self.boat(&req)
        } else if req.url.starts_with(GH_REPOS) {
            self.github(&req)
        } else {
            self.violate(Kind::Unknown, req.line());
            (404, Vec::new(), json!({}))
        };
        match fault {
            Some(Effect::Lost) => Err(NetworkError("sim: answer lost".into())),
            Some(Effect::LostWith(status)) => Ok(Response {
                status,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            }),
            _ => {
                let (status, headers, body) = resp;
                let body = if body.is_null() {
                    Vec::new()
                } else {
                    body.to_string().into_bytes()
                };
                Ok(Response {
                    status,
                    headers,
                    body,
                })
            }
        }
    }

    fn boat(&mut self, req: &Req) -> (u16, Vec<(String, String)>, Value) {
        let path = req.url.strip_prefix(BOAT).unwrap_or_default();
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let now = self.now;
        let ok = |v: Value| (200, Vec::new(), v);
        let err = |status: u16, code: &str| (status, Vec::new(), json!({ "code": code }));
        match (req.method.as_str(), parts.as_slice()) {
            ("GET", ["limits"]) => {
                let price = self.next_price();
                let p = self.knobs.pricing;
                let left = p.included_per_hour.saturating_sub(self.hour_count());
                ok(json!({
                    "canStart": p.can_start,
                    "nextStartDollars": price.map(|c| f64::from(c) / 100.0),
                    "starts": { "hour": { "remaining": left } },
                }))
            }
            ("GET", ["sandboxes"]) => {
                let q = query_map(query);
                let Some((key, value)) = q.get("label").and_then(|l| l.split_once('=')) else {
                    return err(400, "bad_label");
                };
                let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50);
                let page = limit.min(self.knobs.page_size).max(1);
                let start: usize = q.get("cursor").and_then(|c| c.parse().ok()).unwrap_or(0);
                if start == 0 {
                    let j = self.knobs.list_jitter_s;
                    self.jitter_now = self.rng.below(j + 1);
                }
                let lag = self.knobs.list_lag_s + self.jitter_now;
                let gone = self.knobs.deleted_listed_s;
                let all: Vec<Value> = self
                    .sandboxes
                    .iter()
                    .filter(|s| s.labels.get(key).is_some_and(|v| v == value))
                    .filter_map(|s| {
                        let state = s.listed_at(now, lag, gone)?;
                        Some(json!({ "id": s.id, "state": state, "labels": s.labels }))
                    })
                    .collect();
                let end = (start + page).min(all.len());
                let more = end < all.len();
                ok(json!({
                    "sandboxes": all[start.min(end)..end],
                    "pageInfo": {
                        "nextCursor": if more { Value::from(end.to_string()) } else { Value::Null },
                        "hasMore": more,
                        "limit": page,
                    }
                }))
            }
            ("POST", ["sandboxes"]) => {
                let key = req.header("idempotency-key").unwrap_or("").to_string();
                if let Some((id, at)) = self.idempotency.get(&key) {
                    if now < at + DAY_S {
                        let id = id.clone();
                        let state = self.sandbox(&id).map_or("deleted", |s| s.state.listed());
                        return ok(json!({ "sandbox": { "id": id, "state": state } }));
                    }
                }
                let labels: BTreeMap<String, String> =
                    serde_json::from_value(req.body["labels"].clone()).unwrap_or_default();
                let pool = labels.get(POOL_LABEL).cloned().unwrap_or_default();
                let ttl_s = req.body["ttlSeconds"].as_u64().unwrap_or(0);
                if ttl_s == 0 || ttl_s > MAX_TTL_S {
                    return err(400, "bad_ttl");
                }
                if self.take_start(req.who, &pool).is_none() {
                    return (
                        429,
                        vec![("retry-after".into(), "60".into())],
                        json!({"code": "start_limit"}),
                    );
                }
                self.stats.creates += 1;
                let id = self.new_sandbox_id();
                let boot_fails = self.rng.chance(self.faults.boot_error);
                let mut sb = Sandbox {
                    id: id.clone(),
                    labels,
                    owner: req.who,
                    state: BoxState::Creating {
                        up_at: now + self.knobs.boot_s,
                    },
                    ttl_s,
                    expires_at: None,
                    history: Vec::new(),
                    holds_logins: self.knobs.logins,
                    files: BTreeMap::new(),
                    created_at: now,
                    boot_fails,
                };
                sb.history.push((now, "provisioning"));
                let job = sb.job();
                self.sandboxes.push(sb);
                if !key.is_empty() {
                    self.idempotency.insert(key, (id.clone(), now));
                }
                self.check_boxes(req.who, &pool);
                if let Some(job) = job {
                    self.check_double_fork(job);
                }
                ok(json!({ "type": "sandbox.created", "sandbox": { "id": id, "state": "init" } }))
            }
            ("GET", ["sandboxes", id]) => match self.sandbox(id) {
                Some(s) if !matches!(s.state, BoxState::Deleted { .. }) => ok(json!({
                    "sandbox": {
                        "id": s.id, "state": s.state.listed(), "labels": s.labels,
                        "holdsCreatorLogins": s.holds_logins,
                    }
                })),
                _ => err(404, "not_found"),
            },
            ("POST", ["sandboxes", id, "resume"]) => {
                let id = id.to_string();
                self.check_foreign(req, &id);
                let Some(sb) = self.sandbox(&id) else {
                    return err(404, "not_found");
                };
                let BoxState::Stopped { since } = sb.state else {
                    return err(409, "invalid_state");
                };
                if now >= since + self.knobs.disk_keep_s {
                    return err(409, "snapshot_data_expired");
                }
                let (owner, pool) = (sb.owner, sb.pool().unwrap_or_default().to_string());
                if now >= since + self.knobs.keep_s && self.take_start(owner, &pool).is_none() {
                    return (
                        429,
                        vec![("retry-after".into(), "60".into())],
                        json!({"code": "start_limit"}),
                    );
                }
                self.stats.resumes += 1;
                let up_at = now + self.knobs.boot_s;
                let sb = self.sandbox_mut(&id).expect("found above");
                sb.set(now, BoxState::Creating { up_at });
                // A noEnv resume scrubs the creator's logins.
                sb.holds_logins = false;
                self.check_boxes(owner, &pool);
                ok(json!({}))
            }
            ("POST", ["sandboxes", id, "stop"]) => {
                let id = id.to_string();
                self.check_foreign(req, &id);
                let busy = self.busy_on(&id);
                let stop_s = self.knobs.stop_s;
                let Some(sb) = self.sandbox_mut(&id) else {
                    return err(404, "not_found");
                };
                match sb.state {
                    BoxState::Up | BoxState::Creating { .. } => {
                        let done_at = now + stop_s;
                        sb.set(now, BoxState::Stopping { done_at });
                    }
                    BoxState::Stopping { .. } => return (202, Vec::new(), json!({})),
                    BoxState::Stopped { .. } => return ok(json!({})),
                    BoxState::Error | BoxState::Deleted { .. } => {
                        return err(409, "invalid_state");
                    }
                }
                self.stats.stops += 1;
                if !busy.is_empty() {
                    self.violate(Kind::BusyStop, format!("stop of {id} under {busy:?}"));
                }
                self.kill_runners_on(&id, None);
                (202, Vec::new(), json!({}))
            }
            ("DELETE", ["sandboxes", id]) => {
                let id = id.to_string();
                if req.header("x-ascii-confirm-delete") != Some(id.as_str()) {
                    return err(400, "confirm_header");
                }
                self.check_foreign(req, &id);
                let busy = self.busy_on(&id);
                let Some(sb) = self.sandbox_mut(&id) else {
                    return err(404, "not_found");
                };
                if matches!(sb.state, BoxState::Deleted { .. }) {
                    return err(404, "not_found");
                }
                sb.set(now, BoxState::Deleted { at: now });
                self.stats.deletes += 1;
                if !busy.is_empty() {
                    self.violate(Kind::BusyStop, format!("delete of {id} under {busy:?}"));
                }
                self.kill_runners_on(&id, None);
                ok(json!({}))
            }
            ("PUT", ["sandboxes", id, "files"]) => {
                let id = id.to_string();
                self.check_foreign(req, &id);
                let path = req.body["path"].as_str().unwrap_or_default().to_string();
                let content = req.body["content"].as_str().unwrap_or_default().to_string();
                match self.sandbox_mut(&id) {
                    Some(sb) if sb.state == BoxState::Up => {
                        sb.files.insert(path, content);
                        ok(json!({}))
                    }
                    Some(sb) if sb.state.live() => err(409, "invalid_state"),
                    _ => err(404, "not_found"),
                }
            }
            ("POST", ["sandboxes", id, "commands"]) => {
                let id = id.to_string();
                self.check_foreign(req, &id);
                let command = req.body["command"].as_str().unwrap_or_default().to_string();
                match self.sandbox(&id) {
                    Some(sb) if sb.state == BoxState::Up => {}
                    Some(sb) if sb.state.live() => return err(409, "invalid_state"),
                    _ => return err(404, "not_found"),
                }
                self.start_runner(&id, &command);
                ok(json!({ "processId": self.fresh_id() }))
            }
            _ => {
                self.violate(Kind::Unknown, req.line());
                err(404, "not_found")
            }
        }
    }

    /// The daemon's start command: kills the slot's old runner, reads the JIT config from
    /// the file it names, deletes the file and starts the runner it belongs to.
    fn start_runner(&mut self, machine: &str, command: &str) {
        const JIT: &str = "/home/user/.sbr-jit-";
        let Some(rest) = command.split(JIT).nth(1) else {
            return;
        };
        let slot: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let Ok(slot) = slot.parse::<u32>() else {
            return;
        };
        let killed = self.kill_runners_on(machine, Some(slot));
        if killed > 0 {
            let detail = format!("start on {machine} slot {slot} killed a Busy runner");
            self.violate(Kind::BusyKill, detail);
        }
        let path = format!("{JIT}{slot}");
        let Some(jit) = self
            .sandbox_mut(machine)
            .and_then(|s| s.files.remove(&path))
        else {
            return;
        };
        let online_at = self.now + self.knobs.runner_online_s;
        let Some(r) = self
            .runners
            .iter_mut()
            .find(|r| jit == format!("jit-{}", r.id))
        else {
            return;
        };
        if r.started || r.machine != machine {
            return;
        }
        r.started = true;
        r.proc = RunnerProc::Starting { online_at };
    }

    fn github(&mut self, req: &Req) -> (u16, Vec<(String, String)>, Value) {
        let rest = req.url.strip_prefix(GH_REPOS).unwrap_or_default();
        let Some((repo, path)) = rest.split_once("/actions/") else {
            return (404, Vec::new(), json!({"message": "Not Found"}));
        };
        let repo = repo.to_string();
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let q = query_map(query);
        let parts: Vec<&str> = path.split('/').collect();
        let token = req.header("authorization").unwrap_or("").to_string();
        let now = self.now;

        let (status, body) = match (req.method.as_str(), parts.as_slice()) {
            ("GET", ["runners"]) => {
                let per: usize = q.get("per_page").and_then(|p| p.parse().ok()).unwrap_or(30);
                let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                let lag = self.knobs.runner_list_lag_s;
                let mut mine: Vec<&GhRunner> = self
                    .runners
                    .iter()
                    .chain(self.lingering.iter().map(|(r, _)| r))
                    .filter(|r| r.repo == repo && r.created_at + lag <= now)
                    .collect();
                mine.sort_by_key(|r| r.id);
                let list: Vec<Value> = mine
                    .iter()
                    .skip(per * (page.max(1) - 1))
                    .take(per)
                    .map(|r| {
                        let online = matches!(r.proc, RunnerProc::Idle | RunnerProc::Busy { .. });
                        json!({
                            "id": r.id, "name": r.name,
                            "status": if online { "online" } else { "offline" },
                            "busy": matches!(r.proc, RunnerProc::Busy { .. }),
                            "labels": r.labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                (200, json!({ "total_count": mine.len(), "runners": list }))
            }
            ("POST", ["runners", "generate-jitconfig"]) => {
                let name = req.body["name"].as_str().unwrap_or_default().to_string();
                if self
                    .runners
                    .iter()
                    .any(|r| r.repo == repo && r.name == name)
                {
                    (
                        409,
                        json!({"message": "Already exists - A runner with the same name already exists."}),
                    )
                } else if let Some(n) = github::parse_runner_name(&name) {
                    let labels: Vec<String> =
                        serde_json::from_value(req.body["labels"].clone()).unwrap_or_default();
                    let id = self.fresh_id();
                    self.stats.registers += 1;
                    self.runners.push(GhRunner {
                        id,
                        repo: repo.clone(),
                        name: name.clone(),
                        labels,
                        machine: n.machine,
                        slot: n.slot,
                        proc: RunnerProc::Registered,
                        started: false,
                        created_at: now,
                        online_since: None,
                    });
                    (
                        201,
                        json!({ "runner": { "id": id, "name": name }, "encoded_jit_config": format!("jit-{id}") }),
                    )
                } else {
                    (422, json!({"message": "bad name"}))
                }
            }
            ("DELETE", ["runners", id]) => {
                let id: u64 = id.parse().unwrap_or(0);
                let lingered = self.lingering.len();
                self.lingering.retain(|(r, _)| r.id != id);
                match self
                    .runners
                    .iter()
                    .position(|r| r.id == id && r.repo == repo)
                {
                    None if lingered != self.lingering.len() => (204, Value::Null),
                    None => (404, json!({"message": "Not Found"})),
                    Some(i) if matches!(self.runners[i].proc, RunnerProc::Busy { .. }) => (
                        422,
                        json!({"message": "Bad request - Runner \"x\" is still running a job\""}),
                    ),
                    Some(i) => {
                        // A deleted runner's process stops taking jobs.
                        self.runners.remove(i);
                        self.stats.runner_deletes += 1;
                        (204, Value::Null)
                    }
                }
            }
            ("GET", ["runs"]) => {
                let want = q.get("status").cloned().unwrap_or_default();
                let per: usize = q.get("per_page").and_then(|p| p.parse().ok()).unwrap_or(30);
                let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                let mut runs: Vec<Value> = Vec::new();
                for run in self.runs.iter().rev().filter(|r| r.repo == repo) {
                    let status = self.run_status(run);
                    if status.as_deref() == Some(want.as_str()) {
                        runs.push(json!({
                            "id": run.id, "run_attempt": run.attempt, "status": status,
                            "updated_at": format!("u{}-{}", run.created_at, run.changes),
                        }));
                    }
                }
                let total = runs.len();
                let runs: Vec<Value> = runs
                    .into_iter()
                    .skip(per * (page.max(1) - 1))
                    .take(per)
                    .collect();
                (200, json!({ "total_count": total, "workflow_runs": runs }))
            }
            ("GET", ["runs", run, "jobs"]) => {
                let run: u64 = run.parse().unwrap_or(0);
                let per: usize = q.get("per_page").and_then(|p| p.parse().ok()).unwrap_or(30);
                let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                match self.runs.iter().find(|r| r.id == run && r.repo == repo) {
                    None => (404, json!({"message": "Not Found"})),
                    Some(r) => {
                        let all: Vec<Value> = r
                            .jobs
                            .iter()
                            .map(|&j| &self.jobs[j])
                            .filter(|j| j.status != JobStatus::Future)
                            .map(|j| {
                                let status = match j.status {
                                    JobStatus::Queued => "queued",
                                    JobStatus::InProgress { .. } => "in_progress",
                                    _ => "completed",
                                };
                                json!({ "id": j.id, "run_attempt": j.attempt,
                                        "status": status, "labels": j.labels })
                            })
                            .collect();
                        let total = all.len();
                        let jobs: Vec<Value> = all
                            .into_iter()
                            .skip(per * (page.max(1) - 1))
                            .take(per)
                            .collect();
                        (200, json!({ "total_count": total, "jobs": jobs }))
                    }
                }
            }
            _ => {
                self.violate(Kind::Unknown, req.line());
                (404, json!({"message": "Not Found"}))
            }
        };

        // ETags and the rate limit. A 304 is not counted.
        let text = body.to_string();
        let etag = format!("\"{:016x}\"", fnv(&text));
        let hour = now / 3_600;
        let limit = self.knobs.gh_rate_limit;
        if req.method == "GET" && status == 200 && req.header("if-none-match") == Some(&etag) {
            let used = self.gh_used.get(&(token, hour)).copied().unwrap_or(0);
            let headers = rate_headers(limit, used, hour);
            return (304, headers, Value::Null);
        }
        let used = self.gh_used.entry((token, hour)).or_default();
        *used += 1;
        let mut headers = rate_headers(limit, *used, hour);
        if req.method == "GET" && status == 200 {
            headers.push(("etag".into(), etag));
        }
        (status, headers, body)
    }

    /// GitHub's run status: `queued` before any of its jobs started, `in_progress` while
    /// any job is running or queued after another started, `completed` after all.
    fn run_status(&self, run: &Run) -> Option<String> {
        let jobs: Vec<&Job> = run
            .jobs
            .iter()
            .map(|&j| &self.jobs[j])
            .filter(|j| j.status != JobStatus::Future)
            .collect();
        if jobs.is_empty() {
            return None;
        }
        let queued = jobs
            .iter()
            .filter(|j| j.status == JobStatus::Queued)
            .count();
        let running = jobs
            .iter()
            .any(|j| matches!(j.status, JobStatus::InProgress { .. }));
        Some(
            if running || (queued > 0 && queued < jobs.len()) {
                "in_progress"
            } else if queued > 0 {
                "queued"
            } else {
                "completed"
            }
            .to_string(),
        )
    }

    // --- end-of-run checks ---------------------------------------------------------------

    /// No job a pool serves is queued or running, and no job fork is left.
    pub fn quiet(&self) -> bool {
        let busy = self.jobs.iter().any(|j| match j.status {
            JobStatus::Queued => self.servable(j),
            JobStatus::InProgress { .. } => true,
            _ => false,
        });
        let forks = self
            .sandboxes
            .iter()
            .any(|s| s.job().is_some() && s.state.live());
        !busy && !forks
    }

    /// Some pool of the job's repository serves its labels.
    pub fn servable(&self, j: &Job) -> bool {
        self.pools.iter().any(|o| {
            o.repo == j.repo
                && !j.labels.is_empty()
                && j.labels
                    .iter()
                    .all(|l| o.pool.labels.iter().any(|p| p.eq_ignore_ascii_case(l)))
        })
    }

    /// Records a `Starved` violation for each queued job one of `owner`'s pools serves
    /// that never ran.
    pub fn check_served(&mut self) {
        let mut starved = Vec::new();
        for j in &self.jobs {
            if !matches!(j.status, JobStatus::Queued) {
                continue;
            }
            if self.servable(j) {
                starved.push(format!(
                    "job {}-{} {:?} queued since {}",
                    j.id, j.attempt, j.labels, j.created_at
                ));
            }
        }
        for s in starved {
            self.violate(Kind::Starved, s);
        }
    }

    /// Records a `Leak` violation for each job fork that is still there.
    pub fn check_no_forks_left(&mut self) {
        let left: Vec<String> = self
            .sandboxes
            .iter()
            .filter(|s| s.job().is_some() && s.state.live())
            .map(|s| format!("{} ({:?}, job {:?})", s.id, s.state, s.job()))
            .collect();
        for l in left {
            self.violate(Kind::Leak, format!("job fork left: {l}"));
        }
    }

    /// Machines of any daemon's pools that are not deleted.
    pub fn live_pool_machines(&self) -> Vec<&Sandbox> {
        self.sandboxes
            .iter()
            .filter(|s| s.state.live())
            .filter(|s| self.pools.iter().any(|o| s.pool() == Some(&o.pool.name)))
            .collect()
    }

    pub fn requests_matching(&self, f: impl Fn(&Req) -> bool) -> Vec<&Req> {
        self.log.iter().filter(|r| f(r)).collect()
    }
}

fn rate_headers(limit: u64, used: u64, hour: u64) -> Vec<(String, String)> {
    vec![
        ("x-ratelimit-limit".into(), limit.to_string()),
        (
            "x-ratelimit-remaining".into(),
            limit.saturating_sub(used).to_string(),
        ),
        ("x-ratelimit-reset".into(), ((hour + 1) * 3_600).to_string()),
    ]
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn query_map(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), percent_decode(v)))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------------------
// The transport

/// boat and GitHub behind the daemon's `Transport`.
pub struct Net {
    pub world: RefCell<World>,
}

impl Transport for Net {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, NetworkError> {
        self.world.borrow_mut().handle(method, url, headers, body)
    }
}

// ---------------------------------------------------------------------------------------
// The harness

pub type Decide = fn(&Snapshot, &Pool) -> Vec<Action>;

pub fn boat_key(i: usize) -> String {
    format!("boat-key-{i}")
}

pub fn gh_token(i: usize) -> String {
    format!("gh-token-{i}")
}

/// A policy from TOML, panicking on an invalid one.
pub fn policy(text: &str) -> Config {
    policy::parse(text).unwrap_or_else(|e| panic!("policy: {e}\n{text}"))
}

struct Daemon {
    cfg: &'static Config,
    lp: Option<Loop<'static>>,
}

/// Daemons ticking over one world. The world and policies are leaked so the loops can
/// borrow them for `'static`; a test leaks a few hundred KB at most.
pub struct Sim {
    pub net: &'static Net,
    daemons: Vec<Daemon>,
    pub now: UnixSecs,
    pub tick_s: u64,
    pub decide: Decide,
    /// Ticks each daemon has left to stay down after an exit; a supervisor restarts it.
    down: Vec<u64>,
    /// Ticks a daemon stays down after its tick returned an error.
    pub restart_ticks: u64,
}

impl Sim {
    /// Daemon `i` runs `cfgs[i]` with credentials `boat_key(i)` and `gh_token(i)`, and all
    /// start at `world.now`.
    pub fn new(world: World, cfgs: Vec<Config>) -> Sim {
        let mut world = world;
        for (owner, cfg) in cfgs.iter().enumerate() {
            for pool in &cfg.pools {
                world.pools.push(Owned {
                    owner,
                    repo: cfg.repo.clone(),
                    pool: pool.clone(),
                });
            }
        }
        let now = world.now;
        let tick_s = cfgs.first().map_or(15, |c| c.tick_s);
        let net: &'static Net = Box::leak(Box::new(Net {
            world: RefCell::new(world),
        }));
        let mut sim = Sim {
            net,
            daemons: Vec::new(),
            now,
            tick_s,
            decide: decide::decide,
            down: vec![0; cfgs.len()],
            restart_ticks: 1,
        };
        for cfg in cfgs {
            let cfg: &'static Config = Box::leak(Box::new(cfg));
            sim.daemons.push(Daemon { cfg, lp: None });
        }
        for i in 0..sim.daemons.len() {
            sim.start(i);
        }
        sim
    }

    /// As `new`, but the daemons started `HTTP_TIMEOUT_S` before `world.now`, so their
    /// restart wait is over when the first tick runs at `world.now`.
    pub fn warmed(world: World, cfgs: Vec<Config>) -> Sim {
        let mut world = world;
        let at = world.now;
        world.now = at - HTTP_TIMEOUT_S;
        let mut s = Sim::new(world, cfgs);
        s.world_only(HTTP_TIMEOUT_S, HTTP_TIMEOUT_S);
        s
    }

    pub fn world(&self) -> Ref<'_, World> {
        self.net.world.borrow()
    }

    pub fn world_mut(&self) -> RefMut<'_, World> {
        self.net.world.borrow_mut()
    }

    pub fn cfg(&self, i: usize) -> &'static Config {
        self.daemons[i].cfg
    }

    /// Starts daemon `i` as a fresh process at `now`.
    pub fn start(&mut self, i: usize) {
        let d = &mut self.daemons[i];
        d.lp = Some(Loop::new(
            d.cfg,
            self.net,
            boat_key(i),
            github::Auth::Token(gh_token(i)),
            self.now,
        ));
    }

    /// The process dies and comes back with empty memory.
    pub fn restart(&mut self, i: usize) {
        self.world_mut().stats.restarts += 1;
        self.start(i);
    }

    /// The process stops for good.
    pub fn kill(&mut self, i: usize) {
        self.daemons[i].lp = None;
    }

    pub fn kill_all(&mut self) {
        for i in 0..self.daemons.len() {
            self.kill(i);
        }
    }

    pub fn running(&self, i: usize) -> bool {
        self.daemons[i].lp.is_some()
    }

    /// One tick of every running daemon, then the clock moves on a tick. A tick starts
    /// at `now` or, when the requests of the last one took longer than a tick
    /// (`Knobs::latency_s`), as soon as it ended, as `run::run` does.
    pub fn step(&mut self) {
        let now = self.now.max(self.world().now);
        self.now = now;
        {
            let mut w = self.world_mut();
            w.advance(now);
            w.tick_faults();
        }
        for i in 0..self.daemons.len() {
            if self.down[i] > 0 {
                self.down[i] -= 1;
                if self.down[i] == 0 {
                    self.start(i);
                }
                continue;
            }
            let decide = self.decide;
            let Some(lp) = self.daemons[i].lp.as_mut() else {
                continue;
            };
            let result = lp.tick(now, decide);
            let crashed = std::mem::take(&mut self.world_mut().crashed);
            if crashed {
                self.restart(i);
                continue;
            }
            if let Err(e) = result {
                let mut w = self.world_mut();
                w.stats.exits += 1;
                w.violate(Kind::Exit, format!("daemon {i}: {e}"));
                drop(w);
                // The process exits; a supervisor starts it again later.
                self.daemons[i].lp = None;
                self.down[i] = self.restart_ticks.max(1);
            }
        }
        self.now += self.tick_s;
    }

    pub fn run_for(&mut self, secs: u64) {
        let end = self.now + secs;
        while self.now < end {
            self.step();
        }
    }

    /// Runs `drain` for daemon `i` once a tick until it reports 0 or `max_ticks` pass.
    /// Returns the drain results in order.
    pub fn drain(&mut self, i: usize, max_ticks: usize) -> Vec<Result<usize, String>> {
        let mut out = Vec::new();
        for _ in 0..max_ticks {
            let now = self.now;
            self.world_mut().advance(now);
            let d = &self.daemons[i];
            // `drain` is a fresh process each time, as `sw-boat-runner drain` is.
            let mut lp = Loop::new(
                d.cfg,
                self.net,
                boat_key(i),
                github::Auth::Token(gh_token(i)),
                now,
            );
            let r = lp.drain(now).map_err(|e| e.to_string());
            let done = r == Ok(0);
            out.push(r);
            self.now += self.tick_s;
            if done {
                break;
            }
        }
        out
    }

    /// Advances the world alone, with no daemon running, in steps of `step_s`.
    pub fn world_only(&mut self, secs: u64, step_s: u64) {
        let end = self.now + secs;
        while self.now < end {
            self.now = (self.now + step_s).min(end);
            let now = self.now;
            self.world_mut().advance(now);
        }
    }

    /// Faults off and starts free, then ticks until nothing is left to do (no job a pool
    /// serves is queued or running and no job fork is left) or `max_secs` pass, so that
    /// the end checks ask "eventually". Returns the seconds it took.
    pub fn settle(&mut self, max_secs: u64) -> u64 {
        {
            let mut w = self.world_mut();
            w.faults = Faults::none();
            w.rules.clear();
            w.knobs.pricing = Pricing::free();
        }
        for i in 0..self.daemons.len() {
            if !self.running(i) && self.down[i] == 0 {
                self.start(i);
            }
        }
        let start = self.now;
        while self.now < start + max_secs {
            self.step();
            if self.world().quiet() {
                break;
            }
        }
        self.now - start
    }

    /// After `settle`: every queued job a pool serves ran, and no job fork is left.
    pub fn end_checks(&mut self) {
        let mut w = self.world_mut();
        w.check_served();
        w.check_no_forks_left();
    }

    /// Stops daemon `i`, waits `wait_s` with no daemon running, drains it, and checks the
    /// drain reaches 0 and that boat then has none of its pools' machines, listed or not.
    pub fn drain_checks(&mut self, i: usize, wait_s: u64) {
        self.kill(i);
        self.world_only(wait_s, self.tick_s);
        let results = self.drain(i, 40);
        let reached = results.last() == Some(&Ok(0));
        // Whatever boat had not listed yet would show by now.
        let lag = self.world().knobs.list_lag_s;
        self.world_only(lag + 60, 15);
        let owner = i;
        let mut w = self.world_mut();
        let pools: BTreeSet<String> = w
            .pools
            .iter()
            .filter(|o| o.owner == owner)
            .map(|o| o.pool.name.clone())
            .collect();
        let left: Vec<String> = w
            .sandboxes
            .iter()
            .filter(|s| s.state.live() && s.owner == owner)
            .filter(|s| s.pool().is_some_and(|p| pools.contains(p)))
            .map(|s| format!("{} {:?}", s.id, s.state))
            .collect();
        if !reached {
            w.violate(Kind::Drain, format!("drain never reported 0: {results:?}"));
        }
        if !left.is_empty() {
            w.violate(
                Kind::Drain,
                format!(
                    "after drain reported {:?}, boat still has {left:?}",
                    results.last()
                ),
            );
        }
    }

    /// Kills every daemon and runs the world alone past boat's longest TTL: nothing may
    /// stay Up.
    pub fn abandon_checks(&mut self) {
        self.kill_all();
        self.world_only(MAX_TTL_S + DAY_S, 6 * 3_600);
        let mut w = self.world_mut();
        let up: Vec<String> = w
            .sandboxes
            .iter()
            .filter(|s| s.state.holds_box())
            .map(|s| format!("{} {:?} ttl {}", s.id, s.state, s.ttl_s))
            .collect();
        for u in up {
            w.violate(Kind::UpForever, u);
        }
    }

    /// Violations other than those of `allowed` kinds.
    pub fn violations_except(&self, allowed: &[Kind]) -> Vec<Violation> {
        self.world()
            .violations
            .iter()
            .filter(|v| !allowed.contains(&v.kind))
            .cloned()
            .collect()
    }

    /// Panics with every violation not of an `allowed` kind.
    pub fn assert_clean(&self, allowed: &[Kind]) {
        let v = self.violations_except(allowed);
        if !v.is_empty() {
            let lines: Vec<String> = v.iter().take(20).map(ToString::to_string).collect();
            panic!(
                "{} violation(s):\n{}\nstats: {:?}",
                v.len(),
                lines.join("\n"),
                self.world().stats
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// Workloads

/// Labels of a job the default test pool serves.
pub fn large() -> Vec<String> {
    vec!["self-hosted".into(), "large".into()]
}

/// A one-pool policy for `o/r` with `preset` and extra pool keys.
pub fn one_pool(preset: &str, extra: &str) -> Config {
    policy(&format!(
        "repo = \"o/r\"\n[[pool]]\nname = \"ci\"\nlabels = [\"self-hosted\", \"large\"]\n\
         machine_type = \"large\"\nsnapshot = \"snap\"\npreset = \"{preset}\"\n{extra}\n"
    ))
}
