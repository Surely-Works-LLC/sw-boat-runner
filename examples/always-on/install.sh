#!/bin/sh
# Installs sw-boat-runner as a systemd service on the machine this runs on.
# usage: BOAT_API_KEY=... GITHUB_TOKEN=... sh install.sh policy.toml
set -eu

: "${BOAT_API_KEY:?set BOAT_API_KEY}" "${GITHUB_TOKEN:?set GITHUB_TOKEN}"
policy=${1:?usage: install.sh policy.toml}

if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
fi
cargo install --locked --git https://github.com/Surely-Works-LLC/sw-boat-runner
bin=$(command -v sw-boat-runner)
"$bin" check "$policy"

sudo install -d -m 700 /etc/sw-boat-runner
sudo install -m 600 "$policy" /etc/sw-boat-runner/policy.toml
sudo install -m 755 "$bin" /usr/local/bin/sw-boat-runner
# Written through a pipe, so the keys are in no command line.
printf 'BOAT_API_KEY=%s\nGITHUB_TOKEN=%s\n' "$BOAT_API_KEY" "$GITHUB_TOKEN" |
    sudo sh -c 'umask 077; cat > /etc/sw-boat-runner/env'

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

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable --now sw-boat-runner
systemctl --no-pager status sw-boat-runner
