#!/usr/bin/env bash
# Builds sw-boat-runner and runs `sw-boat-runner setup`, which asks for the repository,
# labels and snapshot, creates a GitHub App, asks for BOAT_API_KEY without echo, and
# installs the systemd service. `sw-boat-runner setup --help` lists its flags.
# usage: bash install.sh [policy.toml] [setup flags...]
# With a policy file, setup uses it instead of asking. A GITHUB_TOKEN in the environment
# means a personal token instead of an App (setup --pat); GITHUB_APP_ID and
# GITHUB_APP_PRIVATE_KEY_FILE mean an App you already have.
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

args=()
if [ $# -gt 0 ] && [[ $1 != -* ]]; then
    args+=(--policy "$1")
    shift
fi
if [ -n "${GITHUB_TOKEN:-}" ]; then
    args+=(--pat)
fi

if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
fi
cargo install --locked --git https://github.com/Surely-Works-LLC/sw-boat-runner "${from[@]}"
exec "$(command -v sw-boat-runner)" setup ${args[@]+"${args[@]}"} "$@"
