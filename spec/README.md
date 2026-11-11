# BoatRunner TLA+ spec

`BoatRunner.tla` models one sw-boat-runner pool: GitHub's job queue and JIT runners,
boat's machines, and the daemon as `src/decide.rs` and `src/run.rs` implement the rules
in [docs/design.md](../docs/design.md). The spec does not read Rust code.

## What is modelled

- GitHub. A bounded set of jobs arrives (a re-run is another job key), waits in the
  queue, is assigned to an Idle runner and finishes. A JIT runner exits after its job.
  Runner states are Registered, Expired (Registered past `register_timeout_s`), Idle,
  Busy and Offline.
- boat. Machines are Starting, Up, Stopped or Gone. A stopped machine keeps its disk
  for 10 minutes (`keep`), and resuming it then is free; any other resume or fork is a
  counted start. `Included` starts a day are free and each later one costs one unit.
- The daemon (`ToolStep`), one action at a time in `run::rank` order: Unregister,
  Delete, Stop, Register (fresh job forks before free kept slots), Resume, Fork without a
  job (for the `min_up` shortfall, or for demand in a pool with `resume` and no `fork`),
  Fork for a job. `Observe` is `run::observe` updating `last_busy`.
- The snapshot-to-act gap. Stop and Delete are decided from one snapshot (`plan`) and
  carried out by a later `Execute`, because `run::execute` does not look at the runner
  again. GitHub may assign a job in between. No new decision is made while a plan is
  pending.
- The budget. `spent_today` is the daemon's in-memory spend. Each start adds the quoted
  price, re-read after every start. With `FixForkKey` off, a fork with a reused key adds
  it although boat starts nothing. A start needs a price of 0 or
  `spent_today + price <= PaidBudget`, and fewer than `max_boxes` live machines that the
  daemon can see.
- boat's listing lag. boat lists a created machine some time after the create returns.
  Until then the machine is `Hidden` from the snapshot, or `Known` if the daemon
  remembers creating it (`FixListLag`). A create whose answer is lost while boat makes
  the machine leaves it `Lost`. A crash turns every unlisted machine into an `Orphan`,
  because every fork key is a fresh UUID v4 and the restarted daemon cannot find the
  create again. The listing catches up by the time the machine boots.
- Faults, at most `MaxFaults` per behaviour: GitHub reassigns a running job; boat loses
  a kept disk; boat refuses a start; a create's answer is lost while boat makes the
  machine; a JIT runner never comes online; an Idle runner goes offline; GitHub fails a
  runner delete before a Stop or Delete; the daemon crashes. A crash loses the
  unexecuted plan, `since`, `last_busy`, the set of runners seen online, `spent_today`
  and the memory of unlisted forks.
- Time is abstract. `age` is 0, 1 (past `register_timeout_s`) or 2 (past `fork_ttl_s`),
  counted from `since`, which restarts on every state change and on a crash. `idle`
  means `idle_stop_s` since `last_busy` and `min_up_s` since `since`. `min_up` counts
  kept machines only.

## What is abstracted away

- One pool with one label set and one slot per machine. No routing between pools.
- Placement is fixed for a run. `decide` also deletes an idle kept machine that is Up or
  Starting in a pool without `slot`, which is left over when a pool's placement changes.
  The models cannot reach that state: kept machines start `Stopped`, the only model
  without `slot` (`BoatRunnerFork.cfg`) has no `resume` and, by `ASSUME`, no `MinUp`, so
  it never resumes or forks a kept machine, and the `Stopped` rule deletes them. So
  `DeleteGuard` leaves that rule out.
- One price unit per paid start, and no day rollover.
- Stopping is folded into Stopped. HTTP errors other than a refused start, a lost
  create and a failed runner delete are not modelled. A lost create of a machine boat
  never made behaves like a refused start with a longer wait.
- A stopped machine keeps its disk. The daemon's delete of a machine whose disk boat
  removed, and its cap on free starts per tick, are not modelled: the model makes one
  start per step.
- The listing lag is shorter than the `FixListLag` memory, the lost-create wait
  (`HTTP_TIMEOUT` plus one tick) and the restart wait. A lag in GitHub's runner list is
  not modelled.
- A stopped or deleted machine requeues its running job, where GitHub fails it. Either
  way the job leaves the runner.
- The daemon registers within a tick and GitHub assigns within seconds, so a machine
  that could serve a queued job does not age into `idle` or past `register_timeout_s`.

## Properties

Safety: `NoBusyStop`, `OneJobPerRunner`, `MaxBoxes`, `StartBudget` (boat's own count of
paid starts, across crashes), `NoDoubleFork` (no two live forks for one job key) and
`EphemeralIsolation` (in a fork-only pool, a runner takes a job only on a machine that
has run none). `ForkIsolation` asks the same of job forks in any placement.

Liveness: `NoLeak` (a job fork whose job is done or whose TTL passed is deleted),
`NoStarvation` (a queued job leaves the queue unless nothing is live and no start is
possible), `IdleStops`, and `Settles` (once arrivals and faults stop, the daemon has
nothing left to do). Every daemon action, time, boot, runner start-up and GitHub's
dispatcher are weakly fair; arrivals and faults are not.

TLC runs with `-deadlock`, which turns the deadlock check off: arrivals and faults are
bounded, so every behaviour ends in a state where nothing is enabled. A state stuck with
work left fails `NoStarvation` or `Settles`.

## Fix flags

Each `Fix*` constant turns on one rule from
[docs/design.md](../docs/design.md#observed-state). The code has all nine, and the main
models set all nine to TRUE. With a flag FALSE, TLC finds a counterexample, and the `tla`
workflow checks that it still does.

`FixCrashSpend` (crash-spend, `StartBudget`). `spent_today` lives only in
memory, so a restarted daemon starts from 0 and can pay for more starts than the cap
allows. Fix: after a restart, count the largest pool cap as spent until 00:00 UTC.

`FixStaleAct` (stale-act, `NoBusyStop`). A runner seen Idle takes a job between the
snapshot and the Stop or Delete. Fix: delete the machine's GitHub runner first; GitHub
refuses while it runs a job, and that or any other error skips the machine this tick.
The model's trace is a job fork past `fork_ttl_s`; the likelier real case, a job that
arrives just before an idle Stop, needs job cancellation, which the model leaves out.

`FixRegSeen` (reg-seen, `EphemeralIsolation`). A job fork's runner runs its job and
GitHub requeues the job before any Observe sees the runner busy, so the fork gets a
second runner. Fix: Register sets `last_busy` on the machine.

`FixCrashSeen` (crash-seen, `EphemeralIsolation`). A crash forgets that a fork had a
runner, and it gets another. Fix: after a restart, an Up job fork counts as used. A fork
that booted but never had a runner is then deleted, wasting its start.

`FixForkKey` (fork-key, `NoStarvation`). With the job as Idempotency-Key, a fork after
the job's first fork was deleted gets boat's answer for the old, deleted machine, and
the job stays queued. Fix: every fork request carries a fresh UUID v4 key. TLC does not
name a failing temporal property, so this model keeps `NoStarvation` alone.

`FixKeptInflight` (kept-inflight, `MaxBoxes`). A restarted daemon counts machines from
the listing, which does not show forks sent before the crash yet, so each crash can add
up to `max_boxes` machines. The model uses `MinUp = 1`, no stopped machine and
`Included = 3`, so the extra starts are free. Fix: start nothing until `HTTP_TIMEOUT`
after a restart. `FixListLag` does not help, because its memory is lost in the crash.

`FixListLag` (list-lag `MaxBoxes`, list-lag-fork `NoDoubleFork`). The next tick does not
see a fork boat has not listed yet, and forks its job again. Fix: remember each created
fork as a Starting machine until a listing shows it or `HTTP_TIMEOUT` plus one tick
passes. TLC stops at the first failing invariant, so each model keeps one of the two.

`FixLostCreate` (lost-create `MaxBoxes`, lost-create-fork `NoDoubleFork`). A create with
no definite answer (timeout, transport error, 5xx) may have made a machine the daemon
has no id for. Fix: start nothing for `HTTP_TIMEOUT` plus one tick, by which time boat
lists the machine if it exists.

`FixLostSpend` (lost-spend, `StartBudget`). boat counts the start of a machine it made
under a lost answer, and without the fix the daemon adds the price only when boat
answers with a machine. Fix: a create with no definite answer also adds its price to
`spent_today`. If boat made no machine, the daemon counts one start too many.

## Running TLC

TLC needs Java 21 and `tla2tools.jar` v1.7.4. From the repository root:

```sh
curl -fsSL -o tla2tools.jar \
  https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar
echo "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88  tla2tools.jar" \
  | shasum -a 256 -c -
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto -deadlock \
  -config spec/BoatRunner.cfg spec/BoatRunner.tla
```

Every other model is a base config with a sed edit, listed in the matrix of
`.github/workflows/tla.yml`:

```sh
sed -e "$EDIT" spec/BoatRunner.cfg > spec/Variant.cfg
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto -deadlock \
  -config spec/Variant.cfg spec/BoatRunner.tla
```

The workflow runs every model on a push to `main` that changes `spec/` or the workflow,
and on manual dispatch, not on pull requests. A passing model must exit 0; a
counterexample model must fail with the message in its `expect` field. The passing
models:

| Model | Base | Change | Distinct states |
| --- | --- | --- | --- |
| hybrid | `BoatRunner.cfg` | none | 18827 |
| hybrid-minup | `BoatRunner.cfg` | `MinUp = 1`, no stopped machine at the start | 40676 |
| warm | `BoatRunner.cfg` | placement `slot, resume` | 4131 |
| fork | `BoatRunnerFork.cfg` | none | 148353 |

The counterexample models stop at the first violation, and the number of states they
explore by then varies between runs.
