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

`warm` is the default. Only `warm` forks kept machines for demand; `hybrid` forks them
only up to `min_up`, so it stops only kept machines left above a lowered `min_up`. Steps
keep the order `slot`, `resume`, `fork`, and `resume` and `min_up` need `slot`, the only
step that registers runners on kept machines.

Every runner is a just-in-time runner: it is registered through GitHub's JIT config,
takes one job and exits. A long-lived runner would need a registration token on the
machine, and boat stores every command for 90 days, so the token would end up in boat's
command history. On a kept machine, a slot whose runner finished is free again, and a
slot whose runner went `Offline` gets `Unregister`. Either slot takes a fresh `Register`
in the same tick when a job is waiting. The runner's work directory stays on the
machine, so its caches survive from job to job. A stop and resume restores the machine
from a boat snapshot, which does not save `/tmp` or `~/.cache`, so caches meant to outlive
a stop go elsewhere under `/home`, set in the runner's `.env`;
[Snapshots](#snapshots) has an example.

## Snapshots

Every pool starts its machines from one boat snapshot. It holds an extracted
[actions/runner](https://github.com/actions/runner) release at `/home/user/actions-runner`,
left unconfigured, its dependencies, and whatever the jobs expect to find. A job can read
every file in the snapshot, so keep logins and tokens out. Machines keep the snapshot's
systemd timers, so remove cleanup timers.

Make it from a sandbox created with `--no-env`: boat says "a saved snapshot of a
`--no-env` sandbox stays `--no-env`". A snapshot of any other sandbox carries your
personal logins (GitHub token, model logins) into every machine forked from it, even
though the daemon forks with `noEnv`. `check` cannot see this. Before it registers a
runner, the daemon asks boat whether the machine holds its creator's logins
(`holdsCreatorLogins`); if so, or if boat does not say, it registers nothing and logs an
error that names the snapshot. A job's fork is deleted and the pool's forks back off; a
kept machine waits until it is stopped and resumed.

```sh
boat new --no-env --type large   # the pool's machine_type; prints the machine's id
boat ssh <id>                    # extract the runner, run its bin/installdependencies.sh
boat snapshot <id> snap-1        # the name goes in the policy's `snapshot`
```

`setup` can do this for you: for a snapshot name that is not saved, or the answer `new`,
it builds one in a temporary `noEnv` sandbox with the latest runner release only, then
checks and saves it as `sw-boat-runner snapshot` does
([details](../examples/always-on/README.md#on-the-machine)).

On a create, boat adds none of the account's secrets, but logins saved in the snapshot
stay. On a resume, boat also scrubs owner secrets from the restored disk before the
machine is reachable. The daemon labels its machines `sw-boat-runner=<pool>`, names its
runners `sbr-<pool>-...`, and ignores every other machine and runner. To update the
runner, rebuild the snapshot.

Keep the snapshot small: every start that is not a resume of a kept machine copies it to
a new disk, and a snapshot larger than `machine_type`'s disk makes boat refuse each fork
with 409 `deploy_failed`. boat snapshots do not save `/tmp` or `~/.cache` (except
`~/.cache/ms-playwright` and deno's cache), and a stop saves a machine the same way, so
build caches a warm or kept machine should keep across a stop and resume must live
elsewhere under `/home`. Point tools at it in the runner's `.env` file,
`/home/user/actions-runner/.env`, which every slot's copy of the runner reads:

```sh
CARGO_TARGET_DIR=/home/user/ci-cache/target
XDG_CACHE_HOME=/home/user/ci-cache/xdg
```

`sw-boat-runner snapshot <sandbox-id> <name> [--machine-type T] [--fix] [--keep-fork] [--yes]`
forks the sandbox with `noEnv` (the sandbox itself is not changed), waits for the fork to
hydrate, and checks it: boat's `holdsCreatorLogins`, an unconfigured runner with `run.sh`
at `/home/user/actions-runner`, enabled `actions.runner.*` systemd units, credential files
(`gh`, git, ssh, codex, claude, netrc, docker, aws, npm, gcloud), runner `.env` cache
directories under `/tmp` or `~/.cache`, and the size against `machine_type`'s disk (51 GB
for small and default, 111 GB for large, 251 GB for xlarge). It prints the largest
directories. With no FAIL it saves the fork under `<name>`, prints the size and deletes
the fork. `--fix` disables old runner units and removes `.runner` and `.credentials` in
the fork, and removes credential files from it after a yes. Replacing an existing name
also asks first; `--yes` answers both. A replacement is first saved under a temporary
`sbr-tmp-` name to read its real size, and the old snapshot is kept when it is too big.
Ctrl-C or SIGTERM deletes the fork before exiting; a second one exits at once. `run` warns
once at start about a pool whose saved snapshot is larger than its machine type's disk.

## Start budget

Every action that costs a start passes the pool's `paid_start_cents_per_day` check first:

- `0` (default): start only while boat's `GET /limits` reports `nextStartDollars == 0`,
  so only included starts are used.
- `N > 0`: also allow paid starts while the cents spent on starts today, plus the price
  of this start, stay at or under N.

`GET /limits` quotes one price, for the next start, so `decide` prices every start of a
tick at that quote. At a quote of 0 it plans no more starts than the fewest left among
boat's minute, hour and day windows, because the price past them is not known until the
next tick. Before each start that follows an accepted one, the daemon reads `GET /limits`
again and makes no more starts that tick if the price changed. One account serves every pool, so each pool's
cap is checked against the account-wide spend, including the starts that pools earlier in
the file planned in the same tick. Prices are counted in whole cents, and a positive price
under half a cent counts as 1 cent, so a paid start is never counted as free.

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

[GitHub requests](#github-requests) describes how the daemon keeps these reads inside the
API's rate limit.

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
listed yet, the cents spent on starts today, the time before which it makes no start, and
the GitHub request budget with the last read of each in-progress run's jobs.
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
  because boat may have charged for it. So does a resume that fails without a definite
  answer, and the daemon makes no more starts that tick. A refusal adds nothing.
- Restart spend (`FixCrashSpend`): see [Start budget](#start-budget).

## GitHub requests

The REST API allows a token a number of requests an hour: 5,000 for a personal access
token, shared with everything else its owner runs, and at least 5,000 of its own for a
GitHub App installation token. The daemon keeps its own use well inside that with a
token bucket in `src/github.rs`:

- It fills at the requests left above 500 (`RESERVE`) spread evenly until
  `X-RateLimit-Reset`, `(remaining - RESERVE) / (reset - now)` a second, so the pace
  follows what is actually left, including what others spent. The pace is capped at a
  share of `X-RateLimit-Limit` an hour: a quarter for a personal access token, whose
  limit its owner's other tools share, and the whole limit for a GitHub App installation
  token, which has a limit of its own. The client knows which from its credentials. With
  4,993 of 5,000 left and half an hour to the reset, an installation token gets 5,000 an
  hour and a personal token 1,250.
- It holds 120 s of its rate (`BURST_S`), and room for at least 50 requests
  (`BURST_MIN`), so a tick that has to read several runs is not starved. Under `RESERVE`
  it holds nothing. Every request GitHub counts takes a token; a 304 answer to a read with
  the cached ETag takes none.
- A tick observes only with a token in hand. A tick without one sends nothing and acts on
  nothing, rather than act on a stale snapshot, so a short budget stretches the time
  between observes. Even with no budget, the daemon observes once every 5 minutes
  (`PACE_MAX_S`), so finished machines are still deleted. The "pacing" line is
  logged at most once every 10 minutes.
- Once an observe starts, its reads and the writes `decide` asks for always go, so a
  tick never acts on half a snapshot. Under 200 requests left (`RATE_FLOOR`) nothing is
  sent until the reset, leaving the rest to whoever else uses the token.
- `drain` does not read queued jobs, so it needs only the runner list and its deletes.

The runner list and the queued runs' jobs rarely change, so most of those reads are 304s.
An in-progress run's jobs change with every step, and such a run can still queue a later
job (`needs`), so the daemon keeps its last read of each and reads it again:

- every tick while the run is new or re-run, or its last read found a queued job a pool
  serves, so a job that a runner took leaves the snapshot at once;
- with a token, when its `updated_at` changed since its last read, or when that read is
  60 s old (`RUN_MAX_AGE_S`), changed runs first, then the oldest reads.

A run that is not read keeps its last read, whose queued jobs no pool serves, so `decide`
never sees a job that may have been taken. GitHub does not promise that `updated_at`
moves when a run queues a job, so the 60 s age is what bounds the delay: a job queued in a
run that looks unchanged is found within 60 s while the budget covers every run, and
later when it does not. The tests in `src/github.rs` count the requests over an hour that
starts with 5,000 left: 30 in-progress runs cost 32 requests a tick before this budget,
7,680 an hour at `tick_s` 15. With it, a personal access token sends about 1,290 an hour
and a job one of the runs queues is found within 10 ticks (150 s). An installation token
sends about 2,300 an hour, never skips an observe, and finds the job within 4 ticks
(60 s), as fast as `RUN_MAX_AGE_S` allows; under the old fixed quarter it took 150 s, as
for a personal token. With 100 runs, one full page of the list, an installation token
finds the job within 7 ticks (105 s) and a personal token within 32 (480 s).

The run lists and each run's jobs are read page by page, 100 to a page, up to 10 pages,
so a matrix of more than 100 jobs has its later jobs seen. Paging stops at a short page
or once the pages cover the list's `total_count`, so a list of 100 or fewer costs one
request, and each page keeps its own ETag. The ETag cache drops a URL only after a tick
that sent requests without it; a tick the budget skips sends nothing and keeps every
ETag, so the next observe is mostly free 304s.

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
| `Delete(machine)` | ephemeral machine with no `Busy` runner whose job finished, or past `fork_ttl_s`; or a `Stopped` kept machine with no `Busy` runner in a pool without `resume`, which nothing would resume; or a kept machine `Up` or `Starting` with no `Busy` or `Registered` runner in a pool without `slot`, which nothing would use |
| `Unregister(runner)` | `Registered` past `register_timeout_s`, `Offline`, or runner on a machine that the pool's listing does not show `Up` |

Demand for a pool is the number of queued jobs whose labels it matches, each id and
attempt counted once, minus its runners in `Registered` or `Idle` that keep their slot
this tick. Unmet demand is what is left after this tick's registers and the capacity of the pool's `Starting` machines (one for an
ephemeral machine, `slots_per_box` for any other). A job matches a pool when the job's
labels, compared without case, are a subset of the pool's. A JIT runner carries only the
pool's labels and self-hosted jobs ask for `self-hosted`, so the policy rejects a pool
without it. A job that matches several pools counts only for the first one in the file.

Stop needs `resume` in placement because without it a stopped machine never comes back.
A pool without `slot` keeps no kept machine: one is left only when the pool's placement
changed after it was made, and without this rule it would hold a `max_boxes` box until
boat's 30-day TTL. A `Stopping` one is deleted once it is `Stopped`.
A `Busy` runner keeps its machine even past `fork_ttl_s`, which counts from the machine's
last state change. The policy accepts `fork_ttl_s` from 1 to 2592000 (30 days, boat's
longest TTL) and rejects anything else rather than clamping it.

An ephemeral machine's job has finished when the machine is `Stopped`, or when it is `Up`
with no live runner and either a runner was seen on it or it has been `Up` longer than
`register_timeout_s`.

## Properties

Safety, checked by TLC. Every property below but `OneJobPerRunner`, which is GitHub's
behaviour, also has a property test on one tick of `decide`, named after it in
`src/decide.rs`; for liveness the test checks the tick that makes progress.

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
