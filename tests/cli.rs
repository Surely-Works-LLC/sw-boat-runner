//! The binary's argument handling, run as a user would.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sw-boat-runner"))
        .args(args)
        .env_remove("BOAT_API_KEY")
        .env_remove("GITHUB_TOKEN")
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
fn unknown_arguments_print_usage_and_exit_2() {
    for args in [&[][..], &["--bogus"], &["check"], &["run", "a", "b"]] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(text(&out.stderr).starts_with("usage:"), "{args:?}");
    }
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
