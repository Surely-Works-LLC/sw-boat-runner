//! Each directory in tests/scenarios holds a `policy.toml`, a `snapshot.json` and, in
//! `expected.jsonl`, the lines `sw-boat-runner plan policy.toml snapshot.json` prints.

use std::fs;
use std::path::Path;

use serde_json::Value;
use sw_boat_runner::model::Snapshot;
use sw_boat_runner::{decide, policy, run};

fn read(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn plan(dir: &Path) -> Result<Vec<Value>, String> {
    let cfg = policy::parse(&read(dir, "policy.toml")).map_err(|e| e.to_string())?;
    let snap: Snapshot =
        serde_json::from_str(&read(dir, "snapshot.json")).map_err(|e| e.to_string())?;
    Ok(run::plan(&cfg, &snap, decide::decide)
        .into_iter()
        .map(|(pool, action)| {
            let mut line = serde_json::to_value(action).expect("an Action always serializes");
            line["pool"] = Value::from(pool.name.as_str());
            line
        })
        .collect())
}

#[test]
fn scenarios_plan_the_expected_actions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut dirs: Vec<_> = fs::read_dir(&root)
        .expect("tests/scenarios")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty());
    let mut failed = Vec::new();
    for dir in &dirs {
        let expected: Result<Vec<Value>, _> = read(dir, "expected.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect();
        let actual = plan(dir);
        let ok = matches!((&actual, &expected), (Ok(a), Ok(e)) if a == e);
        if !ok {
            failed.push(format!(
                "{}:\n  actual   {actual:?}\n  expected {expected:?}",
                dir.display()
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
