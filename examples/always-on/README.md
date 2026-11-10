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
curl -fsSLO https://raw.githubusercontent.com/Surely-Works-LLC/sw-boat-runner/main/examples/always-on/install.sh
BOAT_API_KEY=... GITHUB_TOKEN=... sh install.sh policy.toml
```

[install.sh](install.sh) builds the daemon, stores the policy and the two keys under
`/etc/sw-boat-runner` (readable by root only), and enables a systemd service. An enabled
service starts again after the machine stops and resumes, and the daemon is safe to
restart at any time. `journalctl -u sw-boat-runner` shows what it does.

Type the keys over `boat ssh`, not `boat exec`: boat keeps every command sent through
`exec` for 90 days. The [README](../../README.md#connect-a-repository) says what the
snapshot and the two keys need.

Then point jobs at the pool:

```yaml
runs-on: [self-hosted, large]
```

The script has been checked for syntax only. It has not been run on a boat machine.
