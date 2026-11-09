//! One tick's decision: a snapshot and a pool in, that pool's actions out. Pure and
//! deterministic, so the property tests and spec/BoatRunner.tla can check it directly.
//! Guards follow docs/design.md#state-machine; where the design leaves a rule open, the
//! rule is written next to the code that applies it.

use std::collections::BTreeSet;

use crate::model::{
    Action, Cents, JobKey, Limits, Machine, MachineState, Placement, Pool, Runner, RunnerState,
    Snapshot,
};

/// The actions for `pool`, in this order: runner cleanup, deletes, registers, resumes,
/// forks, stops. Inputs are sorted by id first, so their order in `snap` does not matter.
/// `snap.queued` holds only the jobs routed to this pool (see `run::route`). Jobs or not,
/// a pool keeps `min_up` kept machines up or starting: it resumes stopped ones if it can,
/// and forks kept machines for the rest.
///
/// Ephemeral machines are the ones with `job: Some(_)`, made by a job `Fork`. One gets exactly one
/// runner in its life, so "fresh" and "finished" are decided from `last_busy` (the last time
/// a runner on it was `Busy` or `Registered`):
/// - fresh: `Up`, no runner at all, and `last_busy < since`, so no runner was ever seen on
///   it since it came up. Only a fresh machine gets a `Register`.
/// - finished: `Stopped`, or `Up` with no runner other than `Offline` ones, and either a
///   runner was seen (`last_busy >= since`) or it has waited longer than
///   `register_timeout_s` for one.
/// - expired: in its current state longer than `fork_ttl_s`.
///
/// A finished or expired machine is deleted, but never while a runner on it is `Busy`.
pub fn decide(snap: &Snapshot, pool: &Pool) -> Vec<Action> {
    let now = snap.now;
    let mut machines: Vec<&Machine> = snap
        .machines
        .iter()
        .filter(|m| m.pool == pool.name)
        .collect();
    machines.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let mut runners: Vec<&Runner> = snap
        .runners
        .iter()
        .filter(|r| r.pool == pool.name)
        .collect();
    runners.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    let mut jobs: Vec<JobKey> = snap
        .queued
        .iter()
        .filter(|j| pool.serves(&j.labels))
        .map(|j| j.key)
        .collect();
    jobs.sort();
    jobs.dedup();
    let has = |step: Placement| pool.placement.contains(&step);
    let ephemeral = |m: &&&Machine| m.job.is_some();
    let kept = |m: &&&Machine| m.job.is_none();
    let in_state = |state: MachineState| move |m: &&&Machine| m.state == state;
    let mut actions = Vec::new();

    let mut unregistered = BTreeSet::new();
    for r in &runners {
        let on_up = machines
            .iter()
            .any(|m| m.id == r.machine && m.state == MachineState::Up);
        let timed_out = r.state == RunnerState::Registered
            && now.saturating_sub(r.registered_at) > pool.register_timeout_s;
        // Every runner is JIT: once offline it never comes back, and it holds its slot.
        if !on_up || timed_out || r.state == RunnerState::Offline {
            unregistered.insert(r.name.as_str());
            actions.push(Action::Unregister {
                runner: r.name.clone(),
            });
        }
    }

    let mut deleted = BTreeSet::new();
    let mut fresh = Vec::new();
    for m in machines
        .iter()
        .filter(ephemeral)
        .filter(|m| m.state != MachineState::Gone)
    {
        let busy = runners_on(snap, &m.id).any(|r| r.state == RunnerState::Busy);
        let live = runners_on(snap, &m.id).any(|r| r.state != RunnerState::Offline);
        let seen = m.last_busy >= m.since;
        let age = now.saturating_sub(m.since);
        let finished = match m.state {
            MachineState::Up => !live && (seen || age > pool.register_timeout_s),
            MachineState::Stopped => true,
            _ => false,
        };
        if !busy && (finished || age > pool.fork_ttl_s) {
            deleted.insert(m.id.as_str());
            actions.push(Action::Delete {
                machine: m.id.clone(),
            });
        } else if m.state == MachineState::Up && !seen && runners_on(snap, &m.id).next().is_none() {
            fresh.push(*m);
        }
    }

    let supply = runners
        .iter()
        .filter(|r| !unregistered.contains(r.name.as_str()))
        .filter(|r| matches!(r.state, RunnerState::Registered | RunnerState::Idle))
        .count();
    let mut want = jobs.len().saturating_sub(supply);

    // A fresh forked machine is already paid for, so it is filled before any free slot.
    let mut registered_on = BTreeSet::new();
    for m in fresh {
        if want == 0 {
            break;
        }
        actions.push(Action::Register {
            pool: pool.name.clone(),
            machine: m.id.clone(),
            slot: 0,
        });
        registered_on.insert(m.id.as_str());
        want -= 1;
    }
    if has(Placement::Slot) {
        for m in machines
            .iter()
            .filter(kept)
            .filter(in_state(MachineState::Up))
        {
            for slot in 0..pool.slots_per_box {
                if want == 0 {
                    break;
                }
                // A slot freed by an Unregister above takes a fresh runner in the same tick.
                if runners_on(snap, &m.id)
                    .any(|r| r.slot == slot && !unregistered.contains(r.name.as_str()))
                {
                    continue;
                }
                actions.push(Action::Register {
                    pool: pool.name.clone(),
                    machine: m.id.clone(),
                    slot,
                });
                registered_on.insert(m.id.as_str());
                want -= 1;
            }
        }
    }

    // Machines already starting will take jobs once up; without this every tick until then
    // would start more machines for the same demand.
    let pending: usize = machines
        .iter()
        .filter(in_state(MachineState::Starting))
        .filter(|m| !deleted.contains(m.id.as_str()))
        .map(|m| match m.job {
            Some(_) => 1,
            None => pool.slots_per_box as usize,
        })
        .sum();
    want = want.saturating_sub(pending);

    let mut starts = Starts {
        limits: snap.limits,
        cap: pool.paid_start_cents_per_day,
        boxes: machines
            .iter()
            .filter(|m| matches!(m.state, MachineState::Up | MachineState::Starting))
            .count(),
        max_boxes: pool.max_boxes as usize,
        taken: 0,
    };
    // `min_up` counts kept machines that are up or on their way up.
    let held = machines
        .iter()
        .filter(kept)
        .filter(|m| matches!(m.state, MachineState::Up | MachineState::Starting))
        .count();
    let mut short = (pool.min_up as usize).saturating_sub(held);
    if has(Placement::Resume) {
        let stopped = machines
            .iter()
            .filter(kept)
            .filter(in_state(MachineState::Stopped));
        // boat may have dropped the kept disk, so every resume is priced as a start.
        for m in stopped {
            if (want == 0 && short == 0) || !starts.take() {
                break;
            }
            actions.push(Action::Resume {
                machine: m.id.clone(),
            });
            short = short.saturating_sub(1);
            want = want.saturating_sub(pool.slots_per_box as usize);
        }
        // A kept machine still stopping is resumed once it has stopped; a fork now would
        // leave the pool above `min_up` after that.
        let stopping = machines
            .iter()
            .filter(kept)
            .filter(in_state(MachineState::Stopping))
            .count();
        short = short.saturating_sub(stopping);
    }
    while short > 0 && starts.take() {
        actions.push(Action::Fork {
            pool: pool.name.clone(),
            job: None,
        });
        short -= 1;
        want = want.saturating_sub(pool.slots_per_box as usize);
    }
    if has(Placement::Fork) {
        // Every pool and state but Gone: a job forked by another pool must not be forked
        // again here, while a job whose machine is gone may need a new one.
        let forked: BTreeSet<JobKey> = snap
            .machines
            .iter()
            .filter(|m| m.state != MachineState::Gone)
            .filter_map(|m| m.job)
            .collect();
        for &job in &jobs {
            if want == 0 {
                break;
            }
            if forked.contains(&job) {
                continue;
            }
            if !starts.take() {
                break;
            }
            actions.push(Action::Fork {
                pool: pool.name.clone(),
                job: Some(job),
            });
            want -= 1;
        }
    }

    // Only pools that can resume stop machines: a `pool` machine that stopped would never
    // come back (docs/design.md#properties, IdleStops). A machine is not
    // stopped while demand is still unmet, since a start just refused would want it back.
    if has(Placement::Resume) && want == 0 {
        let mut up = machines
            .iter()
            .filter(kept)
            .filter(in_state(MachineState::Up))
            .count();
        for m in machines
            .iter()
            .filter(kept)
            .filter(in_state(MachineState::Up))
        {
            if up <= pool.min_up as usize {
                break;
            }
            let held = runners_on(snap, &m.id)
                .any(|r| matches!(r.state, RunnerState::Busy | RunnerState::Registered));
            if held
                || now.saturating_sub(m.last_busy) < pool.idle_stop_s
                || now.saturating_sub(m.since) < pool.min_up_s
                || registered_on.contains(m.id.as_str())
            {
                continue;
            }
            actions.push(Action::Stop {
                machine: m.id.clone(),
            });
            up -= 1;
        }
    }
    actions
}

/// Every runner on the machine, whatever its pool, so a stray runner still blocks a stop.
fn runners_on<'a>(snap: &'a Snapshot, machine: &'a str) -> impl Iterator<Item = &'a Runner> {
    snap.runners.iter().filter(move |r| r.machine == machine)
}

/// docs/design.md#start-budget and `max_boxes`, counting the starts already taken this tick.
struct Starts {
    limits: Limits,
    cap: Cents,
    boxes: usize,
    max_boxes: usize,
    taken: u64,
}

impl Starts {
    fn take(&mut self) -> bool {
        // Every start this tick is priced at `next_start_cents`; boat reports no more.
        let price = u64::from(self.limits.next_start_cents);
        let spent = u64::from(self.limits.spent_today_cents) + (self.taken + 1) * price;
        let affordable = price == 0 || spent <= u64::from(self.cap);
        if self.boxes >= self.max_boxes || !self.limits.can_start || !affordable {
            return false;
        }
        self.boxes += 1;
        self.taken += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MachineState as Ms, QueuedJob, RunnerState as Rs};

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

    /// Up for two hours and idle all that time.
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
        assert_eq!(decide(&s, &p), [fork_kept()]);
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
        assert_eq!(
            decide(&snap(vec![used], vec![], &[]), &p),
            [Action::Delete {
                machine: "m1".into()
            }]
        );
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

            /// NoDoubleFork, for job forks; a kept fork carries no job.
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

            /// Settles: with nothing queued, nothing is registered or forked for a job, and
            /// a resume or kept fork only brings the pool's kept machines up to `min_up`.
            #[test]
            fn no_queue_no_starts(mut snap in arb_snapshot(), pool in arb_pool()) {
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

            /// MinUp: with starts allowed and free, a pool ends the tick with `min_up` kept
            /// machines up, starting, or (in a pool that can resume) stopping, as far as
            /// `max_boxes` leaves room.
            #[test]
            fn reaches_min_up(mut snap in arb_snapshot(), pool in arb_pool()) {
                snap.limits = Limits { can_start: true, next_start_cents: 0, spent_today_cents: 0 };
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

            /// A settled pool, every action of the last tick applied, gets no more actions.
            #[test]
            fn applied_tick_settles(pool in arb_pool(), up in 0u32..5, idle in 0u64..200) {
                // Machines above min_up are settled only while they have been busy recently.
                let up = if pool.idle_stop_s == 0 {
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
                    limits: Limits { can_start: true, next_start_cents: 0, spent_today_cents: 0 },
                };
                let actions = decide(&snap, &pool);
                prop_assert!(actions.is_empty(), "{actions:?}");
            }

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

            /// Register only fills a free slot on an `Up` machine, and an ephemeral machine
            /// only once (EphemeralIsolation). A slot is free on a kept machine when every
            /// runner in it is unregistered in the same tick.
            #[test]
            fn register_fills_free_slots(snap in arb_snapshot(), pool in arb_pool()) {
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
