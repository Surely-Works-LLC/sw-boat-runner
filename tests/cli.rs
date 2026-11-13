//! The binary's argument handling, run as a user would.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sw-boat-runner"))
        .args(args)
        .env_remove("BOAT_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GITHUB_APP_ID")
        .env_remove("GITHUB_APP_INSTALLATION_ID")
        .env_remove("GITHUB_APP_PRIVATE_KEY_FILE")
        .output()
        .expect("the binary runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn help_and_version_go_to_stdout() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{flag}");
        assert!(
            text(&out.stdout).starts_with("usage: sw-boat-runner check"),
            "{flag}"
        );
        assert!(out.stderr.is_empty(), "{flag}");
    }
    for flag in ["--version", "-V"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{flag}");
        let want = format!("sw-boat-runner {}\n", env!("CARGO_PKG_VERSION"));
        assert_eq!(text(&out.stdout), want, "{flag}");
    }
}

#[test]
fn unknown_arguments_print_why_and_the_usage_and_exit_2() {
    for (args, why) in [
        (&[][..], "missing a command"),
        (&["--bogus"], "unknown option \"--bogus\""),
        (&["bogus"], "unknown command \"bogus\""),
        (&["check"], "check: missing the policy file (<policy.toml>)"),
        (&["run"], "run: missing the policy file (<policy.toml>)"),
        (&["drain"], "drain: missing the policy file (<policy.toml>)"),
        (&["plan"], "plan: missing the policy file (<policy.toml>)"),
        (&["check", "a", "b"], "check: unexpected argument \"b\""),
        (&["plan", "a", "b", "c"], "plan: unexpected argument \"c\""),
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = text(&out.stderr);
        assert!(
            err.starts_with(&format!("sw-boat-runner: {why}\n\nusage:")),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn bad_run_and_drain_flags_say_what_is_wrong() {
    let dir = std::env::temp_dir().join(format!("sbr-cli-flags-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("policy.toml");
    std::fs::write(
        &path,
        "repo = \"acme/app\"\n[[pool]]\nname = \"ci\"\nlabels = [\"self-hosted\"]\n\
         machine_type = \"large\"\nsnapshot = \"s\"\nmax_boxes = 2\n",
    )
    .unwrap();
    let policy = path.to_str().unwrap();
    for (args, why) in [
        (vec!["run", policy, "b"], "run: unknown argument \"b\""),
        (
            vec!["drain", policy, "--bogus"],
            "drain: unknown argument \"--bogus\"",
        ),
        (
            vec!["drain", policy, "--settle-s"],
            "drain: --settle-s needs a number of seconds",
        ),
        (
            vec!["drain", policy, "--timeout-s", "x"],
            "drain: --timeout-s \"x\" is not a number of seconds",
        ),
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = text(&out.stderr);
        assert!(
            err.starts_with(&format!("sw-boat-runner: {why}\n\nusage:")),
            "{args:?}: {err}"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Without systemd, a real local setup stops before its first prompt. Only where this
/// host has no systemd, such as macOS; the unit test covers the check itself.
#[test]
fn setup_without_systemd_stops_before_any_prompt() {
    if std::path::Path::new("/run/systemd/system").is_dir() {
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_sw-boat-runner"))
        .arg("setup")
        .env_remove("BOAT_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GITHUB_APP_ID")
        .env_remove("SBR_REPO")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    let err = text(&out.stderr);
    assert!(
        err.starts_with("setup: this machine has no systemd") && !err.contains("Repository"),
        "{err}"
    );
}

#[test]
fn check_warns_about_a_register_timeout_inside_the_home_wait() {
    let dir = std::env::temp_dir().join(format!("sbr-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("policy.toml");
    std::fs::write(
        &path,
        "repo = \"acme/app\"\n[[pool]]\nname = \"ci\"\nlabels = [\"self-hosted\"]\n\
         machine_type = \"large\"\nsnapshot = \"s\"\nmax_boxes = 2\nregister_timeout_s = 300\n",
    )
    .unwrap();
    let out = run(&["check", path.to_str().unwrap()]);
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("pool ci "));
    assert!(text(&out.stderr).contains("warning: pool \"ci\": register_timeout_s (300)"));
}

#[test]
fn setup_dry_run_asks_no_secret_and_changes_nothing() {
    let out = Command::new(env!("CARGO_BIN_EXE_sw-boat-runner"))
        .args([
            "setup",
            "--dry-run",
            "--repo",
            "acme/app",
            "--labels",
            "boat",
        ])
        .args(["--snapshot", "snap-1", "--bin", "/tmp/sbr-none"])
        .env_remove("BOAT_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GITHUB_APP_ID")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the binary runs");
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", text(&out.stderr));
    assert!(
        stdout.contains("labels = [\"self-hosted\", \"boat\"]"),
        "{stdout}"
    );
    // `check`'s line, with the defaults applied.
    assert!(
        stdout.contains("pool boat labels self-hosted,boat machine default"),
        "{stdout}"
    );
    assert!(stdout.contains("\"administration\": \"write\""), "{stdout}");
    assert!(
        stdout.contains("LoadCredential=github-app.pem:"),
        "{stdout}"
    );
    assert!(stdout.contains("BOAT_API_KEY, GITHUB_APP_ID"), "{stdout}");

    let out = run(&["setup", "--repo"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stdout).is_empty());
}

#[test]
fn remote_dry_run_prints_the_plan_and_asks_no_secret() {
    let out = Command::new(env!("CARGO_BIN_EXE_sw-boat-runner"))
        .args(["setup", "--remote", "--dry-run", "--repo", "acme/app"])
        .args(["--labels", "boat", "--snapshot", "snap-1"])
        .env_remove("BOAT_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GITHUB_APP_ID")
        .env("SBR_REF", "v9.9.9")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the binary runs");
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", text(&out.stderr));
    assert!(stdout.contains("snapshot = \"snap-1\""), "{stdout}");
    assert!(
        stdout.contains("http://127.0.0.1:<random port>/callback"),
        "{stdout}"
    );
    assert!(stdout.contains("install.sh at v9.9.9"), "{stdout}");

    let out = run(&["setup", "--remote", "--bin", "/x"]);
    assert_eq!(out.status.code(), Some(2));
    let out = run(&["setup", "--paste"]);
    assert_eq!(out.status.code(), Some(2));
}
