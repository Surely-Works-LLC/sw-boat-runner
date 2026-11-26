//! Reads boat and GitHub into the `Snapshot` that `decide` sees, adding what this process
//! remembers between ticks (docs/design.md#observed-state).

use std::collections::{BTreeMap, HashMap, HashSet};

use super::idle_file::{self, Clock};
use super::{Error, HTTP_TIMEOUT, Loop, script};
use crate::boat::{self, Sandbox};
use crate::decide::STUCK_START_S;
use crate::github::{self, GhRunner, RunnerRef};
use crate::model::{
    Config, JobKey, Limits, Machine, MachineState, Runner, RunnerState, Snapshot, UnixSecs, Warming,
};

/// A snapshot, and the GitHub id of each of its runners, which deleting one needs.
pub(super) struct Observed {
    pub snapshot: Snapshot,
    pub runner_ids: HashMap<String, RunnerRef>,
    /// Runners whose start failed, still listed and never online; left out of `snapshot`.
    pub abandoned: Vec<Abandoned>,
}

/// A runner GitHub issued a jit config for whose file write or start command then failed.
pub(super) struct Abandoned {
    pub name: String,
    pub runner: RunnerRef,
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
    /// Runners GitHub listed `busy` at the last observe; see `Memory::runners`.
    busy: HashSet<String>,
    /// Starts boat accepted that its listing may not show yet.
    inflight: Vec<Inflight>,
    /// Whether an observe has succeeded since this process started.
    warm: bool,
    /// Failed starts by runner name, with their original GitHub registration.
    abandoned: HashMap<String, (String, UnixSecs, RunnerRef)>,
    /// Job forks already warned about for a `Busy` runner near their TTL.
    ttl_warned: HashSet<String>,
    /// Machines whose stop boat refuses, already warned about while it does.
    stop_refused: HashSet<String>,
    /// Idle clocks of kept machines saved by a predecessor (`idle_file`), by id, until
    /// each machine is first seen.
    saved_clocks: HashMap<String, Clock>,
    /// When each job a listed fork was made for was last seen queued (`decide`'s
    /// `orphaned` times its grace from it).
    queued_seen: HashMap<JobKey, UnixSecs>,
    /// When a job each pool serves first was last seen queued, by pool.
    pool_queued: std::collections::BTreeMap<String, UnixSecs>,
    /// When each job queued now was first seen queued, for the register log's wait.
    first_queued: HashMap<JobKey, UnixSecs>,
    /// Runners this process registered that came online at the last observe, with the
    /// seconds from register to online, for the log.
    came_online: Vec<(String, u64)>,
    /// When this process started. A job it never saw queued counts as queued then, for
    /// `orphaned`: one may have been queued just before a restart.
    started: UnixSecs,
    /// When each machine last started a warm, for `Snapshot::warmed_at`.
    warmed: HashMap<String, UnixSecs>,
    /// The boat process id of each warm whose launch boat answered, until it ends.
    /// Another process's ids are not trusted: a restart cancels its warms instead.
    warm_commands: HashMap<String, u64>,
}

struct Seen {
    /// Not forked for a job.
    kept: bool,
    state: MachineState,
    since: UnixSecs,
    last_busy: UnixSecs,
    /// A kept machine found holding its creator's logins in its current state. No runner
    /// goes on it until it starts again.
    holds_logins: bool,
    restore_retry: Option<RestoreRetry>,
}

/// A direct restore refusal can outlast boat's optimistic listing. Preserve the first
/// refusal's clock until a launch succeeds or the machine leaves this boot.
struct RestoreRetry {
    since: UnixSecs,
    next_check: UnixSecs,
    delay: u64,
}

const RESTORE_RETRY_MIN_S: u64 = 15;
const RESTORE_RETRY_MAX_S: u64 = 120;

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
    /// The memory of a process that started at `now`.
    pub(super) fn new(now: UnixSecs) -> Self {
        Memory {
            started: now,
            ..Memory::default()
        }
    }

    /// When `job` was last seen queued, or this process's start if it never was.
    fn queued_at(&self, job: JobKey) -> UnixSecs {
        self.queued_seen.get(&job).copied().unwrap_or(self.started)
    }

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
            seen.restore_retry = None;
        }
    }

    pub(super) fn waiting_for_restore(&self, machine: &str) -> bool {
        self.machines
            .get(machine)
            .is_some_and(|seen| seen.restore_retry.is_some())
    }

    pub(super) fn restore_check_due(&self, machine: &str, now: UnixSecs) -> bool {
        let Some(retry) = self
            .machines
            .get(machine)
            .and_then(|m| m.restore_retry.as_ref())
        else {
            return true;
        };
        now >= retry.next_check
            && now.saturating_sub(retry.since) < STUCK_START_S
            && !self.abandoned.values().any(|(id, _, _)| id == machine)
    }

    pub(super) fn defer_restore(&mut self, machine: &str, now: UnixSecs) {
        if let Some(seen) = self.machines.get_mut(machine) {
            let retry = seen.restore_retry.get_or_insert(RestoreRetry {
                since: if seen.kept { now } else { seen.since },
                next_check: now,
                delay: 0,
            });
            retry.delay = (retry.delay * 2).clamp(RESTORE_RETRY_MIN_S, RESTORE_RETRY_MAX_S);
            retry.next_check = now.saturating_add(retry.delay);
        }
    }

    /// Notes a warm launch sent to `machine` at `now`, and its boat process id once boat
    /// answered with one.
    pub(super) fn warm_sent(&mut self, machine: &str, now: UnixSecs, command: Option<u64>) {
        self.warmed.insert(machine.to_string(), now);
        if let Some(id) = command {
            self.warm_commands.insert(machine.to_string(), id);
        }
    }

    /// Takes the idle clocks a predecessor saved; see `idle_file`.
    pub(super) fn restore_clocks(&mut self, clocks: HashMap<String, Clock>) {
        self.saved_clocks = clocks;
    }

    /// The idle clocks of the kept machines up now, for `idle_file`.
    pub(super) fn idle_clocks(&self) -> String {
        let up = self
            .machines
            .iter()
            .filter(|(_, seen)| seen.kept && seen.state == MachineState::Up);
        idle_file::format(up.map(|(id, seen)| {
            let clock = Clock {
                since: seen.since,
                last_busy: seen.last_busy,
            };
            (id.as_str(), clock)
        }))
    }

    /// Notes a runner whose start failed after GitHub issued its jit config. While GitHub
    /// lists it and it has never been online, it is left out of the snapshot, so its slot
    /// is free and it covers no job, and `act` deletes it. One that comes online after all
    /// (a start command that ran without an answer) is a runner like any other.
    pub(super) fn abandon(
        &mut self,
        name: String,
        machine: &str,
        now: UnixSecs,
        runner: RunnerRef,
    ) {
        self.abandoned
            .insert(name, (machine.to_string(), now, runner));
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
    /// A runner listed busy at the last observe and offline now stays `Busy` for this
    /// observe, as a listing that lags or blips can show a runner offline while it still
    /// runs its job. `decide` unregisters an `Offline` runner and gives its slot a new one,
    /// whose start would kill that job. Offline twice in a row, it is `Offline`; a runner
    /// that really died keeps its slot one tick longer.
    fn runners(
        &mut self,
        cfg: &Config,
        now: UnixSecs,
        lag_s: u64,
        listed: Vec<GhRunner>,
    ) -> (Vec<Runner>, HashMap<String, RunnerRef>, Vec<Abandoned>) {
        let mut ids = HashMap::new();
        let mut runners = Vec::new();
        let mut abandoned = Vec::new();
        let mut listed_names = HashSet::new();
        let mut busy = HashSet::new();
        for r in listed {
            listed_names.insert(r.name.clone());
            let Some(n) = github::parse_runner_name(&r.name) else {
                continue;
            };
            if !cfg.pools.iter().any(|p| p.name == n.pool) {
                continue;
            }
            let online = r.status == "online";
            if online && self.online.insert(r.name.clone()) && n.registered_at >= self.started {
                let took = now.saturating_sub(n.registered_at);
                self.came_online.push((r.name.clone(), took));
            }
            let mut state = github::runner_state(online, r.busy, self.online.contains(&r.name));
            if state == RunnerState::Busy {
                busy.insert(r.name.clone());
            } else if state == RunnerState::Offline && self.busy.contains(&r.name) {
                state = RunnerState::Busy;
            }
            if let Some((machine, _, failed)) = self.abandoned.get(&r.name) {
                let same_registration = *failed == r.reference();
                if same_registration && state == RunnerState::Registered {
                    abandoned.push(Abandoned {
                        runner: r.reference(),
                        name: r.name,
                        machine: machine.clone(),
                    });
                    continue;
                }
                if same_registration {
                    eprintln!("warn: {} came online after its start failed; kept", r.name);
                }
                self.abandoned.remove(&r.name);
            }
            ids.insert(r.name.clone(), r.reference());
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
        self.busy = busy;
        // One GitHub has not listed within a request timeout and a tick is gone.
        self.abandoned.retain(|name, (_, noted, _)| {
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
        let mut refused = HashSet::new();
        let mut machines: Vec<Machine> = listed
            .into_iter()
            .map(|(pool, sb)| {
                if let Some(why) = sb.refused_stop() {
                    if !self.stop_refused.contains(&sb.id) {
                        eprintln!(
                            "warn: boat refuses to stop {} (pool {pool}): {why}; it counts \
                             as stopping, not capacity, until boat stops it",
                            sb.id
                        );
                    }
                    refused.insert(sb.id.clone());
                }
                let state = sb.machine_state();
                let job = sb.job();
                // After a restart nothing records whether an Up fork already had a runner,
                // so it counts as used (`FixCrashSeen`): deleted, never registered again. A
                // fork that never had one wastes its start. Otherwise, just before first
                // sight, so it has not.
                let used = !self.warm && job.is_some() && state == MachineState::Up;
                // A kept machine still up keeps the clocks a predecessor saved for it.
                let saved = self.saved_clocks.remove(&sb.id);
                let saved = saved.filter(|_| job.is_none() && state == MachineState::Up);
                let seen = self.machines.entry(sb.id.clone()).or_insert(Seen {
                    kept: job.is_none(),
                    state,
                    since: saved.map_or(now, |c| c.since.min(now)),
                    last_busy: match saved {
                        Some(c) => c.last_busy.min(now.saturating_sub(1)),
                        None if used => now,
                        None => now.saturating_sub(1),
                    },
                    holds_logins: false,
                    restore_retry: None,
                });
                if seen.state != state {
                    seen.state = state;
                    seen.since = now;
                    seen.holds_logins = false;
                }
                if live.contains(sb.id.as_str()) {
                    seen.last_busy = now;
                }
                if !matches!(state, MachineState::Up | MachineState::Starting) {
                    seen.restore_retry = None;
                }
                let mut since = seen.since;
                let mut state = state;
                if let Some(retry) = &seen.restore_retry {
                    if !seen.kept {
                        // A listing that alternates between Up and Starting must not keep
                        // extending the fork's existing registration timeout or TTL.
                        since = since.min(retry.since);
                    } else if now.saturating_sub(retry.since) >= STUCK_START_S
                        && !runners
                            .iter()
                            .any(|r| r.machine == sb.id && r.state != RunnerState::Offline)
                    {
                        // Reuse the kept-machine stuck-start cleanup once its slots are empty.
                        state = MachineState::Starting;
                        since = retry.since;
                    }
                }
                Machine {
                    since,
                    last_busy: seen.last_busy,
                    id: sb.id,
                    pool,
                    state,
                    job,
                    job_queued_at: job.map(|j| self.queued_at(j)),
                }
            })
            .collect();
        self.warm = true;
        self.stop_refused = refused;

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
                    job_queued_at: job.map(|j| self.queued_at(j)),
                }),
            }
        }
        let jobs: HashSet<JobKey> = machines.iter().filter_map(|m| m.job).collect();
        self.queued_seen.retain(|j, _| jobs.contains(j));
        machines
    }

    /// The jobs of `queued` that go to `pool` and the longest any of them has been seen
    /// queued at `now`, or `None` when none does.
    pub(super) fn queue_wait(
        &self,
        cfg: &Config,
        pool: &str,
        queued: &[crate::model::QueuedJob],
        now: UnixSecs,
    ) -> Option<(usize, u64)> {
        let first: Vec<UnixSecs> = queued
            .iter()
            .filter(|j| {
                cfg.pools
                    .iter()
                    .find(|p| p.serves(&j.labels))
                    .is_some_and(|p| p.name == pool)
            })
            .map(|j| self.first_queued.get(&j.key).copied().unwrap_or(now))
            .collect();
        let oldest = first.iter().min()?;
        Some((first.len(), now.saturating_sub(*oldest)))
    }

    /// Notes that `queued` were seen queued at `now`, each job for the first pool in the
    /// file that serves it, as `run::plan` routes it.
    fn saw_queued(&mut self, cfg: &Config, now: UnixSecs, queued: &[crate::model::QueuedJob]) {
        let keys: HashSet<JobKey> = queued.iter().map(|j| j.key).collect();
        self.first_queued.retain(|j, _| keys.contains(j));
        for j in queued {
            self.queued_seen.insert(j.key, now);
            self.first_queued.entry(j.key).or_insert(now);
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
        let gh_runners = self.gh.policy_runners(self.cfg)?;
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
        for (name, took) in std::mem::take(&mut self.memory.came_online) {
            self.log(format_args!("online {name} {took}s after its register"));
        }
        self.memory.saw_queued(self.cfg, now, &queued);
        let machines = self.memory.machines(now, lag_s, listed, &runners);
        let clocks = self.memory.idle_clocks();
        self.idle_file.save(&clocks);
        let warming = self.warms(&machines);
        let listed: HashSet<&str> = machines.iter().map(|m| m.id.as_str()).collect();
        self.memory
            .warmed
            .retain(|id, _| listed.contains(id.as_str()));
        let warmed_at = machines
            .iter()
            .filter(|m| !m.is_ephemeral() && m.state == MachineState::Up)
            .filter(|m| self.pool(&m.pool).is_ok_and(|p| p.idle_warm.is_some()))
            .map(|m| {
                let at = self.memory.warmed.get(&m.id).copied();
                (m.id.clone(), at.unwrap_or(self.memory.started))
            })
            .collect();
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
            queued_at: self
                .cfg
                .pools
                .iter()
                .map(|p| {
                    let at = self.memory.pool_queued.get(&p.name).copied();
                    (p.name.clone(), at.unwrap_or(self.memory.started))
                })
                .collect(),
            warming,
            warmed_at,
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

    /// The warms in the state file, after dropping those of machines boat no longer
    /// lists and those that ended. A warm whose command this process watches on an `Up`
    /// machine is `Running` until boat says it ended; it is `Unknown` once its machine is
    /// down (a resumed machine may still get a late launch), when it ended without a
    /// result (something may be left running), or when this process never had its id.
    fn warms(&mut self, machines: &[Machine]) -> BTreeMap<String, Warming> {
        let listed = |id: &str| {
            machines
                .iter()
                .find(|m| m.id == id && m.state != MachineState::Gone)
        };
        self.warm_file.retain(|id| listed(id).is_some());
        let warms = self.warm_file.warms();
        self.memory
            .warm_commands
            .retain(|id, _| warms.contains_key(id));
        let mut warming = BTreeMap::new();
        for id in self.warm_file.warms().keys().cloned().collect::<Vec<_>>() {
            let Some(m) = listed(&id) else { continue };
            let command = self.memory.warm_commands.get(&id).copied();
            let command = command.filter(|_| m.state == MachineState::Up);
            let state = match command.map(|c| self.boat.command_status(&id, c)) {
                Some(Ok(s)) if s.running => Warming::Running,
                // The supervisor's result means it ended every process of the warm.
                Some(Ok(s)) if s.exit_code == Some(0) => match script::warm_result(&s.stdout) {
                    Some((event, detail)) => {
                        self.log(format_args!(
                            "warm {event} {id} (pool {}): {detail}",
                            m.pool
                        ));
                        self.warm_file.retain(|w| w != id);
                        self.memory.warm_commands.remove(&id);
                        continue;
                    }
                    None => Warming::Unknown,
                },
                // Asked again next tick.
                Some(Err(e)) if !matches!(e, boat::Error::NotFound) => Warming::Running,
                _ => Warming::Unknown,
            };
            if state == Warming::Unknown {
                self.memory.warm_commands.remove(&id);
            }
            warming.insert(id, state);
        }
        warming
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::decide::decide;
    use crate::model::{Action, Placement};

    fn queued(ids: &[u64]) -> Vec<crate::model::QueuedJob> {
        ids.iter()
            .map(|&id| crate::model::QueuedJob {
                key: JobKey { id, attempt: 1 },
                labels: vec!["self-hosted".into(), "large".into()],
            })
            .collect()
    }

    /// The register log's wait counts from when a job was first seen queued, restarts when
    /// it leaves the queue and comes back, and goes only to the first pool that serves it.
    #[test]
    fn a_failed_start_does_not_abandon_a_new_registration_with_the_same_name() {
        let cfg = config();
        let mut memory = Memory::new(NOW);
        let mut runner = GhRunner {
            id: 20,
            name: "sbr-ci-bx_22222222-1-1000".into(),
            status: "offline".into(),
            busy: false,
            organization_group: None,
        };
        memory.abandon(runner.name.clone(), "bx_22222222", NOW, runner.reference());
        runner.id = 21;
        let (runners, ids, abandoned) = memory.runners(&cfg, NOW + 15, 90, vec![runner]);
        assert!(abandoned.is_empty());
        assert_eq!(runners.len(), 1);
        assert_eq!(ids[&runners[0].name].id, 21);
    }

    #[test]
    fn queue_wait_counts_from_first_sight_for_the_first_pool() {
        let mut cfg = config();
        let mut wide = cfg.pools[0].clone();
        wide.name = "wide".into();
        wide.labels.push("gpu".into());
        cfg.pools.push(wide);
        let mut memory = Memory::new(NOW);
        memory.saw_queued(&cfg, NOW, &queued(&[70]));
        let both = queued(&[70, 71]);
        memory.saw_queued(&cfg, NOW + 30, &both);
        assert_eq!(
            memory.queue_wait(&cfg, "ci", &both, NOW + 30),
            Some((2, 30))
        );
        assert_eq!(memory.queue_wait(&cfg, "wide", &both, NOW + 30), None);
        memory.saw_queued(&cfg, NOW + 45, &queued(&[71]));
        let back = queued(&[70]);
        memory.saw_queued(&cfg, NOW + 60, &back);
        assert_eq!(memory.queue_wait(&cfg, "ci", &back, NOW + 60), Some((1, 0)));
    }

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

    /// GitHub lists runner 11 busy, then offline twice; runner 12 idle, then offline
    /// twice. Runner 11 may still run its job after the first offline listing, so it
    /// stays `Busy` and `decide` neither unregisters it nor gives its slot a new runner;
    /// the second makes it `Offline`. Runner 12 had no job and is `Offline` at once.
    #[test]
    fn a_busy_runner_listed_offline_once_stays_busy() {
        let cfg = config();
        let listings = std::cell::Cell::new(0);
        let fake = crate::transport::fake::Fake::new(move |s| {
            if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                listings.set(listings.get() + 1);
                let (status, busy) = if listings.get() == 1 {
                    ("online", true)
                } else {
                    ("offline", false)
                };
                let body = format!(
                    r#"{{"total_count":2,"runners":[
                    {{"id":11,"name":"sbr-ci-bx_22222222-0-900","status":"{status}","busy":{busy}}},
                    {{"id":12,"name":"sbr-ci-bx_22222222-1-900","status":"{status}","busy":false}}]}}"#
                );
                return crate::transport::fake::respond(200, &[], &body);
            }
            reply(s)
        });
        let mut tick_loop = started(&cfg, &fake);
        let states: Vec<Vec<RunnerState>> = [NOW, NOW + 15, NOW + 30]
            .into_iter()
            .map(|now| {
                let snapshot = tick_loop.observe(now).unwrap().snapshot;
                snapshot.runners.iter().map(|r| r.state).collect()
            })
            .collect();
        use RunnerState::{Busy, Idle, Offline};
        assert_eq!(states, [[Busy, Idle], [Busy, Offline], [Offline, Offline]]);
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

    /// The fork waits for its listing from the time boat answered, not the tick start.
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
        let fork_answer = NOW + 30 * (observe_requests().len() as u64 + 1);
        assert_eq!(pending_forks(&tick_loop), [fork_answer]);
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

    /// boat refuses to stop a machine with no recent snapshot and keeps it up. It is
    /// warned about once while boat refuses, gets no runner and holds no box: the queued
    /// job goes to a fork of a new kept machine.
    #[test]
    fn a_machine_whose_stop_boat_refuses_is_not_capacity() {
        let mut cfg = config();
        cfg.pools[0].placement = vec![Placement::Slot, Placement::Resume];
        cfg.pools[0].max_boxes = 1;
        let fake = crate::transport::fake::Fake::new(|s| {
            let kept = r#"{"id":"bx_77777777","state":"idle","hydrated":true,
                "labels":{"sw-boat-runner":"ci"},
                "stop":{"status":"failing","error":"Refusing archive/stop: no snapshot"}}"#;
            lagging_reply(s, kept, JOB_70)
        });
        let mut tick_loop = started(&cfg, &fake);
        let refused = |l: &Loop| l.memory.stop_refused.contains("bx_77777777");
        let seen = observed(&mut tick_loop, NOW);
        assert_eq!(seen, [("bx_77777777".to_string(), MachineState::Stopping)]);
        assert!(refused(&tick_loop));
        tick_loop.tick(NOW + 15, decide).unwrap();
        assert!(refused(&tick_loop));
        assert_eq!(registers(&fake), 0);
        assert_eq!(forks(&fake).len(), 1);
    }

    /// A kept machine busy before a restart keeps its idle clock through the restart when
    /// the clocks are saved, so it stops `idle_stop_s` after its last job, not after the
    /// restart. Without the file the clock starts over.
    #[test]
    fn a_restart_keeps_saved_idle_clocks() {
        let mut cfg = config();
        cfg.pools[0].placement = vec![Placement::Slot, Placement::Resume];
        let kept = r#"{"id":"bx_77777777","state":"idle","hydrated":true,
            "labels":{"sw-boat-runner":"ci"}}"#;
        let busy = crate::transport::fake::Fake::new(move |s| {
            if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                let body = r#"{"total_count":1,"runners":[
                    {"id":15,"name":"sbr-ci-bx_77777777-0-900","status":"online","busy":true}]}"#;
                return crate::transport::fake::respond(200, &[], body);
            }
            lagging_reply(s, kept, "")
        });
        let idle = crate::transport::fake::Fake::new(move |s| lagging_reply(s, kept, ""));
        let dir = std::env::temp_dir().join(format!("sbr-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idle-clocks.tsv");
        let _ = std::fs::remove_file(&path);

        let mut before = started(&cfg, &busy);
        before.idle_file = idle_file::IdleFile::at(Some(path.clone()));
        before.observe(NOW).unwrap();

        let later = NOW + cfg.pools[0].idle_stop_s;
        let stops_after_restart = |file: Option<std::path::PathBuf>| {
            let mut after = started(&cfg, &idle);
            after.idle_file = idle_file::IdleFile::at(file);
            let clocks = after.idle_file.load();
            after.memory.restore_clocks(clocks);
            let snap = after.observe(later).unwrap().snapshot;
            let busy_at: Vec<UnixSecs> = snap.machines.iter().map(|m| m.last_busy).collect();
            let stops = decide(&snap, &cfg.pools[0]);
            (
                busy_at,
                stops.iter().any(|a| matches!(a, Action::Stop { .. })),
            )
        };
        assert_eq!(stops_after_restart(Some(path)), (vec![NOW], true));
        assert_eq!(stops_after_restart(None), (vec![later - 1], false));
    }

    /// After a restart nothing is known of when the jobs were last queued. An up fork
    /// whose job is not queued, with an Idle runner that may have just taken it, is kept
    /// for `ORPHAN_GRACE_S` from the process's start, not deleted at once.
    #[test]
    fn after_a_restart_an_idle_fork_waits_the_grace_from_the_start() {
        let cfg = config();
        let fake = crate::transport::fake::Fake::new(|s| {
            if s.method == "GET" && s.url.starts_with(&format!("{GH}/runners?")) {
                let body = r#"{"total_count":1,"runners":[
                    {"id":15,"name":"sbr-ci-bx_66666666-0-900","status":"online","busy":false}]}"#;
                return crate::transport::fake::respond(200, &[], body);
            }
            let fork = r#"{"id":"bx_66666666","state":"running",
                "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"70-1"}}"#;
            lagging_reply(s, fork, "")
        });
        let mut tick_loop = Loop::new(&cfg, &fake, BOAT_KEY.into(), gh_auth(), NOW);
        let delete = format!("DELETE {BOAT}/sandboxes/bx_66666666");
        tick_loop.tick(NOW, decide).unwrap();
        tick_loop
            .tick(NOW + crate::decide::ORPHAN_GRACE_S - 1, decide)
            .unwrap();
        assert!(!requests(&fake).contains(&delete), "{:?}", requests(&fake));
        tick_loop
            .tick(NOW + crate::decide::ORPHAN_GRACE_S, decide)
            .unwrap();
        assert!(requests(&fake).contains(&delete), "{:?}", requests(&fake));
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
