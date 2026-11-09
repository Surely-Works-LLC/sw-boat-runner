//! The tick loop: observe boat and GitHub, ask `decide` per pool, carry out its actions.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use crate::boat;
use crate::github;
use crate::model::{
    Action, Cents, Config, JobKey, Limits, Machine, MachineState, Pool, Runner, RunnerState,
    Snapshot, UnixSecs,
};

const DAY_S: u64 = 86_400;
/// boat's file write and command calls wait up to 60 s for a machine that is starting. Also
/// how long after a restart a create sent before the crash may still be in flight.
const HTTP_TIMEOUT: Duration = Duration::from_secs(90);

pub struct NetworkError(pub String);

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

fn find_header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The one seam between both clients and the network, so tests run without one.
pub trait Transport {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, NetworkError>;
}

pub struct Ureq(ureq::Agent);

impl Ureq {
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Both clients map every status themselves.
            .http_status_as_error(false)
            .build();
        Ureq(ureq::Agent::new_with_config(config))
    }
}

impl Transport for Ureq {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Response, NetworkError> {
        let mut req = ureq::http::Request::builder().method(method).uri(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let sent = match body {
            Some(b) => req.body(b.to_vec()).map(|r| self.0.run(r)),
            None => req.body(()).map(|r| self.0.run(r)),
        };
        // These errors name the URL at most, never headers or bodies.
        let mut resp = sent
            .map_err(|e| NetworkError(e.to_string()))?
            .map_err(|e| NetworkError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let body = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| NetworkError(e.to_string()))?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

#[derive(Debug)]
pub enum Error {
    MissingEnv(&'static str),
    Boat(boat::Error),
    GitHub(github::Error),
    /// A start after boat refused one, or after the price moved, in the same tick.
    StartsClosed,
    /// An action naming a pool or machine the snapshot does not have.
    Invalid(String),
}

impl Error {
    fn is_fatal(&self) -> bool {
        match self {
            Error::MissingEnv(_) => true,
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

pub fn run(cfg: &Config, decide: fn(&Snapshot, &Pool) -> Vec<Action>) -> Result<(), Error> {
    let boat_key = env("BOAT_API_KEY")?;
    let token = env("GITHUB_TOKEN")?;
    let transport = Ureq::new(HTTP_TIMEOUT);
    let mut tick_loop = Loop::new(cfg, &transport, boat_key, token, unix_now());
    let period = Duration::from_secs(cfg.tick_s.max(1));
    loop {
        let started = Instant::now();
        tick_loop.tick(unix_now(), decide)?;
        std::thread::sleep(period.saturating_sub(started.elapsed()));
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

/// Execution order for one tick: free capacity before using it, cheap before paid.
fn rank(a: &Action) -> u8 {
    match a {
        Action::Unregister { .. } => 0,
        Action::Delete { .. } => 1,
        Action::Stop { .. } => 2,
        Action::Register { .. } => 3,
        Action::Resume { .. } => 4,
        Action::Fork { .. } => 5,
    }
}

/// `<job id>-<attempt>`: the job label of a forked machine.
fn job_label(job: JobKey) -> String {
    format!("{}-{}", job.id, job.attempt)
}

fn parse_job_label(v: &str) -> Option<JobKey> {
    let (id, attempt) = v.split_once('-')?;
    Some(JobKey {
        id: id.parse().ok()?,
        attempt: attempt.parse().ok()?,
    })
}

fn cents(dollars: Option<f64>) -> Cents {
    dollars.map_or(0, |d| (d * 100.0).round().max(0.0) as Cents)
}

/// Where the jit config of `slot` waits for the start command; deleted once read.
fn jit_path(slot: u32) -> String {
    format!("/home/user/.sbr-jit-{slot}")
}

/// Starts slot `slot`'s runner from the snapshot's `/home/user/actions-runner`, in its own
/// directory because a runner keeps its state files next to `run.sh`. boat stores every
/// command for 90 days and offers no stdin or env on commands, so the jit config travels
/// as a file the command reads into `ACTIONS_RUNNER_INPUT_JITCONFIG` and then deletes. The
/// `[b]in` pattern keeps `pkill` from matching this shell's own command line.
fn start_command(slot: u32) -> String {
    let f = jit_path(slot);
    format!(
        "set -eu; d=/home/user/sbr/{slot}; f={f}; \
         pkill -f \"/home/user/sbr/{slot}/[b]in/Runner\" || true; \
         if [ ! -x \"$d/run.sh\" ]; then mkdir -p /home/user/sbr; cp -a /home/user/actions-runner \"$d\"; fi; \
         cd \"$d\"; rm -f .runner .credentials .credentials_rsaparams; \
         ACTIONS_RUNNER_INPUT_JITCONFIG=\"$(cat \"$f\")\"; rm -f \"$f\"; \
         export ACTIONS_RUNNER_INPUT_JITCONFIG; exec ./run.sh"
    )
}

struct Seen {
    state: MachineState,
    since: UnixSecs,
    last_busy: UnixSecs,
}

/// Whether this tick may still start machines, and the price `decide` was shown.
struct Starts {
    open: bool,
    price: Cents,
}

/// The actions `decide` asks for across all pools, in file order. Each pool sees only the
/// queued jobs it is the first in the file to serve, so two pools never fork for one job,
/// and sees the spend of the starts earlier pools plan, so no pool overruns its cap.
pub fn plan<'c>(
    cfg: &'c Config,
    snap: &Snapshot,
    decide: fn(&Snapshot, &Pool) -> Vec<Action>,
) -> Vec<(&'c Pool, Action)> {
    let mut out = Vec::new();
    let mut planned: Cents = 0;
    for pool in &cfg.pools {
        let mut routed = snap.clone();
        routed.queued.retain(|j| {
            cfg.pools
                .iter()
                .find(|p| p.serves(&j.labels))
                .is_some_and(|p| p.name == pool.name)
        });
        routed.limits.spent_today_cents = snap.limits.spent_today_cents.saturating_add(planned);
        let actions = decide(&routed, pool);
        let starts = actions
            .iter()
            .filter(|a| matches!(a, Action::Resume { .. } | Action::Fork { .. }))
            .count();
        let starts = Cents::try_from(starts).unwrap_or(Cents::MAX);
        planned = planned.saturating_add(starts.saturating_mul(snap.limits.next_start_cents));
        out.extend(actions.into_iter().map(|a| (pool, a)));
    }
    out
}

pub struct Loop<'a> {
    cfg: &'a Config,
    boat: boat::Client<'a>,
    gh: github::Client<'a>,
    machines: HashMap<String, Seen>,
    /// Runner names this process has seen online; see `github::runner_state`.
    online: HashSet<String>,
    /// The UTC day `spent_cents` counts; `None` until the first tick.
    spent_day: Option<u64>,
    spent_cents: Cents,
    starts_blocked_until: UnixSecs,
    /// True until the first observe that succeeds after this process started.
    restarted: bool,
    /// Forks this process created that boat has not listed yet.
    pending: Vec<Pending>,
}

/// A fork boat accepted but may not list for a while. Until it is listed, `decide` sees
/// it as a `Starting` machine, so boat's lag cannot fork its job again or pass `max_boxes`.
struct Pending {
    id: String,
    pool: String,
    job: Option<JobKey>,
    sent: UnixSecs,
}

impl<'a> Loop<'a> {
    pub fn new(
        cfg: &'a Config,
        t: &'a dyn Transport,
        boat_key: String,
        token: String,
        now: UnixSecs,
    ) -> Self {
        Loop {
            cfg,
            boat: boat::Client::new(t, boat_key),
            gh: github::Client::new(t, token, cfg.repo.clone()),
            machines: HashMap::new(),
            online: HashSet::new(),
            spent_day: None,
            // boat does not report what was spent today, so after a restart every pool's
            // budget counts as spent until 00:00 UTC.
            spent_cents: cfg
                .pools
                .iter()
                .map(|p| p.paid_start_cents_per_day)
                .max()
                .unwrap_or(0),
            // A fork sent before a crash may not be listed yet. Its key is a fresh UUID, so
            // a new fork would not find it; wait until that create is listed or has failed.
            starts_blocked_until: now.saturating_add(HTTP_TIMEOUT.as_secs()),
            restarted: true,
            pending: Vec::new(),
        }
    }

    /// One observe-decide-act pass. Returns `Err` only for errors no later tick can fix.
    pub fn tick(
        &mut self,
        now: UnixSecs,
        decide: fn(&Snapshot, &Pool) -> Vec<Action>,
    ) -> Result<(), Error> {
        let day = now / DAY_S;
        if self.spent_day.is_some_and(|d| d != day) {
            self.spent_cents = 0;
        }
        self.spent_day = Some(day);
        self.gh.begin_tick(now);
        let (snapshot, mut runner_ids) = match self.observe(now) {
            Ok(v) => v,
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => {
                eprintln!("observe: {e}");
                return Ok(());
            }
        };
        let mut actions: Vec<Action> = plan(self.cfg, &snapshot, decide)
            .into_iter()
            .map(|(_, a)| a)
            .collect();
        actions.sort_by_key(rank);
        let mut starts = Starts {
            open: snapshot.limits.can_start,
            price: snapshot.limits.next_start_cents,
        };
        for action in &actions {
            match self.execute(now, action, &mut runner_ids, &mut starts) {
                Ok(()) => {}
                Err(e) if e.is_fatal() => return Err(e),
                Err(e @ Error::GitHub(github::Error::RateLimited { .. })) => {
                    eprintln!("{action:?}: {e}; rest of tick skipped");
                    break;
                }
                Err(e) => eprintln!("{action:?}: {e}"),
            }
        }
        Ok(())
    }

    fn observe(&mut self, now: UnixSecs) -> Result<(Snapshot, HashMap<String, u64>), Error> {
        let mut observed = Vec::new();
        for pool in &self.cfg.pools {
            for sb in self.boat.list(&pool.name)? {
                let job = sb
                    .labels
                    .get(boat::JOB_LABEL)
                    .map(String::as_str)
                    .and_then(parse_job_label);
                observed.push((
                    sb.id,
                    pool.name.clone(),
                    boat::machine_state(&sb.state),
                    job,
                ));
            }
        }
        let gh_runners = self.gh.runners()?;
        let queued = self.gh.queued_jobs()?;
        let limits = self.boat.limits()?;

        let mut runner_ids = HashMap::new();
        let mut runners = Vec::new();
        for r in gh_runners {
            let Some(n) = github::parse_runner_name(&r.name) else {
                continue;
            };
            if !self.cfg.pools.iter().any(|p| p.name == n.pool) {
                continue;
            }
            let online = r.status == "online";
            if online {
                self.online.insert(r.name.clone());
            }
            let state = github::runner_state(online, r.busy, self.online.contains(&r.name));
            runner_ids.insert(r.name.clone(), r.id);
            runners.push(Runner {
                name: r.name,
                pool: n.pool,
                machine: n.machine,
                slot: n.slot,
                registered_at: n.registered_at,
                state,
            });
        }
        self.online.retain(|name| runner_ids.contains_key(name));

        let busy: HashSet<&str> = runners
            .iter()
            .filter(|r| matches!(r.state, RunnerState::Busy | RunnerState::Registered))
            .map(|r| r.machine.as_str())
            .collect();
        let mut machines = Vec::with_capacity(observed.len());
        for (id, pool, state, job) in observed {
            // After a restart nothing records whether an Up fork already had a runner, so
            // it counts as used: deleted, never registered again. A fork that never had one
            // wastes its start. Otherwise, just before first sight, so it has not.
            let used = self.restarted && job.is_some() && state == MachineState::Up;
            let seen = self.machines.entry(id.clone()).or_insert(Seen {
                state,
                since: now,
                last_busy: if used { now } else { now.saturating_sub(1) },
            });
            if seen.state != state {
                seen.state = state;
                seen.since = now;
            }
            if busy.contains(id.as_str()) {
                seen.last_busy = now;
            }
            machines.push(Machine {
                since: seen.since,
                last_busy: seen.last_busy,
                id,
                pool,
                state,
                job,
            });
        }
        self.machines
            .retain(|id, _| machines.iter().any(|m| &m.id == id));
        self.restarted = false;
        // A create that answered has had its full request timeout and a tick to show up.
        let lag_s = HTTP_TIMEOUT.as_secs().saturating_add(self.cfg.tick_s);
        self.pending.retain(|p| {
            if machines.iter().any(|m| m.id == p.id) {
                return false;
            }
            let keep = now.saturating_sub(p.sent) <= lag_s;
            if !keep {
                eprintln!(
                    "warn: fork {} in pool {} was never listed; dropped",
                    p.id, p.pool
                );
            }
            keep
        });
        machines.extend(self.pending.iter().map(|p| Machine {
            id: p.id.clone(),
            pool: p.pool.clone(),
            state: MachineState::Starting,
            since: p.sent,
            last_busy: p.sent.saturating_sub(1),
            job: p.job,
        }));

        let limits = Limits {
            // A null price means boat would refuse the start.
            can_start: limits.can_start
                && limits.next_start_dollars.is_some()
                && now >= self.starts_blocked_until,
            next_start_cents: cents(limits.next_start_dollars),
            spent_today_cents: self.spent_cents,
        };
        let snapshot = Snapshot {
            now,
            machines,
            runners,
            queued,
            limits,
        };
        Ok((snapshot, runner_ids))
    }

    fn execute(
        &mut self,
        now: UnixSecs,
        action: &Action,
        runner_ids: &mut HashMap<String, u64>,
        starts: &mut Starts,
    ) -> Result<(), Error> {
        match action {
            Action::Unregister { runner } => match runner_ids.get(runner) {
                Some(&id) => {
                    self.delete_runner(id)?;
                    runner_ids.remove(runner);
                    Ok(())
                }
                None => Ok(()),
            },
            Action::Delete { machine } => {
                self.remove_runners(machine, runner_ids)?;
                match self.boat.delete(machine) {
                    Err(boat::Error::NotFound) => Ok(()),
                    r => r.map_err(Error::from),
                }
            }
            Action::Stop { machine } => {
                self.remove_runners(machine, runner_ids)?;
                self.boat.stop(machine).map_err(Error::from)
            }
            Action::Register {
                pool,
                machine,
                slot,
            } => self.register(now, pool, machine, *slot),
            Action::Resume { machine } => self.start(now, starts, |b| b.resume(machine)),
            Action::Fork { pool, job } => {
                let p = self.pool(pool)?;
                let mut labels = BTreeMap::from([(boat::POOL_LABEL.to_string(), p.name.clone())]);
                let ttl_s = match job {
                    Some(job) => {
                        labels.insert(boat::JOB_LABEL.to_string(), job_label(*job));
                        p.fork_ttl_s
                    }
                    None => boat::MAX_TTL_S,
                };
                // A fresh key per fork, never reused: boat answers a known key with the
                // first machine for 24 h, even a deleted one, so a reused key would give a
                // job or a kept fork a dead machine (docs/design.md#observed-state).
                let key = Uuid::new_v4().to_string();
                let mut created = None;
                let result = self.start(now, starts, |b| {
                    created = Some(b.create_from_snapshot(
                        &p.snapshot,
                        &p.machine_type,
                        &labels,
                        ttl_s,
                        &key,
                    )?);
                    Ok(())
                });
                if let Some(id) = created {
                    self.pending.push(Pending {
                        id,
                        pool: p.name.clone(),
                        job: *job,
                        sent: now,
                    });
                }
                result
            }
        }
    }

    fn pool(&self, name: &str) -> Result<&'a Pool, Error> {
        let cfg: &'a Config = self.cfg;
        cfg.pools
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| Error::Invalid(format!("unknown pool {name}")))
    }

    fn delete_runner(&mut self, id: u64) -> Result<(), Error> {
        match self.gh.delete_runner(id) {
            Err(github::Error::NotFound) => Ok(()),
            r => r.map_err(Error::from),
        }
    }

    /// Deletes every GitHub runner on `machine` before boat stops or deletes it, because a
    /// runner `decide` saw idle may have taken a job since. A deleted runner takes no job,
    /// and GitHub refuses to delete one that runs a job; that error skips the machine this
    /// tick.
    fn remove_runners(
        &mut self,
        machine: &str,
        runner_ids: &mut HashMap<String, u64>,
    ) -> Result<(), Error> {
        let mut on_machine: Vec<(String, u64)> = runner_ids
            .iter()
            .filter(|(name, _)| {
                github::parse_runner_name(name).is_some_and(|n| n.machine == machine)
            })
            .map(|(name, &id)| (name.clone(), id))
            .collect();
        on_machine.sort_unstable_by_key(|&(_, id)| id);
        for (name, id) in on_machine {
            self.delete_runner(id)?;
            runner_ids.remove(&name);
        }
        Ok(())
    }

    /// If the file write or the command fails, the GitHub runner stays `Registered` and
    /// `decide` unregisters it after `register_timeout_s`.
    fn register(
        &mut self,
        now: UnixSecs,
        pool: &str,
        machine: &str,
        slot: u32,
    ) -> Result<(), Error> {
        let p = self.pool(pool)?;
        let name = github::runner_name(pool, machine, slot, now);
        let jit = self.gh.generate_jitconfig(&name, &p.labels)?;
        // The runner may run its job and vanish before the next observe; mark the machine
        // used now, so a fork never gets a second runner.
        if let Some(seen) = self.machines.get_mut(machine) {
            seen.last_busy = now;
        }
        self.boat
            .write_file(machine, &jit_path(slot), jit.expose())?;
        self.boat.exec_detached(machine, &start_command(slot))?;
        Ok(())
    }

    /// Runs one paid-or-free start. An accepted start adds the price `decide` saw to
    /// today's spend; a refused one closes starts until boat's `Retry-After` passes.
    fn start(
        &mut self,
        now: UnixSecs,
        starts: &mut Starts,
        call: impl FnOnce(&boat::Client) -> Result<(), boat::Error>,
    ) -> Result<(), Error> {
        if !starts.open {
            return Err(Error::StartsClosed);
        }
        match call(&self.boat) {
            Ok(()) => {
                self.spent_cents = self.spent_cents.saturating_add(starts.price);
                // `decide` priced every start at one quote; stop once the quote moves.
                match self.boat.limits() {
                    Ok(l)
                        if l.can_start
                            && l.next_start_dollars.is_some()
                            && cents(l.next_start_dollars) == starts.price => {}
                    Ok(_) => starts.open = false,
                    Err(e) if e.is_fatal() => return Err(e.into()),
                    Err(e) => {
                        starts.open = false;
                        eprintln!("limits after start: {e}");
                    }
                }
                Ok(())
            }
            Err(boat::Error::RefusedStart {
                code,
                retry_after_s,
            }) => {
                self.starts_blocked_until = now + retry_after_s;
                starts.open = false;
                Err(boat::Error::RefusedStart {
                    code,
                    retry_after_s,
                }
                .into())
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
pub mod fake {
    use std::cell::RefCell;

    use super::{NetworkError, Response, Transport, find_header};

    #[derive(Debug, Clone)]
    pub struct Sent {
        pub method: String,
        pub url: String,
        pub headers: Vec<(String, String)>,
        pub body: Option<Vec<u8>>,
    }

    impl Sent {
        pub fn header(&self, name: &str) -> Option<&str> {
            find_header(&self.headers, name)
        }

        pub fn body_text(&self) -> String {
            String::from_utf8_lossy(self.body.as_deref().unwrap_or_default()).into_owned()
        }
    }

    /// Answers every request with `reply` and records what was sent.
    pub struct Fake {
        reply: Box<dyn Fn(&Sent) -> Response>,
        sent: RefCell<Vec<Sent>>,
    }

    impl Fake {
        pub fn new(reply: impl Fn(&Sent) -> Response + 'static) -> Self {
            Fake {
                reply: Box::new(reply),
                sent: RefCell::new(Vec::new()),
            }
        }

        pub fn sent(&self) -> Vec<Sent> {
            self.sent.borrow().clone()
        }
    }

    impl Transport for Fake {
        fn send(
            &self,
            method: &str,
            url: &str,
            headers: &[(&str, &str)],
            body: Option<&[u8]>,
        ) -> Result<Response, NetworkError> {
            let sent = Sent {
                method: method.to_string(),
                url: url.to_string(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: body.map(<[u8]>::to_vec),
            };
            let resp = (self.reply)(&sent);
            self.sent.borrow_mut().push(sent);
            Ok(resp)
        }
    }

    pub fn respond(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Fake, Sent, respond};
    use super::*;
    use crate::model::{Placement, QueuedJob};

    const BOAT: &str = "https://boat.dev/api/v1";
    const GH: &str = "https://api.github.com/repos/o/r/actions";
    const NOW: UnixSecs = 1_000;
    /// A process started this long before `NOW` has waited out the restart wait.
    const STARTED: UnixSecs = NOW - HTTP_TIMEOUT.as_secs();

    fn config() -> Config {
        Config {
            repo: "o/r".into(),
            tick_s: 15,
            pools: vec![Pool {
                name: "ci".into(),
                labels: vec!["self-hosted".into(), "large".into()],
                machine_type: "large".into(),
                snapshot: "runner-base".into(),
                placement: vec![Placement::Fork],
                min_up: 0,
                max_boxes: 4,
                slots_per_box: 2,
                idle_stop_s: 600,
                min_up_s: 0,
                register_timeout_s: 300,
                fork_ttl_s: 3_600,
                paid_start_cents_per_day: 100,
            }],
        }
    }

    fn reply(s: &Sent) -> Response {
        let url = s.url.as_str();
        let body = match (s.method.as_str(), url) {
            ("GET", u) if u.starts_with(&format!("{BOAT}/sandboxes?")) => {
                r#"{"sandboxes":[
                    {"id":"bx_22222222","state":"idle","labels":{"sw-boat-runner":"ci"}},
                    {"id":"bx_33333333","state":"archived","labels":{"sw-boat-runner":"ci"}},
                    {"id":"bx_44444444","state":"running",
                     "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"7-2"}}],
                   "pageInfo":{"nextCursor":null,"hasMore":false,"limit":200}}"#
            }
            ("GET", u) if u.starts_with(&format!("{GH}/runners?")) => {
                r#"{"total_count":3,"runners":[
                    {"id":11,"name":"sbr-ci-bx_22222222-0-900","status":"online","busy":false},
                    {"id":12,"name":"sbr-ci-bx_44444444-0-900","status":"offline","busy":false},
                    {"id":13,"name":"sbr-other-bx_44444444-0-900","status":"online","busy":true}]}"#
            }
            ("GET", u) if u.contains("status=queued") => {
                r#"{"workflow_runs":[{"id":5,"run_attempt":1}]}"#
            }
            ("GET", u) if u.contains("status=in_progress") => r#"{"workflow_runs":[]}"#,
            ("GET", u) if u.contains("/runs/5/jobs") => {
                r#"{"jobs":[{"id":70,"run_attempt":1,"status":"queued","labels":["self-hosted","large"]}]}"#
            }
            ("GET", u) if u.ends_with("/v1/limits") => {
                r#"{"canStart":true,"nextStartDollars":0.25}"#
            }
            ("POST", u) if u.ends_with("/generate-jitconfig") => {
                return respond(
                    201,
                    &[],
                    r#"{"runner":{"id":20},"encoded_jit_config":"JITSECRET"}"#,
                );
            }
            ("POST", u) if u.ends_with("/v1/sandboxes") => {
                r#"{"type":"sandbox.created","sandbox":{"id":"bx_55555555","state":"init"}}"#
            }
            ("DELETE", u) if u.starts_with(GH) => return respond(204, &[], ""),
            _ => "{}",
        };
        respond(200, &[], body)
    }

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
                spent_today_cents: 100
            }
        );
        let m = |id: &str| id.to_string();
        vec![
            Action::Fork {
                pool: m("ci"),
                job: Some(JobKey { id: 70, attempt: 1 }),
            },
            Action::Resume {
                machine: m("bx_33333333"),
            },
            Action::Register {
                pool: m("ci"),
                machine: m("bx_22222222"),
                slot: 1,
            },
            Action::Stop {
                machine: m("bx_22222222"),
            },
            Action::Delete {
                machine: m("bx_44444444"),
            },
            Action::Unregister {
                runner: m("sbr-ci-bx_44444444-0-900"),
            },
        ]
    }

    fn requests(fake: &Fake) -> Vec<String> {
        fake.sent()
            .iter()
            .map(|s| format!("{} {}", s.method, s.url))
            .collect()
    }

    #[test]
    fn tick_executes_in_order() {
        let cfg = config();
        let fake = Fake::new(reply);
        let mut tick_loop = Loop::new(&cfg, &fake, "boat_k".into(), "gh_t".into(), STARTED);
        tick_loop.tick(NOW, decide_all).unwrap();

        let register = [
            format!("POST {GH}/runners/generate-jitconfig"),
            format!("PUT {BOAT}/sandboxes/bx_22222222/files"),
            format!("POST {BOAT}/sandboxes/bx_22222222/commands"),
        ];
        let mut expected = vec![
            format!("GET {BOAT}/sandboxes?label=sw-boat-runner%3Dci&limit=200"),
            format!("GET {GH}/runners?per_page=100&page=1"),
            format!("GET {GH}/runs?status=queued&per_page=100"),
            format!("GET {GH}/runs/5/jobs?per_page=100"),
            format!("GET {GH}/runs?status=in_progress&per_page=100"),
            format!("GET {BOAT}/limits"),
            format!("DELETE {GH}/runners/12"),
            // Runner 12 is already gone, so only the machine is deleted.
            format!("DELETE {BOAT}/sandboxes/bx_44444444"),
            // Stop deletes the machine's runner first.
            format!("DELETE {GH}/runners/11"),
            format!("POST {BOAT}/sandboxes/bx_22222222/stop"),
        ];
        expected.extend(register.clone());
        expected.extend([
            format!("POST {BOAT}/sandboxes/bx_33333333/resume"),
            format!("GET {BOAT}/limits"),
            format!("POST {BOAT}/sandboxes"),
            format!("GET {BOAT}/limits"),
        ]);
        assert_eq!(requests(&fake), expected);

        let sent = fake.sent();
        let fork = &sent[15];
        assert!(fork.body_text().contains(r#""sw-boat-runner-job":"70-1""#));
        assert!(fork.body_text().contains(r#""ttlSeconds":3600"#));
        let jit = &sent[10];
        assert!(
            jit.body_text()
                .contains(r#""name":"sbr-ci-bx_22222222-1-1000""#)
        );
        assert!(
            sent[11]
                .body_text()
                .contains(r#""path":"/home/user/.sbr-jit-1""#)
        );
        assert!(sent[12].body_text().contains("/home/user/.sbr-jit-1"));
        // The jit config goes into the file write only: never a URL, a command or a header.
        for (i, s) in sent.iter().enumerate() {
            let leaks = s.url.contains("JITSECRET")
                || s.headers.iter().any(|(_, v)| v.contains("JITSECRET"))
                || s.body_text().contains("JITSECRET");
            assert_eq!(leaks, i == 11, "request {i}: {}", s.url);
        }
        // Two accepted starts at the 25-cent quote, on top of the assumed 100.
        assert_eq!(tick_loop.spent_cents, 150);

        tick_loop.tick(DAY_S, |_, _| Vec::new()).unwrap();
        assert_eq!(tick_loop.spent_cents, 0);
    }

    fn decide_starts(s: &Snapshot, _: &Pool) -> Vec<Action> {
        if s.now > NOW {
            assert!(!s.limits.can_start, "starts stay closed until Retry-After");
        }
        vec![
            Action::Fork {
                pool: "ci".into(),
                job: Some(JobKey { id: 70, attempt: 1 }),
            },
            Action::Resume {
                machine: "bx_33333333".into(),
            },
        ]
    }

    #[test]
    fn refused_start_closes_starts() {
        let cfg = config();
        let fake = Fake::new(|s| {
            if s.url.ends_with("/resume") {
                respond(429, &[("Retry-After", "60")], r#"{"code":"rate_limited"}"#)
            } else {
                reply(s)
            }
        });
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        tick_loop.tick(NOW, decide_starts).unwrap();
        tick_loop.tick(NOW + 30, decide_starts).unwrap();
        let reqs = requests(&fake);
        let resumes = reqs.iter().filter(|r| r.ends_with("/resume")).count();
        assert_eq!(resumes, 1);
        assert!(!reqs.contains(&format!("POST {BOAT}/sandboxes")));
        assert_eq!(tick_loop.spent_cents, 100);
    }

    #[test]
    fn a_restart_makes_no_paid_starts_until_the_day_ends() {
        let mut cfg = config();
        let mut big = cfg.pools[0].clone();
        big.name = "big".into();
        big.paid_start_cents_per_day = 500;
        cfg.pools.insert(0, big);
        let fake = Fake::new(reply);
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        assert_eq!(tick_loop.spent_cents, 500);
        tick_loop.tick(NOW, crate::decide::decide).unwrap();
        // Job 70 is queued and every start is quoted at 25 cents.
        let reqs = requests(&fake);
        assert!(
            !reqs.contains(&format!("POST {BOAT}/sandboxes")),
            "{reqs:?}"
        );
        assert!(!reqs.iter().any(|r| r.ends_with("/resume")), "{reqs:?}");
        assert_eq!(tick_loop.spent_cents, 500);
    }

    fn fork_twice_each(_: &Snapshot, p: &Pool) -> Vec<Action> {
        let kept = Action::Fork {
            pool: p.name.clone(),
            job: None,
        };
        let job = Action::Fork {
            pool: p.name.clone(),
            job: Some(JobKey { id: 70, attempt: 1 }),
        };
        vec![kept.clone(), kept, job.clone(), job]
    }

    fn forks(fake: &Fake) -> Vec<Sent> {
        fake.sent()
            .into_iter()
            .filter(|s| s.method == "POST" && s.url == format!("{BOAT}/sandboxes"))
            .collect()
    }

    /// The second fork of a job stands for a fork after the first one was deleted: with the
    /// same key boat would return the deleted machine.
    #[test]
    fn forks_send_fresh_uuid_v4_keys() {
        let cfg = config();
        let fake = Fake::new(reply);
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        tick_loop.tick(NOW, fork_twice_each).unwrap();
        let forks = forks(&fake);
        let keys: HashSet<_> = forks
            .iter()
            .map(|s| s.header("idempotency-key").unwrap().to_string())
            .collect();
        assert_eq!(keys.len(), 4, "{keys:?}");
        for key in &keys {
            assert_eq!(Uuid::parse_str(key).unwrap().get_version_num(), 4, "{key}");
        }
        for fork in &forks[..2] {
            assert!(!fork.body_text().contains("sw-boat-runner-job"));
            assert!(fork.body_text().contains(r#""ttlSeconds":2592000"#));
        }
        for fork in &forks[2..] {
            assert!(fork.body_text().contains(r#""sw-boat-runner-job":"70-1""#));
            assert!(fork.body_text().contains(r#""ttlSeconds":3600"#));
        }
    }

    fn fork_kept(_: &Snapshot, p: &Pool) -> Vec<Action> {
        vec![Action::Fork {
            pool: p.name.clone(),
            job: None,
        }]
    }

    #[test]
    fn a_restart_makes_no_start_until_an_inflight_create_has_landed() {
        let cfg = config();
        let fake = Fake::new(reply);
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), NOW);
        tick_loop.tick(NOW, fork_kept).unwrap();
        tick_loop.tick(NOW + 89, fork_kept).unwrap();
        assert!(forks(&fake).is_empty());
        tick_loop.tick(NOW + 90, fork_kept).unwrap();
        assert_eq!(forks(&fake).len(), 1);
    }

    fn stop_and_delete(_: &Snapshot, _: &Pool) -> Vec<Action> {
        vec![
            Action::Stop {
                machine: "bx_22222222".into(),
            },
            Action::Delete {
                machine: "bx_44444444".into(),
            },
        ]
    }

    /// Runner 11 took a job after the snapshot showed it idle.
    #[test]
    fn stop_and_delete_skip_a_machine_whose_runner_took_a_job() {
        let cfg = config();
        let fake = Fake::new(|s| {
            if s.method == "DELETE" && s.url == format!("{GH}/runners/11") {
                respond(422, &[], r#"{"message":"Runner is still running a job"}"#)
            } else {
                reply(s)
            }
        });
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        tick_loop.tick(NOW, stop_and_delete).unwrap();
        let reqs = requests(&fake);
        assert_eq!(
            reqs[6..],
            [
                format!("DELETE {GH}/runners/12"),
                format!("DELETE {BOAT}/sandboxes/bx_44444444"),
                format!("DELETE {GH}/runners/11"),
            ]
        );
    }

    /// One job fork `bx_66666666` for queued job 70 and no runners. boat lists it in
    /// `states[i]` on the i-th tick, and in the last state after that.
    fn one_fork(states: &'static [&'static str]) -> Fake {
        let ticks = std::cell::Cell::new(0);
        Fake::new(move |s| {
            if s.method == "GET" && s.url.starts_with(&format!("{BOAT}/sandboxes?")) {
                let state = states[ticks.get().min(states.len() - 1)];
                ticks.set(ticks.get() + 1);
                let body = format!(
                    r#"{{"sandboxes":[{{"id":"bx_66666666","state":"{state}",
                         "labels":{{"sw-boat-runner":"ci","sw-boat-runner-job":"70-1"}}}}],
                       "pageInfo":{{"nextCursor":null,"hasMore":false,"limit":200}}}}"#
                );
                respond(200, &[], &body)
            } else if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                respond(200, &[], r#"{"total_count":0,"runners":[]}"#)
            } else {
                reply(s)
            }
        })
    }

    fn registers(fake: &Fake) -> usize {
        requests(fake)
            .iter()
            .filter(|r| r.ends_with("/generate-jitconfig"))
            .count()
    }

    /// The runner ran job 70 and went away between two ticks, and GitHub requeued the job.
    #[test]
    fn a_fork_that_got_a_runner_never_gets_another() {
        let cfg = config();
        let fake = one_fork(&["provisioning", "idle"]);
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        tick_loop.tick(NOW - 15, crate::decide::decide).unwrap();
        tick_loop.tick(NOW, crate::decide::decide).unwrap();
        assert_eq!(registers(&fake), 1);
        tick_loop.tick(NOW + 15, crate::decide::decide).unwrap();
        assert_eq!(registers(&fake), 1);
        let reqs = requests(&fake);
        assert!(
            reqs.contains(&format!("DELETE {BOAT}/sandboxes/bx_66666666")),
            "{reqs:?}"
        );
    }

    /// The fork may have run job 70 before the restart, and GitHub requeued the job.
    #[test]
    fn after_a_restart_an_up_fork_counts_as_used() {
        let cfg = config();
        let fake = one_fork(&["idle"]);
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        tick_loop.tick(NOW, crate::decide::decide).unwrap();
        assert_eq!(registers(&fake), 0);
        let reqs = requests(&fake);
        assert!(
            reqs.contains(&format!("DELETE {BOAT}/sandboxes/bx_66666666")),
            "{reqs:?}"
        );
    }

    /// boat lists `listing(t)` and GitHub queues jobs `jobs(t)` on the t-th tick (from 1);
    /// starts are included, and every create answers `bx_55555555`.
    fn lagging(listing: fn(usize) -> &'static str, jobs: fn(usize) -> &'static str) -> Fake {
        let ticks = std::cell::Cell::new(0);
        Fake::new(move |s| {
            let url = s.url.as_str();
            if s.method == "GET" && url.starts_with(&format!("{BOAT}/sandboxes?")) {
                ticks.set(ticks.get() + 1);
                let body = format!(
                    r#"{{"sandboxes":[{}],
                       "pageInfo":{{"nextCursor":null,"hasMore":false,"limit":200}}}}"#,
                    listing(ticks.get())
                );
                respond(200, &[], &body)
            } else if s.method == "GET" && url.starts_with(&format!("{GH}/runners?")) {
                respond(200, &[], r#"{"total_count":0,"runners":[]}"#)
            } else if s.method == "GET" && url.contains("/runs/5/jobs") {
                respond(200, &[], &format!(r#"{{"jobs":[{}]}}"#, jobs(ticks.get())))
            } else if s.method == "GET" && url.ends_with("/v1/limits") {
                respond(200, &[], r#"{"canStart":true,"nextStartDollars":0}"#)
            } else {
                reply(s)
            }
        })
    }

    const JOB_70: &str =
        r#"{"id":70,"run_attempt":1,"status":"queued","labels":["self-hosted","large"]}"#;
    const JOBS_70_71: &str = r#"{"id":70,"run_attempt":1,"status":"queued","labels":["self-hosted","large"]},
        {"id":71,"run_attempt":1,"status":"queued","labels":["self-hosted","large"]}"#;
    const LISTED_55: &str = r#"{"id":"bx_55555555","state":"provisioning",
        "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"70-1"}}"#;

    /// Machines `observe` hands `decide` at `now`.
    fn observed(tick_loop: &mut Loop, now: UnixSecs) -> Vec<(String, MachineState)> {
        tick_loop.gh.begin_tick(now);
        let (snapshot, _) = tick_loop.observe(now).unwrap();
        snapshot
            .machines
            .into_iter()
            .map(|m| (m.id, m.state))
            .collect()
    }

    fn fork_job_70(_: &Snapshot, p: &Pool) -> Vec<Action> {
        vec![Action::Fork {
            pool: p.name.clone(),
            job: Some(JobKey { id: 70, attempt: 1 }),
        }]
    }

    /// boat omits the fork for job 70 from the next listing. Once it lists it, the fork
    /// counts once toward `max_boxes`, so job 71 still gets the second box.
    #[test]
    fn a_fork_boat_has_not_listed_is_not_forked_again() {
        let mut cfg = config();
        cfg.pools[0].paid_start_cents_per_day = 0;
        cfg.pools[0].max_boxes = 2;
        let fake = lagging(
            |t| if t < 3 { "" } else { LISTED_55 },
            |t| if t < 3 { JOB_70 } else { JOBS_70_71 },
        );
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        tick_loop.tick(NOW, crate::decide::decide).unwrap();
        assert_eq!(forks(&fake).len(), 1);
        tick_loop.tick(NOW + 15, crate::decide::decide).unwrap();
        assert_eq!(forks(&fake).len(), 1);
        tick_loop.tick(NOW + 30, crate::decide::decide).unwrap();
        let forks = forks(&fake);
        assert_eq!(forks.len(), 2);
        assert!(
            forks[1]
                .body_text()
                .contains(r#""sw-boat-runner-job":"71-1""#)
        );
    }

    #[test]
    fn a_pending_fork_clears_once_boat_lists_it() {
        let cfg = config();
        // Listed on the third tick only; boat dropped it by the fourth.
        let fake = lagging(|t| if t == 3 { LISTED_55 } else { "" }, |_| JOB_70);
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        tick_loop.tick(NOW, fork_job_70).unwrap();
        let starting = [("bx_55555555".to_string(), MachineState::Starting)];
        assert_eq!(observed(&mut tick_loop, NOW + 15), starting);
        assert_eq!(observed(&mut tick_loop, NOW + 30), starting);
        assert!(observed(&mut tick_loop, NOW + 45).is_empty());
    }

    #[test]
    fn a_pending_fork_expires_after_the_request_timeout_and_a_tick() {
        let cfg = config();
        let fake = lagging(|_| "", |_| JOB_70);
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        tick_loop.tick(NOW, fork_job_70).unwrap();
        let lag_s = HTTP_TIMEOUT.as_secs() + cfg.tick_s;
        assert_eq!(
            observed(&mut tick_loop, NOW + lag_s),
            [("bx_55555555".to_string(), MachineState::Starting)]
        );
        assert!(observed(&mut tick_loop, NOW + lag_s + 1).is_empty());
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
            },
        };
        let planned: Vec<_> = plan(&cfg, &snap, fork_each_job)
            .into_iter()
            .map(|(p, a)| match a {
                Action::Fork { job: Some(j), .. } => (p.name.as_str(), j.id),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(planned, [("ci", 1), ("b", 2)]);
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
        let mut tick_loop = Loop::new(&cfg, &fake, "k".into(), "t".into(), STARTED);
        let err = tick_loop.tick(NOW, |_, _| Vec::new()).unwrap_err();
        assert!(matches!(err, Error::Boat(boat::Error::BillingRequired)));
    }

    #[test]
    fn job_labels_round_trip() {
        let job = JobKey { id: 9, attempt: 3 };
        assert_eq!(parse_job_label(&job_label(job)), Some(job));
        assert_eq!(parse_job_label("9"), None);
        assert_eq!(cents(Some(0.29)), 29);
        assert_eq!(cents(None), 0);
    }
}
