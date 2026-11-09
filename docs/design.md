# Design

sw-boat-runner keeps self-hosted GitHub Actions runners on [boat](https://boat.dev) machines,
so jobs reuse machines instead of paying for a new one per job.

## Why

boat's managed `runs-on: boat` starts a fresh machine for every job. A start is a create,
fork or resume. Each plan includes a number of starts per minute, hour and day, and
starts beyond that cost $0.03 to $0.07 each. A busy repository runs thousands of jobs
a day, so the managed runner can cost more in starts than in machine time. See
[eval/](../eval/) for a measured case.

A runner that stays on a running machine takes jobs without a start. The open questions
are how many machines to keep up, when to stop them, and when a job needs a fresh machine.
Each pool answers them through its strategy.

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
| `hybrid` | `slot, resume, fork` | stopped after `idle_stop_s` | bursts above the pool size |

`hybrid` is the default. `min_up` needs `slot`, the only step that registers runners on
kept machines.

Every runner is a just-in-time runner: it is registered through GitHub's JIT config,
takes one job and exits. A long-lived runner would need a registration token on the
machine, and boat stores every command for 90 days, so the token would end up in boat's
command history. On a kept machine, a slot whose runner finished or went `Offline` gets
`Unregister`, and then a fresh `Register` for the next queued job, in the same tick when
one is waiting. The runner's work directory
stays on the machine, so its caches survive from job to job.

## Start budget

Every action that costs a start passes the pool's `paid_start_cents_per_day` check first:

- `0` (default): start only while boat's `GET /limits` reports `nextStartDollars == 0`,
  so only included starts are used.
- `N > 0`: also allow paid starts while the cents spent on starts today, plus the price
  of this start, stay at or under N.

`GET /limits` reports the next start's price but not how many included starts are left,
so `decide` prices every start of a tick at the quote it was shown. After each accepted
start the tool reads `GET /limits` again and makes no more starts that tick if the price
changed. One account serves every pool, so each pool's cap is checked against the
account-wide spend, including the starts that pools earlier in the file planned in the
same tick.

The cents spent today are counted in memory. A restart cannot recover them, so after a
restart the tool sets them to the largest pool cap: every pool's budget counts as spent,
and every pool makes only included starts until 00:00 UTC. A crash can delay paid starts
but never exceed a cap.

boat never queues a start. A refused start (429 or 503) is not counted, and the tool
retries it on a later tick.

Stopping an idle machine saves money only when the expected idle gap is longer than
`60 × start price ÷ hourly price` minutes. For a `large` machine ($0.072/h) that is 0 at
an included start, 25 min at $0.03 and 58 min at $0.07. `idle_stop_s` should sit above
that gap.

## Observed state

The tool keeps no database. Each tick it reads:

- boat: `GET /sandboxes?label=sw-boat-runner=<pool>` and `GET /limits`;
- GitHub: the repository's runners whose names start with `sbr-<pool>-`, and the queued
  jobs. Before `decide` runs, each queued job goes only to the first pool in the file
  whose labels match it.

Runner names carry what the tool needs to know about them:
`sbr-<pool>-<machine id>-<slot>-<registered unix time>`. A machine forked for a job
carries the label `sw-boat-runner-job=<job id>-<attempt>`, which `decide` matches to make
at most one fork per job. A kept machine is forked with no job label to bring the pool up
to `min_up`, or, in a pool with `resume` and no `fork`, for demand that no stopped machine
covers. A kept machine gets boat's longest TTL, 30 days, after which boat archives it.

Every fork request's `Idempotency-Key` is a fresh UUID v4. boat answers a key it saw in
the last 24 h with its first machine, even a deleted one, so a reused key would hand a
job the dead machine after a `Delete` and the job would starve. Fresh keys never repeat.
They cannot stop a second fork for one job; the listing-lag and restart rules below do.

Two facts are kept only in memory: when each machine last had a busy runner, and the
cents spent on starts today (see [Start budget](#start-budget)). A machine first seen
counts as never busy, which can only delay a stop. Restarting the process loses nothing
else. Rules that keep the tool safe on a stale snapshot and across a restart, each a
`Fix*` flag in `spec/BoatRunner.tla`:

- Stale act (`FixStaleAct`): `Stop` and `Delete` first delete the machine's GitHub
  runners. If GitHub refuses, because a runner took a job since the snapshot, the tool
  skips that machine this tick.
- Register marks used (`FixRegSeen`): a `Register` counts the machine as busy at once, so
  a requeued job cannot put a second runner on a used ephemeral machine.
- Up fork after a restart is used (`FixCrashSeen`): an `Up` job fork first seen after a
  restart counts as busy. A fork that never got a runner is deleted, wasting one start.
- Fresh fork key (`FixForkKey`): see above.
- Restart wait (`FixKeptInflight`): after a restart the tool makes no start until the
  HTTP request timeout (90 s) has passed, so a create sent before the crash is listed or
  has failed. Without it, kept machines could reach `max_boxes + 1`.
- Listing lag (`FixListLag`): the tool keeps each fork boat accepted, with its pool and
  job, until a listing shows it or the HTTP request timeout plus one tick has passed.
  Until then `decide` counts it as a `Starting` machine, so boat's lag neither forks its
  job again nor passes `max_boxes`. An entry that expires unlisted is dropped with a
  warning.
- Restart spend (`FixCrashSpend`): after a restart every pool's budget counts as spent
  until 00:00 UTC.

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
| `Register(machine, slot)` | machine `Up`, slot free (a slot whose runner gets `Unregister` this tick counts as free), unmet demand for the pool's labels |
| `Resume(machine)` | machine `Stopped`, `resume` in placement, start budget allows, `Up + Starting < max_boxes`, and either unmet demand left after free slots or kept `Up + Starting < min_up`; a pool without `fork` forks a kept machine for the demand left once none is `Stopped` |
| `Fork(job)` | unmet demand left after slots and resumes, `fork` in placement, start budget allows, `Up + Starting < max_boxes`, no machine with this job's label yet |
| `Fork(none)` | kept `Up + Starting`, plus resumes this tick, plus kept `Stopping` machines in a pool with `resume`, below `min_up`; or, in a pool with `resume` and no `fork`, unmet demand left after every `Stopped` kept machine is resumed; start budget allows, `Up + Starting < max_boxes` |
| `Stop(machine)` | machine `Up`, not ephemeral, `resume` in placement, no unmet demand, no `Busy` or `Registered` runner, idle for `idle_stop_s`, up for `min_up_s`, more than `min_up` machines `Up` |
| `Delete(machine)` | ephemeral machine with no `Busy` runner whose job finished, or past `fork_ttl_s`; or a `Stopped` kept machine with no `Busy` runner in a pool without `resume`, which nothing would resume |
| `Unregister(runner)` | `Registered` past `register_timeout_s`, `Offline`, or runner on a machine that is not `Up` |

Demand for a pool is the number of queued jobs whose labels it matches, minus its runners
in `Registered` or `Idle`, minus the capacity of its `Starting` machines (one for an
ephemeral machine, `slots_per_box` for any other). A job matches a pool when the job's
labels, compared without case, are a subset of the pool's. A JIT runner carries only the
pool's labels and self-hosted jobs ask for `self-hosted`, so the policy rejects a pool
without it. A job that matches several pools counts only for the first one in the file.

Stop needs `resume` in placement because without it a stopped machine never comes back.
A `Busy` runner keeps its machine even past `fork_ttl_s`. The policy accepts
`fork_ttl_s` from 1 to 2592000 (30 days, boat's longest TTL) and rejects anything else
rather than clamping it.

An ephemeral machine's job has finished when the machine is `Stopped`, or when it is `Up`
with no live runner and either a runner on it was seen busy or it is older than
`register_timeout_s`.

## Properties

Safety, checked by TLC and by property tests on `decide`:

1. `NoBusyStop`: no `Stop` or `Delete` of a machine with a `Busy` runner.
2. `OneJobPerRunner`: a JIT runner runs at most one job.
3. `MaxBoxes`: machines in `Up` or `Starting` never exceed `max_boxes`.
4. `StartBudget`: paid starts stay within `paid_start_cents_per_day`, so none when it is 0.
5. `NoDoubleFork`: at most one job fork per job id and attempt.
6. `EphemeralIsolation`: a pool whose placement is only `fork` never runs two jobs on one
   machine.
7. `NoLeak`: every ephemeral machine is deleted after its job ends or its TTL passes.

Liveness, under weak fairness of the tool's actions:

1. `NoStarvation`: a queued job that some placement step allows eventually runs, once the
   budget and `max_boxes` allow it.
2. `IdleStops`: in `warm` and `hybrid`, an idle machine above `min_up` is eventually
   stopped.
3. `Settles`: with no new jobs, the system reaches a state where `decide` returns nothing.

The spec also models the faults the tool must survive: GitHub reassigns a job, boat loses
the kept machine (the resume becomes a counted start), boat refuses a start (409, 429,
503), a JIT runner never comes online, and the tool crashes and restarts between any two
actions.

## Out of scope

Kubernetes, webhooks, several tool instances against one pool, providers other than boat,
and updating runner binaries in place (rebuild the pool's snapshot instead).
