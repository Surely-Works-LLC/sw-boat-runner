//! Reads the policy file and applies each pool's preset (docs/design.md#strategies).

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::boat::MAX_TTL_S;
use crate::model::{Cents, Config, Placement, Pool};
use crate::run::HOME_WAIT_S;

const MACHINE_TYPES: [&str; 4] = ["small", "default", "large", "xlarge"];

#[derive(Debug)]
pub enum Error {
    Read(PathBuf, std::io::Error),
    Parse(toml::de::Error),
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Read(path, e) => write!(f, "cannot read {}: {e}", path.display()),
            Error::Parse(e) => write!(f, "{e}"),
            Error::Invalid(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}

pub fn load(path: &Path) -> Result<Config, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Read(path.to_path_buf(), e))?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<Config, Error> {
    let raw: RawConfig = toml::from_str(text).map_err(Error::Parse)?;
    if !valid_repo(&raw.repo) {
        return Err(Error::Invalid(format!(
            "repo {:?} must look like owner/name",
            raw.repo
        )));
    }
    if raw.tick_s == 0 {
        return Err(Error::Invalid("tick_s must be at least 1".into()));
    }
    if raw.pool.is_empty() {
        return Err(Error::Invalid("the policy has no [[pool]]".into()));
    }
    let mut names = BTreeSet::new();
    let mut pools = Vec::with_capacity(raw.pool.len());
    for raw_pool in raw.pool {
        let pool = raw_pool.into_pool();
        validate(&pool)
            .map_err(|reason| Error::Invalid(format!("pool {:?}: {reason}", pool.name)))?;
        if !names.insert(pool.name.clone()) {
            return Err(Error::Invalid(format!(
                "pool {:?} is defined twice",
                pool.name
            )));
        }
        pools.push(pool);
    }
    Ok(Config {
        repo: raw.repo,
        tick_s: raw.tick_s,
        pools,
    })
}

/// Settings that are valid but likely to waste starts, for `check` to print.
pub fn warnings(cfg: &Config) -> Vec<String> {
    cfg.pools
        .iter()
        .filter(|p| p.register_timeout_s <= HOME_WAIT_S)
        .map(|p| {
            format!(
                "pool {:?}: register_timeout_s ({}) is not above the {HOME_WAIT_S}s a runner \
                 may wait for boat's snapshot copy, so a slow copy unregisters it",
                p.name, p.register_timeout_s
            )
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    repo: String,
    #[serde(default = "crate::model::default_tick_s")]
    tick_s: u64,
    #[serde(default)]
    pool: Vec<RawPool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPool {
    name: String,
    labels: Vec<String>,
    machine_type: String,
    snapshot: String,
    max_boxes: u32,
    #[serde(default)]
    preset: Preset,
    placement: Option<Vec<Placement>>,
    min_up: Option<u32>,
    slots_per_box: Option<u32>,
    idle_stop_s: Option<u64>,
    min_up_s: Option<u64>,
    register_timeout_s: Option<u64>,
    fork_ttl_s: Option<u64>,
    paid_start_cents_per_day: Option<Cents>,
}

/// The preset table in docs/design.md#strategies.
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Preset {
    Pool,
    Warm,
    Fork,
    #[default]
    Hybrid,
}

impl Preset {
    /// Placement and `min_up`. A `pool` pool never stops its machines, so it keeps one up
    /// by default; the others scale to zero when idle.
    fn shape(self) -> (&'static [Placement], u32) {
        match self {
            Preset::Pool => (&[Placement::Slot], 1),
            Preset::Warm => (&[Placement::Slot, Placement::Resume], 0),
            Preset::Fork => (&[Placement::Fork], 0),
            Preset::Hybrid => (&[Placement::Slot, Placement::Resume, Placement::Fork], 0),
        }
    }
}

impl RawPool {
    fn into_pool(self) -> Pool {
        let (placement, min_up) = self.preset.shape();
        Pool {
            name: self.name,
            labels: self.labels,
            machine_type: self.machine_type,
            snapshot: self.snapshot,
            placement: self.placement.unwrap_or_else(|| placement.to_vec()),
            min_up: self.min_up.unwrap_or(min_up),
            max_boxes: self.max_boxes,
            slots_per_box: self.slots_per_box.unwrap_or(1),
            // Above the 58 min break-even of a large machine at a $0.07 start would be
            // safer, but most starts are included; see docs/design.md#start-budget.
            idle_stop_s: self.idle_stop_s.unwrap_or(1800),
            min_up_s: self.min_up_s.unwrap_or(600),
            // The runner may wait `HOME_WAIT_S` for boat's snapshot copy, then boots.
            register_timeout_s: self.register_timeout_s.unwrap_or(HOME_WAIT_S + 300),
            fork_ttl_s: self.fork_ttl_s.unwrap_or(3600),
            paid_start_cents_per_day: self.paid_start_cents_per_day.unwrap_or(0),
        }
    }
}

fn validate(p: &Pool) -> Result<(), String> {
    // Runner names are `sbr-<pool>-<machine>-...`. `github::parse_runner_name` would read a
    // `-` in a pool name, but then pool `ci`'s prefix `sbr-ci-` would also match the runners
    // of a pool `ci-big` for anyone filtering GitHub's runner list by name.
    let name_ok = !p.name.is_empty()
        && p.name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !name_ok {
        return Err("name must be lowercase letters, digits or _".into());
    }
    if p.labels.is_empty() || p.labels.iter().any(String::is_empty) {
        return Err("labels must be a non-empty list of non-empty labels".into());
    }
    // A JIT runner carries only the pool's labels, and self-hosted jobs ask for `self-hosted`.
    if !p
        .labels
        .iter()
        .any(|l| l.eq_ignore_ascii_case("self-hosted"))
    {
        return Err("labels must include self-hosted".into());
    }
    if !MACHINE_TYPES.contains(&p.machine_type.as_str()) {
        return Err(format!(
            "machine_type {:?} is not one of {}",
            p.machine_type,
            MACHINE_TYPES.join(", ")
        ));
    }
    if p.snapshot.is_empty() {
        return Err("snapshot must not be empty".into());
    }
    if p.placement.is_empty() {
        return Err("placement must list at least one step".into());
    }
    for (i, step) in p.placement.iter().enumerate() {
        if p.placement[..i].contains(step) {
            return Err(format!("placement lists {step:?} twice"));
        }
    }
    // The first step that fits is used, and a free slot never costs a start, so a list
    // that tries a start before a free slot only wastes starts.
    if !p.placement.windows(2).all(|w| rank(w[0]) < rank(w[1])) {
        return Err("placement steps must keep the order slot, resume, fork".into());
    }
    if p.placement.contains(&Placement::Resume) && !p.placement.contains(&Placement::Slot) {
        return Err("resume needs slot, which registers runners on resumed machines".into());
    }
    if p.min_up > 0 && !p.placement.contains(&Placement::Slot) {
        return Err("min_up needs slot, which registers runners on kept machines".into());
    }
    if p.max_boxes == 0 {
        return Err("max_boxes must be at least 1".into());
    }
    if p.min_up > p.max_boxes {
        return Err(format!(
            "min_up ({}) is above max_boxes ({})",
            p.min_up, p.max_boxes
        ));
    }
    if p.slots_per_box == 0 {
        return Err("slots_per_box must be at least 1".into());
    }
    if !(1..=MAX_TTL_S).contains(&p.fork_ttl_s) {
        return Err(format!("fork_ttl_s must be 1 to {MAX_TTL_S}"));
    }
    Ok(())
}

fn rank(step: Placement) -> u8 {
    match step {
        Placement::Slot => 0,
        Placement::Resume => 1,
        Placement::Fork => 2,
    }
}

fn valid_repo(repo: &str) -> bool {
    let part_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| part_ok(owner) && part_ok(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "repo = \"acme/app\"\n";
    const POOL: &str = r#"
[[pool]]
name = "ci"
labels = ["self-hosted", "large"]
machine_type = "large"
snapshot = "snap-1"
max_boxes = 4
"#;

    fn policy(extra: &str) -> String {
        format!("{REPO}{POOL}{extra}\n")
    }

    fn error(text: &str) -> String {
        parse(text).unwrap_err().to_string()
    }

    #[test]
    fn hybrid_is_the_default_preset() {
        let cfg = parse(&policy("")).unwrap();
        assert_eq!(cfg.repo, "acme/app");
        assert_eq!(cfg.tick_s, 15);
        assert_eq!(
            cfg.pools,
            vec![Pool {
                name: "ci".into(),
                labels: vec!["self-hosted".into(), "large".into()],
                machine_type: "large".into(),
                snapshot: "snap-1".into(),
                placement: vec![Placement::Slot, Placement::Resume, Placement::Fork],
                min_up: 0,
                max_boxes: 4,
                slots_per_box: 1,
                idle_stop_s: 1800,
                min_up_s: 600,
                register_timeout_s: 900,
                fork_ttl_s: 3600,
                paid_start_cents_per_day: 0,
            }]
        );
    }

    #[test]
    fn presets_set_placement_and_min_up() {
        use Placement::{Fork, Resume, Slot};
        let cases: [(&str, &[Placement], u32); 4] = [
            ("pool", &[Slot], 1),
            ("warm", &[Slot, Resume], 0),
            ("fork", &[Fork], 0),
            ("hybrid", &[Slot, Resume, Fork], 0),
        ];
        for (preset, placement, min_up) in cases {
            let cfg = parse(&policy(&format!("preset = \"{preset}\""))).unwrap();
            let pool = &cfg.pools[0];
            assert_eq!(pool.placement, placement, "{preset}");
            assert_eq!(pool.min_up, min_up, "{preset}");
        }
    }

    #[test]
    fn fields_override_the_preset() {
        let text = format!(
            "tick_s = 30\n{}",
            policy(
                "preset = \"warm\"\nmin_up = 2\nidle_stop_s = 3600\nslots_per_box = 2\n\
                 paid_start_cents_per_day = 50"
            )
        );
        let cfg = parse(&text).unwrap();
        let pool = &cfg.pools[0];
        assert_eq!(cfg.tick_s, 30);
        assert_eq!(pool.placement, [Placement::Slot, Placement::Resume]);
        assert_eq!(pool.min_up, 2);
        assert_eq!(pool.idle_stop_s, 3600);
        assert_eq!(pool.slots_per_box, 2);
        assert_eq!(pool.paid_start_cents_per_day, 50);
    }

    /// A fresh machine's runner waits up to `HOME_WAIT_S` for boat's snapshot copy; a
    /// timeout under that would unregister it and fork the job again.
    #[test]
    fn register_timeout_outlasts_the_home_wait() {
        let pool = &parse(&policy("")).unwrap().pools[0];
        assert!(pool.register_timeout_s > HOME_WAIT_S);
        assert!(warnings(&parse(&policy("")).unwrap()).is_empty());
        let short = parse(&policy("register_timeout_s = 300")).unwrap();
        let w = warnings(&short);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("pool \"ci\"") && w[0].contains("600"),
            "{w:?}"
        );
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(error(&policy("idle_stop = 60")).contains("unknown field `idle_stop`"));
        assert!(error(&format!("colour = 1\n{}", policy(""))).contains("unknown field `colour`"));
        assert!(error(&policy("preset = \"lazy\"")).contains("unknown variant `lazy`"));
        assert!(error(&policy("preset = \"pool-jit\"")).contains("unknown variant `pool-jit`"));
        assert!(error(&policy("runner = \"jit\"")).contains("unknown field `runner`"));
    }

    #[test]
    fn max_boxes_is_required() {
        let text = format!("{REPO}{}", POOL.replace("max_boxes = 4", ""));
        assert!(error(&text).contains("missing field `max_boxes`"));
    }

    #[test]
    fn invalid_policies_name_the_problem() {
        let cases = [
            (policy("").replace("acme/app", "acme"), "owner/name"),
            (format!("tick_s = 0\n{}", policy("")), "tick_s"),
            (REPO.to_string(), "no [[pool]]"),
            (format!("{}{POOL}", policy("")), "defined twice"),
            (policy("").replace("\"ci\"", "\"ci-big\""), "lowercase"),
            (
                policy("").replace("[\"self-hosted\", \"large\"]", "[]"),
                "labels",
            ),
            (policy("").replace("\"self-hosted\", ", ""), "self-hosted"),
            (
                policy("").replace("\"large\"\n", "\"huge\"\n"),
                "machine_type",
            ),
            (policy("placement = []"), "at least one step"),
            (policy("placement = [\"slot\", \"slot\"]"), "twice"),
            (policy("placement = [\"resume\", \"slot\"]"), "order"),
            (policy("placement = [\"resume\"]"), "resume needs slot"),
            (policy("preset = \"fork\"\nmin_up = 1"), "min_up needs slot"),
            (policy("fork_ttl_s = 0"), "fork_ttl_s"),
            (policy("fork_ttl_s = 2592001"), "fork_ttl_s"),
            (policy("min_up = 5"), "above max_boxes"),
            (policy("slots_per_box = 0"), "slots_per_box"),
            (
                policy("").replace("max_boxes = 4", "max_boxes = 0"),
                "max_boxes",
            ),
        ];
        for (text, expected) in cases {
            let message = error(&text);
            assert!(
                message.contains(expected),
                "{expected:?} not in {message:?}"
            );
        }
    }
}
