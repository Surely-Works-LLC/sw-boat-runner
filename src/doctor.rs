//! `sw-boat-runner doctor`: checks a setup end to end with the credentials `run` reads,
//! and prints one `ok`, `warn` or `FAIL` line per check. A FAIL line says how to fix it,
//! and any FAIL makes the exit status non-zero.
//!
//! It only reads: boat's limits and saved snapshots, GitHub's runners and the App's
//! installation, and systemd's state. No line holds a secret value; the env line names
//! the variables it found, never what they hold.

use std::collections::BTreeMap;
use std::fmt;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use crate::boat;
use crate::github::{self, API, Auth, GhRunner};
use crate::github_app::{self, App};
use crate::model::{Config, Pool, UnixSecs};
use crate::transport::{Transport, Ureq};
use crate::{policy, run, snapshot};

const POLICY: &str = "/etc/sw-boat-runner/policy.toml";
const ENV: &str = "/etc/sw-boat-runner/env";
const SERVICE: &str = "sw-boat-runner";
/// The credentials `run` reads.
const VARS: [&str; 5] = [
    "BOAT_API_KEY",
    "GITHUB_TOKEN",
    "GITHUB_APP_ID",
    "GITHUB_APP_PRIVATE_KEY_FILE",
    "GITHUB_APP_INSTALLATION_ID",
];
/// A clock this far from GitHub's is worth fixing...
const SKEW_WARN_S: u64 = 30;
/// ...and past this GitHub refuses the App's JWT, whose `iat` and `exp` it checks.
const SKEW_FAIL_S: u64 = 300;

const HELP: &str = "\
usage: sw-boat-runner doctor [policy.toml]

Checks a setup end to end and prints ok, warn or FAIL for each check; exits non-zero on
any FAIL. The policy defaults to /etc/sw-boat-runner/policy.toml.

  policy     parses, as `check` does
  env        BOAT_API_KEY and GITHUB_TOKEN or GITHUB_APP_*; as root with none of them
             set, read from /etc/sw-boat-runner/env, as the service reads them
  clock      within 30 s of GitHub's (a GitHub App's JWT fails past 5 minutes)
  boat       the key works
  snapshot   each pool's snapshot is saved, fits its machine type's disk, and came
             from a sandbox without its creator's logins
  machines   no machine of this repository is in a pool the policy no longer has, and
             no pool shares machines from before 0.3.0, which carry no repo label;
             each warn prints the drain or delete command
  github     the App's installation has Administration write and Actions read and
             mints a token, or the token lists the repository's runners; rate limit left
  service    systemctl is-active sw-boat-runner
  runners    each pool's runners, online and busy";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub level: Level,
    pub check: &'static str,
    pub text: String,
}

fn line(level: Level, check: &'static str, text: impl Into<String>) -> Line {
    // One line each, whatever an error's text holds.
    let text = text.into().replace(['\n', '\r'], " ");
    Line { level, check, text }
}

#[derive(Debug, Default)]
pub(crate) struct Report {
    lines: Vec<Line>,
}

impl Report {
    fn push(&mut self, l: Line) {
        self.lines.push(l);
    }

    fn ok(&mut self, check: &'static str, text: impl Into<String>) {
        self.push(line(Level::Ok, check, text));
    }

    fn warn(&mut self, check: &'static str, text: impl Into<String>) {
        self.push(line(Level::Warn, check, text));
    }

    fn fail(&mut self, check: &'static str, text: impl Into<String>) {
        self.push(line(Level::Fail, check, text));
    }

    fn count(&self, level: Level) -> usize {
        self.lines.iter().filter(|l| l.level == level).count()
    }

    pub(crate) fn failed(&self) -> bool {
        self.count(Level::Fail) > 0
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        if self.failed() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for l in &self.lines {
            let tag = match l.level {
                Level::Ok => "ok",
                Level::Warn => "warn",
                Level::Fail => "FAIL",
            };
            writeln!(f, "{tag:<4}  {:<8}  {}", l.check, l.text)?;
        }
        let (fails, warns) = (self.count(Level::Fail), self.count(Level::Warn));
        if fails == 0 {
            writeln!(f, "doctor: no FAIL, {warns} warn")
        } else {
            writeln!(
                f,
                "doctor: {fails} FAIL, {warns} warn; fix each FAIL line and run again"
            )
        }
    }
}

pub fn cli(args: &[&str]) -> ExitCode {
    let path = match args {
        ["-h" | "--help"] => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        [] => POLICY,
        [path] => path,
        _ => {
            eprintln!("{HELP}");
            return ExitCode::from(2);
        }
    };
    let report = doctor(Path::new(path));
    print!("{report}");
    report.exit_code()
}

/// Runs `doctor` on what `setup` just wrote, as root, so it reads the env file the service
/// reads. A FAIL leaves the setup in place; it only says what to fix.
pub(crate) fn after_setup(bin: &str, policy: &str, sudo: bool) {
    println!("\nChecking the setup with `{bin} doctor {policy}`:\n");
    let mut cmd = if sudo {
        let mut c = Command::new("sudo");
        c.arg(bin);
        c
    } else {
        Command::new(bin)
    };
    // Without them doctor reads the env file, as the service does, not this shell.
    for name in VARS {
        cmd.env_remove(name);
    }
    match cmd.args(["doctor", policy]).status() {
        Ok(s) if s.success() => {}
        Ok(_) => println!(
            "\nThe service is installed, but doctor found a problem: fix each FAIL line \
             above, then run `sudo {bin} doctor` again."
        ),
        Err(e) => println!("\nCould not run doctor ({e}); `sudo {bin} doctor` checks the setup."),
    }
}

fn doctor(path: &Path) -> Report {
    let mut r = Report::default();
    let cfg = match policy::load(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            r.fail(
                "policy",
                format!("{e}; pass the policy's path, or fix the file and run doctor again"),
            );
            return r;
        }
    };
    policy_lines(&mut r, path, &cfg);
    let process = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (env_line, file) = credentials(is_root(), &process, &|| read_env(ENV));
    r.push(env_line);
    let var = |name: &str| match &file {
        Some(vars) => vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty()),
        None => process(name),
    };
    let t = Ureq::new(Duration::from_secs(30));
    checks(&mut r, &cfg, &var, &t, run::unix_now(), service_state());
    r
}

fn policy_lines(r: &mut Report, path: &Path, cfg: &Config) {
    let pools: Vec<&str> = cfg.pools.iter().map(|p| p.name.as_str()).collect();
    r.ok(
        "policy",
        format!(
            "{}: repo {}, pools {}",
            path.display(),
            cfg.repo,
            pools.join(", ")
        ),
    );
    for w in policy::warnings(cfg) {
        r.warn("policy", w);
    }
}

/// The checks after the policy, against `t`, at `now`.
fn checks(
    r: &mut Report,
    cfg: &Config,
    var: &dyn Fn(&str) -> Option<String>,
    t: &dyn Transport,
    now: UnixSecs,
    service: Option<(String, String)>,
) {
    clock(r, t, now);
    boat_checks(r, cfg, var, t);
    let runners = github_checks(r, cfg, var, t, now);
    r.push(service_line(
        service.as_ref().map(|(l, a)| (l.as_str(), a.as_str())),
    ));
    match runners {
        Some(runners) => {
            for l in runner_lines(cfg, &runners) {
                r.push(l);
            }
        }
        None => r.warn("runners", "not listed: GitHub access failed above"),
    }
}

/// Where the credentials come from: this shell when it sets any of them, or else, for
/// root, the env file the service reads. The second value is the file's variables.
#[allow(clippy::type_complexity)]
fn credentials(
    root: bool,
    process: &dyn Fn(&str) -> Option<String>,
    read: &dyn Fn() -> Result<Option<String>, String>,
) -> (Line, Option<Vec<(String, String)>>) {
    let set: Vec<&str> = VARS.into_iter().filter(|n| process(n).is_some()).collect();
    if !set.is_empty() {
        return (
            line(
                Level::Ok,
                "env",
                format!("this shell sets {}", set.join(", ")),
            ),
            None,
        );
    }
    let none = "none of BOAT_API_KEY, GITHUB_TOKEN, GITHUB_APP_* is set";
    if !root {
        let text =
            format!("{none}; run `sudo sw-boat-runner doctor` to read {ENV}, or export them");
        return (line(Level::Warn, "env", text), None);
    }
    match read() {
        Ok(Some(text)) => {
            let vars = parse_env(&text);
            let names: Vec<&str> = vars
                .iter()
                .filter(|(k, v)| !v.is_empty() && VARS.contains(&k.as_str()))
                .map(|(k, _)| k.as_str())
                .collect();
            let text = format!("{ENV} sets {}", names.join(", "));
            (line(Level::Ok, "env", text), Some(vars))
        }
        Ok(None) => {
            let text = format!("{none} and {ENV} does not exist; `sw-boat-runner setup` writes it");
            (line(Level::Warn, "env", text), None)
        }
        Err(e) => (
            line(
                Level::Fail,
                "env",
                format!("{e}; export the variables instead"),
            ),
            None,
        ),
    }
}

/// `None` when the file does not exist. Errors name the path, never the content.
fn read_env(path: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

/// `NAME=value` lines as systemd reads them, as `setup` does: blank lines and `#`
/// comments skipped, a value's surrounding quotes dropped.
fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = ['"', '\'']
                .iter()
                .find_map(|q| v.strip_prefix(*q)?.strip_suffix(*q))
                .unwrap_or(v);
            (k.trim().to_string(), v.to_string())
        })
        .collect()
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
}

/// Compares this clock with the `Date` of an unauthenticated GitHub answer, which costs
/// no rate limit.
fn clock(r: &mut Report, t: &dyn Transport, now: UnixSecs) {
    let url = format!("{API}/rate_limit");
    let headers = [
        ("Accept", "application/vnd.github+json"),
        ("User-Agent", "sw-boat-runner"),
    ];
    match t.send("GET", &url, &headers, None) {
        Err(e) => r.warn(
            "clock",
            format!("could not reach GitHub to compare: {}", e.0),
        ),
        Ok(resp) => match resp.header("date").and_then(parse_http_date) {
            Some(server) => r.push(classify_skew(now, server)),
            None => r.warn("clock", "GitHub's answer had no Date to compare with"),
        },
    }
}

pub(crate) fn classify_skew(local: UnixSecs, server: UnixSecs) -> Line {
    let skew = local.abs_diff(server);
    let side = if local >= server {
        "ahead of"
    } else {
        "behind"
    };
    let sync = "turn on time sync (`sudo timedatectl set-ntp true`)";
    if skew <= SKEW_WARN_S {
        line(Level::Ok, "clock", format!("within {skew} s of GitHub's"))
    } else if skew <= SKEW_FAIL_S {
        line(
            Level::Warn,
            "clock",
            format!("{skew} s {side} GitHub's; {sync}"),
        )
    } else {
        line(
            Level::Fail,
            "clock",
            format!("{skew} s {side} GitHub's, so GitHub refuses a GitHub App's JWT; {sync}"),
        )
    }
}

/// `Wed, 11 Nov 2026 10:00:00 GMT`, the form HTTP's `Date` takes, to Unix seconds.
fn parse_http_date(s: &str) -> Option<UnixSecs> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = parts.as_slice() else {
        return None;
    };
    let month = MONTHS.iter().position(|m| m == month)? + 1;
    let day: u32 = day.parse().ok()?;
    github_app::parse_utc(&format!("{year}-{month:02}-{day:02}T{time}Z"))
}

fn boat_checks(
    r: &mut Report,
    cfg: &Config,
    var: &dyn Fn(&str) -> Option<String>,
    t: &dyn Transport,
) {
    let Some(key) = var("BOAT_API_KEY") else {
        r.fail(
            "boat",
            "BOAT_API_KEY is not set: set it to a boat key for the account that owns the \
             snapshots",
        );
        return;
    };
    let boat = boat::Client::new(t, key);
    match boat.limits() {
        Ok(q) if q.allows => r.ok("boat", "the key works; boat would allow a start now"),
        Ok(_) => r.warn(
            "boat",
            "the key works, but boat would refuse a start now (a start limit or the daily \
             spend); the daemon waits",
        ),
        Err(boat::Error::BillingRequired) => {
            r.fail(
                "boat",
                "boat billing required: add billing to the boat account",
            );
            return;
        }
        Err(e @ boat::Error::Unauthorized { .. }) => {
            r.fail("boat", format!("{e}: check BOAT_API_KEY or make a new key"));
            return;
        }
        Err(e) => {
            r.fail("boat", format!("{e}: is boat.dev reachable from here?"));
            return;
        }
    }
    for pool in &cfg.pools {
        pool_snapshot(r, &boat, pool);
    }
    stray_machines(r, cfg, &boat);
}

/// Machines `run` never lists again: those of a pool the policy no longer has.
fn stray_machines(r: &mut Report, cfg: &Config, boat: &boat::Client) {
    let all = match boat.list_daemon_machines() {
        Ok(all) => all,
        Err(e) => {
            r.warn(
                "machines",
                format!("could not list the account's machines: {e}"),
            );
            return;
        }
    };
    let pools: Vec<&str> = cfg.pools.iter().map(|p| p.name.as_str()).collect();
    let report = boat::stray_report(&cfg.repo, &pools, &all);
    if report.is_empty() {
        r.ok(
            "machines",
            format!(
                "every machine of {} is in one of the policy's pools",
                cfg.repo
            ),
        );
    }
    for line in report {
        r.warn("machines", line);
    }
}

fn pool_snapshot(r: &mut Report, boat: &boat::Client, pool: &Pool) {
    let what = format!("pool {}: {}", pool.name, pool.snapshot);
    let remake = format!("`sw-boat-runner snapshot <sandbox-id> {}`", pool.snapshot);
    let saved = match boat.saved_snapshots(Some(&pool.snapshot)) {
        Ok(saved) => saved,
        Err(e) => {
            r.fail(
                "snapshot",
                format!("{what}: could not list saved snapshots: {e}"),
            );
            return;
        }
    };
    let Some(s) = saved.iter().find(|s| s.name == pool.snapshot) else {
        r.fail(
            "snapshot",
            format!("{what} is not a saved snapshot: save one with {remake}"),
        );
        return;
    };
    match s.status.as_str() {
        "ready" => {}
        "saving" => r.warn("snapshot", format!("{what} is still saving")),
        status => {
            let error = s
                .error
                .as_deref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            r.fail(
                "snapshot",
                format!("{what} is {status}{error}: save it again with {remake}"),
            );
            return;
        }
    }
    let budget = boat::disk_budget_bytes(&pool.machine_type);
    if let Some(too_big) = snapshot::oversized(std::slice::from_ref(pool), &saved).pop() {
        r.fail(
            "snapshot",
            format!(
                "{too_big}; pick a larger machine_type or save a smaller snapshot \
                 (`sw-boat-runner snapshot` lists the largest directories)"
            ),
        );
    } else {
        match (s.size_bytes, budget) {
            (Some(size), Some(budget)) => r.ok(
                "snapshot",
                format!(
                    "{what} holds {}, within {}'s {} disk",
                    snapshot::gb(size),
                    pool.machine_type,
                    snapshot::gb(budget)
                ),
            ),
            _ => r.warn(
                "snapshot",
                format!(
                    "{what}: boat did not say its size, or {} has no known disk size",
                    pool.machine_type
                ),
            ),
        }
    }
    let unknown = "whether it holds its creator's logins is unknown; the daemon asks \
                   boat before it registers each runner";
    match &s.source_sandbox_id {
        None => r.warn(
            "snapshot",
            format!("{what}: no source sandbox, so {unknown}"),
        ),
        Some(id) => match boat.get(id) {
            Ok(sb) => match sb.holds_creator_logins {
                Some(false) => r.ok(
                    "snapshot",
                    format!("{what} came from {id}, which holds no creator logins"),
                ),
                Some(true) => r.fail(
                    "snapshot",
                    format!(
                        "{what} came from {id}, which holds its creator's logins, so the \
                         daemon registers no runner on it: save a new one from a \
                         `--no-env` sandbox with {remake}"
                    ),
                ),
                None => r.warn("snapshot", format!("{what}: boat did not say; {unknown}")),
            },
            Err(boat::Error::NotFound) => r.warn(
                "snapshot",
                format!("{what}: its source {id} is gone, so {unknown}"),
            ),
            Err(e) => r.warn("snapshot", format!("{what}: {e}; {unknown}")),
        },
    }
}

/// The App's installation and permissions, then the runners the credential lists.
/// `None` when GitHub access failed.
fn github_checks(
    r: &mut Report,
    cfg: &Config,
    var: &dyn Fn(&str) -> Option<String>,
    t: &dyn Transport,
    now: UnixSecs,
) -> Option<Vec<GhRunner>> {
    let auth = match run::github_auth(var) {
        Ok(auth) => auth,
        Err(e) => {
            r.fail("github", e.to_string());
            return None;
        }
    };
    if let Auth::App(app) = &auth {
        app_checks(r, app, &cfg.repo, t, now)?;
    }
    let pat = matches!(auth, Auth::Token(_));
    let mut gh = github::Client::new(t, auth, cfg.repo.clone());
    gh.begin_tick(now);
    let runners = match gh.runners() {
        Ok(runners) => runners,
        Err(e) => {
            let fix = match &e {
                github::Error::Unauthorized { .. } if pat => format!(
                    ": GITHUB_TOKEN needs to be a fine-grained token for {} with \
                     Administration read and write and Actions read",
                    cfg.repo
                ),
                github::Error::NotFound => format!(
                    ": check `repo` in the policy, and that the credential can see {}",
                    cfg.repo
                ),
                _ => String::new(),
            };
            r.fail("github", format!("listing runners: {e}{fix}"));
            return None;
        }
    };
    let how = if pat {
        "GITHUB_TOKEN"
    } else {
        "the App's installation token"
    };
    r.ok("github", format!("{how} lists {}'s runners", cfg.repo));
    match gh.rate_left() {
        Some((left, reset)) => {
            let text = format!(
                "{left} GitHub requests left, reset in {} s",
                reset.saturating_sub(now)
            );
            if left < github::RESERVE {
                r.warn(
                    "github",
                    format!(
                        "{text}; under {} the daemon reads less often",
                        github::RESERVE
                    ),
                );
            } else {
                r.ok("github", text);
            }
        }
        None => r.warn("github", "GitHub did not report the rate limit left"),
    }
    Some(runners)
}

fn app_checks(
    r: &mut Report,
    app: &App,
    repo: &str,
    t: &dyn Transport,
    now: UnixSecs,
) -> Option<()> {
    let key_hint = "check GITHUB_APP_ID, the key in GITHUB_APP_PRIVATE_KEY_FILE, and the clock";
    let installation = match app.installation() {
        Some(id) => id,
        None => match app.installation_on(t, repo, now) {
            Ok(Some(id)) => id,
            Ok(None) => {
                r.fail(
                    "github",
                    format!(
                        "the App is not installed on {repo}: install it there (the App's \
                         settings, Install App)"
                    ),
                );
                return None;
            }
            Err(e) => {
                r.fail(
                    "github",
                    format!("finding the App's installation: {e}; {key_hint}"),
                );
                return None;
            }
        },
    };
    match app.installation_permissions(t, installation, now) {
        Ok(perms) => {
            let missing = missing_permissions(&perms);
            if missing.is_empty() {
                r.ok(
                    "github",
                    format!(
                        "App installation {installation} has administration: write and \
                         actions: read"
                    ),
                );
                Some(())
            } else {
                r.fail(
                    "github",
                    format!(
                        "App installation {installation} lacks {}: add them in the App's \
                         permissions, then accept the change on the installation",
                        missing.join(", ")
                    ),
                );
                None
            }
        }
        Err(e) => {
            r.fail(
                "github",
                format!("reading the App's installation: {e}; {key_hint}"),
            );
            None
        }
    }
}

/// What the daemon needs and the grant lacks, as `administration: write (has read)`.
pub(crate) fn missing_permissions(granted: &BTreeMap<String, String>) -> Vec<String> {
    fn rank(level: &str) -> u8 {
        match level {
            "read" => 1,
            "write" => 2,
            "admin" => 3,
            _ => 0,
        }
    }
    [("administration", "write"), ("actions", "read")]
        .into_iter()
        .filter_map(|(name, need)| {
            let has = granted.get(name).map(String::as_str);
            if has.is_some_and(|h| rank(h) >= rank(need)) {
                return None;
            }
            Some(match has {
                Some(h) => format!("{name}: {need} (has {h})"),
                None => format!("{name}: {need}"),
            })
        })
        .collect()
}

/// `(LoadState, ActiveState)` of the service, or `None` without systemd.
fn service_state() -> Option<(String, String)> {
    if !Path::new("/run/systemd/system").exists() {
        return None;
    }
    let out = |args: &[&str]| {
        Command::new("systemctl")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let load = out(&["show", "-p", "LoadState", "--value", SERVICE])?;
    let active = out(&["is-active", SERVICE]).unwrap_or_default();
    Some((load, active))
}

pub(crate) fn service_line(state: Option<(&str, &str)>) -> Line {
    let logs = format!("`journalctl -u {SERVICE} -n 50` says why");
    match state {
        None => line(
            Level::Warn,
            "service",
            "no systemd here; whatever runs `sw-boat-runner run` is not checked",
        ),
        Some((load, _)) if load != "loaded" => line(
            Level::Warn,
            "service",
            format!(
                "{SERVICE}.service is not installed ({load}); `sw-boat-runner setup` installs it"
            ),
        ),
        Some((_, "active")) => line(Level::Ok, "service", format!("{SERVICE} is active")),
        Some((_, s @ ("activating" | "reloading"))) => line(
            Level::Warn,
            "service",
            format!("{SERVICE} is {s}; if it keeps restarting, {logs}"),
        ),
        Some((_, s)) => line(
            Level::Fail,
            "service",
            format!("{SERVICE} is {s}: {logs}; `sudo systemctl restart {SERVICE}` starts it"),
        ),
    }
}

/// Each pool's runners, found by the pool in their `sbr-<pool>-...` names.
pub(crate) fn runner_lines(cfg: &Config, runners: &[GhRunner]) -> Vec<Line> {
    cfg.pools
        .iter()
        .map(|pool| {
            let mine: Vec<&GhRunner> = runners
                .iter()
                .filter(|g| github::parse_runner_name(&g.name).is_some_and(|n| n.pool == pool.name))
                .collect();
            let labels = pool.labels.join(",");
            let online = mine.iter().filter(|g| g.status == "online").count();
            let busy = mine.iter().filter(|g| g.busy).count();
            let what = format!("pool {} [{labels}]", pool.name);
            match mine.len() {
                0 if pool.min_up > 0 => line(
                    Level::Warn,
                    "runners",
                    format!(
                        "{what}: none yet, though min_up is {}; the daemon registers them \
                         within a few ticks, else `journalctl -u {SERVICE}` says why",
                        pool.min_up
                    ),
                ),
                0 => line(
                    Level::Ok,
                    "runners",
                    format!("{what}: none; they are registered when jobs queue"),
                ),
                n => line(
                    Level::Ok,
                    "runners",
                    format!("{what}: {n} registered, {online} online, {busy} busy"),
                ),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake::{Fake, Sent, respond};
    use crate::transport::{NetworkError, Response};

    const DATE: &str = "Wed, 11 Nov 2026 10:00:00 GMT";

    fn now() -> UnixSecs {
        parse_http_date(DATE).unwrap()
    }

    fn cfg(min_up: u32) -> Config {
        let text = format!(
            "repo = \"o/r\"\n[[pool]]\nname = \"ci\"\nlabels = [\"self-hosted\", \"large\"]\n\
             machine_type = \"large\"\nsnapshot = \"snap-1\"\nmin_up = {min_up}\n"
        );
        policy::parse(&text).unwrap()
    }

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn levels(r: &Report) -> Vec<(Level, &'static str)> {
        r.lines.iter().map(|l| (l.level, l.check)).collect()
    }

    #[test]
    fn parses_http_dates() {
        assert_eq!(
            parse_http_date("Sun, 09 Sep 2001 01:46:40 GMT"),
            Some(1_000_000_000)
        );
        assert_eq!(parse_http_date("Sun, 09 Sep 2001 01:46:40 UTC"), None);
        assert_eq!(parse_http_date("Sun, 09 Foo 2001 01:46:40 GMT"), None);
        assert_eq!(parse_http_date(""), None);
    }

    #[test]
    fn classifies_clock_skew() {
        let at = |local: u64| classify_skew(local, 1_000).level;
        assert_eq!(at(1_000), Level::Ok);
        assert_eq!(at(1_030), Level::Ok);
        assert_eq!(at(970), Level::Ok);
        assert_eq!(at(1_031), Level::Warn);
        assert_eq!(at(1_300), Level::Warn);
        assert_eq!(at(699), Level::Fail);
        let fail = classify_skew(1_301, 1_000);
        assert_eq!(fail.level, Level::Fail);
        assert!(
            fail.text.starts_with("301 s ahead of GitHub's"),
            "{}",
            fail.text
        );
        assert!(fail.text.contains("timedatectl"));
        assert!(classify_skew(900, 1_000).text.starts_with("100 s behind"));
    }

    #[test]
    fn checks_the_apps_permissions() {
        let grant = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        let ok = grant(&[
            ("administration", "write"),
            ("actions", "read"),
            ("metadata", "read"),
        ]);
        assert!(missing_permissions(&ok).is_empty());
        let more = grant(&[("administration", "admin"), ("actions", "write")]);
        assert!(missing_permissions(&more).is_empty());
        assert_eq!(
            missing_permissions(&grant(&[("administration", "read")])),
            ["administration: write (has read)", "actions: read"]
        );
    }

    #[test]
    fn a_report_prints_one_line_each_and_fails_on_any_fail() {
        let mut r = Report::default();
        r.ok("policy", "fine");
        r.warn("clock", "a bit\nslow");
        assert!(!r.failed());
        assert_eq!(r.exit_code(), ExitCode::SUCCESS);
        assert_eq!(
            r.to_string(),
            "ok    policy    fine\nwarn  clock     a bit slow\ndoctor: no FAIL, 1 warn\n"
        );
        r.fail("boat", "broken: fix it");
        assert!(r.failed());
        assert_eq!(r.exit_code(), ExitCode::FAILURE);
        assert!(
            r.to_string()
                .ends_with("FAIL  boat      broken: fix it\ndoctor: 1 FAIL, 1 warn; fix each FAIL line and run again\n")
        );
    }

    #[test]
    fn classifies_the_service() {
        assert_eq!(service_line(None).level, Level::Warn);
        assert_eq!(
            service_line(Some(("not-found", "inactive"))).level,
            Level::Warn
        );
        assert_eq!(service_line(Some(("loaded", "active"))).level, Level::Ok);
        assert_eq!(
            service_line(Some(("loaded", "activating"))).level,
            Level::Warn
        );
        let failed = service_line(Some(("loaded", "failed")));
        assert_eq!(failed.level, Level::Fail);
        assert!(failed.text.contains("journalctl -u sw-boat-runner"));
    }

    #[test]
    fn reads_credentials_from_the_shell_or_roots_env_file() {
        let file = || {
            Ok(Some(
                "# setup\nBOAT_API_KEY=\"boat_secret\"\nGITHUB_TOKEN=ghp_secret\n".into(),
            ))
        };
        let shell = vars(&[("BOAT_API_KEY", "boat_shell")]);
        let (l, vars_from_file) = credentials(true, &shell, &file);
        assert_eq!(
            (l.level, l.text.as_str()),
            (Level::Ok, "this shell sets BOAT_API_KEY")
        );
        assert!(vars_from_file.is_none());

        let empty = vars(&[]);
        let (l, got) = credentials(true, &empty, &file);
        assert_eq!(l.text, format!("{ENV} sets BOAT_API_KEY, GITHUB_TOKEN"));
        assert!(!l.text.contains("secret"));
        let got = got.unwrap();
        assert_eq!(
            got[0],
            ("BOAT_API_KEY".to_string(), "boat_secret".to_string())
        );

        let (l, got) = credentials(false, &empty, &file);
        assert_eq!(l.level, Level::Warn);
        assert!(l.text.contains("sudo sw-boat-runner doctor"));
        assert!(got.is_none());

        let (l, _) = credentials(true, &empty, &|| Ok(None));
        assert_eq!(l.level, Level::Warn);
        let (l, _) = credentials(true, &empty, &|| Err(format!("{ENV}: denied")));
        assert_eq!(l.level, Level::Fail);
    }

    #[test]
    fn counts_each_pools_runners() {
        let runner = |id: u64, pool: &str, status: &str, busy: bool| GhRunner {
            id,
            name: github::runner_name(pool, "bx_44444444", id as u32, 100),
            status: status.into(),
            busy,
        };
        let runners = [
            runner(1, "ci", "online", true),
            runner(2, "ci", "online", false),
            runner(3, "ci", "offline", false),
            runner(4, "other", "online", false),
        ];
        let lines = runner_lines(&cfg(1), &runners);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].level, Level::Ok);
        assert_eq!(
            lines[0].text,
            "pool ci [self-hosted,large]: 3 registered, 2 online, 1 busy"
        );
        assert_eq!(runner_lines(&cfg(1), &[])[0].level, Level::Warn);
        assert_eq!(runner_lines(&cfg(0), &[])[0].level, Level::Ok);
    }

    /// boat and GitHub for one pool; `tweak` changes an answer by URL.
    fn world(tweak: impl Fn(&Sent) -> Option<Response> + 'static) -> Fake {
        Fake::failing(move |s| {
            if let Some(resp) = tweak(s) {
                return Ok(resp);
            }
            let runners = format!(
                r#"{{"total_count":1,"runners":[{{"id":1,"name":"{}","status":"online","busy":true}}]}}"#,
                github::runner_name("ci", "bx_44444444", 0, 100)
            );
            let rate = [
                ("x-ratelimit-remaining", "4990"),
                ("x-ratelimit-reset", "1794394800"),
                ("x-ratelimit-limit", "5000"),
            ];
            Ok(match (s.method.as_str(), s.url.as_str()) {
                ("GET", "https://api.github.com/rate_limit") => {
                    respond(200, &[("Date", DATE)], "{}")
                }
                ("GET", "https://boat.dev/api/v1/limits") => {
                    respond(200, &[], r#"{"canStart":true,"nextStartDollars":0}"#)
                }
                ("GET", "https://boat.dev/api/v1/saved-snapshots?name=snap-1") => respond(
                    200,
                    &[],
                    r#"{"snapshots":[{"name":"snap-1","status":"ready",
                        "sourceSandboxId":"bx_55555555","sizeBytes":20000000000}]}"#,
                ),
                ("GET", "https://boat.dev/api/v1/sandboxes?limit=200") => respond(
                    200,
                    &[],
                    r#"{"sandboxes":[{"id":"bx_44444444","state":"running",
                        "labels":{"sw-boat-runner":"ci","sw-boat-runner-repo":"o/r"}}],
                        "pageInfo":{"nextCursor":null,"hasMore":false}}"#,
                ),
                ("GET", "https://boat.dev/api/v1/sandboxes/bx_55555555") => respond(
                    200,
                    &[],
                    r#"{"sandbox":{"id":"bx_55555555","state":"stopped","holdsCreatorLogins":false}}"#,
                ),
                ("GET", url)
                    if url.starts_with("https://api.github.com/repos/o/r/actions/runners") =>
                {
                    respond(200, &rate, &runners)
                }
                ("GET", "https://api.github.com/app/installations/77") => respond(
                    200,
                    &[],
                    r#"{"id":77,"permissions":{"administration":"write","actions":"read","metadata":"read"}}"#,
                ),
                ("POST", "https://api.github.com/app/installations/77/access_tokens") => respond(
                    201,
                    &[],
                    r#"{"token":"ghs_secret","expires_at":"2026-11-11T11:00:00Z"}"#,
                ),
                _ => respond(404, &[], r#"{"message":"Not Found"}"#),
            })
        })
    }

    fn pat() -> [(&'static str, &'static str); 2] {
        [("BOAT_API_KEY", "boat_key"), ("GITHUB_TOKEN", "ghp_secret")]
    }

    #[test]
    fn a_working_pat_setup_has_no_fail() {
        let fake = world(|_| None);
        let mut r = Report::default();
        let service = Some(("loaded".to_string(), "active".to_string()));
        checks(&mut r, &cfg(1), &vars(&pat()), &fake, now() + 3, service);
        let out = r.to_string();
        assert!(!r.failed(), "{out}");
        assert_eq!(
            levels(&r),
            [
                (Level::Ok, "clock"),
                (Level::Ok, "boat"),
                (Level::Ok, "snapshot"),
                (Level::Ok, "snapshot"),
                (Level::Ok, "machines"),
                (Level::Ok, "github"),
                (Level::Ok, "github"),
                (Level::Ok, "service"),
                (Level::Ok, "runners"),
            ],
            "{out}"
        );
        assert!(
            out.contains("snap-1 holds 20.0 GB, within large's 111.0 GB disk"),
            "{out}"
        );
        assert!(out.contains("4990 GitHub requests left"), "{out}");
        assert!(out.contains("1 registered, 1 online, 1 busy"), "{out}");
        assert!(
            !out.contains("secret") && !out.contains("boat_key"),
            "{out}"
        );
        // The clock check is unauthenticated.
        assert!(fake.sent()[0].header("authorization").is_none());
    }

    #[test]
    fn problems_are_fails_that_say_how_to_fix() {
        let fake = world(|s| match s.url.as_str() {
            "https://api.github.com/rate_limit" => Some(respond(
                200,
                &[("Date", "Wed, 11 Nov 2026 09:50:00 GMT")],
                "{}",
            )),
            "https://boat.dev/api/v1/saved-snapshots?name=snap-1" => Some(respond(
                200,
                &[],
                r#"{"snapshots":[{"name":"snap-1","status":"ready",
                    "sourceSandboxId":"bx_55555555","sizeBytes":200000000000}]}"#,
            )),
            "https://boat.dev/api/v1/sandboxes/bx_55555555" => Some(respond(
                200,
                &[],
                r#"{"sandbox":{"id":"bx_55555555","state":"stopped","holdsCreatorLogins":true}}"#,
            )),
            url if url.contains("/actions/runners") => {
                Some(respond(401, &[], r#"{"message":"Bad credentials"}"#))
            }
            // A renamed pool's machine, a pre-0.3.0 machine of a pool not in the policy
            // and one of `ci`, another repository's, and a deleted one.
            "https://boat.dev/api/v1/sandboxes?limit=200" => Some(respond(
                200,
                &[],
                r#"{"sandboxes":[
                    {"id":"bx_22222222","state":"idle",
                     "labels":{"sw-boat-runner":"old","sw-boat-runner-repo":"o/r"}},
                    {"id":"bx_33333333","state":"archived","labels":{"sw-boat-runner":"boat"}},
                    {"id":"bx_44444444","state":"idle","labels":{"sw-boat-runner":"ci"}},
                    {"id":"bx_66666666","state":"idle",
                     "labels":{"sw-boat-runner":"old","sw-boat-runner-repo":"o/other"}},
                    {"id":"bx_77777777","state":"deleted",
                     "labels":{"sw-boat-runner":"old","sw-boat-runner-repo":"o/r"}},
                    {"id":"bx_88888888","state":"idle","labels":{"other":"x"}}],
                    "pageInfo":{"nextCursor":null,"hasMore":false}}"#,
            )),
            _ => None,
        });
        let mut r = Report::default();
        let service = Some(("loaded".to_string(), "failed".to_string()));
        checks(&mut r, &cfg(1), &vars(&pat()), &fake, now(), service);
        let out = r.to_string();
        assert!(r.failed());
        assert_eq!(
            levels(&r),
            [
                (Level::Fail, "clock"),
                (Level::Ok, "boat"),
                (Level::Fail, "snapshot"),
                (Level::Fail, "snapshot"),
                (Level::Warn, "machines"),
                (Level::Warn, "machines"),
                (Level::Warn, "machines"),
                (Level::Fail, "github"),
                (Level::Fail, "service"),
                (Level::Warn, "runners"),
            ],
            "{out}"
        );
        assert!(out.contains("larger machine_type"), "{out}");
        assert!(
            out.contains("pool old is not in the policy but has 1 machine(s) of o/r: bx_22222222")
                && out.contains("`boat delete bx_22222222 --yes`"),
            "{out}"
        );
        assert!(
            out.contains("pool boat is not in the policy and has 1 machine(s) without a repo"),
            "{out}"
        );
        assert!(
            out.contains("pool ci: 1 machine(s) have no repo label"),
            "{out}"
        );
        assert!(
            !out.contains("bx_66666666") && !out.contains("bx_77777777"),
            "{out}"
        );
        assert!(out.contains("--no-env"), "{out}");
        assert!(out.contains("Administration read and write"), "{out}");
        assert!(!out.contains("ghp_secret"), "{out}");
    }

    #[test]
    fn missing_credentials_and_snapshots_fail() {
        let fake = world(|s| {
            s.url
                .contains("saved-snapshots")
                .then(|| respond(200, &[], r#"{"snapshots":[]}"#))
        });
        let mut r = Report::default();
        checks(
            &mut r,
            &cfg(0),
            &vars(&[("BOAT_API_KEY", "k")]),
            &fake,
            now(),
            None,
        );
        let out = r.to_string();
        assert!(
            out.contains("snap-1 is not a saved snapshot: save one with"),
            "{out}"
        );
        assert!(
            out.contains("FAIL  github    GITHUB_TOKEN is not set"),
            "{out}"
        );
        assert!(out.contains("warn  service   no systemd"), "{out}");

        let fake = world(|_| None);
        let mut r = Report::default();
        checks(&mut r, &cfg(0), &vars(&[]), &fake, now(), None);
        assert!(
            r.to_string()
                .contains("FAIL  boat      BOAT_API_KEY is not set")
        );
    }

    #[test]
    fn a_network_failure_is_reported_not_fatal() {
        let fake = Fake::failing(|_| Err(NetworkError("connection refused".into())));
        let mut r = Report::default();
        checks(&mut r, &cfg(0), &vars(&pat()), &fake, now(), None);
        let out = r.to_string();
        assert!(out.contains("warn  clock"), "{out}");
        assert!(
            out.contains("FAIL  boat      boat network error: connection refused"),
            "{out}"
        );
        assert!(out.contains("FAIL  github"), "{out}");
    }

    fn with_app<T>(
        installation: Option<&str>,
        f: impl FnOnce(&dyn Fn(&str) -> Option<String>) -> T,
    ) -> T {
        let path = std::env::temp_dir().join(format!(
            "sbr-doctor-key-{}-{}.pem",
            std::process::id(),
            installation.unwrap_or("none")
        ));
        std::fs::write(&path, github_app::tests::test_pem()).unwrap();
        let path_text = path.to_str().unwrap().to_string();
        let mut pairs = vec![
            ("BOAT_API_KEY", "boat_key".to_string()),
            ("GITHUB_APP_ID", "12345".to_string()),
            ("GITHUB_APP_PRIVATE_KEY_FILE", path_text),
        ];
        if let Some(i) = installation {
            pairs.push(("GITHUB_APP_INSTALLATION_ID", i.to_string()));
        }
        let var = move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        };
        let out = f(&var);
        std::fs::remove_file(&path).unwrap();
        out
    }

    #[test]
    fn an_app_is_checked_for_its_installation_and_permissions() {
        let fake = world(|s| {
            (s.url == "https://api.github.com/repos/o/r/installation")
                .then(|| respond(200, &[], r#"{"id":77}"#))
        });
        let mut r = Report::default();
        with_app(None, |var| checks(&mut r, &cfg(0), var, &fake, now(), None));
        let out = r.to_string();
        assert!(!r.failed(), "{out}");
        assert!(
            out.contains("App installation 77 has administration: write"),
            "{out}"
        );
        assert!(
            out.contains("the App's installation token lists o/r's runners"),
            "{out}"
        );
        assert!(!out.contains("ghs_secret"), "{out}");

        let fake = world(|s| {
            (s.url == "https://api.github.com/app/installations/77").then(|| {
                respond(
                    200,
                    &[],
                    r#"{"id":77,"permissions":{"administration":"read"}}"#,
                )
            })
        });
        let mut r = Report::default();
        with_app(Some("77"), |var| {
            checks(&mut r, &cfg(0), var, &fake, now(), None)
        });
        let out = r.to_string();
        assert!(r.failed());
        assert!(
            out.contains("lacks administration: write (has read), actions: read"),
            "{out}"
        );
        // Nothing past the failed permission check asks GitHub for a token.
        assert!(fake.sent().iter().all(|s| !s.url.contains("access_tokens")));

        let fake = world(|_| None);
        let mut r = Report::default();
        with_app(None, |var| checks(&mut r, &cfg(0), var, &fake, now(), None));
        assert!(r.to_string().contains("the App is not installed on o/r"));
    }
}
