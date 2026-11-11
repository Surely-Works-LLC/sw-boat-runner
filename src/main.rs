use std::path::Path;
use std::process::ExitCode;

use sw_boat_runner::model::{self, Placement};
use sw_boat_runner::{decide, policy, run, snapshot};

const USAGE: &str = "\
usage: sw-boat-runner check <policy.toml>
       sw-boat-runner plan <policy.toml> [snapshot.json]
       sw-boat-runner run <policy.toml>
       sw-boat-runner drain <policy.toml>
       sw-boat-runner snapshot <sandbox-id> <name> [--machine-type T] [--fix] [--keep-fork] [--yes]
       sw-boat-runner setup [--pat] [--dry-run] [--help]
       sw-boat-runner doctor [policy.toml]
       sw-boat-runner --help | --version";

const HELP: &str = "\
Keeps self-hosted GitHub Actions runners on boat machines.

  check   validate a policy and print each pool with its preset applied
  plan    print the actions run would take, from a snapshot file or one live read
  run     observe, decide and act every tick_s seconds
  drain   delete the policy's runners and machines; fails while any are left
  snapshot  check a sandbox in a noEnv fork and save the fork as a CI snapshot
  setup     install the daemon as a systemd service here; setup --help says how
  doctor    check a setup end to end (default /etc/sw-boat-runner/policy.toml); doctor --help

run, drain, snapshot, doctor and a live plan read BOAT_API_KEY, and GITHUB_TOKEN or a GitHub App's
GITHUB_APP_ID, GITHUB_APP_PRIVATE_KEY_FILE and optional GITHUB_APP_INSTALLATION_ID.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["-h" | "--help"] => {
            println!("{USAGE}\n\n{HELP}");
            ExitCode::SUCCESS
        }
        ["-V" | "--version"] => {
            println!("sw-boat-runner {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["check", policy] => with_policy(policy, cmd_check),
        ["plan", policy] => with_policy(policy, |cfg| cmd_plan(cfg, None)),
        ["plan", policy, snapshot] => {
            with_policy(policy, |cfg| cmd_plan(cfg, Some(Path::new(snapshot))))
        }
        ["run", policy] => with_policy(policy, cmd_run),
        ["drain", policy] => with_policy(policy, cmd_drain),
        ["snapshot", rest @ ..] => cmd_snapshot(rest),
        ["setup", rest @ ..] => sw_boat_runner::setup::cli(rest),
        ["doctor", rest @ ..] => sw_boat_runner::doctor::cli(rest),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn with_policy(path: &str, cmd: impl FnOnce(&model::Config) -> ExitCode) -> ExitCode {
    match policy::load(Path::new(path)) {
        Ok(cfg) => cmd(&cfg),
        Err(e) => {
            eprintln!("{path}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// One line per pool, with its preset applied, so a policy can be reviewed before `run`.
fn cmd_check(cfg: &model::Config) -> ExitCode {
    println!("repo {} tick {}s", cfg.repo, cfg.tick_s);
    for p in &cfg.pools {
        let placement: Vec<&str> = p
            .placement
            .iter()
            .map(|step| match step {
                Placement::Slot => "slot",
                Placement::Resume => "resume",
                Placement::Fork => "fork",
            })
            .collect();
        println!(
            "pool {} labels {} machine {} snapshot {} placement {} \
             min_up {} max_boxes {} slots {} idle_stop {}s paid_cap {}c",
            p.name,
            p.labels.join(","),
            p.machine_type,
            p.snapshot,
            placement.join(","),
            p.min_up,
            p.max_boxes,
            p.slots_per_box,
            p.idle_stop_s,
            p.paid_start_cents_per_day,
        );
    }
    for warning in policy::warnings(cfg) {
        eprintln!("warning: {warning}");
    }
    ExitCode::SUCCESS
}

/// The actions `run` would take, one JSON object per line with its pool. Without a
/// snapshot file it observes boat and GitHub once, as `run` does, and changes nothing.
fn cmd_plan(cfg: &model::Config, snapshot: Option<&Path>) -> ExitCode {
    let snap = match snapshot {
        Some(path) => read_snapshot(path).map_err(|e| format!("{}: {e}", path.display())),
        None => run::observe_once(cfg).map_err(|e| format!("observe: {e}")),
    };
    let snap = match snap {
        Ok(snap) => snap,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "snapshot machines {} runners {} queued {} can_start {} next_start {}c spent_today {}c",
        snap.machines.len(),
        snap.runners.len(),
        snap.queued.len(),
        snap.limits.can_start,
        snap.limits.next_start_cents,
        snap.limits.spent_today_cents,
    );
    for (pool, action) in run::plan(cfg, &snap, decide::decide) {
        let mut line = serde_json::to_value(&action).expect("an Action always serializes");
        line["pool"] = serde_json::Value::from(pool.name.as_str());
        println!("{line}");
    }
    ExitCode::SUCCESS
}

fn read_snapshot(path: &Path) -> Result<model::Snapshot, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Fails while a machine is left, so a script can repeat it until the pools are empty.
fn cmd_drain(cfg: &model::Config) -> ExitCode {
    match run::drain(cfg) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(left) => {
            eprintln!("drain: {left} machines left");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("drain: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_run(cfg: &model::Config) -> ExitCode {
    match run::run(cfg, decide::decide) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("run: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Fails unless the snapshot was saved.
fn cmd_snapshot(args: &[&str]) -> ExitCode {
    let opts = match snapshot::parse_args(args) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("snapshot: {e}\nusage: {}", snapshot::USAGE);
            return ExitCode::from(2);
        }
    };
    match snapshot::run(&opts) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("snapshot: {e}");
            ExitCode::FAILURE
        }
    }
}
