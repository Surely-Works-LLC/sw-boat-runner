# sw-boat-runner

Self-hosted GitHub Actions runners on [boat](https://boat.dev) machines. boat's managed
`runs-on: boat` starts a machine for every job, and a busy repository runs more jobs a
day than its plan includes starts. This daemon registers just-in-time runners on warm
machines, stops them when idle, resumes them for the next job, and forks a new one only
when a job needs it.

![boat starts per day against CI jobs per day: runs-on: boat grows one start per job, the warm preset stays near 100](bench/starts.svg)

| Jobs a day | `runs-on: boat` starts | `warm` starts | Queue p50, boat / warm |
| - | - | - | - |
| 962 | 962 (122 paid, $3.66) | 84 | 8 s / 18 s |
| 4,772 | 4,772 (4,011 paid, $253.27) | 118 | 8 s / 17 s |

A simulated day on boat's $100 plan (840 [included starts](https://docs.boat.dev/limits.md)
a day). Under the included starts, `runs-on: boat` is cheaper and faster.
[bench/](bench/) has the method and the other presets.

## Quick start

From your laptop, with the [boat CLI](https://docs.boat.dev/cli-reference) signed in.
Use the release you tested; the machine installs the same one. Releases run on Linux
x86_64 and arm64 with glibc 2.35 or newer, and `--remote` also on macOS arm64; on an
Intel Mac, build from source (`cargo install`, as in
[Any other host](examples/always-on/README.md#any-other-host)).

```sh
# macOS arm64. On Linux: x86_64-unknown-linux-gnu or aarch64-unknown-linux-gnu.
curl -fsSLO https://github.com/Surely-Works-LLC/sw-boat-runner/releases/download/v0.2.0/sw-boat-runner-aarch64-apple-darwin.tar.gz
tar -xzf sw-boat-runner-aarch64-apple-darwin.tar.gz
./sw-boat-runner setup --remote    # --dry-run first prints what it would do
```

`setup --remote` asks for the boat key without echo, then the policy (below). The key
comes from `boat api-key create` (shown once) and must belong to the account the boat
CLI is signed in to. It opens the browser to create the GitHub App, and GitHub sends the
browser back to a one-shot listener on `127.0.0.1`, so there is no code to paste unless
the browser is on another computer. It stops if a machine already runs the daemon for
the repository (`--replace` swaps it for the new one). It then creates a `small` boat
machine with no auto-stop (about $13 a month), installs the same release there with
[install.sh](examples/always-on/install.sh), enables the service and runs `doctor`.
The keys reach the machine only through the stdin of `boat ssh`. It ends with the
machine's id and `boat ssh <id> journalctl -u sw-boat-runner -f` for the logs.

Or run setup on the machine itself:

```sh
boat new --no-auto-stop --type small    # prints the machine's id
boat ssh <id>
# Pin a release you tested, in both places.
curl -fsSLO https://raw.githubusercontent.com/Surely-Works-LLC/sw-boat-runner/v0.2.0/examples/always-on/install.sh
SBR_REF=v0.2.0 bash install.sh    # checks the binary, then runs `sw-boat-runner setup`
```

Either way, `setup` asks for `repo`, `labels` and `snapshot` and writes them as the policy:

```toml
repo = "acme/app"
labels = ["self-hosted", "boat"]
snapshot = "snap-1"
```

The prompts offer defaults: the repository from `gh` or the `origin` remote, labels
`self-hosted, boat`, and your saved boat snapshots when `BOAT_API_KEY` is set. The
snapshot needs an unconfigured [actions/runner](https://github.com/actions/runner) at
`/home/user/actions-runner`, saved from a `--no-env` sandbox
([Snapshots](docs/design.md#snapshots)). If you have none, answer `new` (or name one
that does not exist) and setup offers to build it: a temporary `noEnv` machine gets the
latest runner release, checked against its SHA-256, and is checked and saved as
`sw-boat-runner snapshot` does, in about 5 minutes. It holds only the runner: to add
toolchains, install them in `boat new --no-env --from <name>` and save it again with
`sw-boat-runner snapshot <id> <name>`. Setup then prints a short link to
[a page](docs/new-app.html) on this project's GitHub Pages that creates a private GitHub
App for you (Administration write, Actions read, no webhook), waits until you install it
on the repository, asks for the boat key without echo, and enables a systemd service.
Point jobs at the pool with `runs-on: [self-hosted, boat]`.
[examples/always-on](examples/always-on/) has the details.

To change the policy later, edit `/etc/sw-boat-runner/policy.toml` on the machine and run
`sudo systemctl restart sw-boat-runner`. To remove a `--remote` install, stop the service
and drain the pools on the machine
([how](examples/always-on/README.md#on-the-machine)), `boat delete <id> --yes` the
machine, and delete the App in the GitHub settings of its owner (Developer settings,
GitHub Apps).

## Opt-ins

- `setup --pat`: a fine-grained token (Administration read and write, Actions read)
  instead of an App. It shares your rate limit. Without `--pat`, setup ignores
  `GITHUB_TOKEN`. `setup --help` lists the other flags.
- `preset`: `warm` (default), `pool`, `fork` or `hybrid`, see [Policy](#policy).
- `sw-boat-runner doctor` checks a setup end to end (clock, boat key, snapshot, App
  permissions, service, runners) and says how to fix each failure. `setup` runs it last.
- `sw-boat-runner snapshot <sandbox-id> <name>` checks a sandbox for logins, runner
  state, cache paths and size in a `noEnv` fork, then saves it.
- [In-workflow mode](examples/in-workflow/): no host; a reusable workflow runs the daemon
  for one workflow run.
- Several pools: one `[[pool]]` each. A job goes to the first pool whose `labels` include
  all its `runs-on` labels, so list the pool with more labels first.
- `check`, `plan`, `run` and `drain` run the daemon by hand on
  [any other host](examples/always-on/README.md#any-other-host). `drain` deletes the
  pools' machines and runners when you stop.

## Policy

`repo` (`owner/name`) and `tick_s` (default 15) sit at the top level. One pool can put
its keys at the top level too, as above. Each `[[pool]]` takes:

| Field | Default | Meaning |
| - | - | - |
| `labels` | required | Runner labels. Must include `self-hosted`. |
| `snapshot` | required | boat snapshot to start from. It must fit `machine_type`'s disk. |
| `name` | from the labels | Lowercase letters, digits and `_`. |
| `machine_type` | `default` | `small`, `default`, `large` or `xlarge`. |
| `max_boxes` | 2 | Most machines up or starting at once. |
| `preset` | `warm` | See below. |
| `placement` | from the preset | Overrides the preset's steps. |
| `min_up` | 1 for `pool`, else 0 | Kept machines that stay up without jobs. |
| `slots_per_box` | 1 | Runners per kept machine. |
| `idle_stop_s` | 1800 | Idle time before a kept machine stops. |
| `min_up_s` | 600 | Shortest time a machine stays up before it may stop. |
| `register_timeout_s` | 900 | Time a runner may take to come online. Keep it above 600, the runner's wait for the snapshot copy. |
| `fork_ttl_s` | 3600 (at most 2592000) | Longest life of a machine forked for one job. Keep it above `register_timeout_s` plus 300; `check` warns. |
| `paid_start_cents_per_day` | 0 | Daily cap on paid starts. 0 means only included starts. |

| Preset | Placement | Idle machines |
| - | - | - |
| `pool` | `slot` | `min_up` stay up |
| `warm` | `slot, resume` | stopped after `idle_stop_s`, resumed for the next job; new ones forked up to `max_boxes` |
| `fork` | `fork` | none; each job gets a new machine, deleted afterwards |
| `hybrid` | `slot, resume, fork` | `min_up` stay up; overflow jobs get a new machine each |

Use `fork` for untrusted code such as pull requests from forks. A forked machine lives at
most `fork_ttl_s` (default 3600 s), so raise it for jobs that run longer. Kept machines cost
machine time while up. boat snapshots do not save `/tmp` or `~/.cache`, so keep caches
under a home subdirectory such as `/home/user/ci-cache`. [docs/design.md](docs/design.md)
has the full design and the [GitHub request budget](docs/design.md#github-requests).

## Don't trust us? Fork it

Check a release against its `SHA256SUMS` and `gh attestation verify`
([how](examples/always-on/README.md#verifying-a-release)), and pin a tag or commit SHA.
Or fork it, or build your own. These prompts hand either job to a coding agent.

<details>
<summary>Agent prompt: fork and run your own</summary>

```text
Fork https://github.com/Surely-Works-LLC/sw-boat-runner and pin the latest release's
commit. Review what runs with my keys: .github/workflows/, examples/always-on/install.sh,
src/setup/, and how src/github_app.rs handles the App key and tokens. Report anything
that sends secrets anywhere but GitHub and boat. Build from source at that commit, set up
the daemon for <OWNER/REPO> with snapshot <SNAPSHOT> per README.md, and never put keys
in commands or logs. Confirm a test job with runs-on: [self-hosted, boat] runs.

(Optional. Delete this paragraph if you only want setup.) Then run a hill-climb session:
watch real CI on the pool for a while, tune the policy, snapshot and caches for cost and
queue time, and fix problems the runner hits. Report each issue at
https://github.com/Surely-Works-LLC/sw-boat-runner/issues without secrets or private repo
details. Patch the fork only for a blocker that is not fixed upstream.
```

</details>

<details>
<summary>Agent prompt: paranoia mode, read-only review, then build your own</summary>

```text
Audit https://github.com/Surely-Works-LLC/sw-boat-runner strictly read-only: run, build
or install nothing from it, and give it no secrets. Summarize the design and its risks:
what it can do with the App or token, where keys live, what a CI job can reach. Then
write a minimal independent version in <MY_REPO> using only the documented boat.dev API
and GitHub REST API: just-in-time runners from actions/runners/generate-jitconfig, plus
listing and deleting runners. Give the App only Administration write and Actions read,
on <OWNER/REPO> only. Show me the code before anything runs.

(Optional. Delete this paragraph if you only want setup.) Then run a hill-climb session:
watch real CI on the pool for a while, tune the policy, snapshot and caches for cost and
queue time, and fix problems the runner hits. Report each issue that also affects
sw-boat-runner at https://github.com/Surely-Works-LLC/sw-boat-runner/issues without
secrets or private repo details. Fork only if a blocker is not fixed upstream.
```

</details>

## Security and verification

The App needs Administration write, the least GitHub accepts for registering a runner,
and Actions read. Administration write can change settings, add deploy keys or delete
the repository, so install the App only where you need runners. `setup` keeps the keys
root-only in `/etc/sw-boat-runner`. TLC checks the [TLA+ model](spec/README.md) of the
loop, for example that no machine stops mid-job. Report vulnerabilities per
[SECURITY.md](SECURITY.md).

## Status

Since November 2026 a `warm` pool (`large` machines, up to 6) runs the CI of the authors'
private monorepo, with a GitHub App; `pool`, `fork` and `hybrid` were tested on the same
repository. The queue times above are simulated; [eval/](eval/results/example-2026-11.md)
measured only the setup this daemon replaces.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
