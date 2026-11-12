//! examples/always-on/install.sh, run with fake `curl`, `uname`, `getconf`, `gh` and
//! `cargo` first on PATH. The release it "downloads" is a directory of files, and its
//! binary is a script that records how `setup` was called. Nothing touches the network.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/always-on/install.sh");
const TAG: &str = "v9.9.9";
const ASSET: &str = "sw-boat-runner-x86_64-unknown-linux-gnu.tar.gz";

/// What `setup` (the release's or cargo's fake binary) saw: its arguments, then the
/// GitHub tokens in its environment.
const FAKE_BIN: &str = r#"#!/bin/sh
[ "$2" = --help ] && exit 0
{
    echo "args: $*"
    echo "GITHUB_TOKEN=${GITHUB_TOKEN-unset} GH_TOKEN=${GH_TOKEN-unset}"
    echo "from: $FROM"
} >> "$FAKE_LOG"
"#;

/// Serves `$FAKE_RELEASE/<tag>/<file>` for release downloads, the latest release as
/// `$FAKE_LATEST`, and 404 for anything missing. Every call goes to the log.
const FAKE_CURL: &str = r#"#!/bin/sh
out='' url=''
while [ $# -gt 0 ]; do
    case $1 in
        -o) out=$2; shift ;;
        -w) shift ;;
        --proto) shift ;;
        -*) ;;
        *) url=$1 ;;
    esac
    shift
done
echo "curl $url" >> "$FAKE_LOG"
case $url in
    */releases/latest)
        printf 'https://github.com/Surely-Works-LLC/sw-boat-runner/releases/tag/%s' "$FAKE_LATEST" ;;
    */releases/download/*)
        rest=${url#*/releases/download/}
        if [ -f "$FAKE_RELEASE/$rest" ]; then
            cp "$FAKE_RELEASE/$rest" "$out"
            printf 200
        else
            echo 'Not Found' > "$out"
            printf 404
        fi ;;
    *) exit 22 ;;
esac
"#;

const FAKE_UNAME: &str = r#"#!/bin/sh
case $1 in
    -s) echo Linux ;;
    -m) echo "${FAKE_ARCH:-x86_64}" ;;
    *) echo Linux ;;
esac
"#;

const FAKE_GETCONF: &str = r#"#!/bin/sh
[ "$1" = GNU_LIBC_VERSION ] && echo "glibc ${FAKE_GLIBC:-2.39}"
"#;

/// `$FAKE_GH`: `ok`, `old` (no `gh attestation`), `bad` (the attestation fails) or
/// `out` (not logged in).
const FAKE_GH: &str = r#"#!/bin/sh
echo "gh $*" >> "$FAKE_LOG"
case "$1 $2" in
    "auth status") [ "$FAKE_GH" != out ] ;;
    "--version "*) echo "gh version 2.20.0 (2022-11-01)" ;;
    "attestation --help") [ "$FAKE_GH" != old ] ;;
    "attestation verify") [ "$FAKE_GH" = ok ] ;;
    *) exit 1 ;;
esac
"#;

/// Installs the fake binary under `--root`.
const FAKE_CARGO: &str = r#"#!/bin/sh
echo "cargo $*" >> "$FAKE_LOG"
root=''
while [ $# -gt 0 ]; do
    [ "$1" = --root ] && root=$2
    shift
done
mkdir -p "$root/bin"
cp "$FAKE_BIN_SRC" "$root/bin/sw-boat-runner"
chmod +x "$root/bin/sw-boat-runner"
"#;

struct Run {
    dir: PathBuf,
}

impl Run {
    /// A fresh directory with the fakes and a release `TAG` whose files check out.
    fn new(name: &str) -> Run {
        let dir = std::env::temp_dir().join(format!(
            "sbr-install-sh-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        for sub in ["fake", "release", "pkg", "home"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let exe = |path: &Path, text: &str| {
            fs::write(path, text).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        for (name, text) in [
            ("curl", FAKE_CURL),
            ("uname", FAKE_UNAME),
            ("getconf", FAKE_GETCONF),
            ("gh", FAKE_GH),
            ("cargo", FAKE_CARGO),
        ] {
            exe(&dir.join("fake").join(name), text);
        }
        exe(&dir.join("cargo-bin"), &FAKE_BIN.replace("$FROM", "cargo"));
        exe(
            &dir.join("pkg/sw-boat-runner"),
            &FAKE_BIN.replace("$FROM", "release"),
        );
        let run = Run { dir };
        run.publish(TAG);
        run
    }

    /// Puts the tarball and a SHA256SUMS that matches it in the release `tag`.
    fn publish(&self, tag: &str) {
        let rel = self.dir.join("release").join(tag);
        fs::create_dir_all(&rel).unwrap();
        let status = Command::new("tar")
            .arg("-czf")
            .arg(rel.join(ASSET))
            .arg("-C")
            .arg(self.dir.join("pkg"))
            .arg("sw-boat-runner")
            .status()
            .unwrap();
        assert!(status.success());
        let tarball = fs::read(rel.join(ASSET)).unwrap();
        let digest = ring::digest::digest(&ring::digest::SHA256, &tarball);
        let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        fs::write(
            rel.join("SHA256SUMS"),
            format!("{hex}  {ASSET}\n0000  other.tar.gz\n"),
        )
        .unwrap();
    }

    fn release_file(&self, name: &str) -> PathBuf {
        self.dir.join("release").join(TAG).join(name)
    }

    /// `bash install.sh args` with `env` on top of a bare environment.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> (Output, String) {
        let bash = which("bash");
        let mut cmd = Command::new(bash);
        cmd.arg(SCRIPT)
            .args(args)
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.dir.join("fake").display()
                ),
            )
            .env("HOME", self.dir.join("home"))
            .env("FAKE_LOG", self.dir.join("log"))
            .env("FAKE_RELEASE", self.dir.join("release"))
            .env("FAKE_LATEST", TAG)
            .env("FAKE_BIN_SRC", self.dir.join("cargo-bin"))
            .env("FAKE_GH", "ok")
            .current_dir(&self.dir);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let _ = fs::remove_file(self.dir.join("log"));
        let out = cmd.output().unwrap();
        let log = fs::read_to_string(self.dir.join("log")).unwrap_or_default();
        (out, log)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn which(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(name))
                .find(|f| f.is_file())
        })
        .unwrap_or_else(|| panic!("{name} is not on PATH"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The `args:` line setup logged, if it ran.
fn setup_args(log: &str) -> Option<&str> {
    log.lines().find_map(|l| l.strip_prefix("args: "))
}

#[test]
fn a_github_token_alone_is_not_taken_for_a_personal_token() {
    let r = Run::new("token");
    let (out, log) = r.run(
        &["policy.toml"],
        &[
            ("SBR_REF", TAG),
            ("GITHUB_TOKEN", "creator-token"),
            ("GH_TOKEN", "creator-token"),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        setup_args(&log),
        Some("setup --policy policy.toml"),
        "{log}"
    );
    assert!(
        log.contains("GITHUB_TOKEN=unset GH_TOKEN=unset\nfrom: release"),
        "{log}"
    );
    // gh still had the token for the attestation check.
    assert!(log.contains("gh attestation verify"), "{log}");

    for (args, env) in [
        (vec!["policy.toml"], vec![("SBR_PAT", "1")]),
        (vec!["policy.toml", "--pat"], vec![]),
    ] {
        let mut env = env;
        env.extend([("SBR_REF", TAG), ("GITHUB_TOKEN", "pat")]);
        let r = Run::new("pat");
        let (out, log) = r.run(&args, &env);
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(
            setup_args(&log),
            Some("setup --policy policy.toml --pat"),
            "{log}"
        );
        assert!(log.contains("GITHUB_TOKEN=pat "), "{log}");
    }
}

#[test]
fn the_latest_release_is_downloaded_and_checked() {
    let r = Run::new("latest");
    let (out, log) = r.run(&[], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!(
            "sw-boat-runner {TAG} for x86_64-unknown-linux-gnu, SHA-256 and build provenance \
             attestation checked"
        )),
        "{stdout}"
    );
    assert_eq!(setup_args(&log), Some("setup"), "{log}");
    assert!(!log.contains("cargo "), "{log}");
}

#[test]
fn a_bad_checksum_stops_the_install() {
    let r = Run::new("checksum");
    fs::write(
        r.release_file("SHA256SUMS"),
        format!("{}  {ASSET}\n", "0".repeat(64)),
    )
    .unwrap();
    let (out, log) = r.run(&[], &[("SBR_REF", TAG)]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("does not match its SHA256SUMS"),
        "{}",
        stderr(&out)
    );
    assert_eq!(setup_args(&log), None, "{log}");

    // No line for the file fails too.
    fs::write(r.release_file("SHA256SUMS"), "0000  other.tar.gz\n").unwrap();
    let (out, log) = r.run(&[], &[("SBR_REF", TAG)]);
    assert!(!out.status.success());
    assert_eq!(setup_args(&log), None, "{log}");
}

#[test]
fn a_release_missing_a_file_is_not_built_from_source() {
    for missing in [ASSET, "SHA256SUMS"] {
        let r = Run::new("missing");
        fs::remove_file(r.release_file(missing)).unwrap();
        for env in [vec![("SBR_REF", TAG)], vec![]] {
            let (out, log) = r.run(&[], &env);
            assert!(!out.status.success(), "{missing}");
            let err = stderr(&out);
            assert!(
                err.contains(&format!("{missing} of {TAG}: HTTP 404; not installing")),
                "{err}"
            );
            assert!(err.contains("set SBR_REF to its commit"), "{err}");
            assert!(!log.contains("cargo ") && !log.contains("rustup"), "{log}");
            assert_eq!(setup_args(&log), None, "{log}");
        }
    }
}

#[test]
fn an_old_glibc_builds_the_tag_with_cargo() {
    let r = Run::new("glibc");
    let (out, log) = r.run(&[], &[("SBR_REF", TAG), ("FAKE_GLIBC", "2.31")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("glibc 2.31 is older than 2.35"));
    assert!(log.contains("cargo install --locked --root "), "{log}");
    assert!(log.contains(&format!("--tag {TAG}")), "{log}");
    assert!(!log.contains("/releases/download/"), "{log}");
    assert!(log.contains("from: cargo"), "{log}");
}

#[test]
fn commits_and_branches_build_with_cargo() {
    let r = Run::new("ref");
    let (out, log) = r.run(&[], &[("SBR_REF", "0123abcd")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(log.contains("--rev 0123abcd"), "{log}");
    let (out, log) = r.run(&[], &[("SBR_REF", "main")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(log.contains("--branch main"), "{log}");
}

#[test]
fn an_old_gh_warns_and_a_failed_attestation_stops() {
    let r = Run::new("gh-old");
    let (out, log) = r.run(&[], &[("SBR_REF", TAG), ("FAKE_GH", "old")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("warning: gh version 2.20.0 (2022-11-01) has no `gh attestation`"),
        "{}",
        stderr(&out)
    );
    assert!(setup_args(&log).is_some(), "{log}");

    let (out, log) = r.run(&[], &[("SBR_REF", TAG), ("FAKE_GH", "out")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("attestation not checked"));
    assert!(!log.contains("gh attestation"), "{log}");

    let r = Run::new("gh-bad");
    let (out, log) = r.run(&[], &[("SBR_REF", TAG), ("FAKE_GH", "bad")]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("failed its attestation check"));
    assert_eq!(setup_args(&log), None, "{log}");
}

#[test]
fn another_platform_stops_with_a_hint() {
    let r = Run::new("arch");
    let (out, log) = r.run(&[], &[("SBR_REF", TAG), ("FAKE_ARCH", "riscv64")]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("has no binary for Linux riscv64"),
        "{}",
        stderr(&out)
    );
    assert!(!log.contains("cargo "), "{log}");
}
