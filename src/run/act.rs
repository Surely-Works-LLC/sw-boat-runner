//! Carries out a tick's actions, and keeps the start budget and back-off from tick to tick
//! (docs/design.md#start-budget).

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use super::observe::Observed;
use super::{Error, HTTP_TIMEOUT, Loop, script};
use crate::boat;
use crate::github;
use crate::model::{Action, Cents, Config, JobKey, Snapshot, UnixSecs};

pub(super) const DAY_S: u64 = 86_400;
/// boat's 409 code for a resume of a machine whose disk it has deleted, which it may do
/// once the machine has been stopped for 14 days.
const DISK_EXPIRED: &str = "snapshot_data_expired";
/// Back-off for a start that boat keeps refusing with the same error: the first wait, and
/// the most it doubles to.
const BACKOFF_MIN_S: u64 = 15;
const BACKOFF_MAX_S: u64 = 600;

/// The start budget and back-off, kept from tick to tick. Only this module writes it.
pub(super) struct Ledger {
    /// The UTC day `spent_cents` counts.
    day: u64,
    spent_cents: Cents,
    /// No start before this.
    blocked_until: UnixSecs,
    /// Starts that failed last time they were sent; see `Failing`.
    failing: HashMap<(String, StartKind), Failing>,
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
        }
    }

    pub(super) fn begin_tick(&mut self, now: UnixSecs) {
        if now / DAY_S != self.day {
            self.day = now / DAY_S;
            self.spent_cents = 0;
        }
    }

    pub(super) fn spent_cents(&self) -> Cents {
        self.spent_cents
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
        self.failing.insert(
            key,
            Failing {
                error,
                backoff_s,
                until,
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
    snapshot: &'s Snapshot,
    /// GitHub ids of the snapshot's runners that are still there.
    runner_ids: HashMap<String, u64>,
    starts: Starts,
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

impl Loop<'_> {
    /// Carries out `actions` in `rank` order. Returns `Err` only for errors no later tick
    /// can fix; others are logged, and a GitHub rate limit ends the tick.
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
        self.ledger.failing.retain(|k, _| asked.contains(k));
        let limits = snapshot.limits;
        let mut tick = Tick {
            now,
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
        };
        for action in &actions {
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
                        self.ledger.failing.remove(k);
                    }
                }
                Err(e) if e.is_fatal() => return Err(e),
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
        Ok(())
    }

    fn execute(&mut self, t: &mut Tick, action: &Action) -> Result<(), Error> {
        match action {
            Action::Unregister { runner } => match t.runner_ids.remove(runner) {
                Some(id) => self.delete_runner(id),
                None => Ok(()),
            },
            Action::Delete { machine } => self.delete_machine(t, machine),
            Action::Stop { machine } => {
                self.remove_runners(t, machine)?;
                Ok(self.boat.stop(machine)?)
            }
            Action::Register {
                pool,
                machine,
                slot,
            } => self.register(t.now, pool, machine, *slot),
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
        self.memory.mark_used(machine, now);
        self.boat
            .write_file(machine, &script::jit_path(slot), jit.expose())?;
        self.boat
            .exec_detached(machine, &script::start_command(slot))?;
        Ok(())
    }

    fn resume(&mut self, t: &mut Tick, machine: &str) -> Result<(), Error> {
        match self.start(t, |b| b.resume(machine)) {
            Ok(()) => {
                self.memory.resumed(machine, t.now);
                Ok(())
            }
            // boat deleted the disk of a machine that stayed stopped too long. It can never
            // resume, so delete it; the pool then forks a new one if it needs to.
            Err(Error::Boat(boat::Error::Conflict { code, .. })) if code == DISK_EXPIRED => {
                eprintln!("warn: {machine} lost its disk; deleting it");
                self.delete_machine(t, machine)
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
                self.memory.forked(id, &p.name, job, t.now);
                Ok(())
            }
            Err(e) => {
                // boat may have started a machine this process never heard about, and the
                // next fork's fresh key would not find it (`FixLostCreate`). Wait as after a
                // restart: by then the machine is listed with its job label, or it never
                // existed.
                if matches!(&e, Error::Boat(b) if b.may_have_applied()) {
                    let lag_s = HTTP_TIMEOUT.as_secs().saturating_add(self.cfg.tick_s);
                    self.ledger.block_until(t.now.saturating_add(lag_s));
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
                self.ledger.spent_cents = self.ledger.spent_cents.saturating_add(price);
                t.starts = Starts::Open {
                    price,
                    recheck: true,
                };
                match self.open_starts(t) {
                    Err(e) if e.is_fatal() => Err(e),
                    _ => Ok(v),
                }
            }
            Err(e @ boat::Error::Refused { retry_after_s, .. }) => {
                self.ledger.block_until(t.now.saturating_add(retry_after_s));
                t.starts = Starts::Closed;
                Err(e.into())
            }
            Err(e) => {
                if e.may_have_applied() {
                    self.ledger.spent_cents = self.ledger.spent_cents.saturating_add(price);
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
            match self.boat.limits() {
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
    use crate::model::Pool;
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
        assert_eq!(tick_loop.ledger.spent_cents, 125);
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
        let mut tick_loop = Loop::new(&cfg, &fake, BOAT_KEY.into(), GH_TOKEN.into(), NOW);
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
                snapshot: &observed.snapshot,
                runner_ids: observed.runner_ids.clone(),
                starts: Starts::Closed,
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
}
