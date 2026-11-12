use std::path::Path;
use std::process::ExitCode;

use sw_boat_runner::model::{self, Placement};
use sw_boat_runner::{decide, policy, run, snapshot};

const USAGE: &str = "\
usage: sw-boat-runner check <policy.toml>
       sw-boat-runner plan <policy.toml> [snapshot.json]
       sw-boat-runner run <policy.toml> [--drain-on-stop]
       sw-boat-runner drain <policy.toml> [--settle-s S] [--timeout-s S]
       sw-boat-runner snapshot <sandbox-id> <name> [--machine-type T] [--fix] [--keep-fork] [--yes]
       sw-boat-runner setup [--remote] [--pat] [--dry-run] [--help]
       sw-boat-runner doctor [policy.toml]
       sw-boat-runner --help | --version";

const HELP: &str = "\
Keeps self-hosted GitHub Actions runners on boat machines.

  check   validate a policy and print each pool with its preset applied
  plan    print the actions run would take, from a snapshot file or one live read
  run     observe, decide and act every tick_s seconds. SIGINT or SIGTERM ends it after
          the current tick; --drain-on-stop then drains the pools as drain does
  drain   delete the policy's runners and machines, every tick_s, until boat has listed
          none for --settle-s (default: the 90 s request timeout plus tick_s, how long a
          fork may go unlisted); fails with machines left after --timeout-s (default 900)
  snapshot  check a sandbox in a noEnv fork and save the fork as a CI snapshot
  setup     install the daemon as a systemd service here, or with --remote on a new boat
            machine from this computer; setup --help says how
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
        ["run", policy, flags @ ..] => match flags {
            [] => with_policy(policy, |cfg| cmd_run(cfg, run::RunOpts::default())),
            ["--drain-on-stop"] => with_policy(policy, |cfg| {
                cmd_run(
                    cfg,
                    run::RunOpts {
                        drain_on_stop: true,
                    },
                )
            }),
            _ => usage(),
        },
        ["drain", policy, flags @ ..] => with_policy(policy, |cfg| match drain_opts(cfg, flags) {
            Some(opts) => cmd_drain(cfg, opts),
            None => usage(),
        }),
        ["snapshot", rest @ ..] => cmd_snapshot(rest),
        ["setup", rest @ ..] => sw_boat_runner::setup::cli(rest),
        ["doctor", rest @ ..] => sw_boat_runner::doctor::cli(rest),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// `drain`'s flags, each a number of seconds; `None` for anything else.
fn drain_opts(cfg: &model::Config, flags: &[&str]) -> Option<run::DrainOpts> {
    let mut opts = run::DrainOpts::new(cfg);
    let mut rest = flags;
    while let [flag, value, tail @ ..] = rest {
        let value: u64 = value.parse().ok()?;
        match *flag {
            "--settle-s" => opts.settle_s = value,
            "--timeout-s" => opts.timeout_s = value,
            _ => return None,
        }
        rest = tail;
    }
    rest.is_empty().then_some(opts)
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
fn cmd_drain(cfg: &model::Config, opts: run::DrainOpts) -> ExitCode {
    match run::drain(cfg, opts) {
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

fn cmd_run(cfg: &model::Config, opts: run::RunOpts) -> ExitCode {
    match run::run(cfg, decide::decide, opts) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(left) => {
            eprintln!("run: drain left {left} machines");
            ExitCode::FAILURE
        }
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
