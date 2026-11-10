# sw-boat-runner

A daemon that runs self-hosted GitHub Actions runners on [boat](https://boat.dev)
machines for one repository. boat's managed `runs-on: boat` starts a machine for every
job, and starts past the plan's allowance cost money. This tool keeps machines up
between jobs, stops them when idle, and forks a fresh one only when a job needs it.

Every 15 seconds it lists the pool's boat machines, the repository's runners and its
queued jobs. It then decides what to do and does it. Each runner is a just-in-time
runner that takes one job and exits. [docs/design.md](docs/design.md) has the full
design.

## Safety properties

[spec/BoatRunner.tla](spec/BoatRunner.tla) models the loop together with boat, GitHub,
crashes, timeouts and listing lag. TLC checks these safety properties in CI:

- `NoBusyStop`: no machine is stopped or deleted while its runner runs a job.
- `OneJobPerRunner`: a runner takes at most one job.
- `MaxBoxes`: a pool never has more than `max_boxes` live machines.
- `StartBudget`: paid starts stay within the daily cap, across restarts.
- `NoDoubleFork`: no two live machines for the same job.
- `EphemeralIsolation`: in a fork pool, a job runs on a machine that has run no other job.

It also checks liveness: forks are cleaned up, queued jobs get a runner when a start is
possible, idle machines stop, and the loop settles once jobs stop arriving.
[spec/README.md](spec/README.md) lists what the model leaves out and the bugs it found.

## Presets

Each pool's `placement` lists the ways to find a runner for a queued job, tried in order.

| Preset | Placement | Idle machines |
| - | - | - |
| `pool` | `slot` | `min_up` (default 1) stay up |
| `warm` | `slot, resume` | stopped after `idle_stop_s`, resumed for the next job |
| `fork` | `fork` | none; each job gets a new machine that is deleted afterwards |
| `hybrid` (default) | `slot, resume, fork` | like `warm`, and forks for bursts |

`fork` is the one for untrusted code such as pull requests from forks.

## Policy

The smallest policy `check` accepts:

```toml
repo = "acme/app"

[[pool]]
name = "ci"
labels = ["self-hosted", "large"]
machine_type = "large"
snapshot = "snap-1"
max_boxes = 4
```

`labels` must include `self-hosted`, and jobs pick the pool with `runs-on`.
`machine_type` is `small`, `default`, `large` or `xlarge`. Pool names use lowercase
letters, digits and `_`. Optional pool fields and their defaults:

| Field | Default |
| - | - |
| `preset` | `hybrid` |
| `placement` | from the preset |
| `min_up` | 1 for `pool`, else 0 |
| `slots_per_box` | 1 |
| `idle_stop_s` | 1800 |
| `min_up_s` | 600 |
| `register_timeout_s` | 300 |
| `fork_ttl_s` | 3600 (at most 2592000) |
| `paid_start_cents_per_day` | 0, so only starts the plan includes |

`tick_s` (default 15) sits at the top level next to `repo`.

## Credentials

`run` and a live `plan` read two environment variables:

- `BOAT_API_KEY`: a boat key for the account that owns the snapshot. The tool creates,
  resumes, stops and deletes sandboxes, writes files to them, runs commands, and reads
  `/limits`.
- `GITHUB_TOKEN`: a token for `repo`. A fine-grained token needs repository
  Administration read and write (self-hosted runners) and Actions read (runs and jobs).

## Snapshot

Each pool starts its machines from a boat snapshot. The snapshot needs an extracted
[actions/runner](https://github.com/actions/runner) release at
`/home/user/actions-runner`, with its dependencies installed. Leave it unconfigured,
and keep logins and tokens out of it: a job can read any file in the snapshot. The tool
creates machines with `noEnv`, so boat adds none of the account's secrets.
For each slot the tool copies that directory to `/home/user/sbr/<slot>`, writes the JIT
config to a file, and starts `run.sh`. Rebuild the snapshot to update the runner.

The tool labels its machines `sw-boat-runner=<pool>` and names its runners
`sbr-<pool>-...`. It ignores other machines and runners.

## Usage

```sh
sw-boat-runner check policy.toml                 # validate, print each pool with its preset applied
sw-boat-runner plan policy.toml                  # observe once, print the actions; changes nothing
sw-boat-runner plan policy.toml snapshot.json    # the same against a saved snapshot, offline
sw-boat-runner run policy.toml                   # the loop
```

`plan` prints a summary of what it saw to stderr and one JSON action per line to stdout.
Build with `cargo build --release`. CI uploads Linux x86_64 and macOS arm64 binaries as
workflow artifacts.

## Running the TLA+ models

The `tla` workflow runs 15 TLC models on `main` when `spec/` changes; pull requests skip
it, so run TLC locally before changing the spec. Four models must pass. The other eleven
each turn one fix off and must still find its counterexample. To run one locally with
Java 21 and `tla2tools.jar` v1.7.4:

```sh
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto -deadlock \
  -config spec/BoatRunner.cfg spec/BoatRunner.tla
```

[spec/README.md](spec/README.md#running-tlc) and `.github/workflows/tla.yml` have the
other models.

## Status

The model, the property tests and the fake-transport tests pass. The daemon has not yet
run against a production repository. [eval/](eval/) measures a repository's move from
`runs-on: boat` to persistent boat runners, which is the setup this tool automates.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
