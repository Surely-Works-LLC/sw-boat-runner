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

One daemon serves one repository. It registers its runners on that repository, so the
App needs Administration write; organization-level runners are not supported. `setup`
installs it as a systemd service on a Linux host, at fixed paths, so ten repositories
take ten hosts.

## You need

- A boat account on a plan that allows the machine types your pools use. Plans differ in
  included starts, from 200 a day on the $20 plan to 2,400 on the $2000 plan
  ([Why](docs/design.md#why)).
- The [boat CLI](https://docs.boat.dev/cli-reference), signed in (`boat login`), and a
  boat API key for the same account from `boat api-key create`. boat shows the key once.
- Admin rights on the repository. For an organization's repository, an organization
  owner (or app manager) creates the App and an owner installs it; anyone else gets
  GitHub's "Request" screen and waits for an owner to approve.
- Money for machine time. The host is a `small` machine with no auto-stop ($0.018 an
  hour, about $13 a month). Pool machines cost machine time by type while they are up
  (`large` is $0.072 an hour; [boat's pricing](https://docs.boat.dev/pricing.md) has the
  others), and starts past the plan's included ones cost extra unless
  `paid_start_cents_per_day` is 0, the default.
- Optional: `gh`, signed in. `setup` takes the default repository from it, and with it
  you and install.sh can check the release's attestation.

## Quick start

Releases run on Linux x86_64 and arm64 with glibc 2.35 or newer, and `--remote` also on
macOS arm64. On an Intel Mac, build from source (`cargo install`, as in
[Any other host](examples/always-on/README.md#any-other-host)). Use the release you
tested; v0.3.0 below is an example.

### From your laptop (recommended)

1. Download the release and check it:

   ```sh
   # macOS arm64. On Linux: x86_64-unknown-linux-gnu or aarch64-unknown-linux-gnu.
   T=sw-boat-runner-aarch64-apple-darwin.tar.gz
   base=https://github.com/Surely-Works-LLC/sw-boat-runner/releases/download/v0.3.0
   curl -fsSLO "$base/$T"
   curl -fsSLO "$base/SHA256SUMS"
   grep " $T\$" SHA256SUMS | shasum -a 256 -c
   gh attestation verify "$T" --repo Surely-Works-LLC/sw-boat-runner
   tar -xzf "$T"
   ```

2. Run `./sw-boat-runner setup --remote`. `--dry-run` first prints the plan and asks no
   secret.
3. Answer the prompts: the repository, then the boat key without echo, the labels and
   the snapshot. Before it asks for any secret it stops if a boat machine already runs
   the daemon for the repository (`--replace` swaps it for the new one).
4. Create the GitHub App in the browser it opens and install it on the repository.
   GitHub sends the browser back to a one-shot listener on `127.0.0.1`, so there is
   nothing to paste unless the browser is on another computer.
5. Wait. It creates the host machine, installs the same release there with
   [install.sh](examples/always-on/install.sh) over `boat ssh`, and runs `doctor`. It
   prints the machine's id and `boat ssh <id> journalctl -u sw-boat-runner -f` for the
   logs.

### On a Linux machine

On a boat machine, or any Ubuntu host with systemd and sudo:

1. Create and enter the host (skip on your own host):

   ```sh
   boat new --no-auto-stop --type small    # prints the machine's id
   boat ssh <id>
   ```

2. Get install.sh at the tag you tested and run it with the same tag:

   ```sh
   curl -fsSLO https://raw.githubusercontent.com/Surely-Works-LLC/sw-boat-runner/v0.3.0/examples/always-on/install.sh
   SBR_REF=v0.3.0 bash install.sh
   ```

   It checks the binary against the release's `SHA256SUMS`, and its attestation when `gh`
   is signed in, prints which checks ran (`SHA-256 checked and attestation verified`, or
   `SHA-256 only:` and why), then runs `sw-boat-runner setup`.
3. Answer the prompts: repository, labels and snapshot.
4. Open the short link setup prints, in a browser on any computer. The page creates the
   App; paste the code it shows back into setup, then install the App on the repository.
5. Enter the boat key. setup enables the service and runs `doctor`.

### Both paths

`setup` writes the answers as the policy:

```toml
repo = "acme/app"
labels = ["self-hosted", "boat"]
snapshot = "snap-1"
```

The prompts offer defaults: the repository from `gh` or the `origin` remote, labels
`self-hosted, boat`, and your saved boat snapshots when `BOAT_API_KEY` is set. The
snapshot holds an unconfigured [actions/runner](https://github.com/actions/runner) at
`/home/user/actions-runner`, saved from a `--no-env` sandbox. If you have none, answer
`new` and setup builds one in about 5 minutes
([how](examples/always-on/README.md#on-the-machine)).

Point jobs at the pool with `runs-on: [self-hosted, boat]`.
[examples/always-on](examples/always-on/) has the details.

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

## Operating

Run these on the host (`boat ssh <id>` for a boat machine).

Change the policy: edit `/etc/sw-boat-runner/policy.toml` and run
`sudo systemctl restart sw-boat-runner`.

Upgrade: run install.sh again with the new tag, in the URL and in `SBR_REF`, and pass
the current policy. setup reuses the App stored in `/etc/sw-boat-runner/env`, asks for
the boat key again and restarts the service.

```sh
curl -fsSLO https://raw.githubusercontent.com/Surely-Works-LLC/sw-boat-runner/<tag>/examples/always-on/install.sh
SBR_REF=<tag> bash install.sh /etc/sw-boat-runner/policy.toml
```

From the laptop, `setup --remote --replace` with the new release builds a new host and
deletes the old one once `doctor` finds no FAIL. It creates a new App too, so delete the
old one afterwards.

Rotate the boat key: make a new one with `boat api-key create`, replace the
`BOAT_API_KEY=` line in `/etc/sw-boat-runner/env` (`sudoedit`), run
`sudo systemctl restart sw-boat-runner` and `sudo sw-boat-runner doctor`, then revoke the
old key in boat.

Rotate the App's private key: generate a new one in the App's settings on GitHub, write
it over `/etc/sw-boat-runner/github-app.pem` (`sudo tee` and paste, so the file keeps
mode 600), restart the service, check with `sudo sw-boat-runner doctor`, then delete the
old key on GitHub.

Uninstall: stop the service and drain its pools, which deletes the pools' machines and
runners. If `drain` keeps failing, delete what is left with `boat delete <id> --yes`.

```sh
sudo systemctl disable --now sw-boat-runner
sudo sh -c 'set -a; . /etc/sw-boat-runner/env; /usr/local/bin/sw-boat-runner drain /etc/sw-boat-runner/policy.toml'
sudo rm -rf /etc/sw-boat-runner /etc/systemd/system/sw-boat-runner.service /usr/local/bin/sw-boat-runner
sudo systemctl daemon-reload
```

For a `--remote` install, run the first two lines over `boat ssh <id>` and then
`boat delete <id> --yes` the host instead of the rest. Last, delete the App in its
owner's GitHub settings (Developer settings, GitHub Apps).

## Don't trust us? Fork it

Check a release against its `SHA256SUMS` and `gh attestation verify`
([how](examples/always-on/README.md#verifying-a-release)), and pin a tag or commit SHA.
Or fork it, or build your own. A fork that installs itself must enable GitHub Pages from
`main` and `/docs`, publish its own release, and set `SBR_UPSTREAM=<owner>/<name>` for
install.sh and setup (`SBR_REPO` stays the repository the runners serve). setup then
links to `https://<owner>.github.io/<name>/new-app.html` for the App, with the owner in
lowercase, unless `SBR_NEW_APP_PAGE` names another https address without a query. The
page names the fork as the App's homepage when it is served from such an address. These
prompts hand either job to a coding agent.

<details>
<summary>Agent prompt: fork and run your own</summary>

```text
Fork https://github.com/Surely-Works-LLC/sw-boat-runner and pin the latest release's
commit. Review what runs with my keys: .github/workflows/, examples/always-on/install.sh,
src/setup/, and how src/github_app.rs handles the App key and tokens. Report anything
that sends secrets anywhere but GitHub and boat. Build from source at that commit, set up
the daemon for <OWNER/REPO> with snapshot <SNAPSHOT> per README.md, and never put keys
in commands or logs. Confirm a test job with runs-on: [self-hosted, boat] runs.

(Optional.) Then watch real CI on the pool for a day, tune the policy, snapshot and
caches for cost and queue time, and report problems at
https://github.com/Surely-Works-LLC/sw-boat-runner/issues without secrets or private
repo details.
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
```

</details>

## Security and verification

The App needs Administration write, the least GitHub accepts for registering a runner,
and Actions read. Administration write can change settings, add deploy keys or delete
the repository, so install the App only where you need runners. `setup` keeps the keys
root-only in `/etc/sw-boat-runner`.

The App comes from [docs/new-app.html](docs/new-app.html), which GitHub Pages serves from
`main` and `/docs`. The page builds the manifest that sets the App's permissions, so
every release's `setup` uses the page as it is on `main` today, not as it was at its tag:
a change to the page reaches every released binary once it is on `main`.
`setup --offline-link` prints a `data:` URL with the page built into the binary instead.

The `--remote` host is not a `noEnv` machine, so boat may give it its creator's logins,
such as a GitHub token, next to the boat key and the App's key.

TLC checks a [TLA+ model](spec/README.md) of the 0.2.0 loop, for example that no machine
stops mid-job. Repository scoping, `drain`'s settle wait and the early delete of a fork
whose job left the queue came in 0.3.0 and are not modelled yet.

Report vulnerabilities per [SECURITY.md](SECURITY.md).

## Status

An early release (0.x): a new version may change the policy or setup, so read the
[changelog](CHANGELOG.md) before you upgrade. Since 2026-11-11 it has run the CI of one
busy production monorepo with two pools and a GitHub App; `pool`, `fork` and `hybrid` were
tested on the same repository. The queue times above are simulated;
[eval/](eval/results/example-2026-11.md) measured only the setup this daemon replaces.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
