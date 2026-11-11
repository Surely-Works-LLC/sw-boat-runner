#!/usr/bin/env bash
# Installs sw-boat-runner as a systemd service on the machine this runs on.
# usage: bash install.sh policy.toml
# Asks for BOAT_API_KEY and GITHUB_TOKEN without echo. Either can come from the
# environment instead, but a key typed on a command line lands in shell history.
set -euo pipefail

policy=${1:?usage: install.sh policy.toml}

ask() {
    if [ -z "${!1:-}" ]; then
        read -rsp "$1: " "$1" || true
        echo
    fi
    [ -n "${!1}" ] || { echo "$1 is empty" >&2; exit 1; }
}
ask BOAT_API_KEY
ask GITHUB_TOKEN

if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi
cargo install --locked --git https://github.com/Surely-Works-LLC/sw-boat-runner --tag v0.1.0
bin=$(command -v sw-boat-runner)
"$bin" check "$policy"

# The policy holds no secret, and the service's dynamic user must read it. The keys go
# in a root-only file that systemd reads before it drops to that user.
sudo install -d -m 755 /etc/sw-boat-runner
sudo install -m 644 "$policy" /etc/sw-boat-runner/policy.toml
sudo install -m 755 "$bin" /usr/local/bin/sw-boat-runner
# Written through a pipe, so the keys are in no command line.
printf 'BOAT_API_KEY=%s\nGITHUB_TOKEN=%s\n' "$BOAT_API_KEY" "$GITHUB_TOKEN" |
    sudo sh -c 'umask 077; cat > /etc/sw-boat-runner/env'

# The daemon only reads its policy and talks HTTPS, so it runs as an unprivileged
# dynamic user that cannot write the file system or see home directories.
sudo tee /etc/systemd/system/sw-boat-runner.service >/dev/null <<'UNIT'
[Unit]
Description=sw-boat-runner
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=/etc/sw-boat-runner/env
ExecStart=/usr/local/bin/sw-boat-runner run /etc/sw-boat-runner/policy.toml
Restart=always
RestartSec=30
DynamicUser=yes
NoNewPrivileges=yes
CapabilityBoundingSet=
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable --now sw-boat-runner
systemctl --no-pager status sw-boat-runner
