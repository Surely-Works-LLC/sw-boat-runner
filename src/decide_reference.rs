//! A frozen copy of `decide` from before the guard-per-action rewrite, for the
//! differential property test in `decide::tests`. Deleted once the rewrite is green.

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
    machines.sort_by(|a, b| a.id.cmp(&b.id));
    let mut runners: Vec<&Runner> = snap
        .runners
        .iter()
        .filter(|r| r.pool == pool.name)
        .collect();
    runners.sort_by(|a, b| a.name.cmp(&b.name));
    let mut jobs: Vec<JobKey> = snap
        .queued
        .iter()
        .filter(|j| pool.serves(&j.labels))
        .map(|j| j.key)
        .collect();
    jobs.sort();
    jobs.dedup();
    let has = |step: Placement| pool.placement.contains(&step);
    let kept_in = |state: MachineState| {
        machines
            .iter()
            .filter(move |m| m.job.is_none() && m.state == state)
    };
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
        .filter(|m| m.job.is_some() && m.state != MachineState::Gone)
    {
        let on: Vec<&Runner> = runners_on(snap, &m.id).collect();
        let busy = on.iter().any(|r| r.state == RunnerState::Busy);
        let live = on.iter().any(|r| r.state != RunnerState::Offline);
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
        } else if m.state == MachineState::Up && !seen && on.is_empty() {
            fresh.push(*m);
        }
    }
    // Only a pool that can resume ever brings a stopped kept machine back.
    if !has(Placement::Resume) {
        for m in kept_in(MachineState::Stopped) {
            if runners_on(snap, &m.id).any(|r| r.state == RunnerState::Busy) {
                continue;
            }
            deleted.insert(m.id.as_str());
            actions.push(Action::Delete {
                machine: m.id.clone(),
            });
        }
    }
    // Only `slot` registers runners on kept machines, so in a pool without it (one whose
    // placement changed) a kept machine up or starting would hold a box until boat's TTL.
    // A stopping one becomes stopped, which the rule above deletes; policy ties `resume`
    // and `min_up` to `slot`, so nothing here resumes or keeps one.
    if !has(Placement::Slot) {
        let orphans = kept_in(MachineState::Up).chain(kept_in(MachineState::Starting));
        for m in orphans {
            if runners_on(snap, &m.id)
                .any(|r| matches!(r.state, RunnerState::Busy | RunnerState::Registered))
            {
                continue;
            }
            deleted.insert(m.id.as_str());
            actions.push(Action::Delete {
                machine: m.id.clone(),
            });
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
        for m in kept_in(MachineState::Up) {
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
        .filter(|m| m.state == MachineState::Starting && !deleted.contains(m.id.as_str()))
        .map(|m| match m.job {
            Some(_) => 1,
            None => pool.slots_per_box as usize,
        })
        .sum();
    want = want.saturating_sub(pending);

    let mut starts = StartBudget {
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
    let held = kept_in(MachineState::Up).count() + kept_in(MachineState::Starting).count();
    let mut short = (pool.min_up as usize).saturating_sub(held);
    if has(Placement::Resume) {
        // boat may have dropped the kept disk, so every resume is priced as a start.
        for m in kept_in(MachineState::Stopped) {
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
        short = short.saturating_sub(kept_in(MachineState::Stopping).count());
    }
    // A pool that resumes but never forks for a job would otherwise stay at its stopped
    // and `min_up` machines. Demand left after every resume forks a kept machine, which
    // later stops and resumes with its caches. A failed `take` above fails here too.
    let grows = has(Placement::Resume) && !has(Placement::Fork);
    while (short > 0 || (grows && want > 0)) && starts.take() {
        actions.push(Action::Fork {
            pool: pool.name.clone(),
            job: None,
        });
        short = short.saturating_sub(1);
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
        let mut up = kept_in(MachineState::Up).count();
        for m in kept_in(MachineState::Up) {
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
struct StartBudget {
    limits: Limits,
    cap: Cents,
    boxes: usize,
    max_boxes: usize,
    taken: u64,
}

impl StartBudget {
    fn take(&mut self) -> bool {
        // Every start this tick is priced at `next_start_cents`, the one quote the snapshot has.
        let price = u64::from(self.limits.next_start_cents);
        let spent = u64::from(self.limits.spent_today_cents) + (self.taken + 1) * price;
        let affordable = price == 0 || spent <= u64::from(self.cap);
        // Past the free starts the price is not known; the next tick gets a new quote.
        let quoted = price != 0
            || self
                .limits
                .free_starts_left
                .is_none_or(|left| self.taken < u64::from(left));
        if self.boxes >= self.max_boxes || !self.limits.can_start || !affordable || !quoted {
            return false;
        }
        self.boxes += 1;
        self.taken += 1;
        true
    }
}
