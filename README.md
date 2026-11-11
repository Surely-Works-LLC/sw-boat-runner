# sw-boat-runner

A daemon that runs self-hosted GitHub Actions runners on [boat](https://boat.dev)
machines and reuses the machines across jobs.

We love boat and keep using it. Its managed `runs-on: boat` is the simplest way to run
CI there, but it starts a machine for every job, and a busy repository runs more jobs in
a day than its plan includes starts. This daemon keeps machines up between jobs, stops
them when idle, and forks a fresh one only when a job needs it.

![boat starts per day against CI jobs per day: runs-on: boat grows one start per job, the warm preset stays near 100](bench/starts.svg)

| Jobs a day | Setup | Starts | Extra starts | Queue p50 / p90 |
| - | - | - | - | - |
| 962 | `runs-on: boat` | 962 | 122 ($3.66) | 8 s / 8 s |
| | `warm` | 84 | 0 | 18 s / 25 s |
| 2,000 | `runs-on: boat` | 2,000 | 1,160 ($56.00) | 8 s / 8 s |
| | `warm` | 103 | 0 | 17 s / 24 s |
| 4,772 | `runs-on: boat` | 4,772 | 4,011 ($253.27) | 8 s / 8 s |
| | `warm` | 118 | 0 | 17 s / 24 s |

The numbers come from `cargo run --release --example bench`, which simulates a day of CI
on boat's $100 plan (840 [included starts](https://docs.boat.dev/limits.md) a day).
`runs-on: boat` is mocked as one start per job; the daemon's real loop runs against a
mocked boat and GitHub API. Under the included starts, `runs-on: boat` is cheaper and
starts jobs sooner. Past them, its jobs are extra starts at $0.03 to $0.07 each.
[bench/](bench/) has the assumptions, the other presets and the machine-time cost.

## Connect a repository

The daemon is one process for one repository. It keeps no database and can be restarted
at any time. Something has to run it, because a runner must be registered before a job
can use it; changing `runs-on` alone is not enough. There are three ways:

| Where the daemon runs | You add | Saves starts |
| - | - | - |
| [Inside the workflow](examples/in-workflow/) | one job and two secrets | within one run only; the default is a fresh machine per job |
| [On a small boat machine](examples/always-on/) | one machine, about $13 a month | yes |
| On any host you keep up | the steps below | yes |

1. Install it:

   ```sh
   cargo install --locked --git https://github.com/Surely-Works-LLC/sw-boat-runner
   ```

2. Make a boat snapshot with an extracted
   [actions/runner](https://github.com/actions/runner) release at
   `/home/user/actions-runner`, its dependencies, and whatever your jobs expect to find.
   Leave the runner unconfigured. Keep logins and tokens out: a job can read every file
   in the snapshot. Remove cleanup timers, because machines keep the snapshot's systemd
   timers.

3. Set two environment variables:

   | Variable | Value |
   | - | - |
   | `BOAT_API_KEY` | A boat key for the account that owns the snapshot. |
   | `GITHUB_TOKEN` | A fine-grained personal access token for the repository with Administration read and write (to register runners) and Actions read (to see queued jobs). |

   The `GITHUB_TOKEN` that GitHub Actions creates for each workflow run does not work:
   it cannot be given Administration, so it cannot register runners.

4. Write `policy.toml`:

   ```toml
   repo = "acme/app"

   [[pool]]
   name = "ci"
   labels = ["self-hosted", "large"]
   machine_type = "large"
   snapshot = "snap-1"
   max_boxes = 4
   ```

5. Check it, see what it would do, then run it:

   ```sh
   sw-boat-runner check policy.toml   # validate, print each pool with its preset applied
   sw-boat-runner plan policy.toml    # observe once, print the actions as JSON; changes nothing
   sw-boat-runner run policy.toml     # the loop
   sw-boat-runner drain policy.toml   # delete the pools' machines and runners, to stop using them
   ```

   `plan policy.toml snapshot.json` plans for a saved snapshot instead, with no
   credentials; [tests/scenarios](tests/scenarios/) has snapshots to start from.

6. Point jobs at the pool. A job goes to the first pool whose `labels` include all of
   its `runs-on` labels, compared without case, even when a later pool has a free slot,
   so list the pool with more labels first:

   ```yaml
   runs-on: [self-hosted, large]
   ```

   Runners you registered yourself with the same labels keep taking these jobs. Remove
   them once the pool runs.

The daemon creates machines with `noEnv`, so boat adds none of the account's secrets. It
labels its machines `sw-boat-runner=<pool>`, names its runners `sbr-<pool>-...`, and
ignores every other machine and runner. To update the runner, rebuild the snapshot.

This repository runs its own tests through the first option, in its
[`self-hosted` workflow](.github/workflows/self-hosted.yml).

## Policy

`repo` and `tick_s` (default 15) sit at the top level. Each `[[pool]]` takes:

| Field | Default | Meaning |
| - | - | - |
| `name` | required | Lowercase letters, digits and `_`. |
| `labels` | required | Runner labels. Must include `self-hosted`. |
| `machine_type` | required | `small`, `default`, `large` or `xlarge`. |
| `snapshot` | required | The boat snapshot machines start from. Its disk must fit `machine_type`'s disk budget, or boat refuses each fork with 409 `deploy_failed`; pick a larger `machine_type` for a large snapshot. |
| `max_boxes` | required | Most machines up or starting at once. |
| `preset` | `hybrid` | See below. |
| `placement` | from the preset | Overrides the preset's steps. |
| `min_up` | 1 for `pool`, else 0 | Kept machines that stay up without jobs. |
| `slots_per_box` | 1 | Runners per kept machine. |
| `idle_stop_s` | 1800 | Idle time before a kept machine stops. |
| `min_up_s` | 600 | Shortest time a machine stays up before it may stop. |
| `register_timeout_s` | 300 | How long a registered runner may take to come online. Keep it above the time boat takes to copy the snapshot to a new machine's disk: the runner waits up to 10 minutes for that copy before it starts. |
| `fork_ttl_s` | 3600 (at most 2592000) | Longest life of a machine forked for one job. |
| `paid_start_cents_per_day` | 0 | Daily cap on paid starts. 0 means only starts the plan includes. |

A preset sets `placement`, the ways to find a runner for a queued job, tried in order:

| Preset | Placement | Idle machines |
| - | - | - |
| `pool` | `slot` | `min_up` stay up |
| `warm` | `slot, resume` | stopped after `idle_stop_s`, resumed for the next job; when the stopped ones are not enough, new ones are forked from `snapshot`, up to `max_boxes` |
| `fork` | `fork` | none; each job gets a new machine that is deleted afterwards |
| `hybrid` | `slot, resume, fork` | `min_up` stay up; a job their slots cannot take gets a new machine that is deleted afterwards |

Use `fork` for untrusted code such as pull requests from forks.
[docs/design.md](docs/design.md) has the full design.

## GitHub API requests

A fine-grained personal access token shares its owner's limit of 5,000 REST requests an
hour with everything else the owner runs. A GitHub App installation token has its own
limit, at least 5,000 an hour, but it expires after an hour and the daemon reads
`GITHUB_TOKEN` only at start.

The daemon aims to use at most a quarter of the limit GitHub reports
(`X-RateLimit-Limit`), 1,250 requests an hour for a personal token. When fewer requests
are left, it spreads those above 500 evenly until the limit resets. A 304 answer to a
cached read is free and not counted. Each tick reads the runner list, the queued and the
in-progress runs, and the jobs of every queued run. An in-progress run's jobs are read
when the run is new, while it has a queued job a pool serves, when its `updated_at`
changes, and at least every 60 s while the budget allows. On a quiet repository that is
a few requests a tick, mostly 304s. With 30 in-progress runs the old loop sent 32
requests a tick, 7,680 an hour; it now sends about 1,270 an hour, and a job that such a
run queues is found within 150 s. Under 500 requests left the daemon observes once every
5 minutes, so cleanup goes on, and under 200 it sends nothing until the reset.
[docs/design.md](docs/design.md#github-requests) has the details.

## Verification

[spec/BoatRunner.tla](spec/BoatRunner.tla) models the loop together with boat, GitHub,
crashes, timeouts and listing lag. TLC checks that no machine is stopped while its runner
runs a job, a pool never exceeds `max_boxes`, paid starts stay within the daily cap across
restarts, no job gets two machines, a `fork` pool never runs two jobs on one machine,
queued jobs get a runner, and idle machines stop. [spec/README.md](spec/README.md) lists
what the model leaves out, the bugs it found, and
[how to run TLC](spec/README.md#running-tlc). Property tests check `decide` against the
safety properties, and the loop is tested against a fake transport.

## Status

In November 2026 a `fork` pool ran against a production repository: it forked machines up
to `max_boxes`, each machine's runner took one job, and the daemon deleted the machines
and their runners afterwards. [eval/](eval/) measures a repository's move from
`runs-on: boat` to persistent boat runners, which is the setup this daemon automates.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
