//! One tick's decision: a snapshot and a pool in, that pool's actions out. Pure and
//! deterministic, so the property tests and spec/BoatRunner.tla can check it directly.
//!
//! Each action has one guard, a `View` method named after its row in the table in
//! docs/design.md#state-machine (and its `*Guard` in the spec), in the table's order. The
//! table states each rule once. `decide` runs the guards in a fixed order because later
//! guards read earlier decisions, such as the demand that registers and starts leave.

use std::collections::BTreeSet;

use crate::model::{
    Action, JobKey, Machine, MachineState, Placement, Pool, Runner, RunnerState, Snapshot,
};
use MachineState::{Gone, Starting, Stopped, Stopping, Up};
use Placement::{Fork, Resume, Slot};
use RunnerState::{Busy, Idle, Offline, Registered};

/// The actions for `pool`, in this order: unregisters, deletes, registers, resumes, kept
/// forks, job forks, stops. Inputs are sorted first, so their order in `snap` does not
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
    for (m, slot) in v.register(&unregistered).take(want) {
        registered_on.insert(m.id.as_str());
        actions.push(Action::Register {
            pool: pool.name.clone(),
            machine: m.id.clone(),
            slot,
        });
        want -= 1;
    }

    // Without this, every tick until a machine is up would start more for the same jobs.
    want = want.saturating_sub(v.pending(&deleted));
    let slots = pool.slots_per_box as usize;
    let mut short = v.short();
    let mut starts = 0;
    for m in v.kept(Stopped) {
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

    if pool.has(Resume) && want == 0 {
        let mut up = v.kept(Up).count();
        for m in v.kept(Up) {
            if up <= pool.min_up as usize {
                break;
            }
            if v.stop(m, &registered_on) {
                actions.push(Action::Stop {
                    machine: m.id.clone(),
                });
                up -= 1;
            }
        }
    }
    actions
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
        View {
            boxes: boxes.count(),
            snap,
            pool,
            machines,
            runners,
            by_machine,
            jobs,
        }
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
            m.state != Gone && !holds(on, &[Busy]) && (self.finished(m) || expired)
        } else {
            match m.state {
                Stopped => !self.pool.has(Resume) && !holds(on, &[Busy]),
                Up | Starting => !self.pool.has(Slot) && !holds(on, &[Busy, Registered]),
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

    /// Jobs the pool's `Starting` machines not deleted this tick take once up.
    fn pending(&self, deleted: &BTreeSet<&str>) -> usize {
        let starting = self.machines.iter().filter(|m| m.state == Starting);
        let left = starting.filter(|m| !deleted.contains(m.id.as_str()));
        let slots = self.pool.slots_per_box as usize;
        left.map(|m| if m.is_ephemeral() { 1 } else { slots }).sum()
    }

    /// Kept machines short of `min_up`, which counts kept machines up or starting.
    fn short(&self) -> usize {
        let held = self.kept(Up).count() + self.kept(Starting).count();
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
    use crate::model::{Limits, MachineState as Ms, QueuedJob, RunnerState as Rs};

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

    #[test]
    fn pool_forks_kept_machines_up_to_min_up() {
        let p = pool("pool", "min_up = 2");
        assert_eq!(
            decide(&snap(vec![], vec![], &[]), &p),
            [fork_kept(), fork_kept()]
        );

        // A starting kept machine counts; an ephemeral one does not.
        let s = snap(
            vec![machine("m1", Ms::Starting), forked("f1", 1, Ms::Up)],
            vec![],
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

    /// A pool switched to `fork` keeps kept machines that no step would use again.
    #[test]
    fn a_pool_without_slot_deletes_its_unused_kept_machines() {
        let p = pool("fork", "");
        let s = snap(
            vec![
                machine("m1", Ms::Up),
                machine("m2", Ms::Starting),
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

        let mut starting = stopped;
        starting[0].state = Ms::Starting;
        let s = snap(starting, vec![], &[]);
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
            (shape, 1u32..5, 0u32..5, 1u32..3, times, 0u32..30).prop_map(
                |(placement, max_boxes, min_up, slots, times, cap)| Pool {
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
                    idle_stop_s: times.0,
                    min_up_s: times.1,
                    register_timeout_s: times.2,
                    fork_ttl_s: times.3,
                    paid_start_cents_per_day: cap,
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
            let machine = (any::<bool>(), machine_state, 0u64..=1000, 0u64..=1000, job);
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
                    machines: machines
                        .into_iter()
                        .enumerate()
                        .map(|(i, (own, state, since, last_busy, job))| Machine {
                            id: format!("m{i}"),
                            pool: pool_name(own),
                            state,
                            since,
                            last_busy,
                            job: job.map(|(id, attempt)| JobKey { id, attempt }),
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

            /// Spec `Settles`, first half: with nothing queued, nothing is registered or
            /// forked for a job, and a resume or kept fork only brings the pool's kept
            /// machines up to `min_up`.
            #[test]
            fn settles_with_nothing_queued(mut snap in arb_snapshot(), pool in arb_pool()) {
                snap.queued.clear();
                let mut held = snap
                    .machines
                    .iter()
                    .filter(|m| m.pool == pool.name && m.job.is_none())
                    .filter(|m| matches!(m.state, Ms::Up | Ms::Starting))
                    .count();
                for a in decide(&snap, &pool) {
                    let starts_work =
                        matches!(a, Action::Register { .. } | Action::Fork { job: Some(_), .. });
                    prop_assert!(!starts_work, "{a:?}");
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
            /// applied, gets no more actions.
            #[test]
            fn settles_once_applied(pool in arb_pool(), up in 0u32..5, idle in 0u64..200) {
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
                    })
                    .collect();
                let snap = Snapshot {
                    now: 1000,
                    machines,
                    runners: vec![],
                    queued: vec![],
                    limits: Limits { can_start: true, next_start_cents: 0, spent_today_cents: 0, free_starts_left: None },
                };
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
                    prop_assert_eq!(deleted, leaked && !busy, "{:?}", m);
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
        }
    }
}
