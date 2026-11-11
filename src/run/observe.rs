//! Reads boat and GitHub into the `Snapshot` that `decide` sees, adding what this process
//! remembers between ticks (docs/design.md#observed-state).

use std::collections::{HashMap, HashSet};

use super::{Error, HTTP_TIMEOUT, Loop};
use crate::boat::{self, Sandbox};
use crate::github::{self, GhRunner};
use crate::model::{
    Config, JobKey, Limits, Machine, MachineState, Runner, RunnerState, Snapshot, UnixSecs,
};

/// A snapshot, and the GitHub id of each of its runners, which deleting one needs.
pub(super) struct Observed {
    pub snapshot: Snapshot,
    pub runner_ids: HashMap<String, u64>,
}

/// What boat and GitHub do not report, kept from tick to tick. Only this module writes it.
#[derive(Default)]
pub(super) struct Memory {
    /// Each listed machine's state, when it entered it, and when it last had a live runner.
    machines: HashMap<String, Seen>,
    /// Runner names this process has seen online; see `github::runner_state`.
    online: HashSet<String>,
    /// Starts boat accepted that its listing may not show yet.
    inflight: Vec<Inflight>,
    /// Whether an observe has succeeded since this process started.
    warm: bool,
}

struct Seen {
    state: MachineState,
    since: UnixSecs,
    last_busy: UnixSecs,
    /// A kept machine found holding its creator's logins in its current state. No runner
    /// goes on it until it starts again.
    holds_logins: bool,
}

/// A start boat accepted but may not list for a while (`FixListLag`). Until it is listed,
/// or the HTTP request timeout plus a tick has passed, `decide` sees the machine as
/// `Starting`, so boat's lag can neither start it twice nor pass `max_boxes`.
struct Inflight {
    id: String,
    sent: UnixSecs,
    kind: Kind,
}

enum Kind {
    /// A fork, until the listing has it.
    Fork { pool: String, job: Option<JobKey> },
    /// A resume, while the listing still shows the machine stopped. A resume boat accepted
    /// holds its box from then on; resumed again, it would cost a second start.
    Resume,
}

impl Memory {
    pub(super) fn forked(&mut self, id: String, pool: &str, job: Option<JobKey>, now: UnixSecs) {
        let kind = Kind::Fork {
            pool: pool.to_string(),
            job,
        };
        self.inflight.push(Inflight {
            id,
            sent: now,
            kind,
        });
    }

    pub(super) fn resumed(&mut self, id: &str, now: UnixSecs) {
        let (id, kind) = (id.to_string(), Kind::Resume);
        self.inflight.push(Inflight {
            id,
            sent: now,
            kind,
        });
    }

    /// Counts `machine` as used from `now` (`FixRegSeen`): its runner may run its job and
    /// vanish before the next observe, and a fork must never get a second runner.
    pub(super) fn mark_used(&mut self, machine: &str, now: UnixSecs) {
        if let Some(seen) = self.machines.get_mut(machine) {
            seen.last_busy = now;
        }
    }

    /// Whether `machine` was found holding its creator's logins since it last changed state.
    pub(super) fn holds_logins(&self, machine: &str) -> bool {
        self.machines
            .get(machine)
            .is_some_and(|seen| seen.holds_logins)
    }

    /// Notes that `machine` holds its creator's logins, until it changes state.
    pub(super) fn note_holds_logins(&mut self, machine: &str) {
        if let Some(seen) = self.machines.get_mut(machine) {
            seen.holds_logins = true;
        }
    }

    /// GitHub's runners of the policy's pools, with the state only this process can tell.
    fn runners(
        &mut self,
        cfg: &Config,
        listed: Vec<GhRunner>,
    ) -> (Vec<Runner>, HashMap<String, u64>) {
        let mut ids = HashMap::new();
        let mut runners = Vec::new();
        for r in listed {
            let Some(n) = github::parse_runner_name(&r.name) else {
                continue;
            };
            if !cfg.pools.iter().any(|p| p.name == n.pool) {
                continue;
            }
            let online = r.status == "online";
            if online && !self.online.contains(&r.name) {
                self.online.insert(r.name.clone());
            }
            let state = github::runner_state(online, r.busy, self.online.contains(&r.name));
            ids.insert(r.name.clone(), r.id);
            runners.push(Runner {
                name: r.name,
                pool: n.pool,
                machine: n.machine,
                slot: n.slot,
                registered_at: n.registered_at,
                state,
            });
        }
        self.online.retain(|name| ids.contains_key(name));
        (runners, ids)
    }

    /// boat's listing, by pool, with when each machine changed state and was last busy,
    /// and with the starts it does not show yet.
    fn machines(
        &mut self,
        now: UnixSecs,
        lag_s: u64,
        listed: Vec<(String, Sandbox)>,
        runners: &[Runner],
    ) -> Vec<Machine> {
        let live: HashSet<&str> = runners
            .iter()
            .filter(|r| matches!(r.state, RunnerState::Busy | RunnerState::Registered))
            .map(|r| r.machine.as_str())
            .collect();
        let mut machines: Vec<Machine> = listed
            .into_iter()
            .map(|(pool, sb)| {
                let state = boat::machine_state(&sb.state);
                let job = sb.job();
                // After a restart nothing records whether an Up fork already had a runner,
                // so it counts as used (`FixCrashSeen`): deleted, never registered again. A
                // fork that never had one wastes its start. Otherwise, just before first
                // sight, so it has not.
                let used = !self.warm && job.is_some() && state == MachineState::Up;
                let seen = self.machines.entry(sb.id.clone()).or_insert(Seen {
                    state,
                    since: now,
                    last_busy: if used { now } else { now.saturating_sub(1) },
                    holds_logins: false,
                });
                if seen.state != state {
                    seen.state = state;
                    seen.since = now;
                    seen.holds_logins = false;
                }
                if live.contains(sb.id.as_str()) {
                    seen.last_busy = now;
                }
                Machine {
                    since: seen.since,
                    last_busy: seen.last_busy,
                    id: sb.id,
                    pool,
                    state,
                    job,
                }
            })
            .collect();
        self.warm = true;

        let listing: HashMap<&str, MachineState> =
            machines.iter().map(|m| (m.id.as_str(), m.state)).collect();
        self.machines
            .retain(|id, _| listing.contains_key(id.as_str()));
        self.inflight.retain(|s| {
            let state = listing.get(s.id.as_str());
            let unlisted = match s.kind {
                Kind::Fork { .. } => state.is_none(),
                Kind::Resume => state == Some(&MachineState::Stopped),
            };
            // A start that answered has had its full request timeout and a tick to show up.
            let expired = now.saturating_sub(s.sent) > lag_s;
            if unlisted && expired {
                match &s.kind {
                    Kind::Fork { pool, .. } => {
                        eprintln!(
                            "warn: fork {} in pool {pool} was never listed; dropped",
                            s.id
                        )
                    }
                    Kind::Resume => eprintln!("warn: resume of {} was never listed; dropped", s.id),
                }
            }
            unlisted && !expired
        });

        for s in &self.inflight {
            match &s.kind {
                Kind::Resume => {
                    if let Some(m) = machines.iter_mut().find(|m| m.id == s.id) {
                        m.state = MachineState::Starting;
                    }
                }
                Kind::Fork { pool, job } => machines.push(Machine {
                    id: s.id.clone(),
                    pool: pool.clone(),
                    state: MachineState::Starting,
                    since: s.sent,
                    last_busy: s.sent.saturating_sub(1),
                    job: *job,
                }),
            }
        }
        machines
    }
}

impl Loop<'_> {
    /// Starts GitHub's tick at `now` and observes, whatever the request budget says.
    pub(super) fn observe(&mut self, now: UnixSecs) -> Result<Observed, Error> {
        self.gh.begin_tick(now);
        self.read(now)
    }

    /// Reads every list once (boat's per pool), then updates `Memory`; a failed read
    /// leaves `Memory` as it was. GitHub's tick must have begun at `now`.
    pub(super) fn read(&mut self, now: UnixSecs) -> Result<Observed, Error> {
        let mut listed = Vec::new();
        for pool in &self.cfg.pools {
            let sandboxes = self.boat.list(&pool.name)?;
            listed.extend(sandboxes.into_iter().map(|sb| (pool.name.clone(), sb)));
        }
        let gh_runners = self.gh.runners()?;
        let queued = if self.read_jobs {
            let pools = &self.cfg.pools;
            self.gh
                .queued_jobs(&|labels| pools.iter().any(|p| p.serves(labels)))?
        } else {
            Vec::new()
        };
        let quote = self.boat.limits()?;

        let (runners, runner_ids) = self.memory.runners(self.cfg, gh_runners);
        let lag_s = HTTP_TIMEOUT.as_secs().saturating_add(self.cfg.tick_s);
        let machines = self.memory.machines(now, lag_s, listed, &runners);
        let limits = Limits {
            can_start: quote.allows && self.ledger.starts_open(now),
            next_start_cents: quote.cents,
            spent_today_cents: self.ledger.spent_cents(),
            free_starts_left: quote.free_left,
        };
        let snapshot = Snapshot {
            now,
            machines,
            runners,
            queued,
            limits,
        };
        Ok(Observed {
            snapshot,
            runner_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::decide::decide;
    use crate::model::Placement;

    /// The runner ran job 70 and went away between two ticks, and GitHub requeued the job.
    #[test]
    fn a_fork_that_got_a_runner_never_gets_another() {
        let cfg = config();
        let fake = one_fork(&["provisioning", "idle"]);
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.tick(NOW - 15, decide).unwrap();
        tick_loop.tick(NOW, decide).unwrap();
        assert_eq!(registers(&fake), 1);
        tick_loop.tick(NOW + 15, decide).unwrap();
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
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.tick(NOW, decide).unwrap();
        assert_eq!(registers(&fake), 0);
        let reqs = requests(&fake);
        assert!(
            reqs.contains(&format!("DELETE {BOAT}/sandboxes/bx_66666666")),
            "{reqs:?}"
        );
    }

    /// Machines `observe` hands `decide` at `now`.
    fn observed(tick_loop: &mut Loop, now: UnixSecs) -> Vec<(String, MachineState)> {
        let snapshot = tick_loop.observe(now).unwrap().snapshot;
        snapshot
            .machines
            .into_iter()
            .map(|m| (m.id, m.state))
            .collect()
    }

    /// When the forks `Memory` waits for were sent.
    fn pending_forks(tick_loop: &Loop) -> Vec<UnixSecs> {
        let inflight = &tick_loop.memory.inflight;
        let forks = inflight
            .iter()
            .filter(|s| matches!(s.kind, Kind::Fork { .. }));
        forks.map(|s| s.sent).collect()
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
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.tick(NOW, decide).unwrap();
        assert_eq!(forks(&fake).len(), 1);
        tick_loop.tick(NOW + 15, decide).unwrap();
        assert_eq!(forks(&fake).len(), 1);
        tick_loop.tick(NOW + 30, decide).unwrap();
        let forks = forks(&fake);
        assert_eq!(forks.len(), 2);
        assert!(
            forks[1]
                .body_text()
                .contains(r#""sw-boat-runner-job":"71-1""#)
        );
    }

    const ARCHIVED_33: &str =
        r#"{"id":"bx_33333333","state":"archived","labels":{"sw-boat-runner":"ci"}}"#;
    const RESUMING_33: &str =
        r#"{"id":"bx_33333333","state":"provisioning","labels":{"sw-boat-runner":"ci"}}"#;

    /// boat still lists the resumed machine as archived on the next ticks. Until it lists
    /// the resume or the listing window passes, the machine counts as Starting: it is not
    /// resumed again, and it holds the pool's one box, so job 71 gets no fork.
    #[test]
    fn a_resume_boat_has_not_listed_is_not_resumed_again() {
        let mut cfg = config();
        cfg.pools[0].placement = vec![Placement::Slot, Placement::Resume, Placement::Fork];
        cfg.pools[0].slots_per_box = 1;
        cfg.pools[0].max_boxes = 1;
        let fake = lagging(
            |t| if t < 4 { ARCHIVED_33 } else { RESUMING_33 },
            |_| JOBS_70_71,
        );
        let mut tick_loop = started(&cfg, &fake);
        for k in 0..4 {
            tick_loop.tick(NOW + 15 * k, decide).unwrap();
        }
        assert_eq!((resumes(&fake), forks(&fake).len()), (1, 0));
        assert!(tick_loop.memory.inflight.is_empty());

        // A resume boat never lists is forgotten after the window, like a fork.
        let fake = lagging(|_| ARCHIVED_33, |_| JOB_70);
        let mut tick_loop = started(&cfg, &fake);
        let lag_s = HTTP_TIMEOUT.as_secs() + cfg.tick_s;
        tick_loop.tick(NOW, decide).unwrap();
        tick_loop.tick(NOW + lag_s, decide).unwrap();
        assert_eq!(resumes(&fake), 1);
        tick_loop.tick(NOW + lag_s + 1, decide).unwrap();
        assert_eq!(resumes(&fake), 2);
    }

    #[test]
    fn a_pending_fork_clears_once_boat_lists_it() {
        let cfg = config();
        // Listed on the third tick only; boat dropped it by the fourth.
        let fake = lagging(|t| if t == 3 { LISTED_55 } else { "" }, |_| JOB_70);
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.tick(NOW, |_, _| vec![fork(Some(70))]).unwrap();
        let starting = [("bx_55555555".to_string(), MachineState::Starting)];
        assert_eq!(observed(&mut tick_loop, NOW + 15), starting);
        assert_eq!(observed(&mut tick_loop, NOW + 30), starting);
        assert!(observed(&mut tick_loop, NOW + 45).is_empty());
    }

    /// boat never lists the fork for job 70. For the request timeout and a tick it counts as
    /// Starting, so job 70 gets no second fork. Then the loop drops it with a warning and
    /// forks job 70 again.
    #[test]
    fn a_fork_boat_never_lists_is_dropped_after_its_window() {
        let mut cfg = config();
        cfg.pools[0].paid_start_cents_per_day = 0;
        let fake = lagging(|_| "", |_| JOB_70);
        let mut tick_loop = started(&cfg, &fake);
        let lag_s = HTTP_TIMEOUT.as_secs() + cfg.tick_s;
        tick_loop.tick(NOW, decide).unwrap();
        tick_loop.tick(NOW + lag_s, decide).unwrap();
        assert_eq!(forks(&fake).len(), 1);
        assert_eq!(pending_forks(&tick_loop), [NOW]);
        let expired = NOW + lag_s + 1;
        tick_loop.tick(expired, decide).unwrap();
        assert_eq!(forks(&fake).len(), 2);
        assert_eq!(pending_forks(&tick_loop), [expired]);
    }
}
