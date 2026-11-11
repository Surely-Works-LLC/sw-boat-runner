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
   cargo install --locked --git https://github.com/Surely-Works-LLC/sw-boat-runner --branch main
   ```

   For production, pin a commit you have tested with `--rev <commit hash>` instead of
   `--branch main`.

2. Make a boat snapshot with an extracted
   [actions/runner](https://github.com/actions/runner) release at
   `/home/user/actions-runner`, its dependencies, and whatever your jobs expect to find.
   Leave the runner unconfigured. Keep logins and tokens out: a job can read every file
   in the snapshot. Remove cleanup timers, because machines keep the snapshot's systemd
   timers. Make the snapshot from a sandbox created with `--no-env`: boat says "a saved
   snapshot of a `--no-env` sandbox stays `--no-env`". A snapshot of any other sandbox
   carries your personal logins (GitHub token, model logins) into every machine forked
   from it, even though the daemon forks with `noEnv`. `check` cannot see this. Before it
   registers a runner, the daemon asks boat whether the machine holds its creator's
   logins (`holdsCreatorLogins`); if so, or if boat does not say, it registers nothing and
   logs an error that names the snapshot. A job's fork is deleted and the pool's forks
   back off; a kept machine waits until it is stopped and resumed. With the `boat` CLI:

   ```sh
   boat new --no-env --type large   # the pool's machine_type; prints the machine's id
   boat ssh <id>                    # extract the runner, run its bin/installdependencies.sh
   boat snapshot <id> snap-1        # the name goes in the policy's `snapshot`
   ```

   Keep the snapshot small: every start that is not a resume of a kept machine copies it
   to a new disk. boat snapshots do not save `/tmp` or `~/.cache` (except
   `~/.cache/ms-playwright` and deno's cache), and a stop saves a machine the same way,
   so build caches a warm or kept machine should keep across a stop and resume
   must live elsewhere under `/home`, such as `/home/user/ci-cache`. Point tools at it in
   the runner's `.env` file, `/home/user/actions-runner/.env`, which every slot's copy of
   the runner reads:

   ```sh
   CARGO_TARGET_DIR=/home/user/ci-cache/target
   XDG_CACHE_HOME=/home/user/ci-cache/xdg
   ```

3. Set `BOAT_API_KEY` to a boat key for the account that owns the snapshot, and give the
   daemon GitHub access in one of two ways. Setting both is an error.

   A GitHub App (recommended). It has its own rate limit instead of a share of yours, it
   is no one's personal token, and it reaches only the repositories it is installed on.
   Create an app under your account or organization (Settings, Developer settings,
   GitHub Apps) with webhooks off and two repository permissions: Administration read and
   write (to register and delete self-hosted runners) and Actions read (to see queued
   jobs). Install it on the repository and generate a private key.

   | Variable | Value |
   | - | - |
   | `GITHUB_APP_ID` | The app's ID, or its client ID. |
   | `GITHUB_APP_PRIVATE_KEY_FILE` | Path to the private key (`.pem`) GitHub generated. |
   | `GITHUB_APP_INSTALLATION_ID` | Optional. Without it the daemon looks up the app's installation on the policy's repository. |

   The daemon signs a JWT with the key, exchanges it for an installation token limited
   to the policy's repository, and renews that token five minutes before it expires and
   whenever GitHub answers 401. It never logs the key, the JWT or the token.

   Or a fine-grained personal access token:

   | Variable | Value |
   | - | - |
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

   `check` also warns on stderr about a pool whose `register_timeout_s` is 600 or less.
   `sw-boat-runner --help` lists the commands and `--version` prints the version.

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

The daemon creates and resumes machines with `noEnv`. On a create, boat adds none of the
account's secrets, but logins saved in the snapshot stay, which is why step 2 needs a
`--no-env` snapshot. On a resume, boat also scrubs owner secrets from the restored disk
before the machine is reachable. The daemon labels its machines `sw-boat-runner=<pool>`, names its runners `sbr-<pool>-...`, and
ignores every other machine and runner. To update the runner, rebuild the snapshot.

The [`self-hosted` workflow](.github/workflows/self-hosted.yml) is a working example of
the first option. It is skipped in this repository, because its snapshot variable and
secrets are not set here; a fork that sets them runs its tests on boat.

To stop using the daemon, stop the process and run `sw-boat-runner drain policy.toml`
with the same keys. It deletes the pools' machines and runners, and leaves every other
machine and runner alone. It fails while a machine is left, for example one whose runner
is still running a job, so run it again until it succeeds. Then point `runs-on` back at
another runner.

## Check and save a snapshot

`sw-boat-runner snapshot <sandbox-id> <name> [--machine-type T] [--fix] [--keep-fork] [--yes]`
forks the sandbox with `noEnv` (the sandbox itself is not changed), waits for the fork to
hydrate, and checks it: boat's `holdsCreatorLogins`, an unconfigured runner with
`run.sh` at `/home/user/actions-runner`, enabled `actions.runner.*` systemd units,
credential files (`gh`, git, ssh, codex, claude, netrc, docker, aws, npm, gcloud), runner `.env` cache
directories under `/tmp` or `~/.cache`, and the size against `machine_type`'s disk (51 GB
for small and default, 111 GB for large, 251 GB for xlarge). It prints the largest
directories. With no FAIL it saves the fork under `<name>`, prints the size and deletes
the fork. `--fix` disables old runner units and removes `.runner` and `.credentials` in
the fork, and removes credential files from it after a yes. Replacing an existing name
also asks first; `--yes` answers both. A replacement is first saved under a temporary
`sbr-tmp-` name to read its real size, and the old snapshot is kept when it is too big.
Ctrl-C or SIGTERM deletes the fork before exiting; a second one exits at once. `run` warns once at start about a pool whose saved
snapshot is larger than its machine type's disk.

## Policy

`repo` (`owner/name`; neither part may be `.` or `..`) and `tick_s` (default 15) sit at
the top level. Each `[[pool]]` takes:

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
| `register_timeout_s` | 900 | How long a registered runner may take to come online; after it, the runner is unregistered. A new machine's runner first waits up to 600 s for boat to copy the snapshot to its disk, then boots, so `check` warns at 600 or less. Lower it only for a small snapshot that copies quickly. |
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
limit, at least 5,000 an hour; it expires after an hour, and the daemon renews it.

The daemon spreads the requests left above 500 (`RESERVE`) evenly until the limit
resets, and caps its pace at a share of the limit GitHub reports (`X-RateLimit-Limit`):
a quarter for a personal token, 1,250 requests an hour, and all of it for an App
installation token, 5,000 an hour. A 304 answer to a cached read is free and not counted.
Each tick reads the runner list, the queued and the in-progress runs, and the jobs of
every queued run. An in-progress run's jobs are read when the run is new, while it has a
queued job a pool serves, when its `updated_at` changes, and at least every 60 s while the
budget allows. On a quiet repository that is a few requests a tick, mostly 304s. With 30
in-progress runs the old loop sent 32 requests a tick, 7,680 an hour. Now a personal
token sends about 1,290 an hour and a job such a run queues is found within 150 s; an App
sends about 2,300 and finds it within 60 s. Under 500 requests left the daemon observes
once every 5 minutes, so cleanup goes on, and under 200 it sends nothing until the reset.
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

In November 2026 the authors ran a `fork` pool against one of their private
repositories: it forked machines up to `max_boxes`, each machine's runner took one job,
and the daemon deleted the machines and their runners afterwards. This repository links
no log of that run; the tests, the bench and the TLA+ model are what you can check.

The queue times at the top are simulated. The only real measurement,
[eval/results/example-2026-11.md](eval/results/example-2026-11.md), is slower: queue p50
5.6 min and p90 27.8 min. It measured the setup this daemon replaces: four long-lived
runners registered once on always-on machines, too few for peak load and with no way to
grow with the queue. It shows the starts saved by keeping machines up, not the daemon's
queue times, which have not been measured on a real repository yet.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
