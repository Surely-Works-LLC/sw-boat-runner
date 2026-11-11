# Design

sw-boat-runner keeps self-hosted GitHub Actions runners on [boat](https://boat.dev) machines,
so jobs reuse machines instead of starting a new one per job.

## Why

boat's managed `runs-on: boat` starts a fresh machine for every job. A start is a create,
fork or resume. Each plan [includes](https://docs.boat.dev/limits.md) a number of starts
per minute, hour and day, from 200 a day on the $20 plan to 2,400 on the $2000 plan, and
starts beyond that cost $0.03 to $0.07 each. A busy repository runs thousands of jobs
a day, so the managed runner can cost more in starts than in machine time. See
[eval/](../eval/) for a measured case.

A runner on a machine that stays up takes jobs without a start. Each pool's strategy
decides how many machines to keep up, when to stop them, and when a job needs a fresh
machine.

## Strategies

A pool is a set of boat machines with the same type and the same runner labels.
Its `placement` lists, in order, the ways to find a runner for a queued job:

| Step | What it does | Costs a start |
| - | - | - |
| `slot` | Register a runner on a free slot of a machine that is already up. | no |
| `resume` | Resume a stopped pool machine, then use its slot. | yes, unless boat returns the kept machine (within 10 min of the stop) |
| `fork` | Fork a new machine from the pool's saved snapshot for this one job, and delete it after. | yes |

The first step that fits is used. If none fits, the job waits in GitHub's queue and the
next tick tries again. Presets:

| Preset | `placement` | Idle machines | Fits |
| - | - | - | - |
| `pool` | `slot` | `min_up` always up | steady load, trusted code, lowest overhead |
| `warm` | `slot, resume` | stopped after `idle_stop_s`; demand no stopped machine covers forks a kept one | load with quiet hours |
| `fork` | `fork` | none | untrusted code (fork PRs), rare jobs |
| `hybrid` | `slot, resume, fork` | `min_up` always up; a job their slots cannot take gets its own fork | bursts above `min_up` |

`hybrid` is the default. Only `warm` forks kept machines for demand; `hybrid` forks them
only up to `min_up`, so it stops only kept machines left above a lowered `min_up`. Steps
keep the order `slot`, `resume`, `fork`, and `resume` and `min_up` need `slot`, the only
step that registers runners on kept machines.

Every runner is a just-in-time runner: it is registered through GitHub's JIT config,
takes one job and exits. A long-lived runner would need a registration token on the
machine, and boat stores every command for 90 days, so the token would end up in boat's
command history. On a kept machine, a slot whose runner finished is free again, and a
slot whose runner went `Offline` gets `Unregister`. Either slot takes a fresh `Register`
in the same tick when a job is waiting. The runner's work directory stays on the
machine, so its caches survive from job to job.

## Start budget

Every action that costs a start passes the pool's `paid_start_cents_per_day` check first:

- `0` (default): start only while boat's `GET /limits` reports `nextStartDollars == 0`,
  so only included starts are used.
- `N > 0`: also allow paid starts while the cents spent on starts today, plus the price
  of this start, stay at or under N.

`GET /limits` quotes one price, for the next start, so `decide` prices every start of a
tick at that quote. At a quote of 0 it plans no more starts than the fewest left among
boat's minute, hour and day windows, because the price past them is not known until the
next tick. After each accepted
start the daemon reads `GET /limits` again and makes no more starts that tick if the
price changed. One account serves every pool, so each pool's cap is checked against the
account-wide spend, including the starts that pools earlier in the file planned in the
same tick.

The cents spent today are counted in memory. A restart cannot recover them, so after a
restart the daemon sets them to the largest pool cap: every pool makes only included
starts until 00:00 UTC. A crash can delay paid starts but never exceed a cap.

boat never queues a start. A refused start (429, or 503 with code `no_ready_machine` or
`out_of_capacity`) is not counted, and the daemon makes no start until its `Retry-After`
has passed.

Stopping an idle machine saves money only when the expected idle gap is longer than
`60 × start price ÷ hourly price` minutes. For a `large` machine ($0.072/h) that is 0 at
an included start, 25 min at $0.03 and 58 min at $0.07. `idle_stop_s` should sit above
that gap.

## Observed state

The daemon keeps no database. Each tick it reads:

- boat: `GET /sandboxes?label=sw-boat-runner=<pool>` and `GET /limits`;
- GitHub: the repository's runners whose names start with `sbr-<pool>-`, and the queued
  jobs. Before `decide` runs, each queued job goes only to the first pool in the file
  whose labels match it.

Runner names carry what the daemon needs to know about them:
`sbr-<pool>-<machine id>-<slot>-<registered unix time>`. A machine forked for a job
carries the label `sw-boat-runner-job=<job id>-<attempt>`, which `decide` matches to make
at most one fork per job. A kept machine is forked with no job label to bring the pool up
to `min_up`, or, in a pool with `resume` and no `fork`, for demand that no stopped machine
covers. A kept machine gets boat's longest TTL, 30 days, after which boat archives it.
boat may delete the disk of a machine that has been stopped for 14 days. A `Resume` of one
fails with `409 snapshot_data_expired`; the daemon then deletes the machine, and the pool
forks a new one if it still needs it.

Every fork request's `Idempotency-Key` is a fresh UUID v4. boat answers a key it saw in
the last 24 h with its first machine, even a deleted one, so a reused key would hand a
job the dead machine after a `Delete` and the job would starve. Fresh keys cannot stop a
second fork for one job; the listing-lag, lost-create and restart rules below do.

The daemon keeps a few facts in memory only: when each machine changed state and last had
a busy or registered runner, which runners it has seen online, the forks boat has not
listed yet, the cents spent on starts today, and the time before which it makes no start.
A machine first seen counts as never busy, which can only delay a stop. The rules below
keep the daemon safe on a stale snapshot and across a restart. Each is a `Fix*` flag in
`spec/BoatRunner.tla`, and [spec/README.md](../spec/README.md#fix-flags) describes the
counterexample TLC finds without it.

- Stale act (`FixStaleAct`): `Stop` and `Delete` first delete the machine's GitHub
  runners. GitHub refuses while a runner runs a job, which it may have taken since the
  snapshot. On that or any other error the daemon skips the machine this tick.
- Register marks used (`FixRegSeen`): a `Register` counts the machine as busy at once, so
  a requeued job cannot put a second runner on a used ephemeral machine.
- Up fork after a restart is used (`FixCrashSeen`): an `Up` job fork first seen after a
  restart counts as busy. A fork that never got a runner is deleted, wasting one start.
- Fresh fork key (`FixForkKey`): see above.
- Restart wait (`FixKeptInflight`): after a restart the daemon makes no start until the
  HTTP request timeout (90 s) has passed, so a create sent before the crash is listed or
  has failed. Without it, each restart can start machines next to ones boat has not
  listed yet, passing `max_boxes` and forking a job twice.
- Listing lag (`FixListLag`): the daemon keeps each fork boat accepted, with its pool and
  job, until a listing shows it or the HTTP request timeout plus one tick has passed.
  Until then `decide` counts it as a `Starting` machine, so boat's lag neither forks its
  job again nor passes `max_boxes`. An entry that expires unlisted is dropped with a
  warning.
- Lost create (`FixLostCreate`): a create that fails without a definite answer (a
  transport error, a timeout, a 5xx that is not a refusal, or an unreadable 2xx body)
  may still have started a machine. The daemon then makes no start until the HTTP
  request timeout plus one tick has passed, so the machine is listed with its job label
  or never existed. A refused start waits its `Retry-After` instead, and any other 4xx
  does not wait.
- Lost spend (`FixLostSpend`): such a create also adds its quoted price to today's spend,
  because boat may have charged for it. A refusal adds nothing.
- Restart spend (`FixCrashSpend`): see [Start budget](#start-budget).

## State machine

The model shared by `src/decide.rs` and `spec/BoatRunner.tla`.

Machine states, mapped from boat's states:

| State | boat states |
| - | - |
| `Starting` | `init`, `provisioning`, `provisioned`, `cloning`, and any state boat adds later |
| `Up` | `ready`, `idle`, `running` |
| `Stopping` | `archiving` |
| `Stopped` | `archived` |
| `Gone` | `error`, `cancelled`, `deleted` |

Runner states, mapped from GitHub's `status` and `busy`:

| State | GitHub |
| - | - |
| `Busy` | `busy` |
| `Idle` | `online`, not busy |
| `Offline` | `offline`, after this process saw it `online` |
| `Registered` | `offline` and never seen `online`: JIT config issued, not started yet |

A `Registered` runner that never comes online times out `register_timeout_s` after the
registration time in its name. After a restart, a runner that went offline before the
process saw it online is `Registered` until that timeout.

`decide(snapshot, policy) -> [Action]` is a pure function. Actions:

| Action | Guard |
| - | - |
| `Register(machine, slot)` | demand above 0, machine `Up`, and either an ephemeral machine with no runner seen since it came up (slot 0, filled first), or `slot` in placement and a free slot on a kept machine (a slot whose runner gets `Unregister` this tick counts as free) |
| `Resume(machine)` | machine `Stopped`, `resume` in placement, start budget allows, `Up + Starting < max_boxes`, and either unmet demand or kept `Up + Starting < min_up` |
| `Fork(job)` | unmet demand left after resumes and kept forks, `fork` in placement, start budget allows, `Up + Starting < max_boxes`, no machine in any pool that is not `Gone` carries this job's label |
| `Fork(none)` | kept `Up + Starting`, plus resumes this tick, plus kept `Stopping` machines in a pool with `resume`, below `min_up`; or, in a pool with `resume` and no `fork`, unmet demand left after every `Stopped` kept machine is resumed; start budget allows, `Up + Starting < max_boxes` |
| `Stop(machine)` | machine `Up`, not ephemeral, `resume` in placement, no unmet demand left after this tick's starts, no `Busy` or `Registered` runner and no `Register` this tick, idle for `idle_stop_s`, up for `min_up_s`, more than `min_up` kept machines `Up` after this tick's other stops |
| `Delete(machine)` | ephemeral machine with no `Busy` runner whose job finished, or past `fork_ttl_s`; or a `Stopped` kept machine with no `Busy` runner in a pool without `resume`, which nothing would resume |
| `Unregister(runner)` | `Registered` past `register_timeout_s`, `Offline`, or runner on a machine that is not `Up` |

Demand for a pool is the number of queued jobs whose labels it matches, minus its runners
in `Registered` or `Idle` that keep their slot this tick. Unmet demand is what is left
after this tick's registers and the capacity of the pool's `Starting` machines (one for an
ephemeral machine, `slots_per_box` for any other). A job matches a pool when the job's
labels, compared without case, are a subset of the pool's. A JIT runner carries only the
pool's labels and self-hosted jobs ask for `self-hosted`, so the policy rejects a pool
without it. A job that matches several pools counts only for the first one in the file.

Stop needs `resume` in placement because without it a stopped machine never comes back.
A `Busy` runner keeps its machine even past `fork_ttl_s`, which counts from the machine's
last state change. The policy accepts `fork_ttl_s` from 1 to 2592000 (30 days, boat's
longest TTL) and rejects anything else rather than clamping it.

An ephemeral machine's job has finished when the machine is `Stopped`, or when it is `Up`
with no live runner and either a runner was seen on it or it has been `Up` longer than
`register_timeout_s`.

## Properties

Safety, checked by TLC. `NoBusyStop`, `MaxBoxes`, `StartBudget`, `NoDoubleFork` and
`EphemeralIsolation` also have property tests on one tick of `decide`.

1. `NoBusyStop`: no `Stop` or `Delete` of a machine with a `Busy` runner.
2. `OneJobPerRunner`: a JIT runner runs at most one job.
3. `MaxBoxes`: machines in `Up` or `Starting` never exceed `max_boxes`.
4. `StartBudget`: paid starts stay within `paid_start_cents_per_day`, so none when it is 0.
5. `NoDoubleFork`: at most one live job fork per job id and attempt.
6. `EphemeralIsolation`: a pool whose placement is only `fork` never runs two jobs on one
   machine.

Liveness, checked by TLC under weak fairness of the daemon's actions and of time, boot,
runner start-up and GitHub's dispatcher:

1. `NoStarvation`: a queued job that some placement step allows eventually runs, once the
   budget and `max_boxes` allow it.
2. `IdleStops`: in `warm` and `hybrid`, an idle machine above `min_up` is eventually
   stopped.
3. `Settles`: with no new jobs, the system reaches a state where `decide` returns nothing.
4. `NoLeak`: every ephemeral machine is deleted after its job ends or its TTL passes.

The spec also models the faults the daemon must survive, among them a refused start, a
create whose answer is lost, a lost kept disk, and a crash between any two actions.
[spec/README.md](../spec/README.md#what-is-modelled) lists them all.

## Out of scope

Kubernetes, webhooks, several daemon instances against one pool, providers other than
boat, and updating runner binaries in place (rebuild the pool's snapshot instead).
