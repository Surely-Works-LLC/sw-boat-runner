# The daemon on a boat machine

One `small` boat machine ($0.018 an hour, about $13 a month) runs the daemon all the
time. After that, a workflow only changes `runs-on`, and the `warm` and `hybrid` presets
can reuse machines across runs.

```sh
boat new --no-auto-stop --type small    # prints the machine's id
boat ssh <id>
```

Inside the machine, write your `policy.toml` ([policy.toml](policy.toml) is a start), then:

```sh
curl -fsSLO https://raw.githubusercontent.com/Surely-Works-LLC/sw-boat-runner/v0.1.0/examples/always-on/install.sh
bash install.sh policy.toml    # asks for BOAT_API_KEY and GITHUB_TOKEN
```

To use a GitHub App instead of a personal token, copy its private key to the machine
(over `boat scp`, or paste it into a file over `boat ssh`) and set the App variables
from the [README](../../README.md#connect-a-repository) before running the script:

```sh
read -rp 'GITHUB_APP_ID: ' GITHUB_APP_ID && export GITHUB_APP_ID
export GITHUB_APP_PRIVATE_KEY_FILE=$HOME/app.pem   # GITHUB_APP_INSTALLATION_ID is optional
bash install.sh policy.toml    # asks for BOAT_API_KEY only
rm "$HOME/app.pem"
```

The App variables need a release after 0.1.0. Until one is out, change `--tag v0.1.0` in
the script to `--branch main`.

[install.sh](install.sh) asks for the two keys without echoing them, builds the daemon,
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
shell history. The [README](../../README.md#connect-a-repository) says what the
snapshot and the two keys need.

Then point jobs at the pool:

```yaml
runs-on: [self-hosted, large]
```

To stop using it, stop the service and delete the pool's machines and runners. Repeat
the `drain` line until it succeeds; the [README](../../README.md#connect-a-repository)
says what it does.

```sh
sudo systemctl disable --now sw-boat-runner
sudo sh -c 'set -a; . /etc/sw-boat-runner/env; /usr/local/bin/sw-boat-runner drain /etc/sw-boat-runner/policy.toml'
```

The script has been checked with `bash -n` and `shellcheck` only. It has not been run on a
boat machine.
