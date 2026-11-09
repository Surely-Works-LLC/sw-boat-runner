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

| Preset | `placement` | `runner` | Idle machines | Fits |
| - | - | - | - | - |
| `pool` | `slot` | persistent | always up | steady load, trusted code, lowest overhead |
| `pool-jit` | `slot` | jit | always up | as `pool`, with a clean runner per job |
| `warm` | `slot, resume` | jit | stopped after `idle_stop_s` | load with quiet hours |
| `fork` | `fork` | jit | none | untrusted code (fork PRs), rare jobs |
| `hybrid` | `slot, resume, fork` | jit | stopped after `idle_stop_s` | bursts above the pool size |

`hybrid` is the default.

- A `persistent` runner registers once and takes job after job. The tool only keeps its
  machine up and restarts the runner service when GitHub reports it offline.
- A `jit` runner is registered through GitHub's just-in-time config, takes one job and
  exits. This is what GitHub recommends for autoscaling.

## Start budget

Every action that costs a start passes the pool's `paid_start_cents_per_day` check first:

- `0` (default): start only while boat's `GET /limits` reports `nextStartDollars == 0`,
  so only included starts are used.
- `N > 0`: also allow paid starts while the cents spent on starts today, plus the price
  of this start, stay at or under N.

Within one tick, starts are free while `GET /limits` reports included starts left, and
each start after that costs the reported price. One account serves every pool, so the
spend each pool's actions add counts against the next pool in the same tick.

The cents spent today are counted in memory. A restart cannot recover them, so after a
restart the tool treats the day's budget as spent and makes only included starts until
00:00 UTC. A crash can delay paid starts but never exceed the cap.

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
  jobs whose labels match a pool.

Runner names carry what the tool needs to know about them:
`sbr-<pool>-<machine id>-<slot>-<registered unix time>`. A forked machine carries the
label `sw-boat-runner-job=<job id>-<attempt>`, and its fork request uses the same value as
its `Idempotency-Key`, so a crash and restart cannot fork twice for one job.

Two facts are kept only in memory. When each machine last had a busy runner: after a
restart it counts from the restart, which can only delay a stop. The cents spent on
starts today: see [Start budget](#start-budget). Restarting the process loses nothing
else.

## State machine

The model shared by `src/decide.rs` and `spec/BoatRunner.tla`.

Machine states, mapped from boat's states:

| State | boat states |
| - | - |
| `Starting` | `provisioning`, `provisioned`, `cloning` |
| `Up` | `ready`, `idle`, `running` |
| `Stopping` | a stop in progress |
| `Stopped` | stopped, disk kept |
| `Gone` | `archiving`, `archived`, `error`, deleted |

Runner states: `Registered` (JIT config issued, not online yet), `Idle` (online, no job),
`Busy` (running a job), `Offline` (was online, now not).

`decide(snapshot, policy) -> [Action]` is a pure function. Actions:

| Action | Guard |
| - | - |
| `Register(machine, slot)` | machine `Up`, slot free, unmet demand for the pool's labels |
| `Resume(machine)` | machine `Stopped`, `resume` in placement, start budget allows, `Up + Starting < max_boxes`, and either unmet demand left after free slots or `Up + Starting < min_up` |
| `Fork(job)` | unmet demand left after slots and resumes, `fork` in placement, start budget allows, `Up + Starting < max_boxes`, no machine with this job's label yet |
| `Stop(machine)` | machine `Up`, not ephemeral, `resume` in placement, no unmet demand, no `Busy` or `Registered` runner, idle for `idle_stop_s`, up for `min_up_s`, more than `min_up` machines `Up` |
| `Delete(machine)` | ephemeral machine with no `Busy` runner whose job finished, or past `fork_ttl_s` |
| `Unregister(runner)` | `Registered` past `register_timeout_s`, JIT runner `Offline`, or runner on a machine that is not `Up` |
| `RestartRunner(machine, slot)` | persistent runner `Offline` on an `Up` machine |

Demand for a pool is the number of queued jobs whose labels it matches, minus its runners
in `Registered` or `Idle`, minus the capacity of its `Starting` machines (one for an
ephemeral machine, `slots_per_box` for any other). A job matches a pool when the job's
labels, compared without case, are a subset of the pool's; list `self-hosted` in the
pool's labels. A job that matches several pools counts only for the first one in the
file.

Stop needs `resume` in placement because without it a stopped machine never comes back.
A `Busy` runner keeps its machine even past `fork_ttl_s`.

An ephemeral machine's job has finished when the machine is `Stopped`, or when it is `Up`
with no live runner and either a runner on it was seen busy or it is older than
`register_timeout_s`.

## Properties

Safety, checked by TLC and by property tests on `decide`:

1. `NoBusyStop`: no `Stop` or `Delete` of a machine with a `Busy` runner.
2. `OneJobPerRunner`: a JIT runner runs at most one job.
3. `MaxBoxes`: machines in `Up` or `Starting` never exceed `max_boxes`.
4. `StartBudget`: paid starts stay within `paid_start_cents_per_day`, so none when it is 0.
5. `NoDoubleFork`: at most one machine per job id and attempt.
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
