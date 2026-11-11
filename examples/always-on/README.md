# The daemon on a boat machine

One `small` boat machine ($0.018 an hour, about $13 a month) runs the daemon all the
time. After that, a workflow only changes `runs-on`, and the `warm` and `hybrid` presets
can reuse machines across runs.

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
creates a GitHub App and asks for `BOAT_API_KEY`. For the App it prints a short link to
[docs/new-app.html](../../docs/new-app.html) on GitHub Pages, a static page that posts the
App's manifest to GitHub and shows the code GitHub sends back, to paste into setup; it
sends the code nowhere. `setup --offline-link` prints the page as a `data:` URL instead. `bash install.sh policy.toml` uses your policy instead
([policy.toml](policy.toml) is a start), and a `GITHUB_TOKEN` in the environment means a
personal token instead of an App.

Pin the same tag in the URL and in `SBR_REF`, so a reinstall gets what you tested. On
Linux x86_64 or arm64 the script downloads that release's static binary and checks it
against the release's `SHA256SUMS`; if `gh` is installed and logged in, it also checks
the binary's build provenance attestation with `gh attestation verify`, and otherwise
says the attestation was not checked. On other machines, or for a release with no binary
for the machine, it builds that tag with cargo. A network error, a server error or a
failed check stops the script; it never falls back to another version. Without
`SBR_REF` it installs the latest release, and stops if that release has no `setup`.
`SBR_REF` can also be a commit hash or a branch name, which are built with cargo; `main`
is built only when you ask for it. [Verifying a release](#verifying-a-release) shows the
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

To stop using it, stop the service and delete the pool's machines and runners. Repeat
the `drain` line until it succeeds; [Any other host](#any-other-host) says what it
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
sw-boat-runner run policy.toml     # the loop
sw-boat-runner drain policy.toml   # delete the pools' machines and runners, to stop using them
```

`check` also warns on stderr about a pool whose `register_timeout_s` is 600 or less.
`plan policy.toml snapshot.json` plans for a saved snapshot instead, with no credentials;
[tests/scenarios](../../tests/scenarios/) has snapshots to start from.

`drain` deletes the pools' machines and runners, and leaves every other machine and
runner alone. It fails while a machine is left, for example one whose runner is still
running a job, so run it again until it succeeds. Then point `runs-on` back at another
runner.

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
