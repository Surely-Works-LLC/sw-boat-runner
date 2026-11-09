# BoatRunner TLA+ spec

`BoatRunner.tla` models one sw-boat-runner pool: GitHub's job queue and JIT
runners, boat's machines, and the reconciler in `docs/design.md` as `src/decide.rs`
and `src/run.rs` implement it on branch `integrate`. TLC checks it in CI (job `tla`).

## What is modelled

- **GitHub.** Jobs arrive (a bounded set; a re-run is another job key), wait in
  the queue, are assigned to an Idle runner, and finish. A JIT runner exits after
  its job. Runner states: Registered (JIT config issued, not online yet), Expired
  (Registered past `register_timeout_s`), Idle, Busy, Offline.
- **boat.** Machines are Starting, Up, Stopped or Gone. A stopped machine keeps
  its disk for a while (`keep`), so resuming it is free; otherwise a resume is a
  counted start. `starts` is boat's count of counted starts today; `Included`
  of them are free, and each one after that costs one price unit.
- **The tool**, one action per decide output, in decide's order: Unregister,
  Delete, Register (fresh job forks before free kept slots), Resume, Fork without
  a job (kept machine, for the `min_up` shortfall or, in a pool with resume and no
  fork, for demand), Fork for a job, Stop. Observe is run::observe updating
  `last_busy`. Every runner is JIT; a kept machine's slot gets a fresh Register
  after its runner finishes or goes offline.
- **The snapshot-to-act gap.** Stop and Delete are decided from one snapshot
  (`plan`) and executed by a later `Execute`, because run::execute calls boat with
  no fresh look at the runner. GitHub may assign a job in between. Other tool
  actions read the current state, and no new decision is made while a plan is
  pending, as in one tick.
- **Budget.** `spent_today` is the tool's in-memory `Limits.spent_today_cents`.
  Each start adds the quoted price; a reused job-fork key adds it although boat
  starts nothing. A start needs a price of 0, or `spent_today + price <=
  PaidBudget`, and fewer than `max_boxes` live machines.
- **Faults**, at most `MaxFaults` per behaviour: GitHub reassigns a running job
  (requeued, runner gone); boat loses a kept disk; boat refuses a start (429 or
  503); a JIT runner never comes online; an Idle runner goes offline; the tool
  crashes, possibly while boat creates a kept fork (`inflight`). A crash wipes the tool's memory: the unexecuted plan, `since`,
  `last_busy`, the online set, and `spent_today`. A kept fork in flight at the
  crash later lands (`Land`, a counted start) or fails (`Lost`), and the restarted
  tool cannot see it until it lands.
- **Time** is abstract. `age` is 0, 1 (past `register_timeout_s`) or 2 (past
  `fork_ttl_s`), counted from `since`, which restarts on every state change and
  on a crash. `idle` means `idle_stop_s` since `last_busy` and `min_up_s` since
  `since`. `keep` lapses after boat's 10 minutes.

## What is abstracted away

- One pool, one runner slot per machine (`slots_per_box = 1`), one label set.
  Routing between pools is out of scope.
- Prices are one unit per paid start; the budget is in those units. No day
  rollover: everything happens within one UTC day.
- Stopping is folded into Stopped. boat's list lag, HTTP errors other than a
  refused start, and the GitHub API's own failures are not modelled.
- A stopped or deleted machine requeues its running job; real GitHub fails it.
  Either way the job leaves the runner, which is what the properties need.
- Kept fork keys are a fresh UUID v4 per request, so they never collide and are
  not modelled; job fork keys are. An in-flight kept fork lands only on a free
  model id; with none free it can only fail.
- Timing assumptions, written into the time actions: the tool registers within a
  tick and GitHub assigns within seconds, both well inside `idle_stop_s` and
  `register_timeout_s`. So a kept machine that could serve a queued job does not
  age into idle, and a fresh fork with demand waiting does not age past
  `register_timeout_s`.

## Properties

Safety: `NoBusyStop`, `OneJobPerRunner`, `MaxBoxes`, `StartBudget` (boat's paid
starts within the cap, across crashes), `NoDoubleFork` (no two live machines for
one job key), `EphemeralIsolation` (in a fork pool, a runner that takes a job is
on a machine that has run none). `ForkIsolation` is the same for job forks in
any placement, which is stronger than the design asks.

Liveness, under weak fairness of every tool action, of time, boot and runner
start-up, and of GitHub's dispatcher; arrivals and faults get no fairness:
`NoLeak` (a job fork whose job is done or whose TTL passed is deleted),
`NoStarvation` (a queued job leaves the queue unless no start is possible and
nothing is live), `IdleStops`, and `Settles` (once arrivals and faults stop, the
tool eventually has nothing to do). No strong fairness is needed.

`-deadlock` turns TLC's deadlock check off. Arrivals and faults are bounded, so
every behaviour ends in a quiescent state where no action is enabled. A state
stuck with work left still fails `NoStarvation` or `Settles`.

## Fix flags

Each `Fix*` constant switches on one fix for a violation below. `FixCrashSpend`
is already in the code; the other five are proposals. The main models set all
six to TRUE, and CI also checks that each flag off still yields its
counterexample, so a model change that hides one fails CI.

| Flag | Fix |
| --- | --- |
| `FixCrashSpend` | after a restart, count the largest pool cap as spent until 00:00 UTC (in the code) |
| `FixStaleAct` | Stop and Delete first delete the machine's GitHub runner; GitHub refuses while it runs a job, and the tool then skips the machine |
| `FixRegSeen` | Register sets `last_busy` on a job fork, so a fork that had a runner is never fresh again |
| `FixCrashSeen` | after a restart, an Up job fork counts as used (it can be deleted, never re-registered) |
| `FixForkKey` | the job fork Idempotency-Key also names the fork attempt, so a fork after a Delete starts a machine |
| `FixKeptInflight` | after a restart, start nothing until boat's create timeout has passed, so a kept fork sent before the crash is listed or failed |

## Running TLC

CI runs, for each model, from the repository root:

```sh
sed -e "$EDIT" spec/BoatRunner.cfg > spec/Variant.cfg
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto -deadlock \
  -config spec/Variant.cfg spec/BoatRunner.tla
```

with `tla2tools.jar` v1.7.4 (sha256
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`). The matrix in
`.github/workflows/ci.yml` lists each model's sed edit. The passing models must
exit 0. The counterexample models must fail with the message given.

| Model | Base | Change | Distinct states | CI job |
| --- | --- | --- | --- | --- |
| hybrid | `BoatRunner.cfg` | none | 27869 | 1m19s |
| hybrid-minup | `BoatRunner.cfg` | `MinUp = 1`, no stopped machine at the start | see run annotation | 2m15s |
| warm | `BoatRunner.cfg` | placement `slot, resume` | 7918 | 30s |
| fork | `BoatRunnerFork.cfg` | none | see run annotation | 7m36s |

## Known violations

Each fix flag set to `FALSE` reproduces one design bug. CI checks every
counterexample, so a model change that hides one fails the build. The counts
are from TLC 2.19 on the CI runner (2 workers). With 2 workers, the count at a
violation varies a little between runs.

### crash-spend: StartBudget

`FixCrashSpend = FALSE`. TLC: `Invariant StartBudget is violated.` (708
distinct states, 3 s.)

Trace: job j1 arrives. The included free start resumes m1. j2 arrives. j1 is
forked onto m3, a paid start (`spent_today` 1). m3 boots, then the daemon
crashes and restarts with `spent_today` 0. Under the seen fix the crash marks m3
used, so it is deleted. j1 is still queued and is forked again onto m2, which
makes 2 paid starts against a budget of 1.

Cause: `spent_today` lives only in memory. A restart forgets the starts it has
already paid for today.

Fix: after a crash, set `spent_today` to the daily cap until the next day, or
persist it. The model sets it to `PaidBudget`.

### stale-act: NoBusyStop

`FixStaleAct = FALSE`. TLC: `Action property NoBusyStop is violated.` (2804
distinct states, 10 s.)

Trace: j1 is forked onto m2. The runner registers and comes online Idle while
j1 is still queued. m2 ages past `fork_ttl_s` and the plan deletes it. Before
the delete runs, GitHub assigns j1 to the runner, so the Busy machine is deleted.

Cause: the plan acts on a snapshot. A runner can turn Busy between the snapshot
and the boat call.

Fix: re-read the runner's state just before each Stop or Delete, and skip it if
the runner is Busy.

Caveat: this trace needs GitHub to leave an Idle runner unassigned for
`fork_ttl_s`. The likely real case is an idle Stop with a job that arrives
between the snapshot and `boat.stop`. The model cannot reach that case, because
it does not model job cancellation, so an Idle runner never meets an empty
queue. The fix covers both cases.

### reg-seen: EphemeralIsolation

`FixRegSeen = FALSE`, fork model. TLC: `Action property EphemeralIsolation is
violated.` (7151 distinct states, 12 s.)

Trace: j1 arrives and is forked onto m1. The runner registers, comes online and
runs j1. The runner then goes away and GitHub requeues j1 before any Observe
records that the fork ran a job. The next tick registers a new runner on m1, and
j1 is assigned to the used fork.

Cause: `seen` is set only by Observe. Nothing records "this fork had a runner"
at the moment of registration.

Fix: set `seen` when the daemon registers a runner on an ephemeral fork, and
never register a second runner on a fork that is marked seen.

### crash-seen: EphemeralIsolation

`FixCrashSeen = FALSE`, fork model. TLC: `Action property EphemeralIsolation
is violated.` (9192 distinct states, 9 s.)

Trace: as in reg-seen, except the fork was already marked seen, and a crash
reset `seen` to empty. The restarted daemon registers a new runner on the used
fork, and the job runs on it.

Cause: `seen` lives only in memory.

Fix: after a crash, treat every Up ephemeral fork as seen, so it is deleted
rather than reused, or persist `seen`.

Cost of this fix: a fork that booted but never had a runner is deleted too,
while its job may still be queued. That wastes a paid start (the crash-spend
trace shows it). A finer rule would keep a fork with no runner record on
GitHub, but that needs the runner list to be fresh, and the model does not
model list lag.

### fork-key: NoStarvation

`FixForkKey = FALSE`, fork model, `NoStarvation` as the only property (TLC
does not name the failing temporal property). TLC: `Temporal properties were
violated.` (5143 distinct states, 4 s.)

Trace: j1 is forked onto m1. The runner never comes online, so it is
unregistered and m1 is deleted. The next Fork for j1 reuses the same
idempotency key. Boat returns the old, deleted fork, and the daemon pays for
nothing new. j1 stays queued forever.

Cause: the fork key comes only from the job, so a retry after a failed fork
gets the old answer.

Fix: put an attempt counter in the key, for example `job-attempt`, and bump it
whenever the previous fork for that job is gone.

### kept-inflight: MaxBoxes

`FixKeptInflight = FALSE`, hybrid model with `MinUp = 1`, no stopped machine at
the start and `Included = 2`. TLC: `Invariant MaxBoxes is violated.` (Counts in the CI run annotation.)

Trace: the pool is short of `min_up`, so the tool sends a free kept fork. The
daemon crashes before boat lists the machine. The restarted daemon sees no kept
machine and forks another, free. j1 and j2 arrive and j1 is forked, free, which
fills `max_boxes` = 2. Then the first kept fork lands: 3 live machines.

Cause: a kept fork's key is a fresh UUID v4, so the restarted daemon cannot find
the create again, and it counts boxes from boat's listing, which does not show
the machine yet. Every start it makes before the orphan lands is counted against
too few boxes. With `PaidBudget = 0` the same stale count also breaks
`StartBudget` (argued, not checked in CI): with `Included = 1` the orphan and the
restarted daemon's kept fork are both priced free, and boat counts one as paid. With a paid
budget of at least one start the cap absorbs it, because an orphan priced free
means no paid start came before it.

Fix: after a restart, make no start until boat's create timeout has passed, so
any kept fork sent before the crash is listed or has failed. The model blocks
every start while `inflight` holds. Alternatives: persist the pending kept-fork
keys, or derive the kept key from the pool and the day (`<pool>-keep-<day>-<n>`)
so a retry returns the same machine, which brings back key reuse after a delete.

## Design notes

These are the places where `docs/design.md` is ambiguous or where the model
picks one reading.

- `fork_ttl_s` counts from the machine's `since`, not from the runner's last
  job. The model ages a machine only while no job is waiting for it.
- `min_up` counts kept machines only, not forks.
- NoDoubleFork is checked at one time: no two live forks serve the same job.
- The refused status codes are 409, 429 and 503 in one place and 429 and 503 in
  another. The model treats every refusal the same way.
- The day never rolls over, so the budget is spent at most once per run.
- `max_boxes` is not a hard bound across a restart: see kept-inflight. design.md
  says the extra kept machine is "bounded by `max_boxes`"; it is bounded by
  `max_boxes + 1` unless the restart waits for in-flight creates.
- The race where the runner list lags behind boat is not modelled.
- Kept forks ignore `placement`.
- A free resume of a kept machine counts as a start, even when it costs nothing.
- Drop requeues a job while GitHub marks it failed. The model requeues it.
- The quote is re-read after each start.
- Checked against `integrate` 8c5f6d9: the restart seeds `spent_cents` with the
  largest pool cap (`FixCrashSpend`), a pool without resume deletes a Stopped
  kept machine with no Busy runner (`DeleteGuard`), and in a pool with resume and
  no fork the kept-fork loop also runs for demand within `max_boxes` and the
  budget (`ForkKeptGuard`). The model already matched all three.
- The model's Stop exclusion (no Stop while any demand is unmet) is coarser
  than the code's, which compares per label.

## Base code

On commit `922a3b7`, the `rust` job failed `cargo fmt --check` in
`src/model.rs`. This branch merges `integrate` 8c5f6d9, which carries the
formatting fix (`0302c82 model: rustfmt`), and does not touch Rust code itself.
