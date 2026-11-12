# The daemon on a boat machine

One `small` boat machine ($0.018 an hour, about $13 a month) runs the daemon all the
time. After that, a workflow only changes `runs-on`, and the `warm` and `hybrid` presets
can reuse machines across runs.

## From your laptop

`sw-boat-runner setup --remote` does all of the below from a macOS or Linux computer
with the boat CLI signed in (`boat login`) to the account `BOAT_API_KEY` belongs to
(`boat api-key create` makes a key). First it lists your boat machines and stops if one
already runs the daemon for the repository: one labelled `sw-boat-runner-host` for it,
or called `sw-boat-runner <owner>/<name>`. Keep that one, delete it with
`boat delete <id> --yes`, or pass `--replace`, which deletes it once the new machine's
service is active. Then:

1. It asks for `BOAT_API_KEY` without echo, then the repository, labels and snapshot,
   and offers to build the snapshot if it is missing.
2. It creates the GitHub App. The browser opens [docs/new-app.html](../../docs/new-app.html)
   with a `redirect` to `http://127.0.0.1:<random port>/callback`, which the page takes
   only in exactly that form and puts in the manifest as `redirect_url`. setup listens
   there for one request with the `state` it sent, answers "You can close this tab",
   and stops. While it waits you can paste the address the browser ends on (or just the
   code) instead, for a browser on another computer; after 15 minutes only a paste
   works. `--paste` skips the listener, `--offline-link` uses a `data:` page. It exchanges the code
   and waits for the installation on your computer. The App's key stays in
   `~/.config/sw-boat-runner/github-app-<id>.pem` (mode 600) until the machine has it,
   so a failed run can be repeated with `GITHUB_APP_ID=<id>
   GITHUB_APP_PRIVATE_KEY_FILE=<that file> sw-boat-runner setup --remote`.
3. It creates a `small` machine with no auto-stop (not `noEnv`: it runs no CI job).
4. It writes the policy, the boat key and the App's key (or `GITHUB_TOKEN` with `--pat`)
   to `~/.sbr-setup` on the machine, each through the stdin of `boat ssh` into a
   `umask 077` file. boat keeps no history of `boat ssh` commands, and no command it
   runs holds a key.
5. Over `boat ssh` it runs [install.sh](install.sh) at `SBR_REF` (default: the version
   of the binary you run), which checks the release binary and runs `setup` there with
   those files, as in the next section. `doctor`'s output shows on your terminal, and
   `~/.sbr-setup` is deleted.

It prints the machine's id; `boat ssh <id> journalctl -u sw-boat-runner -f` shows the
logs. If step 4 fails the machine is deleted; if step 5 fails it is kept for a look, may
still hold the keys in `~/.sbr-setup`, and `boat delete <id> --yes` removes it.
`setup --remote --dry-run` prints the plan and asks no secret.

## On the machine

```sh
boat new --no-auto-stop --type small    # prints the machine's id
boat ssh <id>
```

Inside the machine:

```sh
# v0.2.0 is an example: use the release you tested, here and in SBR_REF.
curl -fsSLO https://raw.githubusercontent.com/Surely-Works-LLC/sw-boat-runner/v0.2.0/examples/always-on/install.sh
SBR_REF=v0.2.0 bash install.sh    # gets the daemon, then runs `sw-boat-runner setup`
```

`setup` asks for the repository, labels (default `self-hosted, boat`) and snapshot,
creates a GitHub App and asks for `BOAT_API_KEY`. A snapshot missing from your saved
boat snapshots (listed when `BOAT_API_KEY` is set), or the answer `new`, makes it offer
to build one: "Build a runner snapshot named <name> now?". It creates a temporary
`noEnv` sandbox of the `default` type from boat's base image, extracts the latest
[actions/runner](https://github.com/actions/runner) release for its architecture,
unconfigured, at `/home/user/actions-runner` after checking the SHA-256 GitHub
publishes, runs `bin/installdependencies.sh`, creates `/home/user/ci-cache` and an
`.env` with `XDG_CACHE_HOME` there, then checks and saves it with the code of
`sw-boat-runner snapshot`. The sandbox is deleted on every path, Ctrl-C included. It
holds the runner only: for toolchains, start a sandbox from it with
`boat new --no-env --from <name>`, install them, and run
`sw-boat-runner snapshot <id> <name>`. For the App it prints a short link to
[docs/new-app.html](../../docs/new-app.html) on GitHub Pages, a static page that posts the
App's manifest to GitHub and shows the code GitHub sends back, to paste into setup; it
sends the code nowhere. `setup --offline-link` prints the page as a `data:` URL instead. `bash install.sh
policy.toml` uses your policy instead ([policy.toml](policy.toml) is a start).
`SBR_PAT=1 bash install.sh` (or `--pat`) uses the `GITHUB_TOKEN` in the environment as a
personal token instead of an App. Without it the script unsets `GITHUB_TOKEN` and
`GH_TOKEN` for setup: every boat machine has its creator's `GITHUB_TOKEN`, a token too
broad for the daemon.

Pin the same tag in the URL and in `SBR_REF`, so a reinstall gets what you tested. On
Linux x86_64 or arm64 the script downloads that release's static binary and checks it
against the release's `SHA256SUMS`; if `gh` is installed and logged in, it also checks
the binary's build provenance attestation with `gh attestation verify`, and otherwise
says the attestation was not checked (or warns, when `gh` is too old for
`gh attestation`). With glibc older than 2.35 it builds that tag with cargo. Anything
else stops the script, and it never falls back to another version: a network error, a
server error, a failed check, another platform, or a release without the binary or
`SHA256SUMS` (a 404). To build such a release from source, set `SBR_REF` to its commit. Without
`SBR_REF` it installs the latest release, and stops if that release has no `setup`.
`SBR_REF` can also be a commit hash or a branch name, which are built with cargo
(installing rustup first if cargo is missing); `main` is built only when you ask for it. [Verifying a release](#verifying-a-release) shows the
same checks by hand.

To use a GitHub App instead of a personal token, copy its private key to the machine
(over `boat scp`, or paste it into a file over `boat ssh`) and set the App variables
from [Any other host](#any-other-host) before running the script:

```sh
read -rp 'GITHUB_APP_ID: ' GITHUB_APP_ID && export GITHUB_APP_ID
export GITHUB_APP_PRIVATE_KEY_FILE=$HOME/app.pem   # GITHUB_APP_INSTALLATION_ID is optional
bash install.sh policy.toml    # asks for BOAT_API_KEY only
rm "$HOME/app.pem"
```

[install.sh](install.sh) gets the daemon, and `setup` asks for the keys without echo
and stores the policy in `/etc/sw-boat-runner` and the keys in `/etc/sw-boat-runner/env`
(mode 0600, readable by root only). An App's private key goes to
`/etc/sw-boat-runner/github-app.pem`, also 0600 and root's, and systemd hands the
service a copy with `LoadCredential`. It then enables a systemd service that runs the
daemon as an unprivileged dynamic user with a read-only file system, no access to home
directories, and no way to gain privileges. An enabled service starts again after the
machine stops and resumes, and the daemon is safe to restart at any time.
`journalctl -u sw-boat-runner` shows what it does.

Run the script over `boat ssh`, not `boat exec`: boat keeps every command sent through
`exec` for 90 days. Do not pass the keys on the command line either, where they land in
shell history. [Snapshots](../../docs/design.md#snapshots) says what the snapshot needs
and [Any other host](#any-other-host) what the two keys need.

Then point jobs at the pool:

```yaml
runs-on: [self-hosted, boat]
```

To stop using it, stop the service and delete the pool's machines and runners. `drain`
repeats until boat has listed none for about two minutes; if it fails, run it again; [Any other host](#any-other-host) says what it
does.

```sh
sudo systemctl disable --now sw-boat-runner
sudo sh -c 'set -a; . /etc/sw-boat-runner/env; /usr/local/bin/sw-boat-runner drain /etc/sw-boat-runner/policy.toml'
```

The script has been run on a boat `small` machine with a GitHub App: the policy, the
unit it writes, `LoadCredential` for the App's key and `DynamicUser`. The service starts
and runs.

## Any other host

The daemon is one process for one repository. It keeps no database and can be restarted
at any time. To run it on a host you keep up without `setup`:

```sh
cargo install --locked --git https://github.com/Surely-Works-LLC/sw-boat-runner --tag v0.2.0
# v0.2.0 is an example: pin the tag, or --rev <commit>, that you tested.
```

Set `BOAT_API_KEY` to a boat key for the account that owns the snapshot, and give the
daemon GitHub access in one of two ways. Setting both is an error.

A GitHub App (recommended). It has its own rate limit instead of a share of yours, it is
no one's personal token, and it reaches only the repositories it is installed on. Create
an app under your account or organization (Settings, Developer settings, GitHub Apps)
with webhooks off and two repository permissions: Administration read and write (to
register and delete self-hosted runners) and Actions read (to see queued jobs). Install
it on the repository and generate a private key.

| Variable | Value |
| - | - |
| `GITHUB_APP_ID` | The app's ID, or its client ID. |
| `GITHUB_APP_PRIVATE_KEY_FILE` | Path to the private key (`.pem`) GitHub generated. |
| `GITHUB_APP_INSTALLATION_ID` | Optional. Without it the daemon looks up the app's installation on the policy's repository. |

The daemon signs a JWT with the key, exchanges it for an installation token limited to
the policy's repository, and renews that token five minutes before it expires and
whenever GitHub answers 401. It never logs the key, the JWT or the token.

Or a fine-grained personal access token in `GITHUB_TOKEN`, for the repository, with
Administration read and write and Actions read. It shares its owner's 5,000 requests an
hour, so the daemon paces itself to a quarter of that
([GitHub requests](../../docs/design.md#github-requests)). The `GITHUB_TOKEN` that GitHub
Actions creates for each workflow run does not work: it cannot be given Administration,
so it cannot register runners.

Then check the policy, see what the daemon would do, and run it:

```sh
sw-boat-runner check policy.toml   # validate, print each pool with its preset applied
sw-boat-runner plan policy.toml    # observe once, print the actions as JSON; changes nothing
sw-boat-runner run policy.toml     # the loop; SIGINT or SIGTERM stops it after the tick
sw-boat-runner drain policy.toml   # delete the pools' machines and runners, to stop using them
```

`check` also warns on stderr about a pool whose `register_timeout_s` is 600 or less.
`plan policy.toml snapshot.json` plans for a saved snapshot instead, with no credentials;
[tests/scenarios](../../tests/scenarios/) has snapshots to start from.

`drain` deletes the pools' machines and runners, and leaves every other machine and
runner alone, including another repository's. It repeats every `tick_s` until boat has
listed no machine for `--settle-s` seconds (default: the 90 s request timeout plus
`tick_s`, how long boat may take to list a fork the daemon made just before it stopped).
It fails if a machine is left after `--timeout-s` (default 900), for example one whose
runner is still running a job; run it again. `run --drain-on-stop` drains the same way
in the daemon itself once SIGINT or SIGTERM stops it. Then point `runs-on` back at another
runner.

`run` and `doctor` warn about machines of this repository in a pool the policy no longer
has, which nothing deletes, and print the `drain` or `boat delete` command for them.

## Verifying a release

Each release has `sw-boat-runner-<target>.tar.gz` for `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu` and `aarch64-apple-darwin`, a `SHA256SUMS` file, and a build
provenance attestation signed by the `release` workflow. To check a download by hand:

```sh
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify sw-boat-runner-x86_64-unknown-linux-gnu.tar.gz --repo Surely-Works-LLC/sw-boat-runner
```

`SHA256SUMS` is published on the same release as the tarballs, so the checksum only
catches a corrupted or truncated download, not a release whose files were replaced
together. The attestation is what shows the tarball was built by this repository's
`release` workflow from the tagged commit; check it whenever you can.
