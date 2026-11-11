#!/usr/bin/env bash
# Installs sw-boat-runner as a systemd service on the machine this runs on.
# usage: bash install.sh policy.toml
# Asks for BOAT_API_KEY and GITHUB_TOKEN without echo. Either can come from the
# environment instead, but a key typed on a command line lands in shell history.
# For a GitHub App instead of GITHUB_TOKEN, set GITHUB_APP_ID, GITHUB_APP_PRIVATE_KEY_FILE
# and, if you like, GITHUB_APP_INSTALLATION_ID in the environment; the key file is copied
# into /etc/sw-boat-runner.
# SBR_REF picks what to build: a branch (default main), a tag starting with v and a digit,
# or a commit hash. Pin a commit for production.
set -euo pipefail

ref=${SBR_REF:-main}
if [[ $ref =~ ^[0-9a-f]{7,40}$ ]]; then
    from=(--rev "$ref")
elif [[ $ref =~ ^v[0-9] ]]; then
    from=(--tag "$ref")
else
    from=(--branch "$ref")
fi

policy=${1:?usage: install.sh policy.toml}

ask() {
    if [ -z "${!1:-}" ]; then
        read -rsp "$1: " "$1" || true
        echo
    fi
    [ -n "${!1}" ] || { echo "$1 is empty" >&2; exit 1; }
}
ask BOAT_API_KEY
app=
if [ -n "${GITHUB_APP_ID:-}${GITHUB_APP_PRIVATE_KEY_FILE:-}${GITHUB_APP_INSTALLATION_ID:-}" ]; then
    app=1
    [ -z "${GITHUB_TOKEN:-}" ] || { echo "set GITHUB_TOKEN or GITHUB_APP_*, not both" >&2; exit 1; }
    : "${GITHUB_APP_ID:?set GITHUB_APP_ID}" "${GITHUB_APP_PRIVATE_KEY_FILE:?set GITHUB_APP_PRIVATE_KEY_FILE}"
    [ -r "$GITHUB_APP_PRIVATE_KEY_FILE" ] || { echo "cannot read $GITHUB_APP_PRIVATE_KEY_FILE" >&2; exit 1; }
else
    ask GITHUB_TOKEN
fi

if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi
cargo install --locked --git https://github.com/Surely-Works-LLC/sw-boat-runner "${from[@]}"
bin=$(command -v sw-boat-runner)
"$bin" check "$policy"

# The policy holds no secret, and the service's dynamic user must read it. The keys go
# in a root-only file that systemd reads before it drops to that user.
sudo install -d -m 755 /etc/sw-boat-runner
sudo install -m 644 "$policy" /etc/sw-boat-runner/policy.toml
sudo install -m 755 "$bin" /usr/local/bin/sw-boat-runner
# Written through a pipe, so the keys are in no command line.
key=/etc/sw-boat-runner/github-app.pem
exec_start='/usr/local/bin/sw-boat-runner run /etc/sw-boat-runner/policy.toml'
credential=
if [ -n "$app" ]; then
    # Read as you, written by root under umask 077.
    # shellcheck disable=SC2024
    sudo sh -c "umask 077; cat > $key" <"$GITHUB_APP_PRIVATE_KEY_FILE"
    {
        printf 'BOAT_API_KEY=%s\nGITHUB_APP_ID=%s\nGITHUB_APP_PRIVATE_KEY_FILE=%s\n' \
            "$BOAT_API_KEY" "$GITHUB_APP_ID" "$key"
        if [ -n "${GITHUB_APP_INSTALLATION_ID:-}" ]; then
            printf 'GITHUB_APP_INSTALLATION_ID=%s\n' "$GITHUB_APP_INSTALLATION_ID"
        fi
    } | sudo sh -c 'umask 077; cat > /etc/sw-boat-runner/env'
    # The dynamic user cannot read the root-only key, so systemd hands the service a copy
    # as a credential and the daemon reads that one. `$$` is a literal `$` to systemd.
    credential="LoadCredential=github-app.pem:$key"
    exec_start="/bin/sh -c 'GITHUB_APP_PRIVATE_KEY_FILE=\"\$\${CREDENTIALS_DIRECTORY}/github-app.pem\" exec $exec_start'"
else
    printf 'BOAT_API_KEY=%s\nGITHUB_TOKEN=%s\n' "$BOAT_API_KEY" "$GITHUB_TOKEN" |
        sudo sh -c 'umask 077; cat > /etc/sw-boat-runner/env'
fi

# The daemon only reads its policy and talks HTTPS, so it runs as an unprivileged
# dynamic user that cannot write the file system or see home directories.
sudo tee /etc/systemd/system/sw-boat-runner.service >/dev/null <<UNIT
[Unit]
Description=sw-boat-runner
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=/etc/sw-boat-runner/env
${credential}
ExecStart=${exec_start}
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
