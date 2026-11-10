# sw-boat-runner

A daemon that runs self-hosted GitHub Actions runners on [boat](https://boat.dev)
machines for one repository, and reuses the machines across jobs.

We love boat and keep using it. Its managed `runs-on: boat` is the simplest way to run
CI there, but it starts a machine for every job, and a busy repository runs more jobs in
a day than its plan includes starts. This daemon keeps machines up between jobs, stops
them when idle, and forks a fresh one only when a job needs it.

Every 15 seconds it lists the pool's boat machines, the repository's runners and its
queued jobs, decides what to do, and does it. Each runner is a just-in-time runner that
takes one job and exits. [docs/design.md](docs/design.md) has the full design.

## Starts per day

![boat starts per day against CI jobs per day: runs-on: boat grows one start per job, the warm preset stays near 100](bench/starts.svg)

`cargo run --release --example bench` simulates a day of CI at eight loads. `runs-on: boat`
is mocked as one start per job. The daemon is not mocked: its tick loop and `decide` run
against a mocked boat and GitHub API. [bench/](bench/) has the assumptions and
[bench/results.csv](bench/results.csv) every number.

| Jobs a day | Setup | Starts | Paid starts | Starts + machine time | Queue p50 / p90 |
| - | - | - | - | - | - |
| 962 | `runs-on: boat` | 962 | 0 | $0 + $4.82 | 8 s / 8 s |
| | `warm` | 84 | 0 | $0 + $14.15 | 18 s / 26 s |
| 2,000 | `runs-on: boat` | 2,000 | 750 | $27.75 + $10.15 | 8 s / 8 s |
| | `warm` | 101 | 0 | $0 + $22.63 | 17 s / 24 s |
| 4,772 | `runs-on: boat` | 4,772 | 3,522 | $130.31 + $24.90 | 8 s / 8 s |
| | `warm` | 119 | 0 | $0 + $43.13 | 17 s / 24 s |

Under the plan's included starts, `runs-on: boat` is cheaper and starts jobs sooner: the
daemon's machines idle for `idle_stop_s` before they stop. Past it, every job on
`runs-on: boat` is a paid start, while `warm` stays near 100 starts a day. `hybrid` forks
for bursts, so its starts grow with load until the included ones run out. It then makes
no paid start, because `paid_start_cents_per_day` defaults to 0, and jobs wait.

The included starts (1,250 a day) and the start price ($0.037) are estimates read off
one bill in [eval/results/](eval/results/example-2026-11.md), not boat's published
numbers, and the dollar columns depend on them. The start counts do not.

## Connect a repository

The daemon is one process for one repository. It keeps no database, so it can run on any
host that stays up and can be restarted at any time.

**1. Build it.**

```sh
cargo install --locked --git https://github.com/Surely-Works-LLC/sw-boat-runner
```

**2. Make a boat snapshot for the runners.** Each pool starts its machines from one. It
needs an extracted [actions/runner](https://github.com/actions/runner) release at
`/home/user/actions-runner` with its dependencies installed, plus whatever your jobs
expect to find. Leave the runner unconfigured, and keep logins and tokens out of the
snapshot: a job can read every file in it. Remove cleanup timers too, because a forked
machine keeps the snapshot's systemd timers.

**3. Get two credentials.** The daemon reads them from its environment.

| Variable | What it is |
| - | - |
| `BOAT_API_KEY` | A boat key for the account that owns the snapshot. |
| `GITHUB_TOKEN` | A fine-grained personal access token for the repository with **Administration: read and write** (to register runners) and **Actions: read** (to see queued jobs). |

The `GITHUB_TOKEN` that GitHub Actions creates for every workflow run does not work
here. It cannot be given the Administration permission, so it cannot register runners.

**4. Write a policy.** This is the smallest one:

```toml
repo = "acme/app"

[[pool]]
name = "ci"
labels = ["self-hosted", "large"]
machine_type = "large"
snapshot = "snap-1"
max_boxes = 4
```

**5. Check it, look at what it would do, then run it.**

```sh
sw-boat-runner check policy.toml   # validate; print each pool with its preset applied
sw-boat-runner plan policy.toml    # observe boat and GitHub once and print the actions; changes nothing
sw-boat-runner run policy.toml     # the loop
```

`plan` prints a summary of what it saw to stderr and one JSON action per line to stdout.
`plan policy.toml snapshot.json` does the same offline against a saved snapshot.

**6. Point jobs at the pool.** A job goes to the first pool whose `labels` include all
of its `runs-on` labels:

```yaml
jobs:
  test:
    runs-on: [self-hosted, large]
```

The daemon creates machines with `noEnv`, so boat adds none of the account's secrets to
them. It labels its machines `sw-boat-runner=<pool>` and names its runners
`sbr-<pool>-...`, and ignores every other machine and runner. To update the runner,
rebuild the snapshot.

A new machine's `/home/user` is a FUSE mount until boat has copied the snapshot to disk,
and the runner starts only once that mount is gone. Keep `register_timeout_s` above the
copy time.

[examples/self-hosted/](examples/self-hosted/) is a complete setup: this repository's
[`self-hosted` workflow](.github/workflows/self-hosted.yml) runs the daemon and has it
fork a machine for its own tests.

## Presets

Each pool's `placement` lists the ways to find a runner for a queued job, tried in order.

| Preset | Placement | Idle machines |
| - | - | - |
| `pool` | `slot` | `min_up` (default 1) stay up |
| `warm` | `slot, resume` | stopped after `idle_stop_s`, resumed for the next job |
| `fork` | `fork` | none; each job gets a new machine that is deleted afterwards |
| `hybrid` (default) | `slot, resume, fork` | like `warm`, and forks for bursts |

`fork` is the one for untrusted code such as pull requests from forks.

## Policy reference

`repo` and `tick_s` (default 15) sit at the top level. Each `[[pool]]` takes:

| Field | Default | Meaning |
| - | - | - |
| `name` | required | Lowercase letters, digits and `_`. |
| `labels` | required | Runner labels. Must include `self-hosted`. |
| `machine_type` | required | `small`, `default`, `large` or `xlarge`. |
| `snapshot` | required | The boat snapshot machines start from. |
| `max_boxes` | required | Most machines up or starting at once. |
| `preset` | `hybrid` | See [Presets](#presets). |
| `placement` | from the preset | Overrides the preset's steps. |
| `min_up` | 1 for `pool`, else 0 | Kept machines that stay up without jobs. |
| `slots_per_box` | 1 | Runners per kept machine. |
| `idle_stop_s` | 1800 | Idle time before a kept machine stops. |
| `min_up_s` | 600 | Shortest time a machine stays up before it may stop. |
| `register_timeout_s` | 300 | How long a registered runner may take to come online. |
| `fork_ttl_s` | 3600 (at most 2592000) | Longest life of a machine forked for one job. |
| `paid_start_cents_per_day` | 0 | Daily cap on paid starts. 0 means only starts the plan includes. |

## Verification

[spec/BoatRunner.tla](spec/BoatRunner.tla) models the loop together with boat, GitHub,
crashes, timeouts and listing lag. TLC checks that no machine is stopped while its runner
runs a job, a pool never exceeds `max_boxes`, paid starts stay within the daily cap across
restarts, no job gets two machines, and a `fork` pool never runs two jobs on one machine,
plus liveness: queued jobs get a runner and idle machines stop. The `tla` workflow runs
the models on `main` when `spec/` changes. [spec/README.md](spec/README.md) lists what the
model leaves out, the bugs it found, and [how to run TLC](spec/README.md#running-tlc).

Property tests check `decide` against the safety properties, and the loop is tested
against a fake transport.

## Status

In November 2026 a `fork` pool ran against a production repository: it forked machines up
to `max_boxes`, each machine's runner took one job, and the daemon deleted the machines
and their runners afterwards.

[eval/](eval/) measures a repository's move from `runs-on: boat` to persistent boat
runners, which is the setup this daemon automates.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
