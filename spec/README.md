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
- **boat's listing lag.** boat lists a created machine some time after the
  create returns (`lag`). Until then the machine is `Hidden` from the tool's
  snapshot, unless the tool remembers creating it (`Known`, the FixListLag
  rule). A crash turns every unlisted machine into an `Orphan`: the memory is
  gone, and every fork key is a fresh UUID v4, so the tool cannot find the
  create again. The listing catches up by the time the machine boots.
- **Faults**, at most `MaxFaults` per behaviour: GitHub reassigns a running job
  (requeued, runner gone); boat loses a kept disk; boat refuses a start (429 or
  503); a JIT runner never comes online; an Idle runner goes offline; GitHub
  fails a runner delete before a Stop or Delete (`RefuseUnregister`); the tool
  crashes. A crash wipes the tool's memory: the unexecuted plan, `since`,
  `last_busy`, the online set, `spent_today`, and the forks it created that boat
  has not listed yet.
- **Time** is abstract. `age` is 0, 1 (past `register_timeout_s`) or 2 (past
  `fork_ttl_s`), counted from `since`, which restarts on every state change and
  on a crash. `idle` means `idle_stop_s` since `last_busy` and `min_up_s` since
  `since`. `keep` lapses after boat's 10 minutes.

## What is abstracted away

- One pool, one runner slot per machine (`slots_per_box = 1`), one label set.
  Routing between pools is out of scope.
- Prices are one unit per paid start; the budget is in those units. No day
  rollover: everything happens within one UTC day.
- Stopping is folded into Stopped. HTTP errors other than a refused start and
  a refused runner delete are not modelled.
- The listing lag ends by boot, and is shorter than both the FixListLag memory
  timeout and the restart wait. An Up machine missing from the listing would
  only delay its Register, because the tool registers runners on listed
  machines alone. A resumed machine stays listed. A create still in flight at a
  crash is an Orphan created a moment earlier: boat counts its start either way.
- A stopped or deleted machine requeues its running job; real GitHub fails it.
  Either way the job leaves the runner, which is what the properties need.
- Fork keys are a fresh UUID v4 per request, so they never collide; the model
  keeps `keys` only to reproduce fork-key with `FixForkKey` off.
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

Each `Fix*` constant switches on one fix for a violation below. `integrate`
e351752 has the first six; the core agent is adding `FixListLag`. The main
models set all seven to TRUE, and CI also checks that each flag off still
yields its counterexample, so a model change that hides one fails CI.

| Flag | Fix |
| --- | --- |
| `FixCrashSpend` | after a restart, count the largest pool cap as spent until 00:00 UTC (in the code) |
| `FixStaleAct` | Stop and Delete first delete the machine's GitHub runner. GitHub refuses while it runs a job, and any other non-fatal error fails the delete too; the tool then skips the machine this tick |
| `FixRegSeen` | Register sets `last_busy` on every machine: a job fork that had a runner is never fresh again, and a kept machine's idle timer restarts |
| `FixCrashSeen` | after a restart, an Up job fork counts as used (it can be deleted, never re-registered) |
| `FixForkKey` | the job fork Idempotency-Key also names the fork attempt, so a fork after a Delete starts a machine |
| `FixKeptInflight` | after a restart, start nothing until boat's create timeout has passed, so every fork sent before the crash, kept or job, is listed or failed |
| `FixListLag` | the tool remembers each fork it created until a listing shows it or a timeout passes, and counts it as a Starting machine |

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
| hybrid | `BoatRunner.cfg` | none | see run annotation | |
| hybrid-minup | `BoatRunner.cfg` | `MinUp = 1`, no stopped machine at the start | see run annotation | |
| warm | `BoatRunner.cfg` | placement `slot, resume` | see run annotation | |
| fork | `BoatRunnerFork.cfg` | none | see run annotation | |

## Known violations

Each fix flag set to `FALSE` reproduces one design bug. CI checks every
counterexample, so a model change that hides one fails the build. The counts
are from TLC 2.19 on the CI runner (2 workers). With 2 workers, the count at a
violation varies a little between runs.

### crash-spend: StartBudget

`FixCrashSpend = FALSE`. TLC: `Invariant StartBudget is violated.` (698
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

`FixStaleAct = FALSE`. TLC: `Action property NoBusyStop is violated.` (2714
distinct states, 9 s.)

Trace: j1 is forked onto m2. The runner registers and comes online Idle while
j1 is still queued. m2 ages past `fork_ttl_s` and the plan deletes it. Before
the delete runs, GitHub assigns j1 to the runner, so the Busy machine is deleted.

Cause: the plan acts on a snapshot. A runner can turn Busy between the snapshot
and the boat call.

Fix: delete the machine's GitHub runner just before each Stop or Delete.
GitHub refuses while the runner is Busy. On that or any other non-fatal error
(network, rate limit, 5xx) the tool skips the machine this tick; the model
refuses Busy every time and any other runner nondeterministically, as a fault
(`RefuseUnregister`). A successful delete leaves the machine with no runner.

Caveat: this trace needs GitHub to leave an Idle runner unassigned for
`fork_ttl_s`. The likely real case is an idle Stop with a job that arrives
between the snapshot and `boat.stop`. The model cannot reach that case, because
it does not model job cancellation, so an Idle runner never meets an empty
queue. The fix covers both cases.

### reg-seen: EphemeralIsolation

`FixRegSeen = FALSE`, fork model. TLC: `Action property EphemeralIsolation is
violated.` (7152 distinct states, 12 s.)

Trace: j1 arrives and is forked onto m1. The runner registers, comes online and
runs j1. The runner then goes away and GitHub requeues j1 before any Observe
records that the fork ran a job. The next tick registers a new runner on m1, and
j1 is assigned to the used fork.

Cause: `seen` is set only by Observe. Nothing records "this fork had a runner"
at the moment of registration.

Fix: set `last_busy` when the daemon registers a runner on any machine. On an
ephemeral fork that marks it seen, and a fork marked seen never gets a second
runner. On a kept machine it restarts the idle timer, which the model shows as
`idle` going back to FALSE.

### crash-seen: EphemeralIsolation

`FixCrashSeen = FALSE`, fork model. TLC: `Action property EphemeralIsolation
is violated.` (8837 distinct states, 9 s.)

Trace: as in reg-seen, except the fork was already marked seen, and a crash
reset `seen` to empty. The restarted daemon registers a new runner on the used
fork, and the job runs on it.

Cause: `seen` lives only in memory.

Fix: after a crash, treat every Up ephemeral fork as seen, so it is deleted
rather than reused, or persist `seen`.

Cost of this fix: a fork that booted but never had a runner is deleted too,
while its job may still be queued. That wastes a paid start (the crash-spend
trace shows it). A finer rule would keep a fork with no runner record on
GitHub, but that needs GitHub's runner list to be fresh, and the model does not
model that list's lag.

### fork-key: NoStarvation

`FixForkKey = FALSE`, fork model, `NoStarvation` as the only property (TLC
does not name the failing temporal property). TLC: `Temporal properties were
violated.` (2933 distinct states, 6 s.)

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
the start and `Included = 3`. TLC: `Invariant MaxBoxes is violated.`

A trace that reaches it (TLC's own trace is in the job log): the pool is short
of `min_up`, so the tool forks a free kept machine. The daemon crashes before
boat lists it. The restarted daemon sees no kept machine and forks another,
then crashes again before boat lists that one. The third daemon forks a third
kept machine: 3 live machines against `max_boxes` = 2. A crash that orphans a
job fork lets the daemon fork the job again, which breaks `NoDoubleFork` too.

Cause: every fork key is a fresh UUID v4, so the restarted daemon cannot find
a create again, and it counts boxes from boat's listing, which does not show
the machine yet. Every start it makes before the orphans are listed is counted
against too few boxes. The third start must be free, because `FixCrashSpend`
blocks paid starts after the crash. With `PaidBudget = 0` the same stale count
also breaks `StartBudget` (argued, not checked in CI): an orphan and the
restarted daemon's fork are both priced free, and boat counts one as paid.

Fix: after a restart, make no start until boat's create timeout and the
listing lag have passed, so every fork sent before the crash, kept or job, is
listed or has failed. The model blocks every start while an `Orphan` exists.
`FixListLag` does not help here: its memory is lost in the crash.

### list-lag: MaxBoxes and NoDoubleFork

`FixListLag = FALSE`, fork model. CI checks the two invariants in separate
models, because TLC stops at the first one that fails.

- list-lag, without `NoDoubleFork`: `Invariant MaxBoxes is violated.`
- list-lag-fork, without `MaxBoxes`: `Invariant NoDoubleFork is violated.`

Trace: j1 arrives and is forked onto m1. On the next tick boat's listing does
not show m1 yet, so the tool sees no live machine, no pending fork and no fork
for j1, and forks j1 again onto m2: two live forks for one job. A third tick
forks j1 onto m3: 3 live machines against `max_boxes` = 2. No crash is needed.

Cause: decide counts boxes, pending forks and the job's existing fork from the
listing alone, and boat lists a machine some time after the create returns.

Fix: remember each fork the loop created until a listing shows it or a timeout
longer than the listing lag passes, and count it as a Starting machine. The
model's `Known` state is that memory; with it on both invariants hold.

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
- `max_boxes` is not a hard bound across a restart: see kept-inflight. Without
  the restart wait, each crash can add up to `max_boxes` orphans.
- NoDoubleFork already states "at most one live fork per job": ephemeral
  machines are never stopped, so not Gone means live. The list-lag models use
  it rather than a second copy.
- The race where GitHub's runner list lags behind boat is not modelled.
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
- Checked against `integrate` e351752: a non-fatal runner-delete error skips
  the Stop or Delete (`RefuseUnregister`), Register marks every machine used
  (`FixRegSeen`), every fork gets a fresh UUID v4 key, so a crash orphans job
  forks as well as kept ones, and the restart wait covers both
  (`FixKeptInflight`). `CrashInFork`, `Land`, `Lost` and `inflight` are gone;
  `lag` models both the in-flight create and the listing lag.

## Base code

This branch merges `integrate` e351752 and does not touch Rust code itself.
