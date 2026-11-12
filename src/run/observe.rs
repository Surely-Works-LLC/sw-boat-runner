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
    /// Runners whose start failed, still listed and never online; left out of `snapshot`.
    pub abandoned: Vec<Abandoned>,
}

/// A runner GitHub issued a jit config for whose file write or start command then failed.
pub(super) struct Abandoned {
    pub name: String,
    pub id: u64,
    pub machine: String,
}

/// How far ahead of a job fork's `fork_ttl_s` a `Busy` runner on it is warned about, at
/// most: boat archives the machine at its TTL, job or no job.
const TTL_WARN_S: u64 = 600;

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
    /// Runners whose start failed, by name: their machine and when they were noted.
    abandoned: HashMap<String, (String, UnixSecs)>,
    /// Job forks already warned about for a `Busy` runner near their TTL.
    ttl_warned: HashSet<String>,
    /// When each job a listed fork was made for was last seen queued (`decide`'s
    /// `orphaned` times its grace from it).
    queued_seen: HashMap<JobKey, UnixSecs>,
    /// When a job each pool serves first was last seen queued, by pool.
    pool_queued: std::collections::BTreeMap<String, UnixSecs>,
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

    /// Notes a runner whose start failed after GitHub issued its jit config. While GitHub
    /// lists it and it has never been online, it is left out of the snapshot, so its slot
    /// is free and it covers no job, and `act` deletes it. One that comes online after all
    /// (a start command that ran without an answer) is a runner like any other.
    pub(super) fn abandon(&mut self, name: String, machine: &str, now: UnixSecs) {
        self.abandoned.insert(name, (machine.to_string(), now));
    }

    /// `act` deleted the runner, or GitHub has no such runner.
    pub(super) fn forget_abandoned(&mut self, name: &str) {
        self.abandoned.remove(name);
    }

    /// Whether `machine` has had no `Busy` or `Registered` runner, and no `Register`,
    /// since `t`.
    pub(super) fn idle_since(&self, machine: &str, t: UnixSecs) -> bool {
        self.machines
            .get(machine)
            .is_some_and(|seen| seen.last_busy < t)
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
        now: UnixSecs,
        lag_s: u64,
        listed: Vec<GhRunner>,
    ) -> (Vec<Runner>, HashMap<String, u64>, Vec<Abandoned>) {
        let mut ids = HashMap::new();
        let mut runners = Vec::new();
        let mut abandoned = Vec::new();
        let mut listed_names = HashSet::new();
        for r in listed {
            listed_names.insert(r.name.clone());
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
            if let Some((machine, _)) = self.abandoned.get(&r.name) {
                if state == RunnerState::Registered {
                    abandoned.push(Abandoned {
                        name: r.name,
                        id: r.id,
                        machine: machine.clone(),
                    });
                    continue;
                }
                eprintln!("warn: {} came online after its start failed; kept", r.name);
                self.abandoned.remove(&r.name);
            }
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
        // One GitHub has not listed within a request timeout and a tick is gone.
        self.abandoned.retain(|name, (_, noted)| {
            listed_names.contains(name) || now.saturating_sub(*noted) <= lag_s
        });
        (runners, ids, abandoned)
    }

    /// Warns, once per machine, about a job fork whose `Busy` runner may outlast its TTL:
    /// boat archives the machine then and the job fails. `since` is when boat first listed
    /// it up, after its create, so the warning can only come late, not early.
    fn warn_near_ttl(&mut self, cfg: &Config, now: UnixSecs, snap: &Snapshot) {
        for m in &snap.machines {
            let Some(pool) = cfg.pools.iter().find(|p| p.name == m.pool) else {
                continue;
            };
            let busy = snap
                .runners
                .iter()
                .any(|r| r.machine == m.id && r.state == RunnerState::Busy);
            let window = TTL_WARN_S.min(pool.fork_ttl_s / 5);
            let age = now.saturating_sub(m.since);
            let near = m.is_ephemeral()
                && m.state == MachineState::Up
                && busy
                && age >= pool.fork_ttl_s.saturating_sub(window);
            if near && self.ttl_warned.insert(m.id.clone()) {
                eprintln!(
                    "warn: {} in pool {} runs a job {age}s after it came up; boat archives \
                     it at fork_ttl_s ({}s) from its create, job or no job. Raise fork_ttl_s \
                     if jobs run this long",
                    m.id, m.pool, pool.fork_ttl_s
                );
            }
        }
        let listed: HashSet<&str> = snap.machines.iter().map(|m| m.id.as_str()).collect();
        self.ttl_warned.retain(|id| listed.contains(id.as_str()));
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
                    job_queued_at: job.and_then(|j| self.queued_seen.get(&j).copied()),
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
                    job_queued_at: job.and_then(|j| self.queued_seen.get(&j).copied()),
                }),
            }
        }
        let jobs: HashSet<JobKey> = machines.iter().filter_map(|m| m.job).collect();
        self.queued_seen.retain(|j, _| jobs.contains(j));
        machines
    }

    /// Notes that `queued` were seen queued at `now`, each job for the first pool in the
    /// file that serves it, as `run::plan` routes it.
    fn saw_queued(&mut self, cfg: &Config, now: UnixSecs, queued: &[crate::model::QueuedJob]) {
        for j in queued {
            self.queued_seen.insert(j.key, now);
            if let Some(p) = cfg.pools.iter().find(|p| p.serves(&j.labels)) {
                self.pool_queued.insert(p.name.clone(), now);
            }
        }
        self.pool_queued
            .retain(|name, _| cfg.pools.iter().any(|p| &p.name == name));
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
        self.ledger.quote_seen(quote.cents);

        let lag_s = HTTP_TIMEOUT.as_secs().saturating_add(self.cfg.tick_s);
        let (runners, runner_ids, abandoned) =
            self.memory.runners(self.cfg, now, lag_s, gh_runners);
        self.memory.saw_queued(self.cfg, now, &queued);
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
            queued_at: self.memory.pool_queued.clone(),
        };
        self.memory.warn_near_ttl(self.cfg, now, &snapshot);
        self.last_observe = Some(super::LastObserve {
            at: now,
            runners: snapshot.runners.clone(),
        });
        Ok(Observed {
            snapshot,
            runner_ids,
            abandoned,
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

    /// The resume of `bx_33333333` timed out: boat may have resumed it, and charged for
    /// it. boat still lists it archived on the next tick, which must not resume it again.
    #[test]
    fn a_resume_without_an_answer_is_not_resumed_again() {
        let mut cfg = config();
        cfg.pools[0].placement = vec![Placement::Slot, Placement::Resume];
        cfg.pools[0].paid_start_cents_per_day = 0;
        let fake = crate::transport::fake::Fake::failing(|s| {
            if s.url.ends_with("/resume") {
                return Err(crate::transport::NetworkError(
                    "timed out reading response".into(),
                ));
            }
            Ok(lagging_reply(s, ARCHIVED_33, JOB_70))
        });
        let mut tick_loop = started(&cfg, &fake);
        tick_loop.tick(NOW, decide).unwrap();
        tick_loop.tick(NOW + 15, decide).unwrap();
        assert_eq!(resumes(&fake), 1);
        assert_eq!(
            observed(&mut tick_loop, NOW + 30),
            [("bx_33333333".to_string(), MachineState::Starting)]
        );
    }

    /// The tick began at `NOW`, but each request took 30 s, so boat answered the fork at
    /// `NOW + 210`. The fork waits for its listing from then, not from `NOW`.
    #[test]
    fn a_fork_is_timed_from_its_answer_not_from_the_tick() {
        let cfg = config();
        let elapsed = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let e = elapsed.clone();
        let fake = crate::transport::fake::Fake::new(move |s| {
            e.set(e.get() + 30);
            lagging_reply(s, "", JOB_70)
        });
        let mut tick_loop = started(&cfg, &fake);
        let e = elapsed.clone();
        tick_loop.clock = Box::new(move || std::time::Duration::from_secs(e.get()));
        tick_loop.tick(NOW, |_, _| vec![fork(Some(70))]).unwrap();
        // Six reads, then the fork, whose answer came 7 × 30 s after the tick began.
        assert_eq!(pending_forks(&tick_loop), [NOW + 210]);
    }

    /// A job fork whose runner is still `Busy` 10 minutes before `fork_ttl_s` is warned
    /// about once: boat archives it at its TTL, job or no job.
    #[test]
    fn a_busy_fork_near_its_ttl_is_warned_about_once() {
        let cfg = config();
        let fake = crate::transport::fake::Fake::new(|s| {
            if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                let body = r#"{"total_count":1,"runners":[
                    {"id":15,"name":"sbr-ci-bx_66666666-0-900","status":"online","busy":true}]}"#;
                return crate::transport::fake::respond(200, &[], body);
            }
            let fork = r#"{"id":"bx_66666666","state":"running",
                "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"70-1"}}"#;
            lagging_reply(s, fork, "")
        });
        let mut tick_loop = started(&cfg, &fake);
        let warned = |l: &Loop| l.memory.ttl_warned.contains("bx_66666666");
        observed(&mut tick_loop, NOW);
        observed(&mut tick_loop, NOW + 2_999);
        assert!(!warned(&tick_loop));
        observed(&mut tick_loop, NOW + 3_000);
        assert!(warned(&tick_loop));
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
