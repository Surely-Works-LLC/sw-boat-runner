//! Carries out a tick's actions, and keeps the start budget and back-off from tick to tick
//! (docs/design.md#start-budget).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use uuid::Uuid;

use super::observe::Observed;
use super::{Error, HTTP_TIMEOUT, Loop, script};
use crate::boat;
use crate::github;
use crate::model::{
    Action, Cents, Config, JobKey, MachineState, Placement, RunnerState, Snapshot, UnixSecs,
};

pub(super) const DAY_S: u64 = 86_400;
/// boat's 409 code for a resume of a machine whose disk it has deleted, which it may do
/// once the machine has been stopped for 14 days.
const DISK_EXPIRED: &str = "snapshot_data_expired";
/// Back-off for a start that boat keeps refusing with the same error: the first wait, and
/// the most it doubles to.
const BACKOFF_MIN_S: u64 = 15;
const BACKOFF_MAX_S: u64 = 600;
/// The first wait of a pool whose starts boat refuses with a 403, such as a
/// `machine_type` the plan does not allow. It doubles to `BACKOFF_MAX_S`.
const REFUSED_MIN_S: u64 = 60;

/// The start budget and back-off, kept from tick to tick. Only this module writes it.
pub(super) struct Ledger {
    /// The UTC day `spent_cents` counts.
    day: u64,
    spent_cents: Cents,
    /// No start before this.
    blocked_until: UnixSecs,
    /// Starts that failed last time they were sent; see `Failing`.
    failing: HashMap<(String, StartKind), Failing>,
    /// The price booked for the last start boat accepted or may have accepted, until a
    /// later quote is read; see `quote_seen`.
    unconfirmed: Option<Cents>,
    /// `spent_cents` was set to the largest cap at start, as the real spend before the
    /// restart is not known, and the UTC day has not turned since.
    spend_unknown: bool,
}

/// The kinds of action that cost a start, which back off apart in each pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum StartKind {
    Fork,
    Resume,
}

/// The last error of a kind of start in a pool. When the next one fails the same way, the
/// pool's starts of that kind wait `backoff_s`, doubling each time, so a refusal no tick
/// can fix (a snapshot too big for its machine type) is not sent every tick. Deletes,
/// stops and runner calls never wait.
struct Failing {
    error: String,
    backoff_s: u64,
    until: UnixSecs,
    why: Why,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Why {
    /// The same error twice in a row; forgotten once `decide` stops asking for the start.
    Repeated,
    /// Machines removed for holding their creator's logins. A fork boat accepts proves
    /// nothing then, so only a machine of the pool found without them clears it.
    Logins,
    /// boat refused the pool's starts with a 403; an accepted start clears it.
    Refused,
}

impl Ledger {
    /// The ledger of a process that started at `now`.
    pub(super) fn new(cfg: &Config, now: UnixSecs) -> Self {
        Ledger {
            day: now / DAY_S,
            // boat does not report what was spent today, so after a restart every pool's
            // budget counts as spent until 00:00 UTC (`FixCrashSpend`).
            spent_cents: cfg
                .pools
                .iter()
                .map(|p| p.paid_start_cents_per_day)
                .max()
                .unwrap_or(0),
            // A fork sent before a crash may not be listed yet. Its key is a fresh UUID, so
            // a new fork would not find it; wait until that create is listed or has failed
            // (`FixKeptInflight`).
            blocked_until: now.saturating_add(HTTP_TIMEOUT.as_secs()),
            failing: HashMap::new(),
            unconfirmed: None,
            spend_unknown: true,
        }
    }

    /// Books `price` for a start boat accepted, or may have, at the quote `decide` saw.
    fn book(&mut self, price: Cents) {
        self.spent_cents = self.spent_cents.saturating_add(price);
        self.unconfirmed = Some(price);
    }

    /// The quote after a start. boat quotes only the next start, so a quote above the
    /// price booked for the last one means that start may have cost the new price:
    /// someone else on the account took the last included start first, or the price
    /// went up. The difference is booked too, so the cap holds; when it was this
    /// daemon's own start that used the last included one, that counts a free start as
    /// paid, which only delays paid starts.
    pub(super) fn quote_seen(&mut self, cents: Cents) {
        if let Some(booked) = self.unconfirmed.take() {
            self.spent_cents = self
                .spent_cents
                .saturating_add(cents.saturating_sub(booked));
        }
    }

    pub(super) fn begin_tick(&mut self, now: UnixSecs) {
        if now / DAY_S != self.day {
            self.day = now / DAY_S;
            self.spent_cents = 0;
            self.unconfirmed = None;
            self.spend_unknown = false;
        }
    }

    pub(super) fn spent_cents(&self) -> Cents {
        self.spent_cents
    }

    /// Today's spend for the log: the real figure, or that it is not known since the
    /// restart and counted as the cap.
    pub(super) fn spend_note(&self) -> String {
        if self.spend_unknown {
            "spend today unknown since restart (counted as the cap)".to_string()
        } else {
            format!("spent {}c today", self.spent_cents)
        }
    }

    pub(super) fn starts_open(&self, now: UnixSecs) -> bool {
        now >= self.blocked_until
    }

    fn block_until(&mut self, until: UnixSecs) {
        self.blocked_until = self.blocked_until.max(until);
    }

    fn backing_off(&self, key: &(String, StartKind), now: UnixSecs) -> bool {
        self.failing.get(key).is_some_and(|f| now < f.until)
    }

    /// Forgets the starts `decide` no longer asks for, except a logins or refusal back-off.
    fn keep_asked(&mut self, asked: &HashSet<(String, StartKind)>) {
        self.failing
            .retain(|k, f| f.why != Why::Repeated || asked.contains(k));
    }

    /// A start boat accepted ends its back-off, except a logins back-off.
    fn succeeded(&mut self, key: &(String, StartKind)) {
        if !self.failing.get(key).is_some_and(|f| f.why == Why::Logins) {
            self.failing.remove(key);
        }
    }

    /// Backs off `key` for `why`, starting at `min_s` and doubling while the same `why`
    /// repeats. Returns the wait.
    fn back_off(
        &mut self,
        now: UnixSecs,
        key: (String, StartKind),
        why: Why,
        min_s: u64,
        error: String,
    ) -> u64 {
        let backoff_s = match self.failing.get(&key) {
            Some(f) if f.why == why => (f.backoff_s * 2).clamp(min_s, BACKOFF_MAX_S),
            _ => min_s,
        };
        let until = now.saturating_add(backoff_s);
        let failing = Failing {
            error,
            backoff_s,
            until,
            why,
        };
        self.failing.insert(key, failing);
        backoff_s
    }

    /// A machine of `pool` held its creator's logins: the pool's forks, of jobs and of kept
    /// machines alike, wait, doubling while new ones keep holding them. Returns the wait.
    fn fork_held_logins(&mut self, now: UnixSecs, pool: &str, error: String) -> u64 {
        let key = (pool.to_string(), StartKind::Fork);
        self.back_off(now, key, Why::Logins, BACKOFF_MIN_S, error)
    }

    /// A machine of `pool` held none of its creator's logins, which ends a logins back-off.
    fn pool_free_of_logins(&mut self, pool: &str) {
        let key = (pool.to_string(), StartKind::Fork);
        if self.failing.get(&key).is_some_and(|f| f.why == Why::Logins) {
            self.failing.remove(&key);
        }
    }

    /// boat refused a start of `pool` with a 403: every start of the pool waits, doubling
    /// while boat keeps refusing, and other pools go on. Returns the wait.
    fn pool_refused(&mut self, now: UnixSecs, pool: &str, error: String) -> u64 {
        let mut wait_s = 0;
        for kind in [StartKind::Fork, StartKind::Resume] {
            let key = (pool.to_string(), kind);
            // A logins back-off stays: only a clean machine ends it.
            if self.failing.get(&key).is_some_and(|f| f.why == Why::Logins) {
                continue;
            }
            wait_s = self.back_off(now, key, Why::Refused, REFUSED_MIN_S, error.clone());
        }
        wait_s
    }

    /// Logs a failed start. The same error twice in a row backs the start off; a repeat
    /// is logged only when it is sent again after its wait.
    fn note_failure(
        &mut self,
        now: UnixSecs,
        key: (String, StartKind),
        action: &Action,
        e: &Error,
    ) {
        let error = e.to_string();
        let backoff_s = match self.failing.get(&key) {
            Some(f) if f.error == error => (f.backoff_s * 2).clamp(BACKOFF_MIN_S, BACKOFF_MAX_S),
            _ => 0,
        };
        if backoff_s == 0 {
            eprintln!("{action:?}: {e}");
        } else {
            eprintln!("{action:?}: {e} (again; next try in {backoff_s}s)");
        }
        let until = now.saturating_add(backoff_s);
        let why = Why::Repeated;
        self.failing.insert(
            key,
            Failing {
                error,
                backoff_s,
                until,
                why,
            },
        );
    }
}

/// Whether this tick may still start machines.
#[derive(Clone, Copy)]
enum Starts {
    /// At `price`, the quote `decide` was shown. `recheck` after an accepted start: the
    /// next start reads the quote again first and goes ahead only if it has not moved.
    Open {
        price: Cents,
        recheck: bool,
    },
    Closed,
}

/// What one tick's actions share.
struct Tick<'s> {
    now: UnixSecs,
    /// `Loop::clock` when the tick began at `now`, before its observe.
    began: Duration,
    snapshot: &'s Snapshot,
    /// GitHub ids of the snapshot's runners that are still there.
    runner_ids: HashMap<String, u64>,
    starts: Starts,
    /// Machines this tick leaves alone: a runner whose start failed is still registered
    /// on them.
    skip: HashSet<String>,
    /// Why `decide` deletes each machine it deletes, for the log.
    delete_reasons: HashMap<String, &'static str>,
    /// Kept machines each pool started this tick (resumes and kept forks), for the log's
    /// reason of a kept fork.
    kept_started: HashMap<String, usize>,
}

impl Tick<'_> {
    /// The time now, from the clock reading `elapsed`: `now` plus the time this tick's
    /// requests took. What waits for a request's answer is timed from here, not from
    /// `now`, which can be several request timeouts earlier.
    fn at(&self, elapsed: Duration) -> UnixSecs {
        self.now
            .saturating_add(elapsed.saturating_sub(self.began).as_secs())
    }
}

/// `<machine> (pool <pool>)`, or the machine alone when the snapshot does not have it.
fn machine_in(snap: &Snapshot, machine: &str) -> String {
    match snap.machines.iter().find(|m| m.id == machine) {
        Some(m) => format!("{machine} (pool {})", m.pool),
        None => machine.to_string(),
    }
}

/// Why `decide` unregisters `runner`, for the log: its rules in `View::unregister`.
fn unregister_reason(cfg: &Config, snap: &Snapshot, runner: &str) -> Option<&'static str> {
    let r = snap.runners.iter().find(|r| r.name == runner)?;
    let up = snap
        .machines
        .iter()
        .any(|m| m.id == r.machine && m.state == MachineState::Up);
    let timeout = cfg
        .pools
        .iter()
        .find(|p| p.name == r.pool)
        .map(|p| p.register_timeout_s);
    if r.state == RunnerState::Offline {
        Some("offline")
    } else if !up {
        Some("machine_down")
    } else if r.state == RunnerState::Registered
        && timeout.is_some_and(|t| snap.now.saturating_sub(r.registered_at) > t)
    {
        Some("register_timeout")
    } else {
        None
    }
}

/// The unregisters, deletes and stops: what frees capacity and stops the bill. An error
/// no tick can fix still lets the rest of these run.
pub(super) fn is_cleanup(a: &Action) -> bool {
    matches!(
        a,
        Action::Unregister { .. } | Action::Delete { .. } | Action::Stop { .. }
    )
}

/// The machine an action works on, if any.
fn machine_of(a: &Action) -> Option<&str> {
    match a {
        Action::Delete { machine }
        | Action::Stop { machine }
        | Action::Register { machine, .. }
        | Action::Resume { machine } => Some(machine),
        Action::Unregister { .. } | Action::Fork { .. } => None,
    }
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

/// The pool and kind a start backs off under; `None` for actions that never wait.
fn backoff_key(snap: &Snapshot, a: &Action) -> Option<(String, StartKind)> {
    match a {
        Action::Fork { pool, .. } => Some((pool.clone(), StartKind::Fork)),
        Action::Resume { machine } => snap
            .machines
            .iter()
            .find(|m| &m.id == machine)
            .map(|m| (m.pool.clone(), StartKind::Resume)),
        Action::Unregister { .. }
        | Action::Delete { .. }
        | Action::Stop { .. }
        | Action::Register { .. } => None,
    }
}

/// Why `decide` forks a kept machine in pool `p`, for the log, and counts it: `min_up`
/// while the pool's kept machines (up, starting, stopping in a pool with `resume`, and
/// those started this tick) are short of it, else queued jobs no stopped machine covers.
fn kept_fork_reason(t: &mut Tick, p: &crate::model::Pool) -> String {
    let held = t
        .snapshot
        .machines
        .iter()
        .filter(|m| m.pool == p.name && !m.is_ephemeral())
        .filter(|m| match m.state {
            MachineState::Up | MachineState::Starting => true,
            MachineState::Stopping => p.has(Placement::Resume),
            MachineState::Stopped | MachineState::Gone => false,
        })
        .count();
    let started = t.kept_started.entry(p.name.clone()).or_default();
    let why = if held + *started < p.min_up as usize {
        format!("up to min_up {}", p.min_up)
    } else {
        "for queued jobs".to_string()
    };
    *started += 1;
    why
}

impl Loop<'_> {
    /// The time now, for what is timed from a request's answer; see `Tick::at`.
    fn at(&self, t: &Tick) -> UnixSecs {
        t.at((self.clock)())
    }

    /// Carries out `actions` in `rank` order. Returns `Err` only for errors no later tick
    /// can fix, and only once every other unregister, delete and stop has run: such an
    /// error skips this tick's registers and starts, never its cleanup. A 403 to one
    /// pool's start backs that pool's starts off and is not fatal (`boat::Error::
    /// is_forbidden`). Other errors are logged, and a GitHub rate limit ends the tick.
    pub(super) fn act(
        &mut self,
        now: UnixSecs,
        observed: Observed,
        mut actions: Vec<Action>,
    ) -> Result<(), Error> {
        actions.sort_by_key(rank);
        let snapshot = &observed.snapshot;
        // A start `decide` no longer asks for starts afresh when it is asked for again.
        let asked: HashSet<_> = actions
            .iter()
            .filter_map(|a| backoff_key(snapshot, a))
            .collect();
        self.ledger.keep_asked(&asked);
        let limits = snapshot.limits;
        let mut tick = Tick {
            now,
            began: self.began,
            snapshot,
            runner_ids: observed.runner_ids,
            starts: if limits.can_start {
                Starts::Open {
                    price: limits.next_start_cents,
                    recheck: false,
                }
            } else {
                Starts::Closed
            },
            skip: HashSet::new(),
            // Only for the log, so worked out only when there is a delete to explain.
            delete_reasons: if !self.draining()
                && actions.iter().any(|a| matches!(a, Action::Delete { .. }))
            {
                super::delete_reasons(self.cfg, snapshot)
            } else {
                HashMap::new()
            },
            kept_started: HashMap::new(),
        };
        let mut fatal: Option<Error> = None;
        // Runners whose start failed last tick hold a slot and count as supply until they
        // are gone, so they go first.
        for a in &observed.abandoned {
            match self.delete_runner(a.id) {
                Ok(()) => {
                    self.memory.forget_abandoned(&a.name);
                    self.log(format_args!("unregister {}: start_failed", a.name));
                }
                Err(e) => {
                    eprintln!("unregister {} (its start failed): {e}", a.name);
                    tick.skip.insert(a.machine.clone());
                    if e.is_fatal() {
                        fatal.get_or_insert(e);
                    }
                }
            }
        }
        for action in &actions {
            if fatal.is_some() && !is_cleanup(action) {
                continue;
            }
            if machine_of(action).is_some_and(|m| tick.skip.contains(m)) {
                continue;
            }
            let key = backoff_key(snapshot, action);
            if key
                .as_ref()
                .is_some_and(|k| self.ledger.backing_off(k, now))
            {
                continue;
            }
            match self.execute(&mut tick, action) {
                Ok(()) => {
                    if let Some(k) = &key {
                        self.ledger.succeeded(k);
                    }
                }
                Err(Error::PoolForbidden(b)) => {
                    let pool = key.map(|(pool, _)| pool).unwrap_or_default();
                    let wait_s = self.ledger.pool_refused(now, &pool, b.to_string());
                    eprintln!(
                        "error: {action:?}: {b}. boat refuses pool {pool}'s starts; it makes \
                         none for {wait_s}s, and other pools go on. Check its machine_type \
                         and snapshot against the boat plan"
                    );
                }
                Err(e) if e.is_fatal() => {
                    eprintln!(
                        "error: {action:?}: {e}; the tick's other unregisters, deletes and \
                         stops still run, its registers and starts do not"
                    );
                    fatal.get_or_insert(e);
                }
                Err(e @ Error::GitHub(github::Error::RateLimited { .. })) => {
                    eprintln!("{action:?}: {e}; rest of tick skipped");
                    break;
                }
                // Closed starts and boat's refusals already wait on their own.
                Err(e @ (Error::StartsClosed | Error::Boat(boat::Error::Refused { .. }))) => {
                    eprintln!("{action:?}: {e}")
                }
                Err(e) => match key {
                    Some(k) => self.ledger.note_failure(now, k, action, &e),
                    None => eprintln!("{action:?}: {e}"),
                },
            }
        }
        fatal.map_or(Ok(()), Err)
    }

    fn execute(&mut self, t: &mut Tick, action: &Action) -> Result<(), Error> {
        match action {
            Action::Unregister { runner } => match t.runner_ids.remove(runner) {
                Some(id) => {
                    self.delete_runner(id)?;
                    let why = if self.draining() {
                        Some("drain")
                    } else {
                        unregister_reason(self.cfg, t.snapshot, runner)
                    };
                    match why {
                        Some(why) => self.log(format_args!("unregister {runner}: {why}")),
                        None => self.log(format_args!("unregister {runner}")),
                    }
                    Ok(())
                }
                None => Ok(()),
            },
            Action::Delete { machine } => {
                self.delete_machine(t, machine)?;
                let what = machine_in(t.snapshot, machine);
                let why = if self.draining() {
                    Some("drain")
                } else {
                    t.delete_reasons.get(machine).copied()
                };
                match why {
                    Some(why) => self.log(format_args!("delete {what}: {why}")),
                    None => self.log(format_args!("delete {what}")),
                }
                Ok(())
            }
            Action::Stop { machine } => {
                match self.remove_runners(t, machine) {
                    // On the way out nothing else could stop it; see `Loop::wind_down`.
                    Err(Error::GitHub(e)) if self.winding_down && super::github_is_fatal(&e) => {
                        eprintln!("exit: {machine}: {e}; stopping it anyway");
                    }
                    r => r?,
                }
                self.boat.stop(machine)?;
                let why = if self.winding_down {
                    "exit"
                } else {
                    "idle_stop"
                };
                let what = machine_in(t.snapshot, machine);
                self.log(format_args!("stop {what}: {why}"));
                Ok(())
            }
            Action::Register {
                pool,
                machine,
                slot,
            } => {
                if self.free_of_logins(t, pool, machine)? {
                    self.register(t, pool, machine, *slot)
                } else {
                    Ok(())
                }
            }
            Action::Resume { machine } => self.resume(t, machine),
            Action::Fork { pool, job } => self.fork(t, pool, *job),
        }
    }

    fn delete_runner(&mut self, id: u64) -> Result<(), Error> {
        match self.gh.delete_runner(id) {
            Err(github::Error::NotFound) => Ok(()),
            r => r.map_err(Error::from),
        }
    }

    /// Deletes every GitHub runner on `machine` before boat stops or deletes it, because a
    /// runner `decide` saw idle may have taken a job since (`FixStaleAct`). A deleted
    /// runner takes no job, and GitHub refuses to delete one that runs a job; that error
    /// skips the machine this tick.
    fn remove_runners(&mut self, t: &mut Tick, machine: &str) -> Result<(), Error> {
        let mut on_machine: Vec<(&str, u64)> = t
            .snapshot
            .runners
            .iter()
            .filter(|r| r.machine == machine)
            .filter_map(|r| Some((r.name.as_str(), *t.runner_ids.get(&r.name)?)))
            .collect();
        on_machine.sort_unstable_by_key(|&(_, id)| id);
        for (name, id) in on_machine {
            self.delete_runner(id)?;
            t.runner_ids.remove(name);
        }
        Ok(())
    }

    fn delete_machine(&mut self, t: &mut Tick, machine: &str) -> Result<(), Error> {
        self.remove_runners(t, machine)?;
        match self.boat.delete(machine) {
            Err(boat::Error::NotFound) => Ok(()),
            r => r.map_err(Error::from),
        }
    }

    /// Whether `machine` holds none of its creator's logins, so a runner may go on it.
    /// boat's `noEnv` create keeps logins saved in the snapshot, and a CI job could read
    /// them. A machine that holds them is taken out of the way, so a replacement can serve
    /// the pool: a job's fork, or a kept machine in a pool without `resume`, is deleted; a
    /// kept machine in a pool with `resume` is stopped, as its `noEnv` resume scrubs them.
    /// Either way the pool's forks back off, since a fork of the same snapshot holds them
    /// too. A machine boat does not say about counts as holding them.
    fn free_of_logins(&mut self, t: &mut Tick, pool: &str, machine: &str) -> Result<bool, Error> {
        if self.memory.holds_logins(machine) {
            return Ok(false);
        }
        let p = self.pool(pool)?;
        let sb = self.boat.get(machine)?;
        let why = match sb.holds_creator_logins {
            Some(false) => {
                self.ledger.pool_free_of_logins(pool);
                return Ok(true);
            }
            Some(true) => "holds its creator's personal logins (GitHub token, model logins)",
            None => {
                "may hold its creator's personal logins: boat did not report holdsCreatorLogins"
            }
        };
        let error = format!(
            "{machine} in pool {pool} {why}. Snapshot {:?} must be saved from a sandbox created \
             with --no-env; a CI job there could read them",
            p.snapshot
        );
        let stop = sb.job().is_none() && p.has(Placement::Resume);
        let what = if stop {
            "stopping it, as its resume scrubs them"
        } else {
            "deleting it"
        };
        let msg = format!("error: {error}. No runner registered; {what}");
        let backoff_s = self.ledger.fork_held_logins(t.now, pool, error);
        eprintln!("{msg}; the pool's next fork in {backoff_s}s");
        let what = format!("{machine} (pool {pool})");
        if stop {
            self.remove_runners(t, machine)?;
            self.boat.stop(machine)?;
            // Until boat lists it stopping, `decide` may still ask for a runner on it.
            self.memory.note_holds_logins(machine);
            self.log(format_args!("stop {what}: logins"));
        } else {
            self.delete_machine(t, machine)?;
            self.log(format_args!("delete {what}: logins"));
        }
        Ok(false)
    }

    /// If the file write or the command fails after GitHub issued the jit config, the
    /// runner would stay `Registered`, hold its slot and count as supply until
    /// `register_timeout_s`. It is noted instead, and the next tick deletes it and leaves
    /// it out of the snapshot (`Memory::abandon`).
    fn register(&mut self, t: &Tick, pool: &str, machine: &str, slot: u32) -> Result<(), Error> {
        let p = self.pool(pool)?;
        let now = self.at(t);
        let name = github::runner_name(pool, machine, slot, now);
        let jit = self.gh.generate_jitconfig(&name, &p.labels)?;
        self.memory.mark_used(machine, now);
        let started = self
            .boat
            .write_file(machine, &script::jit_path(slot), jit.expose())
            .and_then(|()| {
                self.boat
                    .exec_detached(machine, &script::start_command(slot))
            });
        if let Err(e) = started {
            eprintln!("{name}: {e}; its registration is deleted next tick");
            let at = self.at(t);
            self.memory.abandon(name, machine, at);
            return Err(e.into());
        }
        // The name, machine and queue only: the jit config is a secret.
        let queue = match self
            .memory
            .queue_wait(self.cfg, pool, &t.snapshot.queued, now)
        {
            Some((jobs, wait)) => format!(": {jobs} queued, oldest {wait}s"),
            None => String::new(),
        };
        self.log(format_args!(
            "register {name} on {machine} (pool {pool}, slot {slot}){queue}"
        ));
        Ok(())
    }

    fn resume(&mut self, t: &mut Tick, machine: &str) -> Result<(), Error> {
        match self.start(t, |b| b.resume(machine)) {
            Ok(()) => {
                let at = self.at(t);
                self.memory.resumed(machine, at);
                let what = machine_in(t.snapshot, machine);
                if let Some(m) = t.snapshot.machines.iter().find(|m| m.id == machine) {
                    *t.kept_started.entry(m.pool.clone()).or_default() += 1;
                }
                self.log(format_args!("resume {what}"));
                Ok(())
            }
            // boat may have resumed it, and charged for it, without an answer. Held as a
            // resume in flight, it is not resumed again before the listing shows it.
            Err(Error::Boat(e)) if e.may_have_applied() => {
                let at = self.at(t);
                self.memory.resumed(machine, at);
                Err(e.into())
            }
            // boat deleted the disk of a machine that stayed stopped too long. It can never
            // resume, so delete it; the pool then forks a new one if it needs to.
            Err(Error::Boat(boat::Error::Conflict { code, .. })) if code == DISK_EXPIRED => {
                eprintln!("warn: {machine} lost its disk; deleting it");
                self.delete_machine(t, machine)?;
                let what = machine_in(t.snapshot, machine);
                self.log(format_args!("delete {what}: disk_expired"));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn fork(&mut self, t: &mut Tick, pool: &str, job: Option<JobKey>) -> Result<(), Error> {
        let p = self.pool(pool)?;
        let ttl_s = match job {
            Some(_) => p.fork_ttl_s,
            None => boat::MAX_TTL_S,
        };
        // A fresh key per fork, never reused (`FixForkKey`): boat answers a known key with
        // the first machine for 24 h, even a deleted one, so a reused key would give a job
        // or a kept fork a dead machine.
        let key = Uuid::new_v4().to_string();
        let forked =
            |b: &boat::Client| b.fork(&p.snapshot, &p.machine_type, &p.name, job, ttl_s, &key);
        match self.start(t, forked) {
            Ok(id) => {
                let at = self.at(t);
                let what = match job {
                    Some(j) => format!("job {} attempt {}", j.id, j.attempt),
                    None => format!("kept machine, {}", kept_fork_reason(t, p)),
                };
                self.log(format_args!("fork {id} (pool {}): {what}", p.name));
                self.memory.forked(id, &p.name, job, at);
                Ok(())
            }
            Err(e) => {
                // boat may have started a machine this process never heard about, and the
                // next fork's fresh key would not find it (`FixLostCreate`). Wait as after a
                // restart: by then the machine is listed with its job label, or it never
                // existed.
                if matches!(&e, Error::Boat(b) if b.may_have_applied()) {
                    let lag_s = HTTP_TIMEOUT.as_secs().saturating_add(self.cfg.tick_s);
                    let at = self.at(t);
                    self.ledger.block_until(at.saturating_add(lag_s));
                }
                Err(e)
            }
        }
    }

    /// Runs one paid-or-free start. An accepted start adds the price `decide` saw to
    /// today's spend; a refused one closes starts until boat's `Retry-After` passes. A
    /// start without a definite answer may have been charged and may have moved the quote,
    /// so it adds the price too and closes starts for this tick (`FixLostSpend`).
    fn start<T>(
        &mut self,
        t: &mut Tick,
        call: impl FnOnce(&boat::Client) -> Result<T, boat::Error>,
    ) -> Result<T, Error> {
        let price = self.open_starts(t)?;
        match call(&self.boat) {
            Ok(v) => {
                self.ledger.book(price);
                t.starts = Starts::Open {
                    price,
                    recheck: true,
                };
                Ok(v)
            }
            Err(e @ boat::Error::Refused { retry_after_s, .. }) => {
                let at = self.at(t);
                self.ledger.block_until(at.saturating_add(retry_after_s));
                t.starts = Starts::Closed;
                Err(e.into())
            }
            // Refused for this request, maybe for this pool alone; see `is_forbidden`.
            Err(e) if e.is_forbidden() => Err(Error::PoolForbidden(e)),
            Err(e) => {
                if e.may_have_applied() {
                    self.ledger.book(price);
                    t.starts = Starts::Closed;
                }
                Err(e.into())
            }
        }
    }

    /// The price of the next start, or `StartsClosed`. `decide` priced every start of the
    /// tick at one quote, so after an accepted start boat's quote is read again, and starts
    /// close once it has moved.
    fn open_starts(&mut self, t: &mut Tick) -> Result<Cents, Error> {
        let Starts::Open { price, recheck } = t.starts else {
            return Err(Error::StartsClosed);
        };
        if recheck {
            let quote = self.boat.limits();
            if let Ok(q) = &quote {
                self.ledger.quote_seen(q.cents);
            }
            match quote {
                Ok(q) if q.allows && q.cents == price => {
                    let recheck = false;
                    t.starts = Starts::Open { price, recheck };
                }
                Ok(_) => t.starts = Starts::Closed,
                Err(e) if e.is_fatal() => return Err(e.into()),
                Err(e) => {
                    eprintln!("limits after start: {e}");
                    t.starts = Starts::Closed;
                }
            }
        }
        match t.starts {
            Starts::Open { .. } => Ok(price),
            Starts::Closed => Err(Error::StartsClosed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::decide::decide;
    use crate::model::{Placement, Pool};
    use crate::transport::NetworkError;
    use crate::transport::fake::{Fake, Reply, Sent, respond};

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
        let mut tick_loop = started(&cfg, &fake);
        let decide_starts = |s: &Snapshot, _: &Pool| {
            if s.now > NOW {
                assert!(!s.limits.can_start, "starts stay closed until Retry-After");
            }
            vec![fork(Some(70)), resume("bx_33333333")]
        };
        tick_loop.tick(NOW, decide_starts).unwrap();
        tick_loop.tick(NOW + 30, decide_starts).unwrap();
        assert_eq!(resumes(&fake), 1);
        assert!(forks(&fake).is_empty());
        assert_eq!(tick_loop.ledger.spent_cents, 100);
    }

    /// The resume timed out after boat may have started, and charged for, the machine.
    #[test]
    fn a_resume_without_an_answer_counts_its_quote_and_closes_starts() {
        let cfg = config();
        let fake = Fake::failing(|s| {
            if s.url.ends_with("/resume") {
                Err(NetworkError("timed out reading response".into()))
            } else {
                Ok(reply(s))
            }
        });
        let mut tick_loop = started(&cfg, &fake);
        let resume_then_fork = |_: &Snapshot, _: &Pool| vec![resume("bx_33333333"), fork(Some(70))];
        tick_loop.tick(NOW, resume_then_fork).unwrap();
        assert!(forks(&fake).is_empty(), "{:?}", requests(&fake));
        assert_eq!(tick_loop.ledger.spent_cents, 125);
    }

    /// After an accepted start the next one goes ahead only while boat's quote holds.
    #[test]
    fn a_moved_quote_closes_starts_for_the_tick() {
        let cfg = config();
        let reads = std::cell::Cell::new(0);
        let fake = Fake::new(move |s| {
            reads.set(reads.get() + usize::from(s.url.ends_with("/v1/limits")));
            // The tick's own read quotes 25 cents; the read after the first start, 30.
            if s.url.ends_with("/v1/limits") && reads.get() > 1 {
                return respond(200, &[], r#"{"canStart":true,"nextStartDollars":0.30}"#);
            }
            reply(s)
        });
        let mut tick_loop = started(&cfg, &fake);
        tick_loop
            .tick(NOW, |_, _| vec![fork(Some(70)), fork(Some(71))])
            .unwrap();
        assert_eq!(forks(&fake).len(), 1, "{:?}", requests(&fake));
        // The start may have cost the new 30 cents, so 5 more are booked.
        assert_eq!(tick_loop.ledger.spent_cents, 130);
    }

    /// Cap 10 cents, four jobs queued. The observe quotes an included start, but someone
    /// else on the account takes it first, and boat charges this fork the 5-cent paid
    /// price. The quote read after it shows 5 cents, and the 5 are booked, so one more
    /// paid start fits the cap and boat is paid 10 cents. Before, the fork was booked at
    /// 0 and two more paid starts went out: 15 cents against a cap of 10.
    #[test]
    fn a_start_charged_above_its_quote_counts_against_the_cap() {
        let mut cfg = config();
        cfg.pools[0].paid_start_cents_per_day = 10;
        let forked = std::cell::Cell::new(0);
        let fake = Fake::new(move |s| {
            if s.method == "POST" && s.url == format!("{BOAT}/sandboxes") {
                forked.set(forked.get() + 1);
            }
            if s.url.ends_with("/v1/limits") {
                let dollars = if forked.get() > 0 { "0.05" } else { "0" };
                let body = format!(r#"{{"canStart":true,"nextStartDollars":{dollars}}}"#);
                return respond(200, &[], &body);
            }
            let job = |id| {
                format!(
                    r#"{{"id":{id},"run_attempt":1,"status":"queued","labels":["self-hosted","large"]}}"#
                )
            };
            let jobs = [70, 71, 72, 73].map(job).join(",");
            lagging_reply(s, "", &jobs)
        });
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.ledger.spent_cents = 0;
        for k in 0..4 {
            tick_loop.tick(NOW + 15 * k, decide).unwrap();
        }
        assert_eq!(forks(&fake).len(), 2, "{:?}", requests(&fake));
        assert_eq!(tick_loop.ledger.spent_cents, 10);
    }

    #[test]
    fn a_restart_makes_no_paid_starts_until_the_day_ends() {
        let mut cfg = config();
        let mut big = cfg.pools[0].clone();
        big.name = "big".into();
        big.paid_start_cents_per_day = 500;
        cfg.pools.insert(0, big);
        let fake = Fake::new(reply);
        let mut tick_loop = started(&cfg, &fake);
        assert_eq!(tick_loop.ledger.spent_cents, 500);
        // Job 70 is queued all day and every start is quoted at 25 cents.
        for now in [NOW, DAY_S - 15] {
            tick_loop.tick(now, decide).unwrap();
            assert!(forks(&fake).is_empty(), "{:?}", requests(&fake));
            assert_eq!(tick_loop.ledger.spent_cents, 500);
        }
        tick_loop.tick(DAY_S, decide).unwrap();
        let forks = forks(&fake);
        assert_eq!(forks.len(), 1);
        assert!(
            forks[0]
                .body_text()
                .contains(r#""sw-boat-runner-job":"70-1""#)
        );
        assert_eq!(tick_loop.ledger.spent_cents, 25);
    }

    /// The restart's assumed spend lasts until 00:00 UTC of the day the process started,
    /// even when its first tick comes after midnight.
    #[test]
    fn a_restart_just_before_midnight_spends_normally_the_next_day() {
        let cfg = config();
        let fake = Fake::new(reply);
        let mut tick_loop = Loop::new(&cfg, &fake, BOAT_KEY.into(), gh_auth(), DAY_S - 1);
        assert_eq!(tick_loop.ledger.spent_cents, 100);
        tick_loop.tick(DAY_S + 90, |_, _| Vec::new()).unwrap();
        assert_eq!(tick_loop.ledger.spent_cents, 0);
    }

    /// The second fork of a job stands for a fork after the first one was deleted: with the
    /// same key boat would return the deleted machine.
    #[test]
    fn forks_send_fresh_uuid_v4_keys() {
        let cfg = config();
        let fake = Fake::new(reply);
        let mut tick_loop = started(&cfg, &fake);
        let twice_each =
            |_: &Snapshot, _: &Pool| vec![fork(None), fork(None), fork(Some(70)), fork(Some(70))];
        tick_loop.tick(NOW, twice_each).unwrap();
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

    #[test]
    fn a_restart_makes_no_start_until_an_inflight_create_has_landed() {
        let cfg = config();
        let fake = Fake::new(reply);
        let mut tick_loop = Loop::new(&cfg, &fake, BOAT_KEY.into(), gh_auth(), NOW);
        let fork_kept = |_: &Snapshot, _: &Pool| vec![fork(None)];
        tick_loop.tick(NOW, fork_kept).unwrap();
        tick_loop.tick(NOW + 89, fork_kept).unwrap();
        assert!(forks(&fake).is_empty());
        tick_loop.tick(NOW + 90, fork_kept).unwrap();
        assert_eq!(forks(&fake).len(), 1);
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
        let mut tick_loop = started(&cfg, &fake);
        let stop_and_delete =
            |_: &Snapshot, _: &Pool| vec![stop("bx_22222222"), delete("bx_44444444")];
        tick_loop.tick(NOW, stop_and_delete).unwrap();
        assert_eq!(
            requests(&fake)[6..],
            [
                format!("DELETE {GH}/runners/12"),
                format!("DELETE {BOAT}/sandboxes/bx_44444444"),
                format!("DELETE {GH}/runners/11"),
            ]
        );
    }

    #[test]
    fn a_stopped_machine_whose_disk_boat_deleted_is_deleted() {
        let cfg = config();
        let fake = Fake::new(|s| {
            if s.url.ends_with("/resume") {
                respond(409, &[], r#"{"code":"snapshot_data_expired"}"#)
            } else {
                reply(s)
            }
        });
        started(&cfg, &fake)
            .tick(NOW, |_, _| vec![resume("bx_33333333")])
            .unwrap();
        assert_eq!(
            requests(&fake)[6..],
            [
                format!("POST {BOAT}/sandboxes/bx_33333333/resume"),
                format!("DELETE {BOAT}/sandboxes/bx_33333333"),
            ]
        );
    }

    /// A fork boat refuses the same way every time backs off 15 s, doubling to 10 min,
    /// and starts afresh once `decide` stops asking for it.
    #[test]
    fn a_fork_refused_the_same_way_backs_off() {
        let cfg = config();
        let fake = Fake::new(|s| {
            if s.method == "POST" && s.url == format!("{BOAT}/sandboxes") {
                let body = r#"{"code":"deploy_failed","error":"too big","status":409}"#;
                respond(409, &[], body)
            } else {
                reply(s)
            }
        });
        let mut tick_loop = started(&cfg, &fake);
        let fork_70 = |_: &Snapshot, _: &Pool| vec![fork(Some(70))];
        let mut sent_at = Vec::new();
        for t in (0..1800).step_by(15) {
            let before = forks(&fake).len();
            tick_loop.tick(NOW + t, fork_70).unwrap();
            if forks(&fake).len() > before {
                sent_at.push(t);
            }
        }
        assert_eq!(sent_at, [0, 15, 30, 60, 120, 240, 480, 960, 1560]);
        tick_loop.tick(NOW + 1800, |_, _| Vec::new()).unwrap();
        let before = forks(&fake).len();
        tick_loop.tick(NOW + 1815, fork_70).unwrap();
        assert_eq!(forks(&fake).len(), before + 1);
    }

    /// The first create times out after boat may have accepted it.
    fn first_create_lost(t: usize) -> Option<Reply> {
        (t == 1).then(|| Err(NetworkError("timed out reading response".into())))
    }

    /// Ticks every 15 s from `NOW` to `NOW + until_s` and counts forks after each.
    fn forks_per_tick(tick_loop: &mut Loop, fake: &Fake, until_s: UnixSecs) -> Vec<usize> {
        (0..=until_s / 15)
            .map(|k| {
                tick_loop.tick(NOW + 15 * k, decide).unwrap();
                forks(fake).len()
            })
            .collect()
    }

    /// boat never lists the lost create, so it never existed: the next start waits for the
    /// request timeout plus one tick, then forks job 70 again.
    #[test]
    fn a_create_without_an_answer_blocks_starts_until_it_would_be_listed() {
        let cfg = config();
        let fake = lagging_creates(|_| "", |_| JOB_70, first_create_lost, "0");
        let mut tick_loop = started(&cfg, &fake);
        let wait_s = HTTP_TIMEOUT.as_secs() + cfg.tick_s;
        let mut want = vec![1; (wait_s / 15) as usize];
        want.push(2);
        assert_eq!(forks_per_tick(&mut tick_loop, &fake, wait_s), want);
    }

    /// boat accepted the lost create and lists it, with its job label, from the third tick.
    #[test]
    fn a_lost_create_that_boat_lists_is_not_forked_again() {
        let cfg = config();
        let listing = |t| if t < 3 { "" } else { LISTED_55 };
        let fake = lagging_creates(listing, |_| JOB_70, first_create_lost, "0");
        let mut tick_loop = started(&cfg, &fake);
        let until_s = HTTP_TIMEOUT.as_secs() + 2 * cfg.tick_s;
        let counts = forks_per_tick(&mut tick_loop, &fake, until_s);
        assert!(counts.iter().all(|&n| n == 1), "{counts:?}");
    }

    /// boat started and charged for the lost 30-cent create for job 70. Once the wait has
    /// passed, job 71 would take the spend to 60 of a 50-cent cap, so it gets no fork.
    #[test]
    fn a_lost_create_counts_its_quote_as_spent() {
        let mut cfg = config();
        cfg.pools[0].paid_start_cents_per_day = 50;
        let fake = lagging_creates(
            |t| if t < 3 { "" } else { LISTED_55 },
            |t| if t < 3 { JOB_70 } else { JOBS_70_71 },
            first_create_lost,
            "0.30",
        );
        let mut tick_loop = started(&cfg, &fake);
        // As on a new UTC day, past the restart's assumed spend.
        tick_loop.ledger.spent_cents = 0;
        let until_s = HTTP_TIMEOUT.as_secs() + 2 * cfg.tick_s;
        let counts = forks_per_tick(&mut tick_loop, &fake, until_s);
        assert!(counts.iter().all(|&n| n == 1), "{counts:?}");
        assert_eq!(tick_loop.ledger.spent_cents, 30);
    }

    /// Whatever boat and GitHub answer, even when they echo back everything they were sent,
    /// an action's error names neither credential nor the jit config, so the log line `act`
    /// writes for it cannot either.
    #[test]
    fn no_action_error_quotes_a_credential_or_a_jit_config() {
        let cfg = config();
        let echo = |s: &Sent| {
            format!(
                "{} {}",
                s.header("authorization").unwrap_or(""),
                s.body_text()
            )
        };
        // Status 0 stands for a transport error, and a 200 carries a body no call can read.
        for status in [0, 200, 400, 401, 403, 404, 409, 422, 429, 500, 503] {
            // Observe and generate-jitconfig work; every other call fails with `status`.
            let fake = Fake::failing(move |s| {
                let text = echo(s);
                let body = match status {
                    200 => serde_json::json!({ "sandbox": text }),
                    _ => serde_json::json!({"code": "x", "message": text, "error": text}),
                };
                match status {
                    _ if s.method == "GET" || s.url.ends_with("/generate-jitconfig") => {
                        Ok(reply(s))
                    }
                    0 => Err(NetworkError(format!("connection reset: {text}"))),
                    _ => Ok(respond(status, &[], &body.to_string())),
                }
            });
            let mut tick_loop = started(&cfg, &fake);
            let observed = tick_loop.observe(NOW).unwrap();
            let actions = [
                register("bx_22222222", 1),
                unregister("sbr-ci-bx_44444444-0-900"),
                stop("bx_22222222"),
                delete("bx_33333333"),
                resume("bx_33333333"),
                fork(None),
            ];
            let mut tick = Tick {
                now: NOW,
                began: (tick_loop.clock)(),
                skip: HashSet::new(),
                snapshot: &observed.snapshot,
                runner_ids: observed.runner_ids.clone(),
                starts: Starts::Closed,
                delete_reasons: HashMap::new(),
                kept_started: HashMap::new(),
            };
            for action in &actions {
                let (price, recheck) = (0, false);
                tick.starts = Starts::Open { price, recheck };
                if let Err(e) = tick_loop.execute(&mut tick, action) {
                    let text = format!("{e} {e:?}");
                    for secret in [BOAT_KEY, GH_TOKEN, "JITSECRET"] {
                        assert!(!text.contains(secret), "{status} {action:?}: {text}");
                    }
                }
            }
        }
    }

    /// Every kind of action, each on its own machine or runner: Unregister of runner 14,
    /// Delete of `bx_44444444` (runner 12, then the machine), Stop of `bx_22222222`
    /// (runner 11, then the stop), Register on `bx_22222222`, Resume of `bx_33333333`
    /// and a Fork. Each entry is a request and the action it belongs to.
    const MATRIX: [(&str, &str); 12] = [
        ("unregister", "DELETE GH/runners/14"),
        ("delete", "DELETE GH/runners/12"),
        ("delete", "DELETE BOAT/sandboxes/bx_44444444"),
        ("stop", "DELETE GH/runners/11"),
        ("stop", "POST BOAT/sandboxes/bx_22222222/stop"),
        ("register", "GET BOAT/sandboxes/bx_22222222"),
        ("register", "POST GH/runners/generate-jitconfig"),
        ("register", "PUT BOAT/sandboxes/bx_22222222/files"),
        ("register", "POST BOAT/sandboxes/bx_22222222/commands"),
        ("resume", "POST BOAT/sandboxes/bx_33333333/resume"),
        ("fork", "GET BOAT/limits"),
        ("fork", "POST BOAT/sandboxes"),
    ];

    fn matrix_request(r: &str) -> String {
        r.replace("GH", GH).replace("BOAT", BOAT)
    }

    /// The fatal-error matrix: boat answering 401, 402 or 403 and GitHub answering 401 or
    /// 403 to the first call of each kind of action that reaches it. The loop ends (the
    /// tick returns `Err`) for each but a 403 to a start, which backs that pool off. In
    /// every case every other unregister, delete and stop is still sent, except the calls
    /// of the failing action after the one that failed (`FixStaleAct`); after a fatal
    /// error no register or start is sent.
    #[test]
    fn the_fatal_error_matrix() {
        let cfg = config();
        let actions = || {
            vec![
                unregister("sbr-ci-bx_77777777-0-900"),
                delete("bx_44444444"),
                stop("bx_22222222"),
                register("bx_22222222", 1),
                resume("bx_33333333"),
                fork(None),
            ]
        };
        for (service, status) in [
            ("BOAT", 401),
            ("BOAT", 402),
            ("BOAT", 403),
            ("GH", 401),
            ("GH", 403),
        ] {
            // Each call to `service`, of every kind of action that makes one.
            let calls = MATRIX
                .iter()
                .filter(|(_, r)| r.split(' ').nth(1).unwrap().starts_with(service));
            for &(kind, target) in calls {
                let target = matrix_request(target);
                let fail = target.clone();
                // The limits read is the fork's only after a start; the tick's own read
                // must work, so only the second fails.
                let limits_reads = std::cell::Cell::new(0);
                let fake = Fake::new(move |s| {
                    let r = format!("{} {}", s.method, s.url);
                    if r.ends_with("/v1/limits") {
                        limits_reads.set(limits_reads.get() + 1);
                        if r == fail && limits_reads.get() > 1 {
                            return respond(status, &[], r#"{"code":"x","message":"no"}"#);
                        }
                    } else if r == fail {
                        return respond(status, &[], r#"{"code":"x","message":"no"}"#);
                    }
                    if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                        return respond(
                            200,
                            &[],
                            r#"{"total_count":3,"runners":[
                            {"id":11,"name":"sbr-ci-bx_22222222-0-900","status":"online","busy":false},
                            {"id":12,"name":"sbr-ci-bx_44444444-0-900","status":"offline","busy":false},
                            {"id":14,"name":"sbr-ci-bx_77777777-0-900","status":"offline","busy":false}]}"#,
                        );
                    }
                    reply(s)
                });
                let mut tick_loop = started(&cfg, &fake);
                // Starts at 0 cents, so the resume and the fork both fit the budget.
                tick_loop.ledger.spent_cents = 0;
                let case = format!("{service} {status} on {kind} ({target})");
                let result = tick_loop.tick(NOW, |_, _| actions());
                // The start call itself, not the account-wide limits read before it.
                let start = target.ends_with("/resume") || target.ends_with("/v1/sandboxes");
                let ends = !(service == "BOAT" && status == 403 && start);
                assert_eq!(result.is_err(), ends, "{case}: {result:?}");

                let sent = requests(&fake);
                let failed_at = MATRIX
                    .iter()
                    .position(|(_, r)| matrix_request(r) == target)
                    .unwrap();
                for (i, (k, r)) in MATRIX.iter().enumerate() {
                    let r = matrix_request(r);
                    let was_sent = sent.contains(&r);
                    let cleanup = matches!(*k, "unregister" | "delete" | "stop");
                    let same_after = *k == kind && i > failed_at;
                    if r.ends_with("/v1/limits") {
                        continue;
                    }
                    if i == failed_at {
                        assert!(was_sent, "{case}: {r} not sent");
                    } else if cleanup {
                        assert_eq!(was_sent, !same_after, "{case}: {r}");
                    } else if i > failed_at && ends {
                        // After a fatal error nothing registers or starts.
                        assert!(!was_sent, "{case}: {r} sent after a fatal error");
                    }
                }
                if !ends {
                    // A 403 to a start backs off the pool's starts, the other kind too.
                    assert!(
                        tick_loop
                            .ledger
                            .backing_off(&("ci".into(), StartKind::Fork), NOW + 1),
                        "{case}"
                    );
                    assert!(
                        tick_loop
                            .ledger
                            .backing_off(&("ci".into(), StartKind::Resume), NOW + 1),
                        "{case}"
                    );
                }
            }
        }
    }

    /// boat refuses pool `big`'s forks with a 403 (a machine type the plan does not
    /// allow). Pool `ci` still forks, `big` backs off and the loop goes on.
    #[test]
    fn a_403_to_one_pools_start_backs_that_pool_off_only() {
        let mut cfg = config();
        let mut big = cfg.pools[0].clone();
        big.name = "big".into();
        big.machine_type = "xlarge".into();
        cfg.pools.insert(0, big);
        let fake = Fake::new(|s| {
            if s.method == "POST"
                && s.url == format!("{BOAT}/sandboxes")
                && s.body_text().contains("xlarge")
            {
                let body =
                    r#"{"code":"machine_type_not_allowed","message":"xlarge needs a higher plan"}"#;
                respond(403, &[], body)
            } else {
                reply(s)
            }
        });
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.ledger.spent_cents = 0;
        let both = |_: &Snapshot, p: &Pool| {
            vec![Action::Fork {
                pool: p.name.clone(),
                job: None,
            }]
        };
        let xlarge = |f: &Fake| {
            forks(f)
                .iter()
                .filter(|s| s.body_text().contains("xlarge"))
                .count()
        };
        tick_loop.tick(NOW, both).unwrap();
        assert_eq!((xlarge(&fake), forks(&fake).len()), (1, 2));
        // Within its wait `big` sends nothing; `ci` goes on.
        tick_loop.tick(NOW + 15, both).unwrap();
        assert_eq!((xlarge(&fake), forks(&fake).len()), (1, 3));
        tick_loop.tick(NOW + REFUSED_MIN_S, both).unwrap();
        assert_eq!(xlarge(&fake), 2);
        let key = ("big".to_string(), StartKind::Fork);
        assert_eq!(tick_loop.ledger.failing[&key].backoff_s, 2 * REFUSED_MIN_S);
    }

    /// GitHub issued the jit config, then the file write failed. The next tick deletes the
    /// registration and leaves it out of the snapshot, so its slot takes a new runner at
    /// once instead of after `register_timeout_s`.
    #[test]
    fn a_failed_start_deletes_its_registration_and_frees_the_slot() {
        let cfg = config();
        let writes = std::cell::Cell::new(0);
        let jit_name = "sbr-ci-bx_22222222-1-1000";
        let fake = Fake::new(move |s| {
            if s.method == "PUT" {
                writes.set(writes.get() + 1);
                if writes.get() == 1 {
                    return respond(500, &[], r#"{"code":"internal"}"#);
                }
            }
            if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                // GitHub lists the runner the failed start registered, never online.
                let body = format!(
                    r#"{{"total_count":2,"runners":[
                    {{"id":11,"name":"sbr-ci-bx_22222222-0-900","status":"online","busy":false}},
                    {{"id":21,"name":"{jit_name}","status":"offline","busy":false}}]}}"#
                );
                return respond(200, &[], &body);
            }
            reply(s)
        });
        let mut tick_loop = started(&cfg, &fake);
        tick_loop
            .tick(NOW, |_, _| vec![register("bx_22222222", 1)])
            .unwrap();
        let seen = std::cell::RefCell::new(Vec::new());
        tick_loop
            .tick(NOW + 15, |s, _| {
                seen.borrow_mut()
                    .extend(s.runners.iter().map(|r| r.name.clone()));
                vec![register("bx_22222222", 1)]
            })
            .unwrap();
        assert_eq!(*seen.borrow(), ["sbr-ci-bx_22222222-0-900"]);
        let reqs = requests(&fake);
        let deleted = reqs
            .iter()
            .position(|r| *r == format!("DELETE {GH}/runners/21"));
        let second = reqs
            .iter()
            .rposition(|r| r.ends_with("/generate-jitconfig"));
        assert!(deleted.is_some() && deleted < second, "{reqs:?}");
        assert_eq!(registers(&fake), 2);
    }

    /// `bx_66666666` in pool ci, listed in `state`, a job's fork when `job`. boat says
    /// `holds` for its `holdsCreatorLogins`, or nothing for `None`.
    struct Logins {
        fake: Fake,
        state: std::rc::Rc<std::cell::RefCell<(&'static str, Option<bool>)>>,
    }

    impl Logins {
        fn new(job: bool, holds: Option<bool>) -> Self {
            let state = std::rc::Rc::new(std::cell::RefCell::new(("idle", holds)));
            let st = state.clone();
            let labels = if job {
                r#"{"sw-boat-runner":"ci","sw-boat-runner-job":"70-1"}"#
            } else {
                r#"{"sw-boat-runner":"ci"}"#
            };
            let fake = Fake::new(move |s| {
                let (listed, holds) = *st.borrow();
                let sandbox =
                    format!(r#"{{"id":"bx_66666666","state":"{listed}","labels":{labels}"#);
                if s.method == "GET" && s.url.starts_with(&format!("{BOAT}/sandboxes?")) {
                    let body = format!(
                        r#"{{"sandboxes":[{sandbox}}}],
                           "pageInfo":{{"nextCursor":null,"hasMore":false,"limit":200}}}}"#
                    );
                    respond(200, &[], &body)
                } else if s.method == "GET" && s.url == format!("{BOAT}/sandboxes/bx_66666666") {
                    let holds =
                        holds.map_or(String::new(), |h| format!(r#","holdsCreatorLogins":{h}"#));
                    respond(200, &[], &format!(r#"{{"sandbox":{sandbox}{holds}}}}}"#))
                } else if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                    respond(200, &[], r#"{"total_count":0,"runners":[]}"#)
                } else {
                    reply(s)
                }
            });
            Logins { fake, state }
        }

        fn count(&self, request: &str) -> usize {
            requests(&self.fake)
                .iter()
                .filter(|r| *r == request)
                .count()
        }
    }

    fn register_66(_: &Snapshot, _: &Pool) -> Vec<Action> {
        vec![register("bx_66666666", 0)]
    }

    /// A job's fork that holds its creator's logins, or that boat says nothing about, gets
    /// no runner: it is deleted and the pool's forks back off, doubling while new forks
    /// keep holding them, until a machine of the pool holds none.
    #[test]
    fn a_fork_holding_its_creators_logins_is_deleted_and_forks_back_off() {
        for holds in [Some(true), None] {
            let cfg = config();
            let logins = Logins::new(true, holds);
            let mut tick_loop = started(&cfg, &logins.fake);
            let key = ("ci".to_string(), StartKind::Fork);
            let get = format!("GET {BOAT}/sandboxes/bx_66666666");
            let delete = format!("DELETE {BOAT}/sandboxes/bx_66666666");
            let fork_70 = |_: &Snapshot, _: &Pool| vec![fork(Some(70))];

            tick_loop.tick(NOW, register_66).unwrap();
            assert_eq!(registers(&logins.fake), 0);
            assert_eq!((logins.count(&get), logins.count(&delete)), (1, 1));
            let f = &tick_loop.ledger.failing[&key];
            assert!(
                f.why == Why::Logins && f.error.contains(r#"Snapshot "runner-base""#),
                "{}",
                f.error
            );
            assert_eq!((f.backoff_s, f.until), (BACKOFF_MIN_S, NOW + BACKOFF_MIN_S));

            tick_loop.tick(NOW + 5, fork_70).unwrap();
            assert_eq!(forks(&logins.fake).len(), 0);
            // A fork boat accepts does not end the back-off; the next one holding logins
            // doubles it.
            tick_loop.tick(NOW + BACKOFF_MIN_S, fork_70).unwrap();
            assert_eq!(forks(&logins.fake).len(), 1);
            assert!(tick_loop.ledger.failing[&key].why == Why::Logins);
            tick_loop.tick(NOW + 30, register_66).unwrap();
            assert_eq!(tick_loop.ledger.failing[&key].backoff_s, 2 * BACKOFF_MIN_S);
            assert_eq!(registers(&logins.fake), 0);

            logins.state.borrow_mut().1 = Some(false);
            tick_loop.tick(NOW + 45, register_66).unwrap();
            assert_eq!(registers(&logins.fake), 1);
            assert!(!tick_loop.ledger.failing.contains_key(&key));
        }
    }

    /// A kept machine holding its creator's logins gets no runner. In a pool that cannot
    /// resume nothing would ever scrub it, so it is deleted, and the pool's forks back off
    /// as for a job's fork: before, `decide` asked for a runner on it every tick, none
    /// came, and the pool's jobs starved while it billed until boat's 30-day TTL.
    #[test]
    fn a_kept_machine_holding_logins_in_a_pool_without_resume_is_deleted() {
        let mut cfg = config();
        cfg.pools[0].placement = vec![Placement::Slot];
        let logins = Logins::new(false, Some(true));
        let mut tick_loop = started(&cfg, &logins.fake);
        let delete = format!("DELETE {BOAT}/sandboxes/bx_66666666");
        tick_loop.tick(NOW, register_66).unwrap();
        assert_eq!(registers(&logins.fake), 0);
        assert_eq!(logins.count(&delete), 1);
        let key = ("ci".to_string(), StartKind::Fork);
        assert!(tick_loop.ledger.failing[&key].why == Why::Logins);
        // A kept fork of the same snapshot waits too.
        tick_loop.tick(NOW + 5, |_, _| vec![fork(None)]).unwrap();
        assert!(forks(&logins.fake).is_empty());
        tick_loop
            .tick(NOW + BACKOFF_MIN_S, |_, _| vec![fork(None)])
            .unwrap();
        assert_eq!(forks(&logins.fake).len(), 1);
    }

    /// In a pool with resume, the kept machine is stopped instead: a `noEnv` resume scrubs
    /// the logins, and the resumed machine takes a runner.
    #[test]
    fn a_kept_machine_holding_logins_in_a_pool_with_resume_is_stopped_and_resumed() {
        let mut cfg = config();
        cfg.pools[0].placement = vec![Placement::Slot, Placement::Resume];
        let logins = Logins::new(false, Some(true));
        let mut tick_loop = started(&cfg, &logins.fake);
        let get = format!("GET {BOAT}/sandboxes/bx_66666666");
        let stop = format!("POST {BOAT}/sandboxes/bx_66666666/stop");
        tick_loop.tick(NOW, register_66).unwrap();
        // boat still lists it up: it is not asked about or stopped again.
        tick_loop.tick(NOW + 15, register_66).unwrap();
        assert_eq!((logins.count(&get), logins.count(&stop)), (1, 1));
        assert_eq!(registers(&logins.fake), 0);
        assert_eq!(
            logins.count(&format!("DELETE {BOAT}/sandboxes/bx_66666666")),
            0
        );

        *logins.state.borrow_mut() = ("archived", Some(true));
        tick_loop.tick(NOW + 30, |_, _| Vec::new()).unwrap();
        *logins.state.borrow_mut() = ("idle", Some(false));
        tick_loop.tick(NOW + 45, register_66).unwrap();
        assert_eq!(logins.count(&get), 2);
        assert_eq!(registers(&logins.fake), 1);
        assert!(tick_loop.ledger.failing.is_empty());
    }

    /// One line per action that succeeds, with the job or the reason, and nothing on a
    /// tick with nothing to do. The register line names the runner and machine, never
    /// the jit config.
    #[test]
    fn logs_a_line_per_fork_register_and_delete() {
        let cfg = config();
        const RUNNING_55: &str = r#"{"id":"bx_55555555","state":"running",
            "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"70-1"}}"#;
        let fake = lagging(
            |t| if t == 2 || t == 3 { RUNNING_55 } else { "" },
            |t| if t < 3 { JOB_70 } else { "" },
        );
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.set_clock(Box::new(|| Duration::ZERO));
        let log = capture_log(&mut tick_loop);

        tick_loop.tick(NOW, decide).unwrap();
        assert_eq!(
            log.take(),
            [
                "summary: pool ci: 0 up, 0 busy; spend today unknown since restart \
                 (counted as the cap)",
                "fork bx_55555555 (pool ci): job 70 attempt 1",
            ]
        );
        tick_loop.tick(NOW + 15, decide).unwrap();
        assert_eq!(
            log.take(),
            [
                "register sbr-ci-bx_55555555-0-1015 on bx_55555555 (pool ci, slot 0): 1 queued, \
              oldest 15s"
            ]
        );
        // The runner ran its job and is gone, so the fork is finished.
        tick_loop.tick(NOW + 30, decide).unwrap();
        assert_eq!(log.take(), ["delete bx_55555555 (pool ci): finished"]);
        // Nothing left to do: no line.
        tick_loop.tick(NOW + 45, decide).unwrap();
        assert_eq!(log.take(), Vec::<String>::new());
        assert_eq!(registers(&fake), 1);
    }

    /// A runner this process registered logs once, when GitHub first lists it online, how
    /// long it took from its register; one registered before the process started does not.
    #[test]
    fn logs_when_a_runner_it_registered_comes_online() {
        let cfg = config();
        let fake = Fake::new(|s| {
            if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                return respond(
                    200,
                    &[],
                    r#"{"total_count":2,"runners":[
                        {"id":11,"name":"sbr-ci-bx_22222222-0-900","status":"online","busy":true},
                        {"id":14,"name":"sbr-ci-bx_22222222-1-980","status":"online","busy":true}]}"#,
                );
            }
            reply(s)
        });
        let mut tick_loop = started(&cfg, &fake);
        let log = capture_log(&mut tick_loop);
        tick_loop.observe(NOW).unwrap();
        assert_eq!(
            log.take(),
            ["online sbr-ci-bx_22222222-1-980 20s after its register"]
        );
        tick_loop.observe(NOW + 15).unwrap();
        assert_eq!(log.take(), Vec::<String>::new());
    }

    /// `drain` gives every delete and unregister the reason `drain`.
    #[test]
    fn drain_logs_its_deletes() {
        let cfg = config();
        let fake = Fake::new(reply);
        let mut tick_loop = started(&cfg, &fake);
        let log = capture_log(&mut tick_loop);
        tick_loop.drain(NOW).unwrap();
        let lines = log.take();
        assert!(lines.contains(&"unregister sbr-ci-bx_22222222-0-900: drain".to_string()));
        assert!(lines.contains(&"delete bx_22222222 (pool ci): drain".to_string()));
        assert!(lines.iter().all(|l| !l.contains("JITSECRET")), "{lines:?}");
    }

    /// After a restart the summary says the spend is not known; from the next UTC day on
    /// it gives the real figure.
    #[test]
    fn summary_gives_the_real_spend_once_the_day_turns() {
        let cfg = config();
        let fake = lagging(|_| "", |_| "");
        let mut tick_loop = started(&cfg, &fake);
        let log = capture_log(&mut tick_loop);
        tick_loop.tick(NOW, decide).unwrap();
        assert_eq!(
            log.take(),
            [
                "summary: pool ci: 0 up, 0 busy; spend today unknown since restart (counted as \
              the cap)"
            ]
        );
        // Within the hour: no summary.
        tick_loop.tick(NOW + 15, decide).unwrap();
        assert_eq!(log.take(), Vec::<String>::new());
        tick_loop.tick(DAY_S + NOW, decide).unwrap();
        assert_eq!(
            log.take(),
            ["summary: pool ci: 0 up, 0 busy; spent 0c today"]
        );
    }
}
