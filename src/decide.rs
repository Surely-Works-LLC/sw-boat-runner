//! One tick's decision: a snapshot and a pool in, that pool's actions out. Pure and
//! deterministic, so the property tests and spec/BoatRunner.tla can check it directly.
//!
//! Each action has one guard, a `View` method named after its row in the table in
//! docs/design.md#state-machine (and its `*Guard` in the spec), in the table's order. The
//! table states each rule once. `decide` runs the guards in a fixed order because later
//! guards read earlier decisions, such as the demand that registers and starts leave.

use std::collections::BTreeSet;

use crate::model::{
    Action, JobKey, Machine, MachineState, Placement, Pool, Runner, RunnerState, Snapshot, Warming,
};
use MachineState::{Gone, Starting, Stopped, Stopping, Up};
use Placement::{Fork, Resume, Slot};
use RunnerState::{Busy, Idle, Offline, Registered};

/// How long after its job was last seen queued a job's fork with no runner online is
/// deleted (`orphaned`). The grace counts from the job, not from the machine coming up,
/// so a job that left the queue a moment ago, maybe to a runner the listing does not show
/// busy yet, keeps its fork.
pub const ORPHAN_GRACE_S: u64 = 60;

/// How long a kept machine may stay `Starting` before it counts as stuck. boat lists a
/// resumed or forked machine `hydrated: false` while it copies the snapshot in, which
/// takes minutes; a machine whose copy broke (a dead lazy mount) stays that way for good.
/// A stuck machine covers no queued job and no `min_up` place, and is stopped, or deleted
/// in a pool without `resume`. An hour is well above any copy boat finishes.
pub const STUCK_START_S: u64 = 3600;

/// How long after a kept machine last had a `Busy` or `Registered` runner it may still
/// get a standby runner. A registration restarts the machine's idle clock, so a standby
/// runner on a machine that is long idle would keep it up up to a further `idle_stop_s`.
/// Inside this window the restart costs at most the window. Two minutes covers the tick
/// after a job ends even when a pass is slow.
pub const STANDBY_FRESH_S: u64 = 120;

/// The actions for `pool`, in this order: unregisters, deletes, registers (each machine's
/// first after a `CancelWarm` when its warm may run), resumes, kept forks, job forks,
/// stops, warm cancels, a warm. Inputs are sorted first, so their order in `snap` does not
/// matter. `snap.queued` holds only the jobs routed to this pool (see `run::route`).
pub fn decide(snap: &Snapshot, pool: &Pool) -> Vec<Action> {
    let v = View::new(snap, pool);
    let mut actions = Vec::new();

    let mut unregistered = BTreeSet::new();
    for r in v.runners.iter().filter(|r| v.unregister(r)) {
        unregistered.insert(r.name.as_str());
        actions.push(Action::Unregister {
            runner: r.name.clone(),
        });
    }

    let mut deleted = BTreeSet::new();
    let kept = [Stopped, Up, Starting].into_iter().flat_map(|s| v.kept(s));
    for m in v.ephemeral().chain(kept).filter(|m| v.delete(m)) {
        deleted.insert(m.id.as_str());
        actions.push(Action::Delete {
            machine: m.id.clone(),
        });
    }

    let mut want = v.demand(&unregistered);
    let mut registered_on = BTreeSet::new();
    let mut free = v.register(&unregistered);
    for (m, slot) in free.by_ref().take(want) {
        v.register_on(&mut actions, &mut registered_on, m, slot);
        want -= 1;
    }
    // Demand left means no slot is free, so this takes none.
    let ahead = v.standby_short(&unregistered);
    for (m, slot) in free.filter(|(m, _)| v.standby(m)).take(ahead) {
        v.register_on(&mut actions, &mut registered_on, m, slot);
    }

    // Without this, every tick until a machine is up would start more for the same jobs.
    want = want.saturating_sub(v.pending(&deleted));
    let slots = pool.slots_per_box as usize;
    let mut short = v.short();
    let mut starts = 0;
    for m in v.kept(Stopped).filter(|m| !deleted.contains(m.id.as_str())) {
        if !v.resume(want, short) || !v.start(&mut starts) {
            break;
        }
        actions.push(Action::Resume {
            machine: m.id.clone(),
        });
        short = short.saturating_sub(1);
        want = want.saturating_sub(slots);
    }
    let stopping = v.kept(Stopping).filter(|_| pool.has(Resume));
    short = short.saturating_sub(stopping.count());
    // A start refused above is refused here too: the budget and boxes are the same.
    while v.fork_kept(want, short) && v.start(&mut starts) {
        actions.push(Action::Fork {
            pool: pool.name.clone(),
            job: None,
        });
        short = short.saturating_sub(1);
        want = want.saturating_sub(slots);
    }
    if pool.has(Fork) {
        let forked = v.forked();
        let unforked = v.jobs.iter().filter(|j| forked.binary_search(j).is_err());
        for &job in unforked {
            if want == 0 || !v.start(&mut starts) {
                break;
            }
            actions.push(Action::Fork {
                pool: pool.name.clone(),
                job: Some(job),
            });
            want -= 1;
        }
    }

    // A stuck machine takes no job whatever the demand, so it is stopped regardless.
    if pool.has(Resume) {
        for m in v.kept(Starting).filter(|m| v.stuck(m)) {
            actions.push(Action::Stop {
                machine: m.id.clone(),
            });
        }
    }
    if pool.has(Resume) && want == 0 {
        let mut up = v.kept(Up).count();
        for m in v.kept(Up) {
            if up <= pool.min_up as usize {
                break;
            }
            if v.stop(m, &registered_on) {
                deleted.insert(m.id.as_str());
                actions.push(Action::Stop {
                    machine: m.id.clone(),
                });
                up -= 1;
            }
        }
    }

    // From here `deleted` holds the stopped machines too.
    let busy =
        |m: &&Machine| registered_on.contains(m.id.as_str()) || deleted.contains(m.id.as_str());
    for m in v.kept(Up).filter(|m| !busy(m)) {
        if snap.warming.get(&m.id) == Some(&Warming::Unknown) {
            actions.push(Action::CancelWarm {
                machine: m.id.clone(),
            });
        }
    }
    if let Some(m) = v.warm(busy) {
        actions.push(Action::Warm {
            machine: m.id.clone(),
        });
    }
    actions
}

/// Why `decide` deletes each machine of `pool` it deletes, for the log: `finished`,
/// `ttl` or `orphaned` for a job's fork; `no_resume` for a stopped kept machine of a pool
/// without `resume`, `surplus` for one above `max_boxes` in a pool with `resume`, and
/// `no_slot` for a kept machine up or starting in a pool without `slot`. Reads the same
/// guard as `decide`, so the two cannot disagree.
pub fn delete_reasons<'a>(snap: &'a Snapshot, pool: &'a Pool) -> Vec<(&'a str, &'static str)> {
    let v = View::new(snap, pool);
    let deleted = v.machines.iter().filter(|m| v.delete(m));
    deleted
        .map(|m| {
            let why = if !m.is_ephemeral() {
                if m.state == Stopped && pool.has(Resume) {
                    "surplus"
                } else if m.state == Stopped {
                    "no_resume"
                } else if v.stuck(m) && pool.has(Slot) {
                    "stuck_start"
                } else {
                    "no_slot"
                }
            } else if v.finished(m) {
                "finished"
            } else if v.age(m) > pool.fork_ttl_s {
                "ttl"
            } else {
                "orphaned"
            };
            (m.id.as_str(), why)
        })
        .collect()
}

/// One snapshot as one pool sees it, sorted once so that each guard is a lookup.
struct View<'a> {
    snap: &'a Snapshot,
    pool: &'a Pool,
    /// The pool's machines by id, and its runners by name.
    machines: Vec<&'a Machine>,
    runners: Vec<&'a Runner>,
    /// Every runner by machine, whatever its pool: a stray runner still holds a machine.
    by_machine: Vec<&'a Runner>,
    /// The queued jobs the pool serves, by key, each once.
    jobs: Vec<JobKey>,
    /// The pool's machines `Up` or `Starting`, against `max_boxes`.
    boxes: usize,
    /// The pool's job forks deleted as `orphaned`, by id.
    orphans: Vec<&'a str>,
    /// The pool's stopped kept machines deleted as `surplus`, by id.
    surplus: Vec<&'a str>,
}

impl<'a> View<'a> {
    fn new(snap: &'a Snapshot, pool: &'a Pool) -> Self {
        let own = snap.machines.iter().filter(|m| m.pool == pool.name);
        let mut machines: Vec<&Machine> = own.collect();
        machines.sort_by(|a, b| a.id.cmp(&b.id));
        let own = snap.runners.iter().filter(|r| r.pool == pool.name);
        let mut runners: Vec<&Runner> = own.collect();
        runners.sort_by(|a, b| a.name.cmp(&b.name));
        let mut by_machine: Vec<&Runner> = snap.runners.iter().collect();
        by_machine.sort_by(|a, b| a.machine.cmp(&b.machine));
        let served = snap.queued.iter().filter(|j| pool.serves(&j.labels));
        let mut jobs: Vec<JobKey> = served.map(|j| j.key).collect();
        jobs.sort();
        jobs.dedup();
        let boxes = machines.iter().filter(|m| matches!(m.state, Up | Starting));
        let mut v = View {
            orphans: Vec::new(),
            surplus: Vec::new(),
            boxes: boxes.count(),
            snap,
            pool,
            machines,
            runners,
            by_machine,
            jobs,
        };
        v.orphans = v.orphans();
        v.surplus = v.surplus();
        v
    }

    /// Register: fresh ephemeral machines first, as they are already paid for, then free
    /// kept slots. The caller takes as many as there is demand.
    fn register<'s>(
        &'s self,
        unregistered: &'s BTreeSet<&str>,
    ) -> impl Iterator<Item = (&'a Machine, u32)> + 's {
        let fresh = self.ephemeral().filter(|m| self.fresh(m)).map(|m| (m, 0));
        let kept = self.kept(Up).filter(|_| self.pool.has(Slot));
        let slots = kept.flat_map(|m| (0..self.pool.slots_per_box).map(move |s| (m, s)));
        let free = slots.filter(|&(m, slot)| {
            let taken = |r: &&Runner| r.slot == slot && !unregistered.contains(r.name.as_str());
            !self.on(m).iter().any(taken)
        });
        fresh.chain(free)
    }

    /// Register `slot` of `m`, after a `CancelWarm` the first time it picks a machine whose
    /// warm may still run (spec `CancelWarm`, `RegisterGuard`).
    fn register_on(
        &self,
        actions: &mut Vec<Action>,
        registered_on: &mut BTreeSet<&'a str>,
        m: &'a Machine,
        slot: u32,
    ) {
        if registered_on.insert(m.id.as_str()) && self.snap.warming.contains_key(&m.id) {
            actions.push(Action::CancelWarm {
                machine: m.id.clone(),
            });
        }
        actions.push(Action::Register {
            pool: self.pool.name.clone(),
            machine: m.id.clone(),
            slot,
        });
    }

    /// Warm (spec `WarmGuard`), in a pool with `idle_warm` and no machine up whose warm
    /// may run: a kept `Up` machine with no runner, not `busy` (registered on or stopped
    /// this tick), and idle since its last job and its last warm for `interval_s`; the one
    /// idle longest. A machine with no runner left after the registers means no queued
    /// job and no standby runner wants it. The idle clock is left alone, so a warm never
    /// delays a stop.
    fn warm(&self, busy: impl Fn(&&Machine) -> bool) -> Option<&'a Machine> {
        let w = self.pool.idle_warm.as_ref()?;
        if self.kept(Up).any(|m| self.snap.warming.contains_key(&m.id)) {
            return None;
        }
        let idle_from = |m: &Machine| {
            let warmed = self.snap.warmed_at.get(&m.id).copied().unwrap_or(0);
            m.last_busy.max(warmed)
        };
        let due = |m: &&'a Machine| {
            self.on(m).is_empty()
                && !busy(m)
                && self.snap.now >= idle_from(m).saturating_add(w.interval_s)
        };
        self.kept(Up)
            .filter(due)
            .min_by_key(|m| (idle_from(m), m.id.as_str()))
    }

    /// Runners the pool is short of `standby`: `standby` minus its `Registered` and `Idle`
    /// runners that keep their slot and that no queued job will take.
    fn standby_short(&self, unregistered: &BTreeSet<&str>) -> usize {
        let kept = |r: &&&Runner| !unregistered.contains(r.name.as_str());
        let ready = self.runners.iter().filter(kept);
        let supply = ready.filter(|r| matches!(r.state, Registered | Idle));
        let spare = supply.count().saturating_sub(self.jobs.len());
        (self.pool.standby as usize).saturating_sub(spare)
    }

    /// Register ahead of demand (spec `StandbyPick`): a kept machine not yet idle for
    /// `idle_stop_s`, whose idle clock the registration does not set back by more than
    /// `STANDBY_FRESH_S`, or one no `Stop` could take anyway. A machine already due to
    /// stop gets none, or the registration would hold it up in place of the stop. The next job then finds a runner online instead of waiting for
    /// a tick, a registration and the runner's start.
    fn standby(&self, m: &Machine) -> bool {
        let idle_for = self.snap.now.saturating_sub(m.last_busy);
        let fresh = idle_for <= STANDBY_FRESH_S && idle_for < self.pool.idle_stop_s;
        let stays = !self.pool.has(Resume) || self.kept(Up).count() <= self.pool.min_up as usize;
        !m.is_ephemeral() && (fresh || stays)
    }

    /// No runner was ever seen on it since it came up, and it is not deleted this tick
    /// (spec `Fresh`). Only a fresh machine gets a `Register`, so it runs one job.
    fn fresh(&self, m: &Machine) -> bool {
        m.is_ephemeral() && m.state == Up && !seen(m) && self.on(m).is_empty() && !self.delete(m)
    }

    /// Resume, for each `Stopped` kept machine in turn; the caller takes the start.
    fn resume(&self, want: usize, short: usize) -> bool {
        self.pool.has(Resume) && (want > 0 || short > 0)
    }

    /// Fork(job) skips these jobs: forked by a machine not `Gone`, in any pool.
    fn forked(&self) -> Vec<JobKey> {
        let live = self.snap.machines.iter().filter(|m| m.state != Gone);
        let mut forked: Vec<JobKey> = live.filter_map(|m| m.job).collect();
        forked.sort();
        forked
    }

    /// Fork(none); the caller takes the start.
    fn fork_kept(&self, want: usize, short: usize) -> bool {
        short > 0 || (self.pool.has(Resume) && !self.pool.has(Fork) && want > 0)
    }

    /// A kept machine `Starting` for `STUCK_START_S` with no `Busy` or `Registered`
    /// runner: its snapshot copy will not finish, so no runner can start on it.
    fn stuck(&self, m: &Machine) -> bool {
        !m.is_ephemeral()
            && m.state == Starting
            && self.age(m) >= STUCK_START_S
            && !holds(self.on(m), &[Busy, Registered])
    }

    /// Stop, for each kept `Up` machine while more than `min_up` stay up; the caller
    /// checks `resume` and that no demand is unmet.
    fn stop(&self, m: &Machine, registered_on: &BTreeSet<&str>) -> bool {
        !holds(self.on(m), &[Busy, Registered])
            && self.snap.now.saturating_sub(m.last_busy) >= self.pool.idle_stop_s
            && self.age(m) >= self.pool.min_up_s
            && !registered_on.contains(m.id.as_str())
    }

    fn delete(&self, m: &Machine) -> bool {
        let on = self.on(m);
        if m.is_ephemeral() {
            let expired = self.age(m) > self.pool.fork_ttl_s;
            m.state != Gone
                && !holds(on, &[Busy])
                && (self.finished(m) || expired || self.orphaned(m))
        } else {
            match m.state {
                Stopped => {
                    let unused = !self.pool.has(Resume) || self.surplus.contains(&m.id.as_str());
                    unused && !holds(on, &[Busy])
                }
                Up | Starting => {
                    let stuck = self.stuck(m) && !self.pool.has(Resume);
                    (!self.pool.has(Slot) && !holds(on, &[Busy, Registered])) || stuck
                }
                Stopping | Gone => false,
            }
        }
    }

    /// An ephemeral machine's job has finished (spec `Finished`).
    fn finished(&self, m: &Machine) -> bool {
        match m.state {
            Up => {
                let live = self.on(m).iter().any(|r| r.state != Offline);
                !live && (seen(m) || self.age(m) > self.pool.register_timeout_s)
            }
            Stopped => true,
            Starting | Stopping | Gone => false,
        }
    }

    /// Job forks with nothing left to run (spec has no counterpart): `Up`, no runner on
    /// it `Busy`, and its own job not queued, last seen queued `ORPHAN_GRACE_S` ago (or,
    /// never seen, that long after it came up). One with a runner online (`Idle` or
    /// `Registered`) also waits until the pool has had no queued job for the grace,
    /// counted from when one was last seen, since that runner may be taking one the
    /// listing does not show yet. A JIT runner takes any job its labels match, so a fork
    /// made for one job may run another: while jobs are queued, enough forks no `Busy`
    /// runner holds stay to cover every one, whichever job each was made for. `act`
    /// deletes a fork's runners before the fork, and GitHub refuses to delete one that
    /// took a job since, which keeps the fork.
    fn orphans(&self) -> Vec<&'a str> {
        let now = self.snap.now;
        let quiet = |t: u64| now.saturating_sub(t) >= ORPHAN_GRACE_S;
        let pool_quiet = |m: &Machine| {
            self.jobs.is_empty()
                && quiet(
                    self.snap
                        .queued_at
                        .get(&self.pool.name)
                        .copied()
                        .unwrap_or(m.since),
                )
        };
        let candidates = self.ephemeral().filter(|m| {
            let Some(job) = m.job else {
                return false;
            };
            let on = self.on(m);
            let online = on.iter().any(|r| r.state != Offline);
            m.state == Up
                && !holds(on, &[Busy])
                && self.jobs.binary_search(&job).is_err()
                && quiet(m.job_queued_at.unwrap_or(m.since))
                && (!online || pool_quiet(m))
        });
        // A fork that can still take a job: starting, or up and neither finished nor past
        // its TTL, with no runner or only `Registered` and `Idle` ones.
        let free = self
            .ephemeral()
            .filter(|m| match m.state {
                Starting => true,
                Up => {
                    !self.finished(m)
                        && self.age(m) <= self.pool.fork_ttl_s
                        && self
                            .on(m)
                            .iter()
                            .all(|r| matches!(r.state, Registered | Idle))
                }
                Stopping | Stopped | Gone => false,
            })
            .count();
        let spare = free.saturating_sub(self.jobs.len());
        candidates.map(|m| m.id.as_str()).take(spare).collect()
    }

    /// Stopped kept machines above `max_boxes` in a pool with `resume`, while the pool
    /// has no demand: the highest ids first, as `Resume` takes the lowest first. A warm
    /// pool keeps every machine it forks, so without this its kept set stays at the
    /// largest it ever reached: above a lowered `max_boxes`, and one above it when a
    /// machine whose stop boat refused, which holds no box, made room for another. The
    /// count leaves out kept `Stopping` machines, which may come back `Stopped`, so the
    /// stopped machines left are at least the resumes `max_boxes` allows. With no demand,
    /// no tick deletes a machine and forks a new one for the same job.
    fn surplus(&self) -> Vec<&'a str> {
        if !self.pool.has(Resume) {
            return Vec::new();
        }
        let unregistered = self.runners.iter().filter(|r| self.unregister(r));
        let unregistered = unregistered.map(|r| r.name.as_str()).collect();
        if self.demand(&unregistered) > 0 {
            return Vec::new();
        }
        let held: usize = [Up, Starting, Stopped]
            .into_iter()
            .map(|s| self.kept(s).count())
            .sum();
        let over = held.saturating_sub(self.pool.max_boxes as usize);
        let stopped: Vec<&'a Machine> = self.kept(Stopped).collect();
        let idle = stopped
            .into_iter()
            .rev()
            .filter(|m| !holds(self.on(m), &[Busy]));
        idle.take(over).map(|m| m.id.as_str()).collect()
    }

    fn orphaned(&self, m: &Machine) -> bool {
        self.orphans.contains(&m.id.as_str())
    }

    /// Every runner is JIT: once offline it never comes back, and it holds its slot.
    fn unregister(&self, r: &Runner) -> bool {
        let machines = run_of(&self.machines, &r.machine, |m| &m.id);
        let timed_out = r.state == Registered
            && self.snap.now.saturating_sub(r.registered_at) > self.pool.register_timeout_s;
        !machines.iter().any(|m| m.state == Up) || timed_out || r.state == Offline
    }

    /// Queued jobs minus the pool's `Registered` and `Idle` runners that keep their slot.
    fn demand(&self, unregistered: &BTreeSet<&str>) -> usize {
        let kept = |r: &&&Runner| !unregistered.contains(r.name.as_str());
        let ready = self.runners.iter().filter(kept);
        let supply = ready.filter(|r| matches!(r.state, Registered | Idle));
        self.jobs.len().saturating_sub(supply.count())
    }

    /// Jobs the pool's `Starting` machines not deleted this tick take once up. A stuck
    /// machine never comes up, so it takes none.
    fn pending(&self, deleted: &BTreeSet<&str>) -> usize {
        let starting = self
            .machines
            .iter()
            .filter(|m| m.state == Starting && !self.stuck(m));
        let left = starting.filter(|m| !deleted.contains(m.id.as_str()));
        let slots = self.pool.slots_per_box as usize;
        left.map(|m| if m.is_ephemeral() { 1 } else { slots }).sum()
    }

    /// Kept machines short of `min_up`, which counts kept machines up or starting but not
    /// stuck.
    fn short(&self) -> usize {
        let starting = self.kept(Starting).filter(|m| !self.stuck(m));
        let held = self.kept(Up).count() + starting.count();
        (self.pool.min_up as usize).saturating_sub(held)
    }

    /// docs/design.md#start-budget and `max_boxes`, after the starts `taken` this tick.
    fn start(&self, taken: &mut u64) -> bool {
        let limits = self.snap.limits;
        // Every start this tick is priced at `next_start_cents`, the one quote the snapshot has.
        let price = u64::from(limits.next_start_cents);
        let spent = u64::from(limits.spent_today_cents) + (*taken + 1) * price;
        let affordable = price == 0 || spent <= u64::from(self.pool.paid_start_cents_per_day);
        // Past the free starts the price is not known; the next tick gets a new quote.
        let free = limits.free_starts_left.map(u64::from);
        let quoted = price != 0 || free.is_none_or(|left| *taken < left);
        let room = self.boxes + (*taken as usize) < self.pool.max_boxes as usize;
        let ok = room && limits.can_start && affordable && quoted;
        *taken += u64::from(ok);
        ok
    }

    fn kept(&self, state: MachineState) -> impl Iterator<Item = &'a Machine> + '_ {
        let machines = self.machines.iter().copied();
        machines.filter(move |m| !m.is_ephemeral() && m.state == state)
    }

    fn ephemeral(&self) -> impl Iterator<Item = &'a Machine> + '_ {
        self.machines.iter().copied().filter(|m| m.is_ephemeral())
    }

    fn on(&self, m: &Machine) -> &[&'a Runner] {
        run_of(&self.by_machine, &m.id, |r| &r.machine)
    }

    /// Seconds in the machine's current state.
    fn age(&self, m: &Machine) -> u64 {
        self.snap.now.saturating_sub(m.since)
    }
}

/// A runner was `Busy` or `Registered` on the machine since it entered its state.
fn seen(m: &Machine) -> bool {
    m.last_busy >= m.since
}

fn holds(on: &[&Runner], states: &[RunnerState]) -> bool {
    on.iter().any(|r| states.contains(&r.state))
}

/// The items whose key is `key`, from a slice sorted by that key.
fn run_of<'s, T>(sorted: &'s [T], key: &str, key_of: impl Fn(&T) -> &String) -> &'s [T] {
    let start = sorted.partition_point(|x| key_of(x).as_str() < key);
    let len = sorted[start..].partition_point(|x| key_of(x) == key);
    &sorted[start..start + len]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IdleWarm, Limits, MachineState as Ms, QueuedJob, RunnerState as Rs};

    const NOW: u64 = 10_000;

    fn pool(preset: &str, extra: &str) -> Pool {
        let text = format!(
            "repo = \"acme/app\"\n[[pool]]\nname = \"ci\"\nlabels = [\"self-hosted\", \"boat\"]\n\
             machine_type = \"large\"\nsnapshot = \"snap-1\"\nmax_boxes = 3\n\
             preset = \"{preset}\"\n{extra}\n"
        );
        crate::policy::parse(&text).unwrap().pools.remove(0)
    }

    fn key(id: u64) -> JobKey {
        JobKey { id, attempt: 1 }
    }

    /// In `state` for two hours and idle all that time.
    fn machine(id: &str, state: MachineState) -> Machine {
        Machine {
            id: id.into(),
            pool: "ci".into(),
            state,
            since: NOW - 7200,
            last_busy: NOW - 7200,
            job: None,
            job_queued_at: None,
        }
    }

    /// A kept machine resumed or forked a minute ago and still copying its snapshot.
    fn starting(id: &str) -> Machine {
        Machine {
            since: NOW - 60,
            ..machine(id, Ms::Starting)
        }
    }

    /// Forked for `job` a minute ago; `last_busy` is from before it came up.
    fn forked(id: &str, job: u64, state: MachineState) -> Machine {
        Machine {
            since: NOW - 60,
            last_busy: NOW - 120,
            job: Some(key(job)),
            ..machine(id, state)
        }
    }

    fn runner(machine: &str, slot: u32, state: RunnerState) -> Runner {
        Runner {
            name: format!("sbr-ci-{machine}-{slot}-{}", NOW - 60),
            pool: "ci".into(),
            machine: machine.into(),
            slot,
            registered_at: NOW - 60,
            state,
        }
    }

    fn snap(machines: Vec<Machine>, runners: Vec<Runner>, jobs: &[u64]) -> Snapshot {
        Snapshot {
            now: NOW,
            machines,
            runners,
            queued: jobs
                .iter()
                .map(|&id| QueuedJob {
                    key: key(id),
                    labels: vec!["boat".into()],
                })
                .collect(),
            limits: Limits {
                can_start: true,
                next_start_cents: 0,
                spent_today_cents: 0,
                free_starts_left: None,
            },
            queued_at: Default::default(),
            warming: Default::default(),
            warmed_at: Default::default(),
        }
    }

    fn register(machine: &str, slot: u32) -> Action {
        Action::Register {
            pool: "ci".into(),
            machine: machine.into(),
            slot,
        }
    }

    fn resume(machine: &str) -> Action {
        Action::Resume {
            machine: machine.into(),
        }
    }

    fn fork(job: u64) -> Action {
        Action::Fork {
            pool: "ci".into(),
            job: Some(key(job)),
        }
    }

    fn delete(machine: &str) -> Action {
        Action::Delete {
            machine: machine.into(),
        }
    }

    fn stop(machine: &str) -> Action {
        Action::Stop {
            machine: machine.into(),
        }
    }

    fn fork_kept() -> Action {
        Action::Fork {
            pool: "ci".into(),
            job: None,
        }
    }

    const NOTHING: [Action; 0] = [];

    #[test]
    fn pool_replaces_a_finished_runner_in_the_same_slot() {
        let p = pool("pool", "");
        let idle = snap(
            vec![machine("m1", Ms::Up)],
            vec![runner("m1", 0, Rs::Idle)],
            &[],
        );
        assert_eq!(decide(&idle, &p), NOTHING);

        let offline = vec![runner("m1", 0, Rs::Offline)];
        let unregister = Action::Unregister {
            runner: offline[0].name.clone(),
        };
        let quiet = snap(vec![machine("m1", Ms::Up)], offline.clone(), &[]);
        assert_eq!(decide(&quiet, &p), std::slice::from_ref(&unregister));

        let queued = snap(vec![machine("m1", Ms::Up)], offline, &[1]);
        assert_eq!(decide(&queued, &p), [unregister, register("m1", 0)]);
    }

    /// A machine whose job ended a moment ago, with no runner left on it.
    fn just_used(id: &str) -> Machine {
        Machine {
            last_busy: NOW - 20,
            ..machine(id, Ms::Up)
        }
    }

    #[test]
    fn standby_registers_a_runner_when_a_job_ends_and_no_job_waits() {
        let p = pool("warm", "standby = 1");
        let ended = snap(vec![just_used("m1"), just_used("m2")], vec![], &[]);
        assert_eq!(decide(&ended, &p), [register("m1", 0)]);
        // Without the option nothing is registered ahead of a job.
        assert_eq!(decide(&ended, &pool("warm", "")), NOTHING);

        // One runner ready and no job for it: the pool has its standby.
        for state in [Rs::Registered, Rs::Idle] {
            let ready = vec![runner("m1", 0, state)];
            let s = snap(vec![just_used("m1"), just_used("m2")], ready, &[]);
            assert_eq!(decide(&s, &p), NOTHING, "{state:?}");
        }

        // A queued job takes the ready runner, so the next free slot gets one.
        let taken = vec![runner("m1", 0, Rs::Idle)];
        let s = snap(vec![just_used("m1"), just_used("m2")], taken, &[1]);
        assert_eq!(decide(&s, &p), [register("m2", 0)]);

        // Demand fills its slots first, and standby takes what is left.
        let s = snap(vec![just_used("m1"), just_used("m2")], vec![], &[1]);
        assert_eq!(decide(&s, &p), [register("m1", 0), register("m2", 0)]);
        let s = snap(vec![just_used("m1"), just_used("m2")], vec![], &[1, 2]);
        assert_eq!(decide(&s, &p), [register("m1", 0), register("m2", 0)]);
    }

    /// A registration restarts a machine's idle clock, so standby must not go on a
    /// machine that has sat idle: it would stay up a further `idle_stop_s`.
    #[test]
    fn standby_does_not_keep_an_idle_machine_up() {
        let p = pool("warm", "standby = 1\nidle_stop_s = 600\nmin_up_s = 0");
        let idle = Machine {
            last_busy: NOW - STANDBY_FRESH_S - 1,
            ..machine("m1", Ms::Up)
        };
        assert_eq!(decide(&snap(vec![idle.clone()], vec![], &[]), &p), NOTHING);
        let edge = Machine {
            last_busy: NOW - STANDBY_FRESH_S,
            ..idle
        };
        assert_eq!(
            decide(&snap(vec![edge], vec![], &[]), &p),
            [register("m1", 0)]
        );

        // A machine due to stop stops, however recently it was busy.
        let short = pool("warm", "standby = 1\nidle_stop_s = 60\nmin_up_s = 0");
        let due = Machine {
            last_busy: NOW - 60,
            ..machine("m1", Ms::Up)
        };
        assert_eq!(decide(&snap(vec![due], vec![], &[]), &short), [stop("m1")]);

        // An idle standby runner does not hold its machine: it stops on time, and the
        // long-idle machine left gets no standby.
        let since_job = |id: &str| Machine {
            last_busy: NOW - 600,
            ..machine(id, Ms::Up)
        };
        let waiting = vec![runner("m1", 0, Rs::Idle)];
        let s = snap(vec![since_job("m1"), since_job("m2")], waiting, &[]);
        assert_eq!(decide(&s, &p), [stop("m1"), stop("m2")]);
        let s = snap(vec![since_job("m1"), since_job("m2")], vec![], &[]);
        assert_eq!(decide(&s, &p), [stop("m1"), stop("m2")]);
    }

    /// Where no `Stop` can take the machine, a standby runner costs no machine time.
    #[test]
    fn standby_stays_on_machines_that_never_stop() {
        let long_idle = || vec![machine("m1", Ms::Up)];
        let p = pool("warm", "standby = 1\nmin_up = 1");
        assert_eq!(
            decide(&snap(long_idle(), vec![], &[]), &p),
            [register("m1", 0)]
        );
        let p = pool("pool", "standby = 2\nslots_per_box = 2");
        assert_eq!(
            decide(&snap(long_idle(), vec![], &[]), &p),
            [register("m1", 0), register("m1", 1)]
        );
        // A job's fork runs one job and is deleted, so it never gets a standby runner.
        let p = pool("hybrid", "standby = 1");
        let fresh = Machine {
            last_busy: NOW - 20,
            ..forked("m1", 1, Ms::Up)
        };
        let used = decide(&snap(vec![fresh], vec![], &[]), &p);
        assert!(!used.contains(&register("m1", 0)), "{used:?}");
    }

    #[test]
    fn pool_forks_kept_machines_up_to_min_up() {
        let p = pool("pool", "min_up = 2");
        assert_eq!(
            decide(&snap(vec![], vec![], &[]), &p),
            [fork_kept(), fork_kept()]
        );

        // A starting kept machine counts; an ephemeral one does not.
        let s = snap(
            vec![starting("m1"), forked("f1", 1, Ms::Up)],
            vec![runner("f1", 0, Rs::Busy)],
            &[],
        );
        assert_eq!(decide(&s, &p), [fork_kept()]);

        // A stopped kept machine in a pool that cannot resume does not count either.
        let s = snap(
            vec![machine("m1", Ms::Up), machine("m2", Ms::Stopped)],
            vec![],
            &[],
        );
        assert_eq!(decide(&s, &p), [delete("m2"), fork_kept()]);
    }

    #[test]
    fn pool_deletes_a_stopped_kept_machine_nothing_would_resume() {
        let s = snap(vec![machine("m1", Ms::Stopped)], vec![], &[]);
        assert_eq!(decide(&s, &pool("pool", "min_up = 0")), [delete("m1")]);
        assert_eq!(decide(&s, &pool("fork", "")), [delete("m1")]);
        assert_eq!(decide(&s, &pool("warm", "")), NOTHING);

        let busy = snap(
            vec![machine("m1", Ms::Stopped)],
            vec![runner("m1", 0, Rs::Busy)],
            &[],
        );
        let unregister = Action::Unregister {
            runner: busy.runners[0].name.clone(),
        };
        assert_eq!(
            decide(&busy, &pool("pool", "min_up = 0")),
            std::slice::from_ref(&unregister)
        );
    }

    /// A warm pool kept every machine it forked, so a burst, or a lowered `max_boxes`, left
    /// stopped machines above `max_boxes` that nothing deleted.
    #[test]
    fn a_pool_with_resume_deletes_stopped_kept_machines_above_max_boxes() {
        let stopped = |ids: &[&str]| ids.iter().map(|id| machine(id, Ms::Stopped)).collect();
        // max_boxes is 3: the two highest ids go, as resume takes the lowest first.
        let p = pool("warm", "");
        let s = snap(stopped(&["m1", "m2", "m3", "m4", "m5"]), vec![], &[]);
        assert_eq!(decide(&s, &p), [delete("m4"), delete("m5")]);
        assert_eq!(
            delete_reasons(&s, &p),
            [("m4", "surplus"), ("m5", "surplus")]
        );
        assert_eq!(
            decide(&s, &pool("hybrid", "")),
            [delete("m4"), delete("m5")]
        );

        // Up and starting kept machines count; the deletes come from the stopped ones.
        let mut machines = stopped(&["m2", "m3", "m4"]);
        machines.extend([starting("m1"), starting("m5")]);
        let s = snap(machines, vec![], &[]);
        assert_eq!(decide(&s, &p), [delete("m3"), delete("m4")]);

        // At max_boxes nothing goes.
        let s = snap(stopped(&["m1", "m2", "m3"]), vec![], &[]);
        assert_eq!(decide(&s, &p), NOTHING);

        // A stopping machine may come back stopped, so it does not count yet.
        let mut machines = stopped(&["m2", "m3", "m4", "m5"]);
        machines.push(machine("m1", Ms::Stopping));
        assert_eq!(decide(&snap(machines, vec![], &[]), &p), [delete("m5")]);
    }

    /// While a job waits, a surplus machine may be the one its resume takes, so nothing goes.
    #[test]
    fn a_pool_with_demand_keeps_its_surplus_stopped_machines() {
        let p = pool("warm", "");
        let machines = ["m1", "m2", "m3", "m4"].map(|id| machine(id, Ms::Stopped));
        let s = snap(machines.to_vec(), vec![], &[1]);
        assert_eq!(decide(&s, &p), [resume("m1")]);

        // A busy runner on a stopped machine keeps it.
        let s = snap(machines.to_vec(), vec![runner("m4", 0, Rs::Busy)], &[]);
        let unregister = Action::Unregister {
            runner: s.runners[0].name.clone(),
        };
        assert_eq!(decide(&s, &p), [unregister, delete("m3")]);
    }

    /// A pool switched to `fork` keeps kept machines that no step would use again.
    #[test]
    fn a_pool_without_slot_deletes_its_unused_kept_machines() {
        let p = pool("fork", "");
        let s = snap(
            vec![
                machine("m1", Ms::Up),
                starting("m2"),
                machine("m3", Ms::Stopping),
                machine("m4", Ms::Up),
                machine("m5", Ms::Up),
            ],
            vec![runner("m4", 0, Rs::Busy), runner("m5", 0, Rs::Registered)],
            &[],
        );
        assert_eq!(decide(&s, &p), [delete("m1"), delete("m2")]);
        // A pool with slot still uses them.
        assert_eq!(decide(&s, &pool("pool", "min_up = 0")), NOTHING);
    }

    #[test]
    fn warm_forks_a_kept_machine_for_demand_no_stopped_machine_can_take() {
        let mut p = pool("warm", "min_up = 2\nslots_per_box = 2");
        p.max_boxes = 4;
        let machines = vec![machine("m1", Ms::Up), machine("m2", Ms::Up)];
        let runners = vec![
            runner("m1", 0, Rs::Busy),
            runner("m1", 1, Rs::Busy),
            runner("m2", 0, Rs::Busy),
            runner("m2", 1, Rs::Busy),
        ];
        let s = snap(machines, runners, &[1, 2, 3, 4]);
        // Two machines of two slots each cover the four jobs.
        assert_eq!(decide(&s, &p), [fork_kept(), fork_kept()]);
        p.max_boxes = 3;
        assert_eq!(decide(&s, &p), [fork_kept()]);
        p.max_boxes = 4;

        let mut busy = s.clone();
        busy.queued.truncate(2);
        assert_eq!(decide(&busy, &p), [fork_kept()]);

        // A stopped kept machine is resumed first.
        busy.machines.push(machine("m3", Ms::Stopped));
        assert_eq!(decide(&busy, &p), [resume("m3")]);
    }

    #[test]
    fn warm_resumes_before_it_forks_a_kept_machine() {
        let p = pool("warm", "min_up = 3");
        let s = snap(vec![machine("m1", Ms::Stopped)], vec![], &[]);
        assert_eq!(decide(&s, &p), [resume("m1"), fork_kept(), fork_kept()]);

        // A machine still stopping will be resumed later, so it is not replaced now.
        let s = snap(
            vec![machine("m1", Ms::Stopped), machine("m2", Ms::Stopping)],
            vec![],
            &[],
        );
        assert_eq!(decide(&s, &p), [resume("m1"), fork_kept()]);
    }

    #[test]
    fn pool_registers_a_runner_in_the_free_slot() {
        let p = pool("pool", "slots_per_box = 2");
        let s = snap(
            vec![machine("m1", Ms::Up)],
            vec![runner("m1", 0, Rs::Busy)],
            &[1],
        );
        assert_eq!(decide(&s, &p), [register("m1", 1)]);
    }

    #[test]
    fn warm_resumes_for_a_job_and_stops_when_idle() {
        let p = pool("warm", "");
        let queued = snap(vec![machine("m1", Ms::Stopped)], vec![], &[1]);
        assert_eq!(decide(&queued, &p), [resume("m1")]);

        let idle = snap(vec![machine("m1", Ms::Up)], vec![], &[]);
        assert_eq!(
            decide(&idle, &p),
            [Action::Stop {
                machine: "m1".into()
            }]
        );
    }

    /// A kept machine whose snapshot copy never finishes stays `Starting` (boat lists it
    /// `hydrated: false`). Past `STUCK_START_S` it covers no job and no `min_up` place,
    /// and it is stopped even while jobs wait.
    #[test]
    fn warm_stops_a_machine_stuck_starting_and_replaces_it() {
        let p = pool("warm", "min_up = 1");
        let stuck = Machine {
            since: NOW - STUCK_START_S,
            ..machine("m1", Ms::Starting)
        };
        let s = snap(
            vec![stuck.clone(), machine("m2", Ms::Stopped)],
            vec![],
            &[1],
        );
        assert_eq!(decide(&s, &p), [resume("m2"), stop("m1")]);

        // Just under the bound it is still copying: it covers the job and `min_up`.
        let copying = Machine {
            since: NOW - STUCK_START_S + 1,
            ..stuck.clone()
        };
        let s = snap(vec![copying, machine("m2", Ms::Stopped)], vec![], &[1]);
        assert_eq!(decide(&s, &p), NOTHING);

        // A runner on it keeps it this tick, as on an `Up` machine; the runner is
        // unregistered, as on any machine that is not up, and the next tick stops it.
        let s = snap(vec![stuck], vec![runner("m1", 0, Rs::Registered)], &[]);
        let stops = decide(&s, &p);
        assert!(
            !stops.iter().any(|a| matches!(a, Action::Stop { .. })),
            "{stops:?}"
        );
    }

    /// A pool without `resume` could not bring a stopped machine back, so it deletes a
    /// stuck one and forks a new one for `min_up`.
    #[test]
    fn pool_deletes_a_machine_stuck_starting() {
        let p = pool("pool", "min_up = 1");
        let stuck = Machine {
            since: NOW - STUCK_START_S,
            ..machine("m1", Ms::Starting)
        };
        let s = snap(vec![stuck], vec![], &[]);
        assert_eq!(decide(&s, &p), [delete("m1"), fork_kept()]);
        assert_eq!(delete_reasons(&s, &p), [("m1", "stuck_start")]);
    }

    #[test]
    fn warm_resumes_up_to_min_up_without_jobs() {
        let p = pool("warm", "min_up = 2");
        let stopped = vec![
            machine("m1", Ms::Stopped),
            machine("m2", Ms::Stopped),
            machine("m3", Ms::Stopped),
        ];
        let s = snap(stopped.clone(), vec![], &[]);
        assert_eq!(decide(&s, &p), [resume("m1"), resume("m2")]);

        let mut resuming = stopped;
        resuming[0] = starting("m1");
        let s = snap(resuming, vec![], &[]);
        assert_eq!(decide(&s, &p), [resume("m2")]);

        // An ephemeral machine is not one of the `min_up` kept machines.
        let s = snap(
            vec![forked("f1", 1, Ms::Starting), machine("m1", Ms::Stopped)],
            vec![],
            &[1],
        );
        assert_eq!(decide(&s, &pool("hybrid", "min_up = 1")), [resume("m1")]);
    }

    #[test]
    fn fork_gives_each_job_one_machine_and_deletes_it_after_use() {
        let p = pool("fork", "");
        assert_eq!(decide(&snap(vec![], vec![], &[1]), &p), [fork(1)]);

        let starting = snap(vec![forked("m1", 1, Ms::Starting)], vec![], &[1, 2]);
        assert_eq!(decide(&starting, &p), [fork(2)]);

        let fresh = snap(vec![forked("m1", 1, Ms::Up)], vec![], &[1]);
        assert_eq!(decide(&fresh, &p), [register("m1", 0)]);

        let used = Machine {
            last_busy: NOW - 10,
            ..forked("m1", 1, Ms::Up)
        };
        assert_eq!(decide(&snap(vec![used], vec![], &[]), &p), [delete("m1")]);
    }

    /// Job 1 was cancelled. Its fork goes `ORPHAN_GRACE_S` after the job was last seen
    /// queued, not after `register_timeout_s` or `fork_ttl_s`.
    #[test]
    fn a_fork_whose_job_was_cancelled_is_deleted_after_a_short_grace() {
        let p = pool("fork", "");
        assert!(p.register_timeout_s > ORPHAN_GRACE_S);
        let gone = |queued_at: u64| Machine {
            job_queued_at: Some(queued_at),
            ..forked("m1", 1, Ms::Up)
        };
        let quiet = |mut s: Snapshot, at: u64| {
            s.queued_at.insert("ci".into(), at);
            s
        };
        let long_ago = NOW - ORPHAN_GRACE_S;
        let just_now = NOW - ORPHAN_GRACE_S + 1;
        // No runner: the grace counts from the job, not from the machine coming up.
        let s = snap(vec![gone(long_ago)], vec![], &[]);
        assert_eq!(decide(&s, &p), [delete("m1")]);
        let s = snap(vec![gone(just_now)], vec![], &[]);
        assert_eq!(decide(&s, &p), NOTHING);

        // An Idle or Registered runner: the pool must have had no queued job for the grace.
        for state in [Rs::Registered, Rs::Idle] {
            let on = vec![runner("m1", 0, state)];
            let s = quiet(snap(vec![gone(long_ago)], on.clone(), &[]), long_ago);
            assert!(decide(&s, &p).contains(&delete("m1")), "{state:?}");
            let s = quiet(snap(vec![gone(long_ago)], on, &[]), just_now);
            assert!(!decide(&s, &p).contains(&delete("m1")), "{state:?}");
        }
        let busy = snap(vec![gone(long_ago)], vec![runner("m1", 0, Rs::Busy)], &[]);
        assert_eq!(decide(&busy, &p), NOTHING);

        // Job 2 has a fork no Busy runner holds, so job 1's has nothing to run.
        let s = snap(
            vec![gone(long_ago), forked("m2", 2, Ms::Starting)],
            vec![],
            &[2],
        );
        assert_eq!(decide(&s, &p), [delete("m1")]);
        // Job 2 has none (no budget for it), so job 1's fork takes it.
        let mut s = snap(vec![gone(long_ago)], vec![], &[2]);
        s.limits.can_start = false;
        assert_eq!(decide(&s, &p), [register("m1", 0)]);
    }

    /// m2, forked for job 2, runs job 1; m1, forked for job 1, is up with no runner yet
    /// and is job 2's only hope; m3's runner went offline after its job. m3 cannot run
    /// job 2, so it does not count as cover: m1 stays and takes job 2.
    #[test]
    fn a_finished_fork_does_not_cover_a_queued_job() {
        let p = pool("fork", "");
        let m1 = Machine {
            job_queued_at: Some(NOW - 600),
            ..forked("m1", 1, Ms::Up)
        };
        let m3 = Machine {
            job_queued_at: Some(NOW - 600),
            ..forked("m3", 3, Ms::Up)
        };
        let s = snap(
            vec![m1, forked("m2", 2, Ms::Up), m3],
            vec![runner("m2", 0, Rs::Busy), runner("m3", 0, Rs::Offline)],
            &[2],
        );
        let actions = decide(&s, &p);
        assert!(!actions.contains(&delete("m1")), "{actions:?}");
        assert!(actions.contains(&register("m1", 0)), "{actions:?}");
    }

    /// The reviewer's swap: m2, forked for job 2, came up first and its runner took job 1.
    /// m1's Idle runner is the only one left for job 2, so m1 stays, and so does a fork
    /// whose runner the listing shows Idle a moment after it took its job.
    #[test]
    fn a_fork_that_can_still_run_a_queued_job_stays() {
        let p = pool("fork", "");
        let m1 = Machine {
            job_queued_at: Some(NOW - 600),
            ..forked("m1", 1, Ms::Up)
        };
        let swapped = snap(
            vec![m1.clone(), forked("m2", 2, Ms::Up)],
            vec![runner("m1", 0, Rs::Idle), runner("m2", 0, Rs::Busy)],
            &[2],
        );
        assert!(!decide(&swapped, &p).contains(&delete("m1")));
        // Without a runner on m1 the same holds: m2 cannot take job 2.
        let swapped = snap(
            vec![m1, forked("m2", 2, Ms::Up)],
            vec![runner("m2", 0, Rs::Busy)],
            &[2],
        );
        assert!(!decide(&swapped, &p).contains(&delete("m1")));

        // Up 300 s, its job seen queued last tick; the runner just took it.
        let mut s = snap(
            vec![Machine {
                since: NOW - 300,
                job_queued_at: Some(NOW - 15),
                ..forked("m1", 1, Ms::Up)
            }],
            vec![runner("m1", 0, Rs::Idle)],
            &[],
        );
        s.queued_at.insert("ci".into(), NOW - 15);
        assert_eq!(decide(&s, &p), NOTHING);
    }

    #[test]
    fn hybrid_fills_a_slot_then_resumes_then_forks() {
        let p = pool("hybrid", "");
        let machines = vec![machine("m1", Ms::Up), machine("m2", Ms::Stopped)];
        let s = snap(machines, vec![], &[1, 2, 3]);
        assert_eq!(decide(&s, &p), [register("m1", 0), resume("m2"), fork(1)]);
    }

    #[test]
    fn hybrid_starts_only_what_the_budget_allows() {
        let machines = vec![machine("m1", Ms::Up), machine("m2", Ms::Stopped)];
        let mut s = snap(machines, vec![], &[1, 2, 3]);
        s.limits.next_start_cents = 5;
        assert_eq!(decide(&s, &pool("hybrid", "")), [register("m1", 0)]);

        let capped = pool("hybrid", "paid_start_cents_per_day = 10");
        assert_eq!(
            decide(&s, &capped),
            [register("m1", 0), resume("m2"), fork(1)]
        );

        s.limits.spent_today_cents = 1;
        assert_eq!(decide(&s, &capped), [register("m1", 0), resume("m2")]);
    }

    #[test]
    fn free_starts_stop_at_the_number_boat_has_left() {
        let machines = vec![machine("m1", Ms::Up), machine("m2", Ms::Stopped)];
        let mut s = snap(machines, vec![], &[1, 2, 3]);
        let hybrid = pool("hybrid", "");
        assert_eq!(
            decide(&s, &hybrid),
            [register("m1", 0), resume("m2"), fork(1)]
        );
        s.limits.free_starts_left = Some(1);
        assert_eq!(decide(&s, &hybrid), [register("m1", 0), resume("m2")]);
    }

    /// 1,000 machines, 4,000 runners and 2,000 queued jobs in one `hybrid` pool.
    fn large_tick() -> (Snapshot, Pool) {
        let mut p = pool("hybrid", "slots_per_box = 4");
        p.min_up = 600;
        p.max_boxes = 2000;
        let state = |i: usize| match i % 10 {
            0 => Ms::Stopped,
            1 => Ms::Starting,
            _ => Ms::Up,
        };
        let machines: Vec<Machine> = (0..1000)
            .map(|i| Machine {
                job: (i >= 700).then(|| key(i as u64)),
                ..machine(&format!("bx_{i:04}"), state(i))
            })
            .collect();
        let states = [Rs::Busy, Rs::Idle, Rs::Registered, Rs::Offline];
        let runners = (0..4000)
            .map(|i| {
                runner(
                    &format!("bx_{:04}", i % 1000),
                    (i / 1000) as u32,
                    states[i % 4],
                )
            })
            .collect();
        let jobs: Vec<u64> = (500..2500).collect();
        (snap(machines, runners, &jobs), p)
    }

    /// `cargo test --release -- --ignored --nocapture decide_timing` prints the time.
    #[test]
    #[ignore = "timing, not a check"]
    fn decide_timing() {
        let (s, p) = large_tick();
        let rounds = 20;
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(decide(std::hint::black_box(&s), &p));
        }
        println!("decide: {:?} per tick", start.elapsed() / rounds);
    }

    mod properties {
        use super::*;
        use proptest::collection::vec;
        use proptest::prelude::*;

        fn arb_pool() -> impl Strategy<Value = Pool> {
            use Placement::{Fork, Resume, Slot};
            let shape = prop_oneof![
                Just(vec![Slot]),
                Just(vec![Slot, Resume]),
                Just(vec![Fork]),
                Just(vec![Slot, Resume, Fork]),
            ];
            let times = (0u64..200, 0u64..200, 0u64..200, 1u64..400);
            let boxes = (1u32..5, 0u32..5, 1u32..3, 0u32..3);
            let warm = proptest::option::of(0u64..300);
            (shape, boxes, times, 0u32..30, warm).prop_map(
                |(placement, (max_boxes, min_up, slots, standby), times, cap, warm)| {
                    let has_slot = placement.contains(&Slot);
                    Pool {
                        name: "p".into(),
                        labels: vec!["a".into(), "b".into()],
                        machine_type: "large".into(),
                        snapshot: "s".into(),
                        // As policy requires: a kept machine needs slots to run jobs.
                        min_up: if placement.contains(&Slot) {
                            min_up.min(max_boxes)
                        } else {
                            0
                        },
                        placement,
                        max_boxes,
                        slots_per_box: slots,
                        standby: if has_slot { standby } else { 0 },
                        idle_stop_s: times.0,
                        min_up_s: times.1,
                        register_timeout_s: times.2,
                        fork_ttl_s: times.3,
                        paid_start_cents_per_day: cap,
                        organization_runner_group: None,
                        idle_warm: warm.filter(|_| has_slot).map(|interval_s| IdleWarm {
                            argv: vec!["w".into()],
                            cwd: "/".into(),
                            interval_s,
                            timeout_s: 1,
                        }),
                    }
                },
            )
        }

        fn arb_snapshot() -> impl Strategy<Value = Snapshot> {
            let machine_state = prop_oneof![
                Just(Ms::Starting),
                Just(Ms::Up),
                Just(Ms::Stopping),
                Just(Ms::Stopped),
                Just(Ms::Gone),
            ];
            let job = proptest::option::of((0u64..4, 0u32..2));
            let warm = (
                proptest::option::of(prop_oneof![Just(Warming::Running), Just(Warming::Unknown)]),
                proptest::option::of(0u64..=1200),
            );
            let machine = (
                any::<bool>(),
                machine_state,
                0u64..=1000,
                0u64..=1000,
                job,
                warm,
            );
            let runner_state = prop_oneof![
                Just(Rs::Registered),
                Just(Rs::Idle),
                Just(Rs::Busy),
                Just(Rs::Offline)
            ];
            let runner = (any::<bool>(), 0usize..8, 0u32..3, 0u64..=1200, runner_state);
            let labels: [&[&str]; 6] = [&[], &["a"], &["a", "b"], &["b"], &["c"], &["a", "c"]];
            let queued = (
                (0u64..4, 0u32..2),
                proptest::sample::select(labels.to_vec()),
            );
            let limits = (any::<bool>(), 0u32..10, 0u32..40);
            (
                vec(machine, 0..8),
                vec(runner, 0..8),
                vec(queued, 0..6),
                limits,
                1000u64..=1200,
            )
                .prop_map(|(machines, runners, queued, limits, now)| Snapshot {
                    now,
                    warming: machines
                        .iter()
                        .enumerate()
                        .filter_map(|(i, m)| {
                            Some((format!("m{i}"), m.5.0.filter(|_| m.4.is_none())?))
                        })
                        .collect(),
                    warmed_at: machines
                        .iter()
                        .enumerate()
                        .filter_map(|(i, m)| Some((format!("m{i}"), m.5.1?)))
                        .collect(),
                    machines: machines
                        .into_iter()
                        .enumerate()
                        .map(|(i, (own, state, since, last_busy, job, _))| Machine {
                            id: format!("m{i}"),
                            pool: pool_name(own),
                            state,
                            since,
                            last_busy,
                            job: job.map(|(id, attempt)| JobKey { id, attempt }),
                            job_queued_at: None,
                        })
                        .collect(),
                    runners: runners
                        .into_iter()
                        .enumerate()
                        .map(|(i, (own, machine, slot, registered_at, state))| Runner {
                            name: format!("r{i}"),
                            pool: pool_name(own),
                            machine: format!("m{machine}"),
                            slot,
                            registered_at,
                            state,
                        })
                        .collect(),
                    queued: queued
                        .into_iter()
                        .map(|((id, attempt), labels)| QueuedJob {
                            key: JobKey { id, attempt },
                            labels: labels.iter().map(|l| l.to_string()).collect(),
                        })
                        .collect(),
                    limits: Limits {
                        can_start: limits.0,
                        next_start_cents: limits.1,
                        spent_today_cents: limits.2,
                        free_starts_left: None,
                    },
                    queued_at: Default::default(),
                })
        }

        fn pool_name(own: bool) -> String {
            if own { "p" } else { "q" }.into()
        }

        fn pool_machine<'a>(snap: &'a Snapshot, pool: &Pool, id: &str) -> Option<&'a Machine> {
            snap.machines
                .iter()
                .find(|m| m.pool == pool.name && m.id == id)
        }

        fn starts(actions: &[Action]) -> usize {
            actions
                .iter()
                .filter(|a| matches!(a, Action::Resume { .. } | Action::Fork { .. }))
                .count()
        }

        proptest! {
            /// Spec `NoBusyStop`: no `Stop` or `Delete` of a machine with a `Busy` runner.
            #[test]
            fn no_busy_stop(snap in arb_snapshot(), pool in arb_pool()) {
                for a in decide(&snap, &pool) {
                    if let Action::Stop { machine } | Action::Delete { machine } = &a {
                        let busy = snap
                            .runners
                            .iter()
                            .any(|r| r.machine == *machine && r.state == Rs::Busy);
                        prop_assert!(!busy, "{a:?}");
                    }
                }
            }

            /// Spec `MaxBoxes`: starts never take the pool past `max_boxes`.
            #[test]
            fn max_boxes(snap in arb_snapshot(), pool in arb_pool()) {
                let started = starts(&decide(&snap, &pool));
                let boxes = snap
                    .machines
                    .iter()
                    .filter(|m| m.pool == pool.name && matches!(m.state, Ms::Up | Ms::Starting))
                    .count();
                prop_assert!(started == 0 || boxes + started <= pool.max_boxes as usize);
            }

            /// Spec `StartBudget`: starts stay within the quote and the daily cap.
            #[test]
            fn start_budget(snap in arb_snapshot(), pool in arb_pool()) {
                let started = starts(&decide(&snap, &pool)) as u64;
                let l = snap.limits;
                if started > 0 {
                    prop_assert!(l.can_start);
                    if l.next_start_cents > 0 {
                        let spent = u64::from(l.spent_today_cents)
                            + started * u64::from(l.next_start_cents);
                        prop_assert!(spent <= u64::from(pool.paid_start_cents_per_day));
                    }
                }
            }

            /// Spec `NoDoubleFork`, for job forks; a kept fork carries no job.
            #[test]
            fn no_double_fork(snap in arb_snapshot(), pool in arb_pool()) {
                let mut taken: BTreeSet<JobKey> = snap
                    .machines
                    .iter()
                    .filter(|m| m.state != Ms::Gone)
                    .filter_map(|m| m.job)
                    .collect();
                for a in decide(&snap, &pool) {
                    if let Action::Fork { job: Some(job), .. } = a {
                        prop_assert!(taken.insert(job), "{job:?} forked twice");
                    }
                }
            }

            /// Spec `Settles`, first half: with nothing queued, nothing is forked for a job,
            /// at most `standby` runners are registered, each on a kept machine, and a
            /// resume or kept fork only brings the pool's kept machines up to `min_up`.
            #[test]
            fn settles_with_nothing_queued(mut snap in arb_snapshot(), pool in arb_pool()) {
                snap.queued.clear();
                let mut held = snap
                    .machines
                    .iter()
                    .filter(|m| m.pool == pool.name && m.job.is_none())
                    .filter(|m| matches!(m.state, Ms::Up | Ms::Starting))
                    .count();
                let mut ahead = 0;
                for a in decide(&snap, &pool) {
                    prop_assert!(!matches!(a, Action::Fork { job: Some(_), .. }), "{a:?}");
                    if let Action::Register { machine, .. } = &a {
                        ahead += 1;
                        prop_assert!(ahead <= pool.standby, "{a:?}");
                        let on = snap.machines.iter().find(|m| m.id == *machine);
                        prop_assert!(on.is_some_and(|m| m.job.is_none()), "{a:?}");
                    }
                    if matches!(a, Action::Resume { .. } | Action::Fork { job: None, .. }) {
                        held += 1;
                        prop_assert!(held <= pool.min_up as usize, "{a:?}");
                    }
                }
            }

            /// `min_up` (spec `Short` in `ResumeGuard` and `ForkKeptGuard`): with starts allowed and free, a pool ends the tick with `min_up` kept
            /// machines up, starting, or (in a pool that can resume) stopping, as far as
            /// `max_boxes` leaves room.
            #[test]
            fn reaches_min_up(mut snap in arb_snapshot(), pool in arb_pool()) {
                snap.limits = Limits { can_start: true, next_start_cents: 0, spent_today_cents: 0, free_starts_left: None };
                let own = |m: &&Machine| m.pool == pool.name;
                let count = |states: &[Ms]| {
                    snap.machines
                        .iter()
                        .filter(own)
                        .filter(|m| m.job.is_none() && states.contains(&m.state))
                        .count()
                };
                let held = count(&[Ms::Up, Ms::Starting]);
                let boxes = snap
                    .machines
                    .iter()
                    .filter(own)
                    .filter(|m| matches!(m.state, Ms::Up | Ms::Starting))
                    .count();
                let brought = decide(&snap, &pool)
                    .iter()
                    .filter(|a| matches!(a, Action::Resume { .. } | Action::Fork { job: None, .. }))
                    .count();
                let stopping = if pool.placement.contains(&Placement::Resume) {
                    count(&[Ms::Stopping])
                } else {
                    0
                };
                let room = (pool.max_boxes as usize).saturating_sub(boxes);
                let reachable = (pool.min_up as usize).min(held + room);
                prop_assert!(held + brought + stopping >= reachable);
            }

            /// Spec `Settles`, second half: a settled pool, every action of the last tick
            /// applied, gets no more actions. Its standby runners are the one thing a
            /// settled pool may still lack, so they are registered, come online, and
            /// then nothing is left.
            #[test]
            fn settles_once_applied(pool in arb_pool(), up in 0u32..5, idle in 0u64..200) {
                // A warm recurs in a settled pool, so `Settles` leaves it out.
                let pool = Pool { idle_warm: None, ..pool };
                // Machines above min_up are settled only while they have been busy recently,
                // and a pool without slot keeps no kept machine at all.
                let up = if !pool.placement.contains(&Placement::Slot) {
                    0
                } else if pool.idle_stop_s == 0 {
                    pool.min_up
                } else {
                    up.clamp(pool.min_up, pool.max_boxes)
                };
                let machines = (0..up)
                    .map(|i| Machine {
                        id: format!("m{i}"),
                        pool: pool.name.clone(),
                        state: Ms::Up,
                        since: 0,
                        last_busy: 1000 - idle % pool.idle_stop_s.max(1),
                        job: None,
                        job_queued_at: None,
                    })
                    .collect();
                let mut snap = Snapshot {
                    now: 1000,
                    machines,
                    runners: vec![],
                    queued: vec![],
                    limits: Limits { can_start: true, next_start_cents: 0, spent_today_cents: 0, free_starts_left: None },
                    queued_at: Default::default(),
                    warming: Default::default(),
                    warmed_at: Default::default(),
                };
                let standby = decide(&snap, &pool);
                prop_assert!(standby.len() <= pool.standby as usize, "{standby:?}");
                for a in standby {
                    let Action::Register { machine, slot, .. } = a else {
                        return Err(TestCaseError::fail(format!("{a:?}")));
                    };
                    snap.runners.push(Runner {
                        name: format!("sbr-p-{machine}-{slot}-1000"),
                        pool: pool.name.clone(),
                        machine,
                        slot,
                        registered_at: 1000,
                        state: Rs::Idle,
                    });
                }
                let actions = decide(&snap, &pool);
                prop_assert!(actions.is_empty(), "{actions:?}");
            }

            /// `NoLeak` for kept machines, the design's last `Delete` clause, which the spec
            /// leaves out (spec/README.md): a pool without slot never registers a runner on
            /// one, so it deletes every kept machine that is up or starting and holds no
            /// busy or registered runner, and leaves the others.
            #[test]
            fn no_leak_of_kept_machines(
                snap in arb_snapshot(),
                pool in arb_pool(),
            ) {
                prop_assume!(!pool.placement.contains(&Placement::Slot));
                let actions = decide(&snap, &pool);
                let kept = snap.machines.iter().filter(|m| m.pool == pool.name && m.job.is_none());
                for m in kept.filter(|m| matches!(m.state, Ms::Up | Ms::Starting)) {
                    let held = snap.runners.iter().any(|r| {
                        r.machine == m.id && matches!(r.state, Rs::Busy | Rs::Registered)
                    });
                    let deleted = actions.contains(&Action::Delete { machine: m.id.clone() });
                    prop_assert_eq!(deleted, !held, "{:?}", m);
                }
            }

            /// Spec `UnregisterGuard`: an `Idle` or `Busy` runner on an `Up` machine stays.
            #[test]
            fn unregister_spares_healthy_runners(snap in arb_snapshot(), pool in arb_pool()) {
                for a in decide(&snap, &pool) {
                    if let Action::Unregister { runner } = &a {
                        let r = snap.runners.iter().find(|r| r.name == *runner).unwrap();
                        let on_up = pool_machine(&snap, &pool, &r.machine)
                            .is_some_and(|m| m.state == Ms::Up);
                        let healthy = on_up && matches!(r.state, Rs::Idle | Rs::Busy);
                        prop_assert!(r.pool == pool.name && !healthy, "{a:?}");
                    }
                }
            }

            /// Not in the spec, which models one pool: a pool acts only on its own.
            #[test]
            fn actions_stay_in_their_pool(snap in arb_snapshot(), pool in arb_pool()) {
                for a in decide(&snap, &pool) {
                    let ok = match &a {
                        Action::Register { pool: name, machine, .. } => {
                            *name == pool.name && pool_machine(&snap, &pool, machine).is_some()
                        }
                        Action::Fork { pool: name, .. } => *name == pool.name,
                        Action::Resume { machine }
                        | Action::Stop { machine }
                        | Action::Warm { machine }
                        | Action::CancelWarm { machine }
                        | Action::Delete { machine } => pool_machine(&snap, &pool, machine).is_some(),
                        Action::Unregister { runner } => snap
                            .runners
                            .iter()
                            .any(|r| r.name == *runner && r.pool == pool.name),
                    };
                    prop_assert!(ok, "{a:?}");
                }
            }

            /// Spec `EphemeralIsolation` and `ForkIsolation`: a `Register` fills only a
            /// free slot of an `Up` machine, and an ephemeral machine only once. A slot is
            /// free on a kept machine when every runner in it is unregistered this tick.
            #[test]
            fn ephemeral_isolation(snap in arb_snapshot(), pool in arb_pool()) {
                let actions = decide(&snap, &pool);
                let gone = |name: &str| {
                    actions
                        .iter()
                        .any(|a| matches!(a, Action::Unregister { runner } if runner == name))
                };
                let mut filled = BTreeSet::new();
                for a in &actions {
                    if let Action::Register { machine, slot, .. } = a {
                        let m = pool_machine(&snap, &pool, machine).unwrap();
                        let mut on = snap.runners.iter().filter(|r| r.machine == *machine);
                        prop_assert_eq!(m.state, Ms::Up);
                        prop_assert!(filled.insert((machine.clone(), *slot)), "{a:?}");
                        if m.job.is_some() {
                            prop_assert!(*slot == 0 && m.last_busy < m.since, "{a:?}");
                            prop_assert!(on.next().is_none(), "{a:?}");
                        } else {
                            prop_assert!(*slot < pool.slots_per_box, "{a:?}");
                            prop_assert!(on.all(|r| r.slot != *slot || gone(&r.name)), "{a:?}");
                        }
                    }
                }
            }

            /// Spec `NoLeak`: every ephemeral machine whose job finished (spec `Finished`)
            /// or that is past `fork_ttl_s` is deleted, unless a runner on it is `Busy`;
            /// no other ephemeral machine is.
            #[test]
            fn no_leak(snap in arb_snapshot(), pool in arb_pool()) {
                let actions = decide(&snap, &pool);
                let own = snap.machines.iter().filter(|m| m.pool == pool.name);
                for m in own.filter(|m| m.job.is_some() && m.state != Ms::Gone) {
                    let mut on = snap.runners.iter().filter(|r| r.machine == m.id);
                    let busy = on.clone().any(|r| r.state == Rs::Busy);
                    let age = snap.now.saturating_sub(m.since);
                    let finished = match m.state {
                        Ms::Stopped => true,
                        Ms::Up => {
                            on.all(|r| r.state == Rs::Offline)
                                && (m.last_busy >= m.since || age > pool.register_timeout_s)
                        }
                        _ => false,
                    };
                    let leaked = finished || age > pool.fork_ttl_s;
                    let deleted = actions.contains(&Action::Delete { machine: m.id.clone() });
                    if leaked {
                        prop_assert_eq!(deleted, !busy, "{:?}", m);
                    } else if deleted {
                        // Only an orphan: up, no Busy runner, its job not queued.
                        let queued = snap
                            .queued
                            .iter()
                            .any(|j| pool.serves(&j.labels) && Some(j.key) == m.job);
                        prop_assert!(m.state == Ms::Up && !busy && !queued, "{:?}", m);
                    }
                }
            }

            /// Spec `IdleStops`: with nothing queued, a pool with resume stops idle kept
            /// machines, those up for `min_up_s` and idle for `idle_stop_s` with no `Busy`
            /// or `Registered` runner, until `min_up` are left up.
            #[test]
            fn idle_stops(mut snap in arb_snapshot(), pool in arb_pool()) {
                prop_assume!(pool.placement.contains(&Placement::Resume));
                snap.queued.clear();
                let kept_up: Vec<&Machine> = snap
                    .machines
                    .iter()
                    .filter(|m| m.pool == pool.name && m.job.is_none() && m.state == Ms::Up)
                    .collect();
                let idle = kept_up.iter().filter(|m| {
                    let held = snap.runners.iter().any(|r| {
                        r.machine == m.id && matches!(r.state, Rs::Busy | Rs::Registered)
                    });
                    !held
                        && snap.now.saturating_sub(m.last_busy) >= pool.idle_stop_s
                        && snap.now.saturating_sub(m.since) >= pool.min_up_s
                });
                let above = kept_up.len().saturating_sub(pool.min_up as usize);
                let stops = decide(&snap, &pool)
                    .iter()
                    .filter(|a| matches!(a, Action::Stop { .. }))
                    .count();
                prop_assert_eq!(stops, idle.count().min(above));
            }

            /// Spec `NoStarvation`, for one tick: a pool with no machine, budget and room
            /// starts one for a queued job it serves when `fork` or `resume` allows it.
            #[test]
            fn no_starvation(mut snap in arb_snapshot(), pool in arb_pool()) {
                prop_assume!(pool.placement.contains(&Placement::Fork)
                    || pool.placement.contains(&Placement::Resume));
                snap.machines.clear();
                snap.limits = Limits { can_start: true, next_start_cents: 0, spent_today_cents: 0, free_starts_left: None };
                prop_assume!(snap.queued.iter().any(|j| pool.serves(&j.labels)));
                prop_assert!(starts(&decide(&snap, &pool)) > 0);
            }

            /// Not in the spec, which has no input order: the snapshot's order is ignored.
            #[test]
            fn input_order_does_not_matter(snap in arb_snapshot(), pool in arb_pool()) {
                let mut reversed = snap.clone();
                reversed.machines.reverse();
                reversed.runners.reverse();
                reversed.queued.reverse();
                prop_assert_eq!(decide(&snap, &pool), decide(&reversed, &pool));
            }

            /// Spec `NoJobDuringWarm`: a machine whose warm may run gets a `CancelWarm`
            /// before its first `Register`, and act skips the machine if that fails.
            #[test]
            fn no_job_during_warm(snap in arb_snapshot(), pool in arb_pool()) {
                let actions = decide(&snap, &pool);
                for (i, a) in actions.iter().enumerate() {
                    if let Action::Register { machine, .. } = a {
                        let cancel = Action::CancelWarm { machine: machine.clone() };
                        let warming = snap.warming.contains_key(machine);
                        prop_assert!(!warming || actions[..i].contains(&cancel), "{actions:?}");
                    }
                }
            }

            /// Spec `WarmRetainsCapacity` and `WarmingNeverIncreasesBoxes`, for one tick: a
            /// warm, running or not, or `idle_warm` itself, changes no other action; it only
            /// adds `CancelWarm` and `Warm`.
            #[test]
            fn warming_changes_no_other_action(snap in arb_snapshot(), pool in arb_pool()) {
                let mut actions = decide(&snap, &pool);
                actions.retain(|a| !matches!(a, Action::Warm { .. } | Action::CancelWarm { .. }));
                let mut cleared = snap.clone();
                cleared.warming.clear();
                let no_hook = Pool { idle_warm: None, ..pool.clone() };
                prop_assert_eq!(actions, decide(&cleared, &no_hook));
            }

            /// Spec `CancelGuard`: a `CancelWarm` goes, at most once, to a kept `Up`
            /// machine whose warm may run, that the tick neither stops nor deletes; a
            /// running warm only when a `Register` on the machine follows it.
            #[test]
            fn cancel_only_on_a_machine_that_stays_up(snap in arb_snapshot(), pool in arb_pool()) {
                let actions = decide(&snap, &pool);
                let mut cancelled = BTreeSet::new();
                for (i, a) in actions.iter().enumerate() {
                    let Action::CancelWarm { machine } = a else { continue };
                    prop_assert!(cancelled.insert(machine), "{actions:?}");
                    let m = pool_machine(&snap, &pool, machine).unwrap();
                    prop_assert!(!m.is_ephemeral() && m.state == Ms::Up, "{actions:?}");
                    let ends = actions.iter().any(|b| matches!(b,
                        Action::Stop { machine: n } | Action::Delete { machine: n } if n == machine));
                    prop_assert!(!ends, "{actions:?}");
                    let registers = actions[i..].iter().any(|b| matches!(b,
                        Action::Register { machine: n, .. } if n == machine));
                    match snap.warming.get(machine) {
                        Some(Warming::Running) => prop_assert!(registers, "{actions:?}"),
                        Some(Warming::Unknown) => {}
                        None => prop_assert!(false, "{actions:?}"),
                    }
                }
            }

            /// `Warm` (spec `WarmGuard`): at most one, while no machine of the pool that is
            /// up may still run one, on a kept `Up` machine with no runner and no other
            /// action, idle since its last job and warm for `interval_s`.
            #[test]
            fn warm_only_on_a_spare_machine(snap in arb_snapshot(), pool in arb_pool()) {
                let actions = decide(&snap, &pool);
                let warms: Vec<&String> = actions.iter().filter_map(|a| match a {
                    Action::Warm { machine } => Some(machine),
                    _ => None,
                }).collect();
                prop_assert!(warms.len() <= 1);
                let Some(&id) = warms.first() else { return Ok(()) };
                let m = pool_machine(&snap, &pool, id).unwrap();
                let interval = pool.idle_warm.as_ref().unwrap().interval_s;
                let warmed = snap.warmed_at.get(id).copied().unwrap_or(0);
                prop_assert!(!m.is_ephemeral() && m.state == Ms::Up);
                prop_assert!(snap.now >= m.last_busy.max(warmed) + interval);
                prop_assert!(!snap.runners.iter().any(|r| r.machine == *id));
                prop_assert!(!snap.machines.iter().any(|n| n.pool == pool.name
                    && n.state == Ms::Up && snap.warming.contains_key(&n.id)));
                let named = actions.iter().filter(|a| match a {
                    Action::Register { machine, .. } | Action::Stop { machine }
                    | Action::Delete { machine } | Action::CancelWarm { machine }
                    | Action::Warm { machine } => machine == id,
                    _ => false,
                });
                prop_assert_eq!(named.count(), 1, "{:?}", actions);
            }
        }
    }
}
