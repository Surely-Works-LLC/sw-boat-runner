#!/usr/bin/env bash
# Gets the sw-boat-runner binary and runs `sw-boat-runner setup`, which asks for the
# repository, labels and snapshot, creates a GitHub App, asks for BOAT_API_KEY without
# echo, and installs the systemd service. `sw-boat-runner setup --help` lists its flags.
# usage: bash install.sh [policy.toml] [setup flags...]
# With a policy file, setup uses it instead of asking. GITHUB_APP_ID and
# GITHUB_APP_PRIVATE_KEY_FILE mean an App you already have. A personal token is used
# only when you ask for it, with SBR_PAT=1 or --pat; otherwise GITHUB_TOKEN and GH_TOKEN
# are unset for setup, since every boat machine has its creator's GITHUB_TOKEN. They stay
# set for the download, so that `gh` can check the attestation with them.
# SBR_REF picks the version: unset for the latest release, a tag starting with v and a
# digit, a commit hash or a branch. For a release (Linux x86_64 or arm64) it downloads the
# prebuilt binary (dynamically linked against glibc 2.35 or newer), checks it against the
# release's SHA256SUMS and, when `gh` is installed and signed in, its build provenance
# attestation; it prints which of the two checks ran. Only two cases build with cargo: a
# commit or branch, and a release on a host with glibc older than 2.35. Any other failure
# (network, a missing release file, a server error, a checksum or attestation mismatch)
# stops the script. Pin a tag or commit for production. SBR_SOURCE_REPO=owner/name installs
# from a fork instead; setup gets it too.
set -euo pipefail

repo=${SBR_SOURCE_REPO:-Surely-Works-LLC/sw-boat-runner}
if ! [[ $repo =~ ^[A-Za-z0-9_-][A-Za-z0-9._-]*/[A-Za-z0-9_-][A-Za-z0-9._-]*$ ]]; then
    echo "SBR_SOURCE_REPO $repo is not owner/name; not installing" >&2
    exit 1
fi
export SBR_SOURCE_REPO=$repo
ref=${SBR_REF:-}

args=()
if [ $# -gt 0 ] && [[ $1 != -* ]]; then
    args+=(--policy "$1")
    shift
fi
pat=''
for a in "$@"; do
    if [ "$a" = --pat ]; then pat=1; fi
done
if [ "${SBR_PAT:-}" = 1 ] && [ -z "$pat" ]; then
    args+=(--pat)
    pat=1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# fetch URL FILE: downloads URL to FILE and prints the HTTP status, or fails on a network
# error. The caller decides what each status means.
fetch() {
    curl --proto '=https' -sSL -o "$2" -w '%{http_code}' "$1" ||
        { echo "could not download $1; not installing" >&2; return 1; }
}

# Sets bin to the release binary for this machine, or sets tag and returns 1 when this
# host's glibc is too old for it, to be built from that tag. Anything else stops the
# script: a release that lacks a file is not built from source behind your back.
download() {
    local target url base file code checked sum f
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
        *)
            echo "$tag has no binary for $(uname -s) $(uname -m); not installing." >&2
            echo "Set SBR_REF to the tag's commit to build it with cargo." >&2
            exit 1
            ;;
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
    for f in SHA256SUMS "$file"; do
        code=$(fetch "$base/$f" "$tmp/$f") || exit 1
        if [ "$code" != 200 ]; then
            echo "$f of $tag: HTTP $code; not installing." >&2
            if [ "$code" = 404 ]; then
                echo "To build $tag from source, set SBR_REF to its commit." >&2
            fi
            exit 1
        fi
    done
    # No line for the file fails too; not every sha256sum fails on empty input.
    sum=$(awk -v f="$file" '$2 == f' "$tmp/SHA256SUMS")
    if [ -z "$sum" ] || ! (cd "$tmp" && printf '%s\n' "$sum" | sha256sum --strict -c -); then
        echo "$file from $tag does not match its SHA256SUMS; not installing" >&2
        exit 1
    fi
    # gh signs in with GH_TOKEN or GITHUB_TOKEN when set: they are unset only for setup.
    if ! command -v gh >/dev/null; then
        checked="SHA-256 only: gh not installed, attestation not checked"
    elif ! gh auth status >/dev/null 2>&1; then
        checked="SHA-256 only: gh not signed in, attestation not checked"
    elif ! gh attestation --help >/dev/null 2>&1; then
        # `gh attestation` came in gh 2.49.
        echo "warning: $(gh --version | head -1) has no \`gh attestation\`; not checking the attestation" >&2
        checked="SHA-256 only: gh too old, attestation not checked"
    else
        if ! gh attestation verify "$tmp/$file" -R "$repo"; then
            echo "$file from $tag failed its attestation check; not installing" >&2
            exit 1
        fi
        checked="SHA-256 checked and attestation verified"
    fi
    # set -e does not apply here: download runs as an `if` condition.
    if ! tar -xzf "$tmp/$file" -C "$tmp" sw-boat-runner; then
        echo "could not unpack $file; not installing" >&2
        exit 1
    fi
    bin=$tmp/sw-boat-runner
    echo "sw-boat-runner $tag for $target, $checked"
}

tag='' bin=''
if [ -n "$ref" ] && ! [[ $ref =~ ^v[0-9] ]]; then
    # A commit or a branch, asked for by name: there is no release binary for it.
    if [[ $ref =~ ^[0-9a-f]{7,40}$ ]]; then from=(--rev "$ref"); else from=(--branch "$ref"); fi
elif ! download; then
    from=(--tag "$tag")
fi
if [ -z "${bin:-}" ]; then
    echo "building ${from[*]} with cargo"
    if ! command -v cargo >/dev/null; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        # shellcheck source=/dev/null
        . "$HOME/.cargo/env"
    fi
    # Into $tmp, not ~/.cargo/bin, which a non-interactive shell's PATH may lack.
    cargo install --locked --root "$tmp/cargo" --git "https://github.com/$repo" "${from[@]}"
    bin=$tmp/cargo/bin/sw-boat-runner
fi

# Releases before `setup` existed cannot install themselves; say so instead of failing
# halfway, and never fall back to another version.
if ! "$bin" setup --help >/dev/null 2>&1; then
    echo "sw-boat-runner ${tag:-$ref} has no \`setup\` command; set SBR_REF to a newer release" >&2
    exit 1
fi
# setup copies this binary to /usr/local/bin. Not exec, so the EXIT trap removes $tmp.
if [ -n "$pat" ]; then
    "$bin" setup ${args[@]+"${args[@]}"} "$@"
else
    (
        unset GITHUB_TOKEN GH_TOKEN
        "$bin" setup ${args[@]+"${args[@]}"} "$@"
    )
fi
