//! `snapshot`: checks a sandbox for what breaks a CI snapshot, then saves one.
//!
//! The source sandbox is never changed. The checks run in a `noEnv` fork of it, which
//! drops the creator logins the source holds, and the snapshot is saved from that fork,
//! so it stays `noEnv` too. Every command the fork runs only reads, except the `--fix`
//! step, which changes the fork alone.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::boat::{self, Client, Detail, SavedSnapshot};
use crate::model::Pool;
use crate::transport::{Transport, Ureq};

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Label of the fork, so a leftover one is easy to find. Not the pool label: the daemon
/// must never adopt it.
const FORK_LABEL: &str = "sw-boat-runner-snapshot";
/// Label holding this run's id, so a fork whose create got no answer can be found.
const RUN_LABEL: &str = "sw-boat-runner-snapshot-run";
/// Prefix of the name a replacement is first saved under, to measure it.
const TEMP_PREFIX: &str = "sbr-tmp-";
const INTERRUPTED: &str = "interrupted";
/// The fork stops itself after this even if this process dies.
const FORK_TTL_S: u64 = 4 * 3600;
const POLL: Duration = Duration::from_secs(5);
const READY_POLLS: u32 = 120; // 10 minutes
const HYDRATE_POLLS: u32 = 720; // 60 minutes: a 92.8 GB snapshot took 7-12
const EXEC_POLLS: u32 = 360; // 30 minutes of du over a large disk
const SAVE_POLLS: u32 = 720; // 60 minutes
const RETRIES: u32 = 5;
const HOME: &str = "/home/user";
const RUNNER: &str = "/home/user/actions-runner";
const MACHINE_TYPES: [&str; 4] = ["small", "default", "large", "xlarge"];
/// Names boat keeps for itself.
const RESERVED: [&str; 8] = [
    "latest", "tree", "pull", "rm", "save", "current", "self", "new",
];

pub const USAGE: &str =
    "sw-boat-runner snapshot <sandbox-id> <name> [--machine-type T] [--fix] [--keep-fork] [--yes]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub source: String,
    pub name: String,
    /// The pool's `machine_type`; `None` keeps the source's.
    pub machine_type: Option<String>,
    pub fix: bool,
    pub keep_fork: bool,
    pub yes: bool,
}

/// Reads the arguments after `snapshot`.
pub fn parse_args(args: &[&str]) -> Result<Options, String> {
    let mut positional = Vec::new();
    let mut opts = Options {
        source: String::new(),
        name: String::new(),
        machine_type: None,
        fix: false,
        keep_fork: false,
        yes: false,
    };
    let mut it = args.iter();
    while let Some(&arg) = it.next() {
        match arg {
            "--fix" => opts.fix = true,
            "--keep-fork" => opts.keep_fork = true,
            "--yes" | "-y" => opts.yes = true,
            "--machine-type" => {
                let t = it.next().ok_or("--machine-type needs a value")?;
                opts.machine_type = Some(t.to_string());
            }
            a if a.starts_with("--machine-type=") => {
                opts.machine_type = Some(a["--machine-type=".len()..].to_string());
            }
            a if a.starts_with('-') => return Err(format!("unknown option {a}")),
            a => positional.push(a),
        }
    }
    let [source, name] = positional[..] else {
        return Err("need a sandbox id and a snapshot name".into());
    };
    if !boat::is_sandbox_id(source) {
        return Err(format!(
            "{source:?} is not a boat sandbox id (bx_ and 8 characters)"
        ));
    }
    if !valid_name(name) {
        return Err(format!(
            "{name:?} is not a snapshot name: 1-63 lowercase letters, digits or dashes, \
             starting with a letter or digit, and not one of {}",
            RESERVED.join(", ")
        ));
    }
    if let Some(t) = &opts.machine_type {
        if !MACHINE_TYPES.contains(&t.as_str()) {
            return Err(format!(
                "machine type {t:?} is not one of {}",
                MACHINE_TYPES.join(", ")
            ));
        }
    }
    opts.source = source.to_string();
    opts.name = name.to_string();
    Ok(opts)
}

fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase() | b[0].is_ascii_digit()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !RESERVED.contains(&name)
}

/// What the command needs from the outside world besides boat, so tests run instantly.
pub(crate) trait Io {
    fn sleep(&mut self, d: Duration);
    fn say(&mut self, line: &str);
    /// A yes/no question; anything but a yes is a no.
    fn confirm(&mut self, question: &str) -> bool;
    /// True once a SIGINT or SIGTERM asked the command to stop.
    fn interrupted(&self) -> bool;
}

struct Terminal {
    stop: Arc<AtomicBool>,
}

impl Terminal {
    /// Catches SIGINT and SIGTERM so the fork is deleted before exiting; a second
    /// signal exits at once.
    fn new() -> Terminal {
        let stop = Arc::new(AtomicBool::new(false));
        for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            // The shutdown hook goes first, so it sees the flag unset on the first signal.
            let _ = signal_hook::flag::register_conditional_shutdown(sig, 130, Arc::clone(&stop));
            let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
        }
        Terminal { stop }
    }
}

impl Io for Terminal {
    fn sleep(&mut self, d: Duration) {
        let step = Duration::from_millis(200);
        let mut left = d;
        while !left.is_zero() && !self.interrupted() {
            let s = left.min(step);
            std::thread::sleep(s);
            left -= s;
        }
    }

    fn interrupted(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn confirm(&mut self, question: &str) -> bool {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            println!("{question} [y/N] no (stdin is not a terminal; pass --yes)");
            return false;
        }
        print!("{question} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        stdin.lock().read_line(&mut answer).is_ok()
            && matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
    }
}

/// Runs the command with `BOAT_API_KEY`; `Ok(true)` when the snapshot was saved.
pub fn run(opts: &Options) -> Result<bool, String> {
    let key = std::env::var("BOAT_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or("BOAT_API_KEY is not set")?;
    let transport = Ureq::new(HTTP_TIMEOUT);
    snapshot(&transport, key, opts, &mut Terminal::new())
}

pub(crate) fn snapshot(
    t: &dyn Transport,
    key: String,
    opts: &Options,
    io: &mut dyn Io,
) -> Result<bool, String> {
    let boat = Client::new(t, key);
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let labels = BTreeMap::from([(FORK_LABEL, opts.name.clone()), (RUN_LABEL, run_id.clone())]);
    let idempotency_key = format!("sw-boat-runner-snapshot-{run_id}");
    io.say(&format!("forking {} with noEnv", opts.source));
    let fork = match retry(io, "fork", |_| {
        boat.fork_sandbox(
            &opts.source,
            opts.machine_type.as_deref(),
            &labels,
            FORK_TTL_S,
            &idempotency_key,
        )
    }) {
        Ok(fork) => fork,
        Err(e) => {
            delete_lost_forks(&boat, &run_id, io);
            return Err(e);
        }
    };
    io.say(&format!("fork {fork}"));
    let result = inspect_and_save(&boat, &fork, opts, io);
    if opts.keep_fork {
        io.say(&format!("keeping fork {fork}"));
    } else {
        match cleanup_retry(io, "delete fork", |_| boat.delete(&fork)) {
            Ok(()) => io.say(&format!("deleted fork {fork}")),
            Err(e) => io.say(&format!(
                "could not delete fork {fork}: {e}; delete it by hand"
            )),
        }
    }
    result
}

/// After a fork call that failed without a definite answer, or was interrupted, deletes
/// any fork this run made anyway.
fn delete_lost_forks(boat: &Client, run_id: &str, io: &mut dyn Io) {
    let by_hand =
        format!("look for sandboxes labelled {RUN_LABEL}={run_id} and delete them by hand");
    let forks = match cleanup_retry(io, "list forks", |_| boat.list_labelled(RUN_LABEL, run_id)) {
        Ok(forks) => forks,
        Err(e) => {
            io.say(&format!(
                "could not check for a leftover fork ({e}); {by_hand}"
            ));
            return;
        }
    };
    for f in forks {
        match cleanup_retry(io, "delete fork", |_| boat.delete(&f.id)) {
            Ok(()) => io.say(&format!("deleted leftover fork {}", f.id)),
            Err(e) => io.say(&format!(
                "could not delete leftover fork {}: {e}; {by_hand}",
                f.id
            )),
        }
    }
}

fn inspect_and_save(
    boat: &Client,
    fork: &str,
    opts: &Options,
    io: &mut dyn Io,
) -> Result<bool, String> {
    let detail = wait_until_hydrated(boat, fork, io)?;
    let machine_type = opts
        .machine_type
        .clone()
        .or(detail.machine_type.clone())
        .unwrap_or_default();
    let mut report = Report::parse(&exec(boat, fork, CHECK_SCRIPT, io)?);
    let mut checks = evaluate(&report, detail.holds_creator_logins, &machine_type);
    if opts.fix {
        if let Some(script) = fix_script(&report, opts.yes, io)? {
            io.say("fixing the fork:");
            for line in script.lines() {
                io.say(&format!("  {line}"));
            }
            exec(boat, fork, &script, io)?;
            report = Report::parse(&exec(boat, fork, CHECK_SCRIPT, io)?);
            checks = evaluate(&report, detail.holds_creator_logins, &machine_type);
        }
    }
    for c in &checks {
        io.say(&c.to_string());
    }
    io.say("largest directories under /home/user:");
    for (bytes, path) in &report.largest {
        io.say(&format!("  {:>9}  {path}", gb(*bytes)));
    }
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    if fails > 0 {
        io.say(&format!(
            "{fails} check(s) failed; nothing saved{}",
            if opts.fix {
                ""
            } else {
                " (--fix handles some)"
            }
        ));
        return Ok(false);
    }
    let existing = retry(io, "list saved snapshots", |_| {
        boat.saved_snapshots(Some(&opts.name))
    })?;
    let replacing = existing.iter().any(|s| s.name == opts.name);
    if replacing
        && !opts.yes
        && !io.confirm(&format!(
            "a saved snapshot called {} exists; replace it?",
            opts.name
        ))
    {
        io.say("nothing saved");
        return Ok(false);
    }
    if io.interrupted() {
        return Err(INTERRUPTED.into());
    }
    if replacing {
        // boat replaces a name as soon as the new save is ready, so the real size is
        // measured under a name of its own before the old snapshot is touched.
        let temp = temp_name();
        io.say(&format!("saving {fork} as {temp} first, to measure it"));
        let measured = save(boat, fork, &temp, io).map(|s| fits(&temp, &s, &machine_type, io));
        if measured != Ok(true) {
            if measured == Ok(false) {
                io.say(&format!("{} left as it was", opts.name));
            }
            delete_saved(boat, &temp, io);
            return measured;
        }
        io.say(&format!("saving {fork} as {}", opts.name));
        let saved = save(boat, fork, &opts.name, io);
        delete_saved(boat, &temp, io);
        return saved.map(|s| fits(&opts.name, &s, &machine_type, io));
    }
    io.say(&format!("saving {fork} as {}", opts.name));
    let saved = save(boat, fork, &opts.name, io)?;
    Ok(fits(&opts.name, &saved, &machine_type, io))
}

/// A name no one else uses: the temporary prefix and 12 random hex digits.
fn temp_name() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{TEMP_PREFIX}{}", &id[..12])
}

/// Prints the size boat reports for the saved `name`; false when it is larger than
/// `machine_type` can start.
fn fits(name: &str, saved: &SavedSnapshot, machine_type: &str, io: &mut dyn Io) -> bool {
    let size = saved.size_bytes;
    io.say(&format!(
        "saved {name} ({})",
        size.map_or("size unknown".into(), gb)
    ));
    if let (Some(size), Some(budget)) = (size, boat::disk_budget_bytes(machine_type)) {
        if size > budget {
            io.say(&format!(
                "FAIL  the saved snapshot is larger than {machine_type}'s {} disk; \
                 boat will refuse to start it there (409 deploy_failed)",
                gb(budget)
            ));
            return false;
        }
    }
    true
}

fn delete_saved(boat: &Client, name: &str, io: &mut dyn Io) {
    match cleanup_retry(io, "delete saved snapshot", |_| {
        boat.delete_saved_snapshot(name)
    }) {
        Ok(()) => io.say(&format!("deleted saved snapshot {name}")),
        Err(e) => io.say(&format!(
            "could not delete saved snapshot {name}: {e}; delete it by hand"
        )),
    }
}

/// Sleeps; `Err` once a signal asked the command to stop.
fn pause(io: &mut dyn Io, d: Duration) -> Result<(), String> {
    io.sleep(d);
    if io.interrupted() {
        Err(INTERRUPTED.into())
    } else {
        Ok(())
    }
}

/// Calls `f` again after a refusal, an answerless failure (with the same idempotency
/// key, for a create), or a conflict boat calls temporary.
/// Stops early once a signal asked the command to stop.
fn retry<T>(
    io: &mut dyn Io,
    what: &str,
    f: impl FnMut(u32) -> Result<T, boat::Error>,
) -> Result<T, String> {
    retry_inner(io, what, true, f)
}

/// `retry` for cleaning up, which a signal does not stop.
fn cleanup_retry<T>(
    io: &mut dyn Io,
    what: &str,
    f: impl FnMut(u32) -> Result<T, boat::Error>,
) -> Result<T, String> {
    retry_inner(io, what, false, f)
}

fn retry_inner<T>(
    io: &mut dyn Io,
    what: &str,
    stoppable: bool,
    mut f: impl FnMut(u32) -> Result<T, boat::Error>,
) -> Result<T, String> {
    let mut attempt = 0;
    loop {
        if stoppable && io.interrupted() {
            return Err(format!("{what}: {INTERRUPTED}"));
        }
        let wait = match f(attempt) {
            Ok(v) => return Ok(v),
            Err(boat::Error::Refused { retry_after_s, .. }) => {
                Duration::from_secs(retry_after_s.min(300))
            }
            Err(boat::Error::Conflict { code, .. })
                if matches!(
                    code.as_str(),
                    "idempotency_in_progress" | "boat_starting" | "save_in_progress"
                ) =>
            {
                POLL
            }
            Err(e) if e.may_have_applied() => POLL,
            Err(e) => return Err(format!("{what}: {e}")),
        };
        attempt += 1;
        if attempt > RETRIES {
            return Err(format!("{what}: gave up after {RETRIES} retries"));
        }
        io.sleep(wait);
    }
}

/// Waits for the fork to run, then for every file of its snapshot to be on its disk, so
/// `du` measures what the saved snapshot will hold.
fn wait_until_hydrated(boat: &Client, fork: &str, io: &mut dyn Io) -> Result<Detail, String> {
    let mut polls = 0;
    let mut last_percent = None;
    loop {
        let d = retry(io, "read fork", |_| boat.detail(fork))?;
        match boat::machine_state(&d.state) {
            crate::model::MachineState::Gone => {
                return Err(format!("fork {fork} is {}", d.state));
            }
            crate::model::MachineState::Up => {
                // A missing `hydrated` is not taken as done: `du` would measure a
                // half-copied disk.
                if d.hydrated == Some(true) {
                    return Ok(d);
                }
                let percent = d.hydration.as_ref().and_then(|h| h.percent);
                if percent != last_percent {
                    let phase = d.hydration.as_ref().map_or("", |h| h.phase.as_str());
                    io.say(&format!(
                        "hydrating {phase} {}%",
                        percent.map_or("?".into(), |p| p.to_string())
                    ));
                    last_percent = percent;
                }
                if polls > READY_POLLS + HYDRATE_POLLS {
                    return Err(format!("fork {fork} did not finish hydrating"));
                }
            }
            _ if polls > READY_POLLS => {
                return Err(format!("fork {fork} still {} after 10 minutes", d.state));
            }
            _ => {}
        }
        polls += 1;
        pause(io, POLL)?;
    }
}

/// Runs `script` detached in the fork and polls until it exits; returns its stdout.
fn exec(boat: &Client, fork: &str, script: &str, io: &mut dyn Io) -> Result<String, String> {
    // Not retried on an answerless failure: the command may already be running.
    let pid = retry(io, "start command", |_| {
        boat.exec_start(fork, script).map_err(|e| match e {
            e if e.may_have_applied() => boat::Error::Other {
                status: 400,
                code: "exec_unknown".into(),
                message: e.to_string(),
            },
            e => e,
        })
    })?;
    for _ in 0..EXEC_POLLS {
        pause(io, POLL)?;
        let st = retry(io, "read command", |_| boat.command_status(fork, pid))?;
        if st.running {
            continue;
        }
        if st.exit_code != Some(0) {
            return Err(format!(
                "command in the fork ended {} (exit {:?}): {}",
                st.status,
                st.exit_code,
                st.stderr.trim()
            ));
        }
        return Ok(st.stdout);
    }
    Err("command in the fork did not finish in 30 minutes".into())
}

/// Saves and polls until boat settles the save it started from `fork`.
fn save(boat: &Client, fork: &str, name: &str, io: &mut dyn Io) -> Result<SavedSnapshot, String> {
    retry(io, "save snapshot", |attempt| {
        let r = boat.save_snapshot(fork, name);
        // A retry after an answerless failure: the first save may be running already.
        match r {
            Err(boat::Error::Conflict { code, .. })
                if attempt > 0 && code == "save_in_progress" =>
            {
                Ok(())
            }
            r => r,
        }
    })?;
    for _ in 0..SAVE_POLLS {
        pause(io, POLL)?;
        let list = retry(io, "list saved snapshots", |_| {
            boat.saved_snapshots(Some(name))
        })?;
        let Some(s) = list
            .into_iter()
            .find(|s| s.name == name && s.source_sandbox_id.as_deref() == Some(fork))
        else {
            continue;
        };
        match s.status.as_str() {
            "ready" => return Ok(s),
            "failed" => {
                return Err(format!(
                    "save failed: {}",
                    s.error.unwrap_or_else(|| "boat gave no reason".into())
                ));
            }
            _ => {}
        }
    }
    Err(format!("{name} still saving after 60 minutes"))
}

/// Read-only. Prints one `tag<TAB>value` line per finding.
const CHECK_SCRIPT: &str = r#"H=/home/user
R=$H/actions-runner
S=
if sudo -n true 2>/dev/null; then S='sudo -n'; fi
t() { printf '%s\t%s\n' "$1" "$2"; }
[ -d "$R" ] && t runner_dir "$R"
[ -f "$R/run.sh" ] && t run_sh "$R/run.sh"
for f in .runner .credentials .credentials_rsaparams; do [ -e "$R/$f" ] && t runner_config "$R/$f"; done
for l in /etc/systemd/system/*.wants/actions.runner.* $H/.config/systemd/user/*.wants/actions.runner.*; do [ -e "$l" ] || [ -L "$l" ] && t unit "$l"; done
for f in $H/.config/gh/hosts.yml $H/.git-credentials $H/.codex/auth.json $H/.claude/.credentials.json $H/.netrc $H/.aws/credentials $H/.config/gcloud/credentials.db $H/.config/gcloud/access_tokens.db $H/.config/gcloud/application_default_credentials.json $H/.config/gcloud/legacy_credentials/*/*; do [ -s "$f" ] && t cred "$f"; done
for f in $H/.ssh/id_*; do case "$f" in *.pub) ;; *) [ -f "$f" ] && t cred "$f" ;; esac; done
N=$H/.npmrc
[ -f "$N" ] && grep -Eq '_auth|_password' "$N" && t cred "$N"
D=$H/.docker/config.json
[ -f "$D" ] && tr -d ' \t\r\n' < "$D" | grep -Eq '"(auth|identitytoken)":"[^"]' && t cred "$D"
[ -f "$R/.env" ] && t env_file "$R/.env"
[ -f "$R/.env" ] && grep -E '^[A-Za-z0-9_]*(CACHE|TARGET_DIR|_HOME|STORE|BROWSERS_PATH)[A-Za-z0-9_]*=' "$R/.env" | while IFS= read -r l; do t env "$l"; done
v=$($S du -sxB1 "$H" 2>/dev/null | cut -f1); t home_bytes "${v:-0}"
v=$($S du -sxB1 "$H/.cache" 2>/dev/null | cut -f1); t cache_bytes "${v:-0}"
v=$($S du -sxcB1 "$H/.cache/ms-playwright" "$H/.cache/deno" 2>/dev/null | tail -n 1 | cut -f1); t cache_kept_bytes "${v:-0}"
v=$($S du -sxB1 /var/lib/docker 2>/dev/null | cut -f1); t docker_bytes "${v:-0}"
$S du -xB1 -d2 "$H" 2>/dev/null | sort -rn | head -n 15 | while IFS= read -r l; do t big "$l"; done
exit 0
"#;

/// What `CHECK_SCRIPT` found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    runner_dir: bool,
    run_sh: bool,
    runner_config: Vec<String>,
    units: Vec<String>,
    creds: Vec<String>,
    env_file: bool,
    env: Vec<(String, String)>,
    home_bytes: u64,
    cache_bytes: u64,
    cache_kept_bytes: u64,
    docker_bytes: u64,
    largest: Vec<(u64, String)>,
}

impl Report {
    pub(crate) fn parse(out: &str) -> Report {
        let mut r = Report::default();
        let num = |v: &str| v.trim().parse().unwrap_or(0);
        for line in out.lines() {
            let Some((tag, value)) = line.split_once('\t') else {
                continue;
            };
            match tag {
                "runner_dir" => r.runner_dir = true,
                "run_sh" => r.run_sh = true,
                "runner_config" => r.runner_config.push(value.into()),
                "unit" => r.units.push(value.into()),
                "cred" => r.creds.push(value.into()),
                "env_file" => r.env_file = true,
                "env" => {
                    if let Some((k, v)) = value.split_once('=') {
                        r.env.push((k.into(), unquote(v).into()));
                    }
                }
                "home_bytes" => r.home_bytes = num(value),
                "cache_bytes" => r.cache_bytes = num(value),
                "cache_kept_bytes" => r.cache_kept_bytes = num(value),
                "docker_bytes" => r.docker_bytes = num(value),
                "big" => {
                    if let Some((n, path)) = value.split_once('\t') {
                        r.largest.push((num(n), path.into()));
                    }
                }
                _ => {}
            }
        }
        r
    }

    /// What the saved snapshot will hold, roughly: `/home` without the `~/.cache` boat
    /// does not save, plus docker's data.
    fn saved_bytes(&self) -> u64 {
        self.home_bytes
            .saturating_sub(self.cache_bytes.saturating_sub(self.cache_kept_bytes))
            + self.docker_bytes
    }
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Check {
    pub status: Status,
    pub name: &'static str,
    pub detail: String,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.status {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        };
        write!(f, "{s:<5} {:<14} {}", self.name, self.detail)
    }
}

fn check(status: Status, name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        status,
        name,
        detail: detail.into(),
    }
}

pub(crate) fn evaluate(r: &Report, holds_logins: Option<bool>, machine_type: &str) -> Vec<Check> {
    use Status::*;
    let mut out = Vec::new();
    out.push(match holds_logins {
        Some(false) => check(Pass, "logins", "the noEnv fork holds no creator logins"),
        Some(true) => check(
            Fail,
            "logins",
            "boat says the fork holds its creator's logins (holdsCreatorLogins)",
        ),
        None => check(
            Warn,
            "logins",
            "boat did not say whether the fork holds creator logins",
        ),
    });
    out.push(if !r.runner_dir || !r.run_sh {
        check(
            Fail,
            "runner",
            format!("no extracted actions/runner with run.sh at {RUNNER}"),
        )
    } else if !r.runner_config.is_empty() {
        check(
            Fail,
            "runner",
            format!(
                "runner is configured: {} (--fix removes)",
                r.runner_config.join(", ")
            ),
        )
    } else {
        check(Pass, "runner", format!("unconfigured runner at {RUNNER}"))
    });
    out.push(if r.units.is_empty() {
        check(
            Pass,
            "runner-units",
            "no actions.runner.* systemd units enabled",
        )
    } else {
        check(
            Fail,
            "runner-units",
            format!(
                "enabled units would start old runners: {} (--fix disables)",
                r.units
                    .iter()
                    .map(|u| unit_name(u))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    });
    out.push(if r.creds.is_empty() {
        check(Pass, "credentials", "no credential files found")
    } else {
        check(
            Fail,
            "credentials",
            format!(
                "every job could read: {} (--fix removes after asking)",
                r.creds.join(", ")
            ),
        )
    });
    out.push(cache_check(r));
    out.push(disk_check(r, machine_type));
    out
}

fn cache_check(r: &Report) -> Check {
    use Status::*;
    let lost: Vec<String> = r
        .env
        .iter()
        .filter(|(_, v)| is_dropped(v))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if !lost.is_empty() {
        return check(
            Warn,
            "cache-dirs",
            format!(
                "snapshots and stops drop /tmp and ~/.cache: {}; use a directory such as \
                 {HOME}/ci-cache",
                lost.join(", ")
            ),
        );
    }
    if !r.env_file {
        return check(
            Warn,
            "cache-dirs",
            format!("no {RUNNER}/.env; tools cache under ~/.cache, which stops drop"),
        );
    }
    if !r.env.iter().any(|(k, _)| k == "XDG_CACHE_HOME") {
        return check(
            Warn,
            "cache-dirs",
            "XDG_CACHE_HOME is not set in the runner's .env; tools cache under ~/.cache, \
             which stops drop",
        );
    }
    check(
        Pass,
        "cache-dirs",
        "the runner's .env keeps caches out of ~/.cache and /tmp",
    )
}

/// A path boat does not save: `/tmp`, or `~/.cache` other than its playwright and deno
/// caches.
fn is_dropped(value: &str) -> bool {
    let home = |rest: &str| format!("{HOME}{rest}");
    let v = ["~", "$HOME", "${HOME}"]
        .iter()
        .find_map(|p| value.strip_prefix(p).map(home))
        .unwrap_or_else(|| value.to_string());
    let under = |dir: &str| v == dir || v.starts_with(&format!("{dir}/"));
    let cache = format!("{HOME}/.cache");
    under("/tmp")
        || (under(&cache)
            && !under(&format!("{cache}/ms-playwright"))
            && !under(&format!("{cache}/deno")))
}

fn disk_check(r: &Report, machine_type: &str) -> Check {
    use Status::*;
    let size = r.saved_bytes();
    let what = format!(
        "about {} to save ({} in /home, {} of it in ~/.cache, {} docker); every start that \
         is not a resume downloads it",
        gb(size),
        gb(r.home_bytes),
        gb(r.cache_bytes),
        gb(r.docker_bytes)
    );
    match boat::disk_budget_bytes(machine_type) {
        None => check(
            Warn,
            "disk",
            format!("{what}; unknown machine type {machine_type:?}"),
        ),
        Some(budget) if size > budget => check(
            Fail,
            "disk",
            format!("{what}; over {machine_type}'s {} disk", gb(budget)),
        ),
        Some(budget) if size > budget / 10 * 9 => check(
            Warn,
            "disk",
            format!("{what}; close to {machine_type}'s {} disk", gb(budget)),
        ),
        Some(budget) => check(
            Pass,
            "disk",
            format!("{what}; fits {machine_type}'s {}", gb(budget)),
        ),
    }
}

fn unit_name(link: &str) -> &str {
    link.rsplit('/').next().unwrap_or(link)
}

/// The fix commands for what the report found, or `None` when there is nothing to fix.
/// Credential files are listed and removed only after a yes.
fn fix_script(r: &Report, yes: bool, io: &mut dyn Io) -> Result<Option<String>, String> {
    let mut lines = Vec::new();
    for link in &r.units {
        let (unit, link) = (quote(unit_name(link))?, quote(link)?);
        if link.contains("/.config/systemd/user/") {
            lines.push(format!("rm -f {link}"));
        } else {
            lines.push(format!(
                "sudo -n systemctl disable --now {unit} || true; sudo -n rm -f {link}"
            ));
        }
    }
    for f in &r.runner_config {
        lines.push(format!("rm -f {}", quote(f)?));
    }
    if !r.creds.is_empty() {
        io.say("credential files in the fork (the source is not changed):");
        for f in &r.creds {
            io.say(&format!("  {f}"));
        }
        if yes || io.confirm("remove them from the fork before saving?") {
            for f in &r.creds {
                lines.push(format!("rm -f {}", quote(f)?));
            }
        }
    }
    if lines.is_empty() {
        return Ok(None);
    }
    Ok(Some(lines.join("\n") + "\nexit 0\n"))
}

/// Single-quotes an absolute path or a bare file name the check script printed. Anything
/// with a quote or a newline is refused rather than escaped.
fn quote(path: &str) -> Result<String, String> {
    if path.contains(['\'', '\n', '\0']) || !(path.starts_with('/') || !path.contains('/')) {
        return Err(format!("refusing to touch the odd path {path:?}"));
    }
    Ok(format!("'{path}'"))
}

/// Decimal GB, as boat prints them.
fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// One line per pool whose saved snapshot boat reports larger than the pool's
/// machine type can start.
pub(crate) fn oversized(pools: &[Pool], saved: &[SavedSnapshot]) -> Vec<String> {
    pools
        .iter()
        .filter_map(|p| {
            let size = saved.iter().find(|s| s.name == p.snapshot)?.size_bytes?;
            let budget = boat::disk_budget_bytes(&p.machine_type)?;
            (size > budget).then(|| {
                format!(
                    "pool {}: snapshot {} holds {}, more than {}'s {} disk; boat will refuse \
                     to start it (409 deploy_failed)",
                    p.name,
                    p.snapshot,
                    gb(size),
                    p.machine_type,
                    gb(budget)
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;
    use crate::transport::fake::{Fake, Sent, respond};
    use crate::transport::{NetworkError, Response};

    #[derive(Default)]
    struct TestIo {
        said: Vec<String>,
        answers: VecDeque<bool>,
        asked: Vec<String>,
        sleeps: u32,
        /// A signal arrives during this sleep.
        interrupt_at: Option<u32>,
    }

    impl Io for TestIo {
        fn sleep(&mut self, _: Duration) {
            self.sleeps += 1;
        }
        fn interrupted(&self) -> bool {
            self.interrupt_at.is_some_and(|n| self.sleeps >= n)
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn confirm(&mut self, q: &str) -> bool {
            self.asked.push(q.to_string());
            self.answers.pop_front().unwrap_or(false)
        }
    }

    const CLEAN: &str = "runner_dir\t/home/user/actions-runner\n\
        run_sh\t/home/user/actions-runner/run.sh\n\
        env_file\t/home/user/actions-runner/.env\n\
        env\tCARGO_TARGET_DIR=/home/user/ci-cache/target\n\
        env\tXDG_CACHE_HOME=\"/home/user/ci-cache/xdg\"\n\
        home_bytes\t16000000000\n\
        cache_bytes\t1000000000\n\
        cache_kept_bytes\t400000000\n\
        docker_bytes\t2000000000\n\
        big\t16000000000\t/home/user\n\
        big\t9000000000\t/home/user/ci-cache\n";

    fn dirty() -> String {
        format!(
            "{CLEAN}runner_config\t/home/user/actions-runner/.runner\n\
             unit\t/etc/systemd/system/multi-user.target.wants/actions.runner.o-r.box.service\n\
             cred\t/home/user/.config/gh/hosts.yml\n"
        )
    }

    fn opts(fix: bool, yes: bool) -> Options {
        Options {
            source: "bx_22222222".into(),
            name: "ci-2".into(),
            machine_type: Some("large".into()),
            fix,
            keep_fork: false,
            yes,
        }
    }

    /// A boat that forks `bx_44444444`, hydrates it on the third read, answers each
    /// check command from `outputs` in turn, and saves.
    struct Boat {
        outputs: RefCell<VecDeque<String>>,
        current: RefCell<String>,
        reads: RefCell<u32>,
        /// Saved snapshots: name to (source sandbox, size).
        saved: RefCell<BTreeMap<String, (String, Option<u64>)>>,
        holds: &'static str,
        size: u64,
        /// `hydrated` once running; `None` leaves the field out.
        hydrated: Option<&'static str>,
        /// Reads of a saved name, once saved, that first answer with a ready entry
        /// without a source.
        unsourced_reads: RefCell<u32>,
        fork_fails: bool,
    }

    fn boat(outputs: &[&str]) -> Boat {
        Boat {
            outputs: RefCell::new(outputs.iter().map(|s| s.to_string()).collect()),
            current: RefCell::new(String::new()),
            reads: RefCell::new(0),
            saved: RefCell::new(BTreeMap::new()),
            holds: "false",
            size: 15_900_000_000,
            hydrated: Some("true"),
            unsourced_reads: RefCell::new(0),
            fork_fails: false,
        }
    }

    impl Boat {
        fn with_existing(self) -> Boat {
            self.saved
                .borrow_mut()
                .insert("ci-2".into(), ("bx_33333333".into(), Some(10_000_000_000)));
            self
        }
    }

    fn reply(b: &Boat, s: &Sent) -> Result<Response, NetworkError> {
        let path = s.url.trim_start_matches(boat::BASE);
        let ok = |body: String| Ok(respond(200, &[], &body));
        match (s.method.as_str(), path) {
            ("POST", "/sandboxes/bx_22222222/fork") if b.fork_fails => {
                Ok(respond(500, &[], r#"{"ok":false,"error":"oops"}"#))
            }
            ("POST", "/sandboxes/bx_22222222/fork") => {
                ok(r#"{"ok":true,"sandbox":{"id":"bx_44444444","state":"provisioning"}}"#.into())
            }
            ("GET", p) if p.starts_with("/sandboxes?label=sw-boat-runner-snapshot-run%3D") => ok(
                r#"{"ok":true,"sandboxes":[{"id":"bx_44444444","state":"running"}],
                    "pageInfo":{"nextCursor":null,"hasMore":false}}"#
                    .into(),
            ),
            ("GET", "/sandboxes/bx_44444444") => {
                let n = {
                    let mut r = b.reads.borrow_mut();
                    *r += 1;
                    *r
                };
                let (state, hydrated) = match n {
                    1 => ("provisioning", Some("false")),
                    2 => ("running", Some("false")),
                    _ => ("running", b.hydrated),
                };
                let hydrated = hydrated.map_or(String::new(), |h| format!(r#""hydrated":{h},"#));
                ok(format!(
                    r#"{{"ok":true,"sandbox":{{"id":"bx_44444444","state":"{state}",
                        "type":"large","holdsCreatorLogins":{},{hydrated}
                        "hydration":{{"phase":"hydrating","percent":40}}}}}}"#,
                    b.holds
                ))
            }
            ("POST", "/sandboxes/bx_44444444/commands") => {
                let body: serde_json::Value =
                    serde_json::from_slice(s.body.as_deref().unwrap()).unwrap();
                let cmd = body["command"].as_str().unwrap();
                let out = if cmd.starts_with("H=/home/user") {
                    b.outputs.borrow_mut().pop_front().expect("an extra check")
                } else {
                    String::new()
                };
                *b.current.borrow_mut() = out;
                ok(r#"{"ok":true,"success":true,"processId":9,"pid":1,"command":"x","startedAt":"t"}"#.into())
            }
            ("GET", "/sandboxes/bx_44444444/commands/9") => ok(serde_json::json!({
                "ok": true, "success": true, "processId": 9, "status": "exited",
                "running": false, "exitCode": 0, "stdout": *b.current.borrow(), "stderr": ""
            })
            .to_string()),
            ("GET", p) if p.starts_with("/saved-snapshots?name=") => {
                let name = &p["/saved-snapshots?name=".len()..];
                let list = {
                    let mut unsourced = b.unsourced_reads.borrow_mut();
                    if *unsourced > 0 && b.saved.borrow().contains_key(name) {
                        *unsourced -= 1;
                        serde_json::json!([{"name": name, "status": "ready"}])
                    } else {
                        match b.saved.borrow().get(name) {
                            Some((src, size)) => serde_json::json!([{
                                "name": name, "status": "ready", "sourceSandboxId": src,
                                "sizeBytes": size, "createdAt": "t"
                            }]),
                            None => serde_json::json!([]),
                        }
                    }
                };
                ok(format!(r#"{{"ok":true,"snapshots":{list}}}"#))
            }
            ("POST", "/saved-snapshots") => {
                let body: serde_json::Value =
                    serde_json::from_slice(s.body.as_deref().unwrap()).unwrap();
                b.saved.borrow_mut().insert(
                    body["name"].as_str().unwrap().into(),
                    (body["sandboxId"].as_str().unwrap().into(), Some(b.size)),
                );
                Ok(respond(202, &[], r#"{"ok":true,"status":"saving"}"#))
            }
            ("DELETE", p) if p.starts_with("/saved-snapshots/") => {
                b.saved.borrow_mut().remove(&p["/saved-snapshots/".len()..]);
                ok(r#"{"ok":true,"status":"deleted"}"#.into())
            }
            ("DELETE", "/sandboxes/bx_44444444") => Ok(respond(202, &[], "{}")),
            other => panic!("unexpected call {other:?}"),
        }
    }

    fn drive(b: Boat, o: &Options, io: &mut TestIo) -> (Result<bool, String>, Vec<Sent>) {
        drive_rc(&Rc::new(b), o, io)
    }

    fn drive_rc(b: &Rc<Boat>, o: &Options, io: &mut TestIo) -> (Result<bool, String>, Vec<Sent>) {
        let b2 = Rc::clone(b);
        let fake = Fake::failing(move |s| reply(&b2, s));
        let r = snapshot(&fake, "k".into(), o, io);
        (r, fake.sent())
    }

    fn saved_source(b: &Boat, name: &str) -> Option<String> {
        b.saved.borrow().get(name).map(|(src, _)| src.clone())
    }

    fn calls(sent: &[Sent]) -> Vec<String> {
        sent.iter()
            .map(|s| format!("{} {}", s.method, s.url.trim_start_matches(boat::BASE)))
            .collect()
    }

    #[test]
    fn a_clean_sandbox_is_saved_and_the_fork_deleted() {
        let mut io = TestIo::default();
        let (r, sent) = drive(boat(&[CLEAN]), &opts(false, false), &mut io);
        assert_eq!(r, Ok(true), "{:#?}", io.said);
        let calls = calls(&sent);
        assert_eq!(calls[0], "POST /sandboxes/bx_22222222/fork");
        assert!(calls.contains(&"POST /saved-snapshots".to_string()));
        assert_eq!(calls.last().unwrap(), "DELETE /sandboxes/bx_44444444");
        // Nothing is sent to the source after the fork.
        assert!(calls[1..].iter().all(|c| !c.contains("bx_22222222")));
        let fork_body: serde_json::Value =
            serde_json::from_slice(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(fork_body["noEnv"], true);
        assert_eq!(fork_body["type"], "large");
        assert!(sent[0].header("idempotency-key").is_some());
        let save = sent
            .iter()
            .find(|s| s.url.ends_with("/saved-snapshots"))
            .unwrap();
        assert_eq!(
            save.body_text(),
            r#"{"name":"ci-2","sandboxId":"bx_44444444"}"#
        );
        assert!(io.said.iter().any(|l| l == "saved ci-2 (15.9 GB)"));
        assert!(io.said.iter().any(|l| l.starts_with("PASS  disk")));
        assert!(
            io.said
                .iter()
                .any(|l| l.contains("hydrating hydrating 40%"))
        );
        assert!(io.asked.is_empty());
    }

    #[test]
    fn failures_save_nothing() {
        let mut io = TestIo::default();
        let d = dirty();
        let (r, sent) = drive(boat(&[&d]), &opts(false, false), &mut io);
        assert_eq!(r, Ok(false));
        let calls = calls(&sent);
        assert!(!calls.iter().any(|c| c.contains("saved-snapshots")));
        assert_eq!(calls.last().unwrap(), "DELETE /sandboxes/bx_44444444");
        for name in ["runner", "runner-units", "credentials"] {
            assert!(
                io.said
                    .iter()
                    .any(|l| l.starts_with("FAIL") && l.contains(&format!(" {name} "))),
                "{name}: {:#?}",
                io.said
            );
        }
    }

    #[test]
    fn creator_logins_fail() {
        let mut io = TestIo::default();
        let mut b = boat(&[CLEAN]);
        b.holds = "true";
        let (r, _) = drive(b, &opts(false, false), &mut io);
        assert_eq!(r, Ok(false));
        assert!(io.said.iter().any(|l| l.starts_with("FAIL  logins")));
    }

    #[test]
    fn fix_asks_before_removing_credentials() {
        // Declined: units and runner files are fixed, the credential file stays.
        let mut io = TestIo::default();
        let after = format!("{CLEAN}cred\t/home/user/.config/gh/hosts.yml\n");
        let d = dirty();
        let (r, sent) = drive(boat(&[&d, &after]), &opts(true, false), &mut io);
        assert_eq!(r, Ok(false));
        assert_eq!(io.asked.len(), 1);
        let fix = sent
            .iter()
            .find(|s| s.url.ends_with("/commands") && !s.body_text().contains("H=/home/user"))
            .unwrap()
            .body_text();
        assert!(fix.contains("systemctl disable --now 'actions.runner.o-r.box.service'"));
        assert!(fix.contains("rm -f '/home/user/actions-runner/.runner'"));
        assert!(!fix.contains("hosts.yml"));

        // With --yes everything is fixed without a question, and the snapshot saved.
        let mut io = TestIo::default();
        let (r, sent) = drive(boat(&[&d, CLEAN]), &opts(true, true), &mut io);
        assert_eq!(r, Ok(true), "{:#?}", io.said);
        assert!(io.asked.is_empty());
        assert!(sent.iter().any(|s| {
            s.body_text()
                .contains("rm -f '/home/user/.config/gh/hosts.yml'")
        }));
    }

    #[test]
    fn replacing_a_name_needs_a_yes() {
        let mut io = TestIo::default();
        let b = boat(&[CLEAN]).with_existing();
        let (r, sent) = drive(b, &opts(false, false), &mut io);
        assert_eq!(r, Ok(false));
        assert_eq!(io.asked.len(), 1);
        assert!(!calls(&sent).contains(&"POST /saved-snapshots".to_string()));

        // A yes saves under a temporary name, then the real one, and removes the first.
        let mut io = TestIo {
            answers: VecDeque::from([true]),
            ..TestIo::default()
        };
        let b = Rc::new(boat(&[CLEAN]).with_existing());
        let (r, sent) = drive_rc(&b, &opts(false, false), &mut io);
        assert_eq!(r, Ok(true), "{:#?}", io.said);
        let names: Vec<String> = sent
            .iter()
            .filter(|s| s.method == "POST" && s.url.ends_with("/saved-snapshots"))
            .map(|s| {
                serde_json::from_str::<serde_json::Value>(&s.body_text()).unwrap()["name"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names[0].starts_with(TEMP_PREFIX) && valid_name(&names[0]));
        assert_eq!(names[1], "ci-2");
        assert!(calls(&sent).contains(&format!("DELETE /saved-snapshots/{}", names[0])));
        assert_eq!(saved_source(&b, "ci-2").as_deref(), Some("bx_44444444"));
        assert_eq!(b.saved.borrow().len(), 1);
    }

    #[test]
    fn an_oversized_replacement_leaves_the_old_snapshot() {
        let mut io = TestIo::default();
        let mut b = boat(&[CLEAN]).with_existing();
        b.size = 120_000_000_000;
        let b = Rc::new(b);
        let (r, sent) = drive_rc(&b, &opts(false, true), &mut io);
        assert_eq!(r, Ok(false));
        assert!(io.said.iter().any(|l| l.contains("deploy_failed")));
        assert!(io.said.iter().any(|l| l == "ci-2 left as it was"));
        // Only the temporary name was saved, and it is gone again.
        let saves = sent
            .iter()
            .filter(|s| s.method == "POST" && s.url.ends_with("/saved-snapshots"))
            .count();
        assert_eq!(saves, 1);
        assert!(
            !sent
                .iter()
                .any(|s| s.body_text().contains(r#""name":"ci-2""#))
        );
        assert_eq!(saved_source(&b, "ci-2").as_deref(), Some("bx_33333333"));
        assert_eq!(b.saved.borrow().len(), 1);
        assert_eq!(
            calls(&sent).last().unwrap(),
            "DELETE /sandboxes/bx_44444444"
        );
    }

    #[test]
    fn a_save_without_a_source_is_not_taken_for_ours() {
        let mut io = TestIo::default();
        let b = boat(&[CLEAN]);
        *b.unsourced_reads.borrow_mut() = 3;
        let (r, _) = drive(b, &opts(false, false), &mut io);
        assert_eq!(r, Ok(true));
        assert!(
            io.said.iter().any(|l| l == "saved ci-2 (15.9 GB)"),
            "{:#?}",
            io.said
        );
        assert!(!io.said.iter().any(|l| l.contains("size unknown")));
    }

    #[test]
    fn a_missing_hydrated_is_not_done() {
        let mut io = TestIo::default();
        let mut b = boat(&[CLEAN]);
        b.hydrated = None;
        let (r, sent) = drive(b, &opts(false, false), &mut io);
        assert!(r.unwrap_err().contains("did not finish hydrating"));
        let calls = calls(&sent);
        assert!(!calls.iter().any(|c| c.contains("/commands")));
        assert_eq!(calls.last().unwrap(), "DELETE /sandboxes/bx_44444444");
    }

    #[test]
    fn a_signal_deletes_the_fork() {
        let mut io = TestIo {
            interrupt_at: Some(1),
            ..TestIo::default()
        };
        let (r, sent) = drive(boat(&[CLEAN]), &opts(false, false), &mut io);
        assert_eq!(r, Err(INTERRUPTED.into()));
        let calls = calls(&sent);
        assert!(!calls.iter().any(|c| c.contains("/commands")));
        assert_eq!(calls.last().unwrap(), "DELETE /sandboxes/bx_44444444");
    }

    #[test]
    fn a_failed_fork_is_looked_up_by_label_and_deleted() {
        let mut io = TestIo::default();
        let mut b = boat(&[CLEAN]);
        b.fork_fails = true;
        let (r, sent) = drive(b, &opts(false, false), &mut io);
        assert!(r.unwrap_err().contains("gave up"));
        let calls = calls(&sent);
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("GET /sandboxes?label=sw-boat-runner-snapshot-run%3D"))
        );
        assert_eq!(calls.last().unwrap(), "DELETE /sandboxes/bx_44444444");
        assert!(
            io.said
                .iter()
                .any(|l| l == "deleted leftover fork bx_44444444")
        );
    }

    #[test]
    fn keep_fork_keeps_it() {
        let mut io = TestIo::default();
        let o = Options {
            keep_fork: true,
            ..opts(false, false)
        };
        let (r, sent) = drive(boat(&[CLEAN]), &o, &mut io);
        assert_eq!(r, Ok(true));
        assert!(!sent.iter().any(|s| s.method == "DELETE"));
    }

    #[test]
    fn an_oversized_save_is_reported() {
        let mut io = TestIo::default();
        let mut b = boat(&[CLEAN]);
        b.size = 120_000_000_000;
        assert_eq!(drive(b, &opts(false, false), &mut io).0, Ok(false));
        assert!(io.said.iter().any(|l| l.contains("deploy_failed")));
    }

    #[test]
    fn evaluates_cache_dirs() {
        let r = |env: &[(&str, &str)]| Report {
            env_file: true,
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Report::default()
        };
        let status = |rep: &Report| cache_check(rep).status;
        assert_eq!(
            status(&r(&[("XDG_CACHE_HOME", "/home/user/ci-cache")])),
            Status::Pass
        );
        assert_eq!(status(&r(&[("XDG_CACHE_HOME", "~/.cache")])), Status::Warn);
        assert_eq!(
            status(&r(&[
                ("XDG_CACHE_HOME", "/home/user/x"),
                ("CARGO_TARGET_DIR", "/tmp/target")
            ])),
            Status::Warn
        );
        assert_eq!(
            status(&r(&[
                ("XDG_CACHE_HOME", "/home/user/x"),
                ("PLAYWRIGHT_BROWSERS_PATH", "$HOME/.cache/ms-playwright")
            ])),
            Status::Pass
        );
        assert_eq!(status(&r(&[])), Status::Warn);
        assert!(!is_dropped("/home/user/.cachex"));
        assert!(is_dropped("/tmp"));
    }

    #[test]
    fn evaluates_disk() {
        let r = Report::parse(CLEAN);
        // 16 GB of home, less 0.6 GB of dropped cache, plus 2 GB of docker.
        assert_eq!(r.saved_bytes(), 17_400_000_000);
        assert_eq!(disk_check(&r, "small").status, Status::Pass);
        let big = Report {
            home_bytes: 60_000_000_000,
            ..r.clone()
        };
        assert_eq!(disk_check(&big, "default").status, Status::Fail);
        assert_eq!(disk_check(&big, "large").status, Status::Pass);
        let close = Report {
            home_bytes: 105_000_000_000,
            ..r.clone()
        };
        assert_eq!(disk_check(&close, "large").status, Status::Warn);
        assert_eq!(disk_check(&r, "").status, Status::Warn);
        assert_eq!(r.largest[1], (9_000_000_000, "/home/user/ci-cache".into()));
    }

    #[test]
    fn parses_args() {
        let o = parse_args(&["bx_22222222", "ci-2", "--machine-type", "large", "--yes"]).unwrap();
        assert_eq!(o.machine_type.as_deref(), Some("large"));
        assert!(o.yes && !o.fix && !o.keep_fork);
        let o = parse_args(&["--fix", "bx_22222222", "ci-2", "--keep-fork"]).unwrap();
        assert!(o.fix && o.keep_fork);
        assert!(parse_args(&["bx_22222222"]).is_err());
        assert!(parse_args(&["nope", "ci-2"]).is_err());
        assert!(parse_args(&["bx_22222222", "Bad_Name"]).is_err());
        assert!(parse_args(&["bx_22222222", "latest"]).is_err());
        assert!(parse_args(&["bx_22222222", "ci", "--machine-type", "huge"]).is_err());
        assert!(parse_args(&["bx_22222222", "ci", "--force"]).is_err());
    }

    #[test]
    fn quote_refuses_odd_paths() {
        assert_eq!(quote("/home/user/a b").unwrap(), "'/home/user/a b'");
        assert!(quote("/home/user/it's").is_err());
        assert!(quote("relative/path").is_err());
        assert_eq!(quote("a.service").unwrap(), "'a.service'");
    }

    #[test]
    fn warns_about_oversized_pool_snapshots() {
        let pool = |name: &str, t: &str, snap: &str| Pool {
            name: name.into(),
            labels: vec![],
            machine_type: t.into(),
            snapshot: snap.into(),
            placement: vec![],
            min_up: 0,
            max_boxes: 1,
            slots_per_box: 1,
            idle_stop_s: 0,
            min_up_s: 0,
            register_timeout_s: 0,
            fork_ttl_s: 60,
            paid_start_cents_per_day: 0,
        };
        let saved = |name: &str, size: Option<u64>| SavedSnapshot {
            name: name.into(),
            status: "ready".into(),
            error: None,
            source_sandbox_id: None,
            size_bytes: size,
        };
        let pools = [
            pool("a", "default", "big"),
            pool("b", "xlarge", "big"),
            pool("c", "default", "unsized"),
            pool("d", "default", "missing"),
        ];
        let got = oversized(
            &pools,
            &[saved("big", Some(92_800_000_000)), saved("unsized", None)],
        );
        assert_eq!(got.len(), 1);
        assert!(
            got[0].starts_with("pool a: snapshot big holds 92.8 GB"),
            "{got:?}"
        );
    }
}
