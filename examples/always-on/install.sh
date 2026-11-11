#!/usr/bin/env bash
# Gets the sw-boat-runner binary and runs `sw-boat-runner setup`, which asks for the
# repository, labels and snapshot, creates a GitHub App, asks for BOAT_API_KEY without
# echo, and installs the systemd service. `sw-boat-runner setup --help` lists its flags.
# usage: bash install.sh [policy.toml] [setup flags...]
# With a policy file, setup uses it instead of asking. A GITHUB_TOKEN in the environment
# means a personal token instead of an App (setup --pat); GITHUB_APP_ID and
# GITHUB_APP_PRIVATE_KEY_FILE mean an App you already have.
# SBR_REF picks the version: unset for the latest release, a tag starting with v and a
# digit, a commit hash or a branch. For a release (Linux x86_64 or arm64) it downloads the
# static binary, checks it against the release's SHA256SUMS and, when `gh` is installed
# and logged in, its build provenance attestation. A release with no binary for this
# machine is built from its tag with cargo; a commit or branch is built with cargo. Any
# other failure (network, server error, checksum or attestation mismatch) stops the script.
# Pin a tag or commit for production.
set -euo pipefail

repo=Surely-Works-LLC/sw-boat-runner
ref=${SBR_REF:-}

args=()
if [ $# -gt 0 ] && [[ $1 != -* ]]; then
    args+=(--policy "$1")
    shift
fi
if [ -n "${GITHUB_TOKEN:-}" ]; then
    args+=(--pat)
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# fetch URL FILE: downloads URL to FILE and prints the HTTP status, or fails on a network
# error. The caller decides what each status means.
fetch() {
    curl --proto '=https' -sSL -o "$2" -w '%{http_code}' "$1" ||
        { echo "could not download $1; not installing" >&2; return 1; }
}

# Sets bin to the release binary for this machine, or sets tag and returns 1 when the
# release has no binary for it, to be built from that tag. Anything else stops the script.
download() {
    local target url base file code checked
    tag=$ref
    if [ -z "$tag" ]; then
        if ! url=$(curl --proto '=https' -fsSLI -o /dev/null -w '%{url_effective}' \
            "https://github.com/$repo/releases/latest"); then
            echo "could not look up the latest release of $repo; not installing" >&2
            exit 1
        fi
        if ! [[ $url =~ /releases/tag/(v[0-9][^/]*)$ ]]; then
            echo "$repo has no release; set SBR_REF to a tag, a commit or main" >&2
            exit 1
        fi
        tag=${BASH_REMATCH[1]}
    fi
    case "$(uname -s)/$(uname -m)" in
        Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
        Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-gnu ;;
        *) return 1 ;;
    esac
    # The Linux binaries need glibc 2.35 or newer; older hosts build the tag from source.
    if [[ $target == *-linux-gnu ]]; then
        glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')
        if [[ -z $glibc ]] || [[ $(printf '%s\n' 2.35 "$glibc" | sort -V | head -1) != 2.35 ]]; then
            echo "glibc ${glibc:-unknown} is older than 2.35; building $tag from source" >&2
            return 1
        fi
    fi
    base=https://github.com/$repo/releases/download/$tag
    file=sw-boat-runner-$target.tar.gz
    code=$(fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS") || exit 1
    case $code in
        200) ;;
        404) return 1 ;;
        *) echo "SHA256SUMS of $tag: HTTP $code; not installing" >&2; exit 1 ;;
    esac
    code=$(fetch "$base/$file" "$tmp/$file") || exit 1
    case $code in
        200) ;;
        404) return 1 ;;
        *) echo "$file of $tag: HTTP $code; not installing" >&2; exit 1 ;;
    esac
    # An empty match fails too: sha256sum finds no checksum line.
    if ! (cd "$tmp" && awk -v f="$file" '$2 == f' SHA256SUMS | sha256sum --strict -c -); then
        echo "$file from $tag does not match its SHA256SUMS; not installing" >&2
        exit 1
    fi
    if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
        if ! gh attestation verify "$tmp/$file" -R "$repo"; then
            echo "$file from $tag failed its attestation check; not installing" >&2
            exit 1
        fi
        checked="SHA-256 and build provenance attestation checked"
    else
        checked="SHA-256 checked; attestation not checked (needs gh, logged in)"
    fi
    tar -xzf "$tmp/$file" -C "$tmp" sw-boat-runner
    bin=$tmp/sw-boat-runner
    echo "sw-boat-runner $tag for $target, $checked"
}

tag='' bin=''
if [ -n "$ref" ] && ! [[ $ref =~ ^v[0-9] ]]; then
    # A commit or a branch, asked for by name: there is no release binary for it.
    if [[ $ref =~ ^[0-9a-f]{7,40}$ ]]; then from=(--rev "$ref"); else from=(--branch "$ref"); fi
elif ! download; then
    from=(--tag "$tag")
    echo "$tag has no release binary for this machine; building it with cargo"
fi
if [ -z "${bin:-}" ]; then
    echo "building ${from[*]} with cargo"
    if ! command -v cargo >/dev/null; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        # shellcheck source=/dev/null
        . "$HOME/.cargo/env"
    fi
    cargo install --locked --git "https://github.com/$repo" "${from[@]}"
    bin=$(command -v sw-boat-runner)
fi

# Releases before `setup` existed cannot install themselves; say so instead of failing
# halfway, and never fall back to another version.
if ! "$bin" setup --help >/dev/null 2>&1; then
    echo "sw-boat-runner ${tag:-$ref} has no \`setup\` command; set SBR_REF to a newer release" >&2
    exit 1
fi
# setup copies this binary to /usr/local/bin. Not exec, so the EXIT trap removes $tmp.
"$bin" setup ${args[@]+"${args[@]}"} "$@"
