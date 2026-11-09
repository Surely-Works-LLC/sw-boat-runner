mod decide;
mod model;
mod policy;

use std::path::Path;
use std::process::ExitCode;

use model::{Placement, RunnerMode};

const USAGE: &str = "\
usage: sw-boat-runner check <policy.toml>
       sw-boat-runner plan <policy.toml> <snapshot.json>
       sw-boat-runner run <policy.toml>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["check", policy] => with_policy(policy, cmd_check),
        ["plan", policy, snapshot] => with_policy(policy, |cfg| cmd_plan(cfg, Path::new(snapshot))),
        ["run", policy] => with_policy(policy, cmd_run),
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
        let runner = match p.runner {
            RunnerMode::Persistent => "persistent",
            RunnerMode::Jit => "jit",
        };
        println!(
            "pool {} labels {} machine {} snapshot {} placement {} runner {} \
             min_up {} max_boxes {} slots {} idle_stop {}s paid_cap {}c",
            p.name,
            p.labels.join(","),
            p.machine_type,
            p.snapshot,
            placement.join(","),
            runner,
            p.min_up,
            p.max_boxes,
            p.slots_per_box,
            p.idle_stop_s,
            p.paid_start_cents_per_day,
        );
    }
    ExitCode::SUCCESS
}

/// The actions `run` would take on this snapshot, one JSON object per line with its pool.
fn cmd_plan(cfg: &model::Config, snapshot: &Path) -> ExitCode {
    let snap: model::Snapshot = match std::fs::read_to_string(snapshot)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
    {
        Ok(snap) => snap,
        Err(e) => {
            eprintln!("{}: {e}", snapshot.display());
            return ExitCode::FAILURE;
        }
    };
    for pool in &cfg.pools {
        for action in decide::decide(&snap, pool) {
            let mut line = serde_json::to_value(&action).expect("an Action always serializes");
            line["pool"] = serde_json::Value::from(pool.name.as_str());
            println!("{line}");
        }
    }
    ExitCode::SUCCESS
}

fn cmd_run(cfg: &model::Config) -> ExitCode {
    // The clients branch replaces this body with `run::run(cfg, decide::decide)`.
    let _ = cfg;
    eprintln!("run: not wired yet");
    ExitCode::from(2)
}
