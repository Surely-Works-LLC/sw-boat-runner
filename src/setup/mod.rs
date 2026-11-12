//! `sw-boat-runner setup`: installs the daemon as a systemd service on the machine it runs
//! on, usually a boat `small` machine. It writes a policy, creates a GitHub App from a
//! manifest (or takes a personal token with `--pat`), stores the keys root-only and
//! enables the service.
//!
//! Secrets (the boat key, a personal token, the manifest code, the App's private key) are
//! read from the environment or a prompt without echo, never from argv, and are held in
//! `Secret`, which has no `Debug` or `Display`. They reach root-owned files through a
//! command's stdin, never its arguments.

use std::io::{BufRead, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::{API, Error};
use crate::github_app::App;
use crate::policy;
use crate::transport::{Transport, Ureq};

mod remote;

const DIR: &str = "/etc/sw-boat-runner";
const POLICY: &str = "/etc/sw-boat-runner/policy.toml";
const ENV: &str = "/etc/sw-boat-runner/env";
const KEY: &str = "/etc/sw-boat-runner/github-app.pem";
const BIN: &str = "/usr/local/bin/sw-boat-runner";
const UNIT: &str = "/etc/systemd/system/sw-boat-runner.service";
const SERVICE: &str = "sw-boat-runner";
const HOMEPAGE: &str = "https://github.com/Surely-Works-LLC/sw-boat-runner";
/// docs/new-app.html on GitHub Pages: it posts the manifest to GitHub, and GitHub sends
/// the browser back to it with the code to paste into setup.
const NEW_APP_PAGE: &str = "https://surely-works-llc.github.io/sw-boat-runner/new-app.html";
const DEFAULT_LABELS: &str = "self-hosted, boat";
/// The name offered for a snapshot setup builds when the account has none.
const DEFAULT_SNAPSHOT: &str = "sbr-runner";
/// How long `setup` waits for the new App to be installed on the repository.
const INSTALL_WAIT: Duration = Duration::from_secs(30 * 60);
const INSTALL_POLL: Duration = Duration::from_secs(5);

const HELP: &str = "\
usage: sw-boat-runner setup [--repo owner/name] [--labels a,b] [--snapshot name]
                            [--policy file] [--pat] [--new-app] [--offline-link]
                            [--dry-run] [--bin path]
       sw-boat-runner setup --remote [--repo, --labels, --snapshot, --policy, --pat,
                            --offline-link, --paste, --replace, --dry-run]

Installs sw-boat-runner as a systemd service on this machine (Ubuntu, sudo):

  1. writes a policy for the repository, labels and snapshot, and checks it
  2. reuses the GitHub App an earlier setup stored in /etc/sw-boat-runner/env, or creates
     a private one from a manifest (Administration write, Actions read, no webhook) and
     waits until it is installed on the repository
  3. asks for the boat key without echo
  4. writes /etc/sw-boat-runner/{policy.toml,env,github-app.pem}, installs the binary
     to /usr/local/bin and enables the sw-boat-runner service

  --repo, --labels, --snapshot   skip the prompt; or set SBR_REPO, SBR_LABELS, SBR_SNAPSHOT.
                                 The prompts offer defaults: the repository from `gh` or
                                 the origin remote here, labels self-hosted, boat, and (with
                                 BOAT_API_KEY set) your saved boat snapshots
  --policy FILE                  use this policy instead of the three prompts
  --pat                          a fine-grained personal token (GITHUB_TOKEN) instead of an
                                 App. Without --pat, setup ignores GITHUB_TOKEN
  --new-app                      create a new App even when /etc/sw-boat-runner/env names one
  --offline-link                 print the App's manifest page as a long data: URL instead
                                 of the short link to the project's GitHub Pages page
  --dry-run                      print what setup would do; asks no secret, changes nothing
  --bin PATH                     the binary to install (default: this one)
  --remote                       run from your laptop: create the App here (the browser
                                 comes back to a one-shot listener on 127.0.0.1), then a
                                 small always-on boat machine, and install the daemon on
                                 it over `boat ssh` (needs the boat CLI, logged in). It
                                 stops if a machine already runs the daemon for the
                                 repository. SBR_REF picks the release it installs
                                 (default: this binary's version)
  --paste                        with --remote: paste the code GitHub returns instead of
                                 waiting for the 127.0.0.1 redirect (a paste also works
                                 while it waits)
  --replace                      with --remote: delete the repository's existing daemon
                                 machine once the new machine's service is active

When the snapshot named is not among your saved boat snapshots (or you answer `new`),
setup offers to build one: a temporary noEnv machine gets the latest actions/runner,
unconfigured, and is checked and saved as `sw-boat-runner snapshot` does.

Secrets come from the environment or a prompt without echo, never from arguments:
BOAT_API_KEY, GITHUB_TOKEN (with --pat), SBR_APP_CODE (the code GitHub returns after the
App is created). An existing App: set GITHUB_APP_ID, GITHUB_APP_PRIVATE_KEY_FILE and,
optionally, GITHUB_APP_INSTALLATION_ID.";

/// A credential. It has no `Debug` or `Display`, so it cannot be printed by accident.
pub struct Secret(String);

impl Secret {
    fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    repo: Option<String>,
    labels: Option<String>,
    snapshot: Option<String>,
    policy: Option<PathBuf>,
    pat: bool,
    new_app: bool,
    offline_link: bool,
    dry_run: bool,
    bin: Option<PathBuf>,
    remote: bool,
    paste: bool,
    replace: bool,
}

fn parse_args(args: &[&str]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut it = args.iter();
    while let Some(&arg) = it.next() {
        let mut value = || {
            it.next()
                .map(|v| v.to_string())
                .ok_or(format!("{arg} needs a value"))
        };
        match arg {
            "--repo" => o.repo = Some(value()?),
            "--labels" => o.labels = Some(value()?),
            "--snapshot" => o.snapshot = Some(value()?),
            "--policy" => o.policy = Some(value()?.into()),
            "--bin" => o.bin = Some(value()?.into()),
            "--pat" => o.pat = true,
            "--new-app" => o.new_app = true,
            "--offline-link" => o.offline_link = true,
            "--dry-run" => o.dry_run = true,
            "--remote" => o.remote = true,
            "--paste" => o.paste = true,
            "--replace" => o.replace = true,
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    if o.policy.is_some() && (o.repo.is_some() || o.labels.is_some() || o.snapshot.is_some()) {
        return Err("--policy replaces --repo, --labels and --snapshot".into());
    }
    if o.remote && o.bin.is_some() {
        return Err(
            "--remote installs a release on the boat machine; --bin is for this one".into(),
        );
    }
    if o.paste && !o.remote {
        return Err("--paste goes with --remote".into());
    }
    if o.replace && !o.remote {
        return Err("--replace goes with --remote".into());
    }
    if o.paste && o.offline_link {
        return Err("--offline-link already pastes the code; drop --paste".into());
    }
    if o.pat && o.new_app {
        return Err("--new-app creates a GitHub App; --pat uses a token instead".into());
    }
    Ok(o)
}

pub fn cli(args: &[&str]) -> ExitCode {
    if matches!(args, ["-h" | "--help"]) {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("setup: {e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };
    match setup(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("setup: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The policy: the file `--policy` names, or one from the three prompts. The snapshot
/// prompt may build a snapshot, and leaves the boat key it asked for in `boat_key`.
fn choose_policy(opts: &Options, boat_key: &mut Option<Secret>) -> Result<String, String> {
    if let Some(path) = &opts.policy {
        return std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()));
    }
    let repo = value(
        &opts.repo,
        "SBR_REPO",
        "Repository (owner/name)",
        default_repo,
    )?;
    let labels = value(
        &opts.labels,
        "SBR_LABELS",
        "Runner labels, comma-separated (self-hosted is added)",
        || Some(DEFAULT_LABELS.into()),
    )?;
    let snapshot = choose_snapshot(opts, boat_key)?;
    Ok(policy_text(&repo, &split_labels(&labels), &snapshot))
}

/// The snapshot from `--snapshot`, `SBR_SNAPSHOT` or the prompt. When boat's list shows
/// no snapshot by that name, or the answer is `new`, it offers to build one.
fn choose_snapshot(opts: &Options, boat_key: &mut Option<Secret>) -> Result<String, String> {
    if boat_key.is_none() {
        *boat_key = given(&None, "BOAT_API_KEY").map(Secret);
    }
    let saved = boat_key.as_ref().and_then(|k| list_saved(k.expose()));
    let typed = match given(&opts.snapshot, "SBR_SNAPSHOT") {
        Some(s) => s,
        None => ask_snapshot(saved.as_deref())?,
    };
    let asked_new = typed == "new";
    let name = if asked_new {
        value(
            &None,
            "SBR_NEW_SNAPSHOT",
            "Name for the new snapshot",
            || Some(DEFAULT_SNAPSHOT.into()),
        )?
    } else {
        typed
    };
    let exists = saved
        .as_ref()
        .map(|list| list.iter().any(|s| s.name == name));
    match exists {
        Some(true) => {}
        // Without a list there is nothing to compare with: offer only when asked.
        None if !asked_new => {}
        _ => offer_build(&name, boat_key, opts.dry_run)?,
    }
    Ok(name)
}

/// Offers to build the missing snapshot `name` for the `default` machine type the policy
/// setup writes uses.
fn offer_build(name: &str, boat_key: &mut Option<Secret>, dry_run: bool) -> Result<(), String> {
    if !crate::snapshot::valid_name(name) {
        return Err(format!(
            "{name:?} is not a boat snapshot name: 1-63 lowercase letters, digits or dashes"
        ));
    }
    let question = format!(
        "Build a runner snapshot named {name} now? (creates a temporary noEnv machine, ~5 min)"
    );
    println!("No saved boat snapshot is called {name}.");
    if dry_run {
        println!(
            "A real run asks: {question}
"
        );
        return Ok(());
    }
    if !crate::snapshot::ask_yes(&question) {
        println!(
            "Not building it. docs/design.md#snapshots shows how to make one; doctor checks it.
"
        );
        return Ok(());
    }
    if boat_key.is_none() {
        *boat_key = Some(secret("BOAT_API_KEY", "boat API key")?);
    }
    let key = boat_key.as_ref().expect("set above");
    println!(
        "It holds the runner only. For toolchains, install them in a sandbox from it \
         (boat new --no-env --from {name}) and run `sw-boat-runner snapshot <id> {name}` again."
    );
    let t = Ureq::new(Duration::from_secs(60));
    let mut io = crate::snapshot::Terminal::new();
    if crate::snapshot::build(&t, key.expose(), name, "default", &mut io)? {
        println!();
        Ok(())
    } else {
        Err(format!("the snapshot {name} was not saved"))
    }
}

/// The account's saved snapshots, newest first; `None` when boat could not list them.
fn list_saved(key: &str) -> Option<Vec<crate::boat::SavedSnapshot>> {
    let t = Ureq::new(Duration::from_secs(30));
    match crate::boat::Client::new(&t, key.to_string()).saved_snapshots(None) {
        Ok(list) => Some(list),
        Err(e) => {
            eprintln!("Could not list saved boat snapshots ({e}); type the name.");
            None
        }
    }
}

fn setup(opts: &Options) -> Result<(), String> {
    if opts.remote {
        return remote::run(opts);
    }
    let mut boat_key = None;
    let policy_text = choose_policy(opts, &mut boat_key)?;
    let cfg = policy::parse(&policy_text).map_err(|e| format!("policy: {e}"))?;
    let bin = match &opts.bin {
        Some(b) => b.clone(),
        None => std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?,
    };
    println!("Policy:\n\n{policy_text}");
    show_check(&policy_text)?;

    let shell_app_id = std::env::var("GITHUB_APP_ID")
        .ok()
        .filter(|v| !v.is_empty());
    note_ignored_token(opts.pat);
    if opts.pat && shell_app_id.is_some() {
        return Err("set GITHUB_TOKEN or GITHUB_APP_*, not both".into());
    }
    if opts.new_app && shell_app_id.is_some() {
        return Err("--new-app creates a GitHub App: unset GITHUB_APP_ID".into());
    }
    let sudo = !is_root();
    // Only an App setup reuses what an earlier setup stored, and not with --new-app.
    let look_for_app = !opts.pat && !opts.new_app && shell_app_id.is_none();

    if opts.dry_run {
        let existing = if look_for_app {
            match read_root(ENV, sudo, false) {
                Ok(Some(text)) => Existing::Text(text),
                Ok(None) => Existing::Absent,
                Err(_) => Existing::Unreadable,
            }
        } else {
            Existing::Absent
        };
        let plan = (!opts.pat)
            .then(|| plan_app(opts.new_app, shell_app_id.as_deref(), &existing))
            .transpose()?;
        return dry_run(plan, &cfg.repo, &policy_text, &bin);
    }

    let t = Ureq::new(Duration::from_secs(30));
    let boat_key = match boat_key {
        Some(k) => k,
        None => secret("BOAT_API_KEY", "boat API key")?,
    };
    let auth = if opts.pat {
        let token = secret(
            "GITHUB_TOKEN",
            &format!(
                "GitHub fine-grained token for {} (Administration write, Actions read)",
                cfg.repo
            ),
        )?;
        Auth::Pat(token)
    } else {
        let existing = if look_for_app {
            match read_root(ENV, sudo, true)? {
                Some(text) => Existing::Text(text),
                None => Existing::Absent,
            }
        } else {
            Existing::Absent
        };
        match plan_app(opts.new_app, shell_app_id.as_deref(), &existing)? {
            AppPlan::Shell(id) => check_installed(&t, shell_app(id, sudo)?, &cfg.repo)?,
            AppPlan::Reuse {
                id,
                key_file,
                installation,
            } => {
                println!("Using GitHub App {id} from {ENV} (--new-app creates a new one).");
                let auth = app_from_file(id, &key_file, installation, sudo)
                    .map_err(|e| format!("{e}; pass --new-app to create a new App"))?;
                check_installed(&t, auth, &cfg.repo)?
            }
            AppPlan::Unknown => unreachable!("a real run reads {ENV} or fails"),
            AppPlan::Create => new_app(&t, &cfg.repo, opts.offline_link, sudo)?,
        }
    };
    for step in steps(&policy_text, &env_file(&boat_key, &auth)?, &auth, &bin) {
        println!("{}", step.describe());
        step.execute(sudo)?;
    }
    println!(
        "\nDone. `journalctl -u {SERVICE} -f` shows what the daemon does. Point jobs at the \
         pool with this line in the workflow:\n\n    {}\n",
        runs_on(&cfg.pools[0].labels)
    );
    crate::doctor::after_setup(BIN, POLICY, sudo);
    Ok(())
}

fn placeholder(name: &str) -> Secret {
    Secret(format!("<{name}>"))
}

/// Prints the steps with placeholders for the secrets, which it does not ask for. `plan`
/// is `None` with `--pat`.
fn dry_run(plan: Option<AppPlan>, repo: &str, policy_text: &str, bin: &Path) -> Result<(), String> {
    let auth = match plan {
        None => Auth::Pat(placeholder("GITHUB_TOKEN")),
        Some(plan) => {
            let (what, auth) = dry_run_app(plan, repo);
            println!("{what}\n");
            auth
        }
    };
    let env = env_text(&placeholder("BOAT_API_KEY"), &auth);
    let prefix = if is_root() { "" } else { "sudo " };
    println!("Would run, as root ({prefix}for each):\n");
    for step in steps(policy_text, &env, &auth, bin) {
        println!("{}", step.describe());
    }
    Ok(())
}

/// What a real run would do for the App, as a sentence, and the `Auth` it would end with.
fn dry_run_app(plan: AppPlan, repo: &str) -> (String, Auth) {
    let app = |id: String, installation, staged| Auth::App {
        id,
        pem: placeholder("private key"),
        installation,
        staged,
    };
    match plan {
        AppPlan::Shell(id) => (
            format!(
                "Would use GitHub App {id} from GITHUB_APP_ID, with the key at \
                 GITHUB_APP_PRIVATE_KEY_FILE."
            ),
            app(id, None, None),
        ),
        AppPlan::Reuse {
            id, installation, ..
        } => (
            format!("Would reuse GitHub App {id} from {ENV} (--new-app creates a new one)."),
            app(id, installation, None),
        ),
        AppPlan::Unknown => (
            format!(
                "Would reuse the GitHub App {ENV} names, if it names one (reading it needs \
                 sudo), or else create one as below. --new-app always creates one."
            ),
            app("<app id>".into(), None, None),
        ),
        AppPlan::Create => (
            format!(
                "Would create a private GitHub App for {repo} from this manifest (owned by \
                 the repository's owner), store its key at {}, print the link to install it, \
                 and wait until it is installed:\n\n{}",
                staged_key("<app id>"),
                serde_json::to_string_pretty(&manifest(repo, "<name>")).expect("JSON")
            ),
            app("<app id>".into(), None, Some(staged_key("<app id>"))),
        ),
    }
}

/// The line a workflow's job needs to run on the pool.
fn runs_on(labels: &[String]) -> String {
    format!("runs-on: [{}]", labels.join(", "))
}

/// Many shells export a GITHUB_TOKEN, and every boat machine has its creator's, so only
/// `--pat` takes it.
fn note_ignored_token(pat: bool) {
    if !pat && std::env::var("GITHUB_TOKEN").is_ok_and(|v| !v.trim().is_empty()) {
        println!("GITHUB_TOKEN is set; setup ignores it and uses a GitHub App (--pat uses it).");
    }
}

/// `flag`, else the environment variable `env`.
fn given(flag: &Option<String>, env: &str) -> Option<String> {
    flag.clone()
        .or_else(|| std::env::var(env).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `flag`, else the environment variable `env`, else a prompt that offers `default()`.
fn value(
    flag: &Option<String>,
    env: &str,
    prompt: &str,
    default: impl FnOnce() -> Option<String>,
) -> Result<String, String> {
    if let Some(v) = given(flag, env) {
        return Ok(v);
    }
    let default = default();
    let v = read_line(&with_default(prompt, default.as_deref()), false)?;
    accept(v, default).ok_or_else(|| format!("{prompt}: empty; pass it as a flag or set {env}"))
}

/// The prompt with its default in brackets.
fn with_default(prompt: &str, default: Option<&str>) -> String {
    match default {
        Some(d) => format!("{prompt} [{d}]"),
        None => prompt.to_string(),
    }
}

/// What was typed, or the default on Enter.
fn accept(typed: String, default: Option<String>) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        default.filter(|d| !d.is_empty())
    } else {
        Some(typed.to_string())
    }
}

/// The repository of the current directory: `gh repo view`, else the `origin` remote.
fn default_repo() -> Option<String> {
    let run = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    run(
        "gh",
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ],
    )
    .filter(|r| repo_ok(r))
    .or_else(|| run("git", &["remote", "get-url", "origin"]).and_then(|u| github_repo(&u)))
}

/// `owner/name` from a github.com remote URL (https, ssh or scp-like), else `None`.
fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let path = [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git@github.com:",
    ]
    .iter()
    .find_map(|p| url.strip_prefix(p))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let repo = format!("{owner}/{name}");
    repo_ok(&repo).then_some(repo)
}

/// Only a repository the policy takes, so a default never fails the check.
fn repo_ok(repo: &str) -> bool {
    policy::parse(&policy_text(repo, &["self-hosted".into()], "s")).is_ok()
}

/// The snapshot: a choice among the boat account's saved snapshots when boat listed
/// them (`saved`), else a plain prompt. `new` asks setup to build one.
fn ask_snapshot(saved: Option<&[crate::boat::SavedSnapshot]>) -> Result<String, String> {
    const PROMPT: &str = "boat snapshot";
    let Some(saved) = saved else {
        return value(&None, "SBR_SNAPSHOT", PROMPT, || None);
    };
    let names: Vec<String> = saved.iter().map(|s| s.name.clone()).collect();
    if names.is_empty() {
        eprintln!("You have no saved boat snapshots; setup can build one.");
        return value(&None, "SBR_SNAPSHOT", &format!("{PROMPT} to build"), || {
            Some(DEFAULT_SNAPSHOT.into())
        });
    }
    eprintln!("Saved boat snapshots, newest first:");
    for (i, s) in saved.iter().enumerate() {
        let size = s
            .size_bytes
            .map_or(String::new(), |b| format!(", {:.1} GB", b as f64 / 1e9));
        eprintln!("  {}. {} ({}{size})", i + 1, s.name, s.status);
    }
    let default = saved
        .iter()
        .find(|s| s.status == "ready")
        .map(|s| s.name.clone());
    let typed = read_line(
        &with_default(
            &format!("{PROMPT} (number, name, or new to build one)"),
            default.as_deref(),
        ),
        false,
    )?;
    pick_snapshot(&typed, &names, default)
}

/// A number from the list, a name, or the default on Enter.
fn pick_snapshot(typed: &str, names: &[String], default: Option<String>) -> Result<String, String> {
    let typed = accept(typed.to_string(), default)
        .ok_or("boat snapshot: empty; pass --snapshot or set SBR_SNAPSHOT")?;
    match typed.parse::<usize>() {
        Ok(n) if (1..=names.len()).contains(&n) => Ok(names[n - 1].clone()),
        Ok(n) if !names.contains(&typed) => Err(format!(
            "boat snapshot: {n} is not in the list (1 to {})",
            names.len()
        )),
        _ => Ok(typed),
    }
}

/// The environment variable `env`, else a prompt without echo.
fn secret(env: &str, prompt: &str) -> Result<Secret, String> {
    if let Some(v) = std::env::var(env).ok().filter(|v| !v.trim().is_empty()) {
        return Ok(Secret(v.trim().to_string()));
    }
    let v = read_line(&format!("{prompt} (not echoed)"), true)?;
    if v.is_empty() {
        return Err(format!("{env} is empty"));
    }
    Ok(Secret(v))
}

fn read_line(prompt: &str, hidden: bool) -> Result<String, String> {
    let stdin = std::io::stdin();
    if hidden && stdin.is_terminal() {
        // rpassword reads the terminal with echo off and handles Ctrl-C itself: it puts the
        // terminal back before it raises SIGINT, so an interrupt never leaves echo off.
        return rpassword::prompt_password(format!("{prompt}: "))
            .map(|v| v.trim().to_string())
            .map_err(|e| format!("{prompt}: {e}"));
    }
    eprint!("{prompt}: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match stdin.lock().read_line(&mut line) {
        Ok(0) => Err(format!("{prompt}: no input")),
        Ok(_) => Ok(line.trim().to_string()),
        Err(e) => Err(format!("{prompt}: {e}")),
    }
}

/// `check`'s own output for the policy, so setup shows what each default resolved to.
fn show_check(text: &str) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let mut child = Command::new(&me)
        .args(["check", "/dev/stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", me.display()))?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(text.as_bytes())
        .map_err(|e| format!("check: {e}"))?;
    let status = child.wait().map_err(|e| format!("check: {e}"))?;
    println!();
    if status.success() {
        Ok(())
    } else {
        Err("check failed".into())
    }
}

/// Labels from a comma- or space-separated list, with `self-hosted` first if missing.
fn split_labels(text: &str) -> Vec<String> {
    let mut labels: Vec<String> = text
        .split([',', ' '])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if !labels.iter().any(|l| l.eq_ignore_ascii_case("self-hosted")) {
        labels.insert(0, "self-hosted".into());
    }
    labels
}

/// The smallest policy: everything else takes its default (README.md#policy).
fn policy_text(repo: &str, labels: &[String], snapshot: &str) -> String {
    let quote = |s: &str| toml::Value::String(s.to_string()).to_string();
    let labels: Vec<String> = labels.iter().map(|l| quote(l)).collect();
    format!(
        "# Written by `sw-boat-runner setup`. README.md#policy lists the other keys.\n\
         repo = {}\nlabels = [{}]\nsnapshot = {}\n",
        quote(repo),
        labels.join(", "),
        quote(snapshot)
    )
}

pub enum Auth {
    Pat(Secret),
    App {
        id: String,
        pem: Secret,
        installation: Option<u64>,
        /// Where a new App's key waits until the last steps move it into place.
        staged: Option<String>,
    },
}

/// `/etc/sw-boat-runner/env` as an earlier setup left it.
enum Existing {
    Absent,
    /// It exists, but a dry run could not read it without a sudo password.
    Unreadable,
    Text(String),
}

/// Where the App comes from.
#[derive(Debug, PartialEq, Eq)]
enum AppPlan {
    /// `GITHUB_APP_ID` and `GITHUB_APP_PRIVATE_KEY_FILE` in setup's own environment.
    Shell(String),
    /// The App an earlier setup stored in `/etc/sw-boat-runner/env`.
    Reuse {
        id: String,
        key_file: String,
        installation: Option<u64>,
    },
    /// A dry run that could not read the env file.
    Unknown,
    Create,
}

/// The shell's App first, then (unless `--new-app`) the one the env file names, else a
/// new one. Re-running setup so keeps the App, its key and its installation.
fn plan_app(new_app: bool, shell_id: Option<&str>, existing: &Existing) -> Result<AppPlan, String> {
    if let Some(id) = shell_id {
        check_app_id("GITHUB_APP_ID", id)?;
        return Ok(AppPlan::Shell(id.to_string()));
    }
    if new_app {
        return Ok(AppPlan::Create);
    }
    let text = match existing {
        Existing::Absent => return Ok(AppPlan::Create),
        Existing::Unreadable => return Ok(AppPlan::Unknown),
        Existing::Text(text) => text,
    };
    let vars = parse_env(text);
    let get = |name: &str| {
        vars.iter()
            .find(|(k, v)| k == name && !v.is_empty())
            .map(|(_, v)| v.clone())
    };
    let Some(id) = get("GITHUB_APP_ID") else {
        return Ok(AppPlan::Create);
    };
    let hint = "; fix it or pass --new-app to create a new App";
    check_app_id(&format!("GITHUB_APP_ID in {ENV}"), &id).map_err(|e| format!("{e}{hint}"))?;
    let installation = get("GITHUB_APP_INSTALLATION_ID")
        .map(|v| v.parse::<u64>())
        .transpose()
        .map_err(|_| format!("GITHUB_APP_INSTALLATION_ID in {ENV} is not a number{hint}"))?;
    Ok(AppPlan::Reuse {
        id,
        key_file: get("GITHUB_APP_PRIVATE_KEY_FILE").unwrap_or_else(|| KEY.into()),
        installation,
    })
}

/// `NAME=value` lines; blank lines and `#` comments are skipped and a value's surrounding
/// quotes dropped, as systemd reads them.
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

/// An App's ID (digits) or client ID (`Iv` and letters, digits, dots). The daemon puts it
/// in the JWT and systemd reads it from the env file, so nothing else may pass.
fn check_app_id(what: &str, id: &str) -> Result<(), String> {
    let digits = !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit());
    let client = id.strip_prefix("Iv").is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
    });
    if digits || client {
        Ok(())
    } else {
        Err(format!(
            "{what} must be an App ID (digits) or a client ID (Iv...)"
        ))
    }
}

/// A root-only file, read with `sudo cat` when this user cannot read it; `None` when it
/// does not exist. Without `prompt`, sudo fails rather than ask for a password. Errors
/// never hold the content.
fn read_root(path: &str, sudo: bool, prompt: bool) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => return Ok(Some(text)),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) if !(sudo && e.kind() == ErrorKind::PermissionDenied) => {
            return Err(format!("{path}: {e}"));
        }
        Err(_) => {}
    }
    let mut cmd = Command::new("sudo");
    if !prompt {
        cmd.arg("-n");
    }
    let out = cmd
        .args(["cat", "--", path])
        .stdin(Stdio::inherit())
        .stderr(if prompt {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .output()
        .map_err(|e| format!("sudo: {e}"))?;
    if !out.status.success() {
        return Err(format!("sudo cat {path} failed ({})", out.status));
    }
    String::from_utf8(out.stdout)
        .map(Some)
        .map_err(|_| format!("{path}: not UTF-8"))
}

/// An App from `GITHUB_APP_ID` and the key at `GITHUB_APP_PRIVATE_KEY_FILE`.
fn shell_app(id: String, sudo: bool) -> Result<Auth, String> {
    let path = std::env::var("GITHUB_APP_PRIVATE_KEY_FILE")
        .map_err(|_| "GITHUB_APP_ID is set: set GITHUB_APP_PRIVATE_KEY_FILE too".to_string())?;
    let installation = match std::env::var("GITHUB_APP_INSTALLATION_ID") {
        Ok(v) if !v.is_empty() => Some(
            v.parse()
                .map_err(|_| "GITHUB_APP_INSTALLATION_ID must be a number".to_string())?,
        ),
        _ => None,
    };
    app_from_file(id, &path, installation, sudo)
}

/// An App whose key is at `path`, checked before anything is written.
fn app_from_file(
    id: String,
    path: &str,
    installation: Option<u64>,
    sudo: bool,
) -> Result<Auth, String> {
    let pem = read_root(path, sudo, true)?.ok_or_else(|| format!("{path}: not found"))?;
    App::new(id.clone(), None, pem.as_bytes()).map_err(|e| format!("{path}: {e}"))?;
    Ok(Auth::App {
        id,
        pem: Secret(pem),
        installation,
        staged: None,
    })
}

/// An App setup did not just create must be installed on the policy's repository: the
/// env file may come from a setup for another one. Before anything is written, so a
/// refusal leaves the service as it was. The installation becomes the one GitHub names
/// for the repository.
fn check_installed(t: &dyn Transport, auth: Auth, repo: &str) -> Result<Auth, String> {
    let Auth::App {
        id,
        pem,
        installation,
        staged,
    } = auth
    else {
        return Ok(auth);
    };
    let app = App::new(id.clone(), None, pem.expose().as_bytes())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let installation = match app.installation_on(t, repo, now) {
        Ok(Some(found)) => {
            if let Some(old) = installation.filter(|&old| old != found) {
                println!(
                    "GitHub App {id}'s installation on {repo} is {found}, not {old}; setup uses \
                     {found}."
                );
            }
            Some(found)
        }
        Ok(None) => {
            let which = installation.map_or(String::new(), |i| format!(" (installation {i})"));
            return Err(format!(
                "GitHub App {id}{which} is not installed on {repo}. Add {repo} to the app's \
                 installation (the account's Settings > Applications > Installed GitHub Apps > \
                 Configure) and run setup again, or pass --new-app to create an App for it. \
                 Nothing was changed."
            ));
        }
        // The daemon looks again when it starts, and doctor checks it.
        Err(Error::Network(e)) => {
            println!("Could not check GitHub App {id}'s installation on {repo} ({e}).");
            installation
        }
        Err(e) => {
            return Err(format!(
                "check GitHub App {id}'s installation on {repo}: {e}. Nothing was changed."
            ));
        }
    };
    Ok(Auth::App {
        id,
        pem,
        installation,
        staged,
    })
}

/// Where a new App's key waits, next to the key in use, until the last steps.
fn staged_key(id: &str) -> String {
    format!("{KEY}.{id}")
}

/// Creates an App and waits until it is installed. Until the last steps, the service
/// keeps the App and key it had: the new key waits at `staged_key`.
fn new_app(t: &dyn Transport, repo: &str, offline_link: bool, sudo: bool) -> Result<Auth, String> {
    let (id, pem, note) = create_app(t, repo, offline_link, sudo)?;
    let staged = staged_key(&id);
    let app = App::new(id.clone(), None, pem.expose().as_bytes())?;
    println!(
        "Waiting until the app is installed on {repo} (up to {} min)...",
        INSTALL_WAIT.as_secs() / 60
    );
    let installation = wait_installed(t, &app, repo, INSTALL_WAIT, &note, std::thread::sleep)
        .map_err(|e| {
            format!(
                "{e}\nNothing else changed: the service keeps its old settings. The new app's \
                 key is at {staged}; to use the app, install it and run setup again with \
                 GITHUB_APP_ID={id} GITHUB_APP_PRIVATE_KEY_FILE={staged}."
            )
        })?;
    println!("Installed (installation {installation}).");
    Ok(Auth::App {
        id,
        pem,
        installation: Some(installation),
        staged: Some(staged),
    })
}

/// The manifest flow: the user creates the App in a browser, pastes back the code GitHub
/// redirects with, and setup exchanges it for the App's id and private key. It stores
/// the key at once at `staged_key`, since GitHub shows it only once. The third value is
/// what the wait for the installation repeats, if anything.
fn create_app(
    t: &dyn Transport,
    repo: &str,
    offline_link: bool,
    sudo: bool,
) -> Result<(String, Secret, String), String> {
    let (owner, name) = repo.split_once('/').expect("a checked repo");
    let kind = owner_kind(t, owner)?;
    let state = uuid::Uuid::new_v4().simple().to_string();
    let app_name = app_name(name, &state);
    let link = if offline_link {
        let page = manifest_page(&kind.new_app_url(owner), &manifest(repo, &app_name), &state);
        data_url(&page)
    } else {
        pages_link(owner, &kind, name, &state, &app_name, None)
    };
    println!(
        "Create the GitHub App: open this link in your browser, check what the page lists \
         and press its button.\nGitHub then asks you to confirm the app.\n\n\
         {link}\n\n\
         GitHub then opens {NEW_APP_PAGE}?code=...; copy the whole address it shows\n\
         (if that page does not load, copy the address from the address bar).\n"
    );
    let pasted = secret("SBR_APP_CODE", "Paste the address (or just the code)")?;
    let code = parse_code(pasted.expose(), &state)?;
    let created = convert(t, &code)?;
    App::new(
        created.id.to_string(),
        None,
        created.pem.expose().as_bytes(),
    )?;
    println!("Created app {} (id {}).", created.slug, created.id);
    // Stored now, GitHub shows the key only once; not yet over the key in use.
    for step in [
        Step::Dir,
        Step::write(
            staged_key(&created.id.to_string()),
            "600",
            Content::Secret(&created.pem),
        ),
    ] {
        println!("{}", step.describe());
        step.execute(sudo)?;
    }
    let note = print_install_link(repo, &kind, &created.slug);
    Ok((created.id.to_string(), created.pem, note))
}

/// Prints where to install the new App; returns what the wait for the installation
/// repeats, if anything.
fn print_install_link(repo: &str, kind: &OwnerKind, slug: &str) -> String {
    let (owner, _) = repo.split_once('/').expect("a checked repo");
    let note = match kind {
        OwnerKind::Organization if org_owner(owner) != Some(true) => format!(
            "If you are not an owner of {owner}, GitHub shows a \"Request\" screen instead: \
             an owner of {owner} must approve the request before the app is installed."
        ),
        _ => String::new(),
    };
    println!(
        "\nAdministration write can change {repo}'s settings or delete it, so install the app \
         on {repo} only:\nchoose 'Only select repositories' -> {repo}.{}\n\n\
         https://github.com/apps/{slug}/installations/new\n",
        if note.is_empty() {
            String::new()
        } else {
            format!("\n{note}")
        },
    );
    note
}

/// The short link to docs/new-app.html, which builds the same manifest as `manifest`.
/// Every value is in the GitHub name charset or hex, so none needs escaping. `redirect`,
/// a loopback address `setup --remote` listens on, replaces the page as the manifest's
/// `redirect_url`; the page takes only one that `remote::is_loopback_redirect` takes.
fn pages_link(
    owner: &str,
    kind: &OwnerKind,
    repo_name: &str,
    state: &str,
    app: &str,
    redirect: Option<&str>,
) -> String {
    let kind = match kind {
        OwnerKind::User => "user",
        OwnerKind::Organization => "org",
    };
    let mut link = format!(
        "{NEW_APP_PAGE}#owner={owner}&kind={kind}&repo={repo_name}&state={state}&name={app}"
    );
    if let Some(r) = redirect {
        link.push_str("&redirect=");
        link.push_str(r);
    }
    link
}

/// Whether the `gh` user is an owner of the organization; `None` when `gh` cannot tell.
fn org_owner(org: &str) -> Option<bool> {
    let out = Command::new("gh")
        .args([
            "api",
            &format!("user/memberships/orgs/{org}"),
            "-q",
            ".role",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim() == "admin")
}

#[derive(Debug, PartialEq, Eq)]
enum OwnerKind {
    User,
    Organization,
}

impl OwnerKind {
    /// Where GitHub takes a manifest. A private App installs only on its owner's
    /// account, so an organization's repository needs an App the organization owns.
    fn new_app_url(&self, owner: &str) -> String {
        match self {
            OwnerKind::User => "https://github.com/settings/apps/new".into(),
            OwnerKind::Organization => {
                format!("https://github.com/organizations/{owner}/settings/apps/new")
            }
        }
    }
}

/// `GET /users/{owner}`, unauthenticated: whether the owner is a user or an organization.
fn owner_kind(t: &dyn Transport, owner: &str) -> Result<OwnerKind, String> {
    #[derive(Deserialize)]
    struct Out {
        #[serde(rename = "type")]
        kind: String,
    }
    let url = format!("{API}/users/{owner}");
    let resp = t
        .send("GET", &url, &headers(), None)
        .map_err(|e| format!("look up {owner}: {}", e.0))?;
    if resp.status != 200 {
        return Err(format!("look up {owner} on GitHub: status {}", resp.status));
    }
    let out: Out = serde_json::from_slice(&resp.body)
        .map_err(|_| format!("look up {owner}: response not understood"))?;
    Ok(match out.kind.as_str() {
        "Organization" => OwnerKind::Organization,
        _ => OwnerKind::User,
    })
}

fn headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Accept", "application/vnd.github+json"),
        ("X-GitHub-Api-Version", "2022-11-28"),
        ("User-Agent", "sw-boat-runner"),
    ]
}

/// App names are unique on GitHub and at most 34 characters; the suffix tells apart two
/// setups of one repository.
fn app_name(repo_name: &str, random: &str) -> String {
    // `sbr-sbr-e2e-…` reads badly; drop a leading `sbr-` the prefix already says.
    let trimmed = match repo_name.strip_prefix("sbr-") {
        Some(rest) if !rest.is_empty() => rest,
        _ => repo_name,
    };
    let base: String = trimmed.chars().take(21).collect();
    format!("sbr-{base}-{}", &random[..8])
}

/// A private App with no webhook and only the permissions the daemon uses:
/// Administration write to register runners, Actions read to see queued jobs.
fn manifest(repo: &str, name: &str) -> Value {
    json!({
        "name": name,
        "url": HOMEPAGE,
        "description": format!("sw-boat-runner for {repo}"),
        "hook_attributes": { "url": HOMEPAGE, "active": false },
        "redirect_url": NEW_APP_PAGE,
        "public": false,
        "default_permissions": { "administration": "write", "actions": "read" },
        "default_events": [],
    })
}

/// A page with one button that posts the manifest to GitHub, which accepts it only as a
/// form field.
fn manifest_page(action: &str, manifest: &Value, state: &str) -> String {
    let attr = |s: &str| {
        s.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let repo = manifest["description"].as_str().unwrap_or_default();
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Create the GitHub App</title>\
         <form method=\"post\" action=\"{}?state={}\">\
         <p>{}: a private GitHub App with Administration (write) and Actions (read) and no \
         webhook.</p>\
         <input type=\"hidden\" name=\"manifest\" value=\"{}\">\
         <button type=\"submit\">Create the GitHub App</button></form>",
        attr(action),
        attr(state),
        attr(repo),
        attr(&manifest.to_string()),
    )
}

/// The page as a `data:` URL, which a browser opens when it is pasted in the address bar.
fn data_url(html: &str) -> String {
    let mut url = String::from("data:text/html;charset=utf-8,");
    for b in html.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// The code from a pasted redirect address, or a pasted code. An address must carry the
/// `state` setup sent.
fn parse_code(pasted: &str, state: &str) -> Result<Secret, String> {
    let pasted = pasted.trim();
    let code = match pasted.split_once('?') {
        Some((_, query)) => {
            let query = query.split('#').next().unwrap_or_default();
            let param = |name: &str| {
                query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
            };
            match param("state") {
                Some(s) if s == state => {}
                Some(_) => {
                    return Err("the address is from another setup (its state differs)".into());
                }
                None => {
                    return Err(
                        "the address has no state; paste the whole address GitHub opened, \
                         or just the code"
                            .into(),
                    );
                }
            }
            param("code").ok_or("the address has no code")?
        }
        None => pasted,
    };
    // The code goes into a URL path.
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("that is not a code from GitHub".into());
    }
    Ok(Secret(code.to_string()))
}

struct Created {
    id: u64,
    slug: String,
    pem: Secret,
}

/// `POST /app-manifests/{code}/conversions`: the new App's id, slug and private key.
fn convert(t: &dyn Transport, code: &Secret) -> Result<Created, String> {
    #[derive(Deserialize)]
    struct Out {
        id: u64,
        slug: String,
        pem: String,
    }
    let url = format!("{API}/app-manifests/{}/conversions", code.expose());
    // Errors name the endpoint, never the URL, which holds the code.
    let resp = t
        .send("POST", &url, &headers(), None)
        .map_err(|_| "could not reach GitHub to exchange the code".to_string())?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "GitHub refused the code ({}); a code works once, within an hour of creating \
             the app",
            resp.status
        ));
    }
    // serde errors can quote the input, which holds the key.
    let out: Out = serde_json::from_slice(&resp.body)
        .map_err(|_| "the app-manifest conversion response was not understood".to_string())?;
    Ok(Created {
        id: out.id,
        slug: out.slug,
        pem: Secret(out.pem),
    })
}

/// Polls `GET /repos/{repo}/installation` until the App is installed there.
fn wait_installed(
    t: &dyn Transport,
    app: &App,
    repo: &str,
    limit: Duration,
    note: &str,
    mut sleep: impl FnMut(Duration),
) -> Result<u64, String> {
    let mut waited = Duration::ZERO;
    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let network = match app.installation_on(t, repo, now) {
            Ok(Some(id)) => return Ok(id),
            // 404: not installed yet.
            Ok(None) => None,
            // A dropped connection is worth another try; any answer but 404 is not.
            Err(Error::Network(e)) => Some(e),
            Err(Error::Status { status: 401, .. }) => {
                return Err(
                    "GitHub refused the app's JWT (401). Check this machine's clock \
                            (`timedatectl`): GitHub refuses a JWT from a clock more than a \
                            minute off."
                        .into(),
                );
            }
            Err(Error::Status {
                status: 403,
                message,
            }) => {
                return Err(format!(
                    "GitHub refused to show the app's installation on {repo} (403 {message})"
                ));
            }
            Err(e) => return Err(format!("look up the app's installation on {repo}: {e}")),
        };
        if waited >= limit {
            let last = network.map_or(String::new(), |e| format!(" (last try: {e})"));
            return Err(format!("the app is not installed on {repo}{last}"));
        }
        sleep(INSTALL_POLL);
        waited += INSTALL_POLL;
        if waited.as_secs() % 60 == 0 {
            println!(
                "Still waiting for the app to be installed on {repo} ({} of {} min)...{}",
                waited.as_secs() / 60,
                limit.as_secs() / 60,
                if note.is_empty() {
                    String::new()
                } else {
                    format!(" {note}")
                }
            );
        }
    }
}

/// `/etc/sw-boat-runner/env`, which systemd reads as root before it starts the service.
fn env_file(boat_key: &Secret, auth: &Auth) -> Result<Secret, String> {
    let mut values = vec![("BOAT_API_KEY", boat_key.expose())];
    match auth {
        Auth::Pat(token) => values.push(("GITHUB_TOKEN", token.expose())),
        Auth::App { id, .. } => {
            check_app_id("GITHUB_APP_ID", id)?;
            values.push(("GITHUB_APP_ID", id));
        }
    }
    for (name, value) in values {
        // systemd would read a quote, a backslash or a space as syntax.
        if value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\'))
        {
            return Err(format!("{name} holds a space, quote or backslash"));
        }
    }
    Ok(env_text(boat_key, auth))
}

fn env_text(boat_key: &Secret, auth: &Auth) -> Secret {
    let mut text = format!("BOAT_API_KEY={}\n", boat_key.expose());
    match auth {
        Auth::Pat(token) => text += &format!("GITHUB_TOKEN={}\n", token.expose()),
        Auth::App {
            id, installation, ..
        } => {
            text += &format!("GITHUB_APP_ID={id}\nGITHUB_APP_PRIVATE_KEY_FILE={KEY}\n");
            if let Some(i) = installation {
                text += &format!("GITHUB_APP_INSTALLATION_ID={i}\n");
            }
        }
    }
    Secret(text)
}

/// The unit install.sh wrote: the daemon only reads its policy and talks HTTPS, so it runs
/// as an unprivileged dynamic user that cannot write the file system or see home
/// directories. With an App, the dynamic user cannot read the root-only key, so systemd
/// hands the service a copy as a credential; `$$` is a literal `$` to systemd.
fn unit_file(app: bool) -> String {
    let run = format!("{BIN} run {POLICY}");
    let (credential, exec_start) = if app {
        (
            format!("LoadCredential=github-app.pem:{KEY}\n"),
            format!(
                "/bin/sh -c 'GITHUB_APP_PRIVATE_KEY_FILE=\"$${{CREDENTIALS_DIRECTORY}}/github-app.pem\" exec {run}'"
            ),
        )
    } else {
        (String::new(), run)
    };
    format!(
        "[Unit]
Description=sw-boat-runner
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile={ENV}
{credential}ExecStart={exec_start}
Restart=always
RestartSec=30
DynamicUser=yes
NoNewPrivileges=yes
CapabilityBoundingSet=
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes

[Install]
WantedBy=multi-user.target
"
    )
}

enum Content<'a> {
    Public(String),
    Secret(&'a Secret),
}

enum Step<'a> {
    Dir,
    Write {
        path: String,
        mode: &'static str,
        content: Content<'a>,
    },
    /// A new App's key, from where it waited to where the service reads it.
    MoveKey(String),
    Install(PathBuf),
    Run {
        args: &'static [&'static str],
        must_pass: bool,
    },
}

impl<'a> Step<'a> {
    fn write(path: impl Into<String>, mode: &'static str, content: Content<'a>) -> Self {
        Step::Write {
            path: path.into(),
            mode,
            content,
        }
    }

    /// One line, or a file's text when it holds no secret. A secret file shows the
    /// names of its variables only.
    fn describe(&self) -> String {
        match self {
            Step::Dir => format!("+ install -d -m 755 {DIR}"),
            Step::Write {
                path,
                mode,
                content: Content::Public(text),
            } => format!("+ write {path} (mode {mode}):\n{text}"),
            Step::Write {
                path,
                mode,
                content: Content::Secret(s),
            } => {
                let names: Vec<&str> = s
                    .expose()
                    .lines()
                    .filter_map(|l| l.split_once('=').map(|(k, _)| k))
                    .filter(|k| k.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                    .collect();
                let what = if names.is_empty() {
                    "secret".to_string()
                } else {
                    format!("{}, values hidden", names.join(", "))
                };
                format!("+ write {path} (mode {mode}, root only): {what}")
            }
            Step::MoveKey(from) => format!("+ mv -f {from} {KEY}"),
            Step::Install(from) => format!("+ install -m 755 {} {BIN}", from.display()),
            Step::Run { args, .. } => format!("+ {}", args.join(" ")),
        }
    }

    fn execute(&self, sudo: bool) -> Result<(), String> {
        let (args, input, must_pass): (Vec<String>, Option<&str>, bool) = match self {
            Step::Dir => (strings(&["install", "-d", "-m", "755", DIR]), None, true),
            Step::Write {
                path,
                mode,
                content,
            } => {
                // A temporary file under umask 077, renamed over the old one, so the file
                // is never readable by others even for a moment. The content comes on
                // stdin, never in the arguments.
                let script = "umask 077; t=$(mktemp \"$1.XXXXXX\") && cat > \"$t\" && \
                              chmod \"$2\" \"$t\" && mv -f \"$t\" \"$1\"";
                let text = match content {
                    Content::Public(text) => text.as_str(),
                    Content::Secret(s) => s.expose(),
                };
                (
                    strings(&["sh", "-c", script, "sh", path, mode]),
                    Some(text),
                    true,
                )
            }
            // A rename in one directory: the service sees the old key or the new one.
            Step::MoveKey(from) => (strings(&["mv", "-f", from, KEY]), None, true),
            Step::Install(from) => (
                vec![
                    "install".into(),
                    "-m".into(),
                    "755".into(),
                    from.display().to_string(),
                    BIN.into(),
                ],
                None,
                true,
            ),
            Step::Run { args, must_pass } => (strings(args), None, *must_pass),
        };
        let mut cmd = if sudo {
            let mut c = Command::new("sudo");
            c.args(&args);
            c
        } else {
            let mut c = Command::new(&args[0]);
            c.args(&args[1..]);
            c
        };
        cmd.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", args[0]))?;
        if let Some(text) = input {
            child
                .stdin
                .take()
                .expect("piped")
                .write_all(text.as_bytes())
                .map_err(|e| format!("{}: {e}", args[0]))?;
        }
        let status = child.wait().map_err(|e| format!("{}: {e}", args[0]))?;
        if must_pass && !status.success() {
            return Err(format!("{} failed ({status})", self.describe_short()));
        }
        Ok(())
    }

    fn describe_short(&self) -> String {
        self.describe()
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Everything setup does as root, in order. The policy holds no secret, and the service's
/// dynamic user must read it; the keys are root-only.
fn steps<'a>(policy_text: &str, env: &'a Secret, auth: &'a Auth, bin: &Path) -> Vec<Step<'a>> {
    let app = matches!(auth, Auth::App { .. });
    let mut steps = vec![
        Step::Dir,
        Step::write(POLICY, "644", Content::Public(policy_text.to_string())),
        Step::write(ENV, "600", Content::Secret(env)),
    ];
    // The env and the key together, and before the restart.
    match auth {
        Auth::App {
            staged: Some(staged),
            ..
        } => steps.push(Step::MoveKey(staged.clone())),
        Auth::App { pem, .. } => steps.push(Step::write(KEY, "600", Content::Secret(pem))),
        Auth::Pat(_) => {}
    }
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same(bin, Path::new(BIN)) {
        steps.push(Step::Install(bin.to_path_buf()));
    }
    steps.extend([
        Step::write(UNIT, "644", Content::Public(unit_file(app))),
        Step::Run {
            args: &["systemctl", "daemon-reload"],
            must_pass: true,
        },
        Step::Run {
            args: &["systemctl", "enable", SERVICE],
            must_pass: true,
        },
        // A restart, so running setup again applies the new files.
        Step::Run {
            args: &["systemctl", "restart", SERVICE],
            must_pass: true,
        },
        Step::Run {
            args: &["systemctl", "--no-pager", "status", SERVICE],
            must_pass: false,
        },
    ]);
    steps
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github_app::tests::test_pem;
    use crate::transport::fake::{Fake, respond};

    #[test]
    fn parses_flags() {
        let o = parse_args(&["--repo", "a/b", "--labels", "x,y", "--pat", "--dry-run"]).unwrap();
        assert_eq!(o.repo.as_deref(), Some("a/b"));
        assert_eq!(o.labels.as_deref(), Some("x,y"));
        assert!(o.pat && o.dry_run);
        assert_eq!(parse_args(&[]).unwrap(), Options::default());
        assert!(
            parse_args(&["--repo"])
                .unwrap_err()
                .contains("needs a value")
        );
        assert!(
            parse_args(&["--token", "x"])
                .unwrap_err()
                .contains("unknown")
        );
        assert!(parse_args(&["--policy", "p", "--repo", "a/b"]).is_err());
        assert!(parse_args(&["--new-app"]).unwrap().new_app);
        assert!(parse_args(&["--new-app", "--pat"]).is_err());
        let o = parse_args(&["--remote", "--paste", "--dry-run"]).unwrap();
        assert!(o.remote && o.paste && o.dry_run);
        assert!(parse_args(&["--remote", "--bin", "/x"]).is_err());
        assert!(parse_args(&["--paste"]).is_err());
        assert!(parse_args(&["--remote", "--paste", "--offline-link"]).is_err());
        assert!(parse_args(&["--remote", "--replace"]).unwrap().replace);
        assert!(parse_args(&["--replace"]).is_err());
    }

    #[test]
    fn writes_a_minimal_policy_that_checks() {
        let labels = split_labels("boat, linux");
        assert_eq!(labels, ["self-hosted", "boat", "linux"]);
        assert_eq!(split_labels("Self-Hosted x"), ["Self-Hosted", "x"]);
        let text = policy_text("acme/app", &labels, "snap \"1\"");
        assert_eq!(
            text,
            "# Written by `sw-boat-runner setup`. README.md#policy lists the other keys.\n\
             repo = \"acme/app\"\nlabels = [\"self-hosted\", \"boat\", \"linux\"]\n\
             snapshot = 'snap \"1\"'\n"
        );
        let cfg = policy::parse(&text).unwrap();
        assert_eq!(cfg.pools[0].name, "boat_linux");
        assert_eq!(cfg.pools[0].snapshot, "snap \"1\"");
    }

    #[test]
    fn the_manifest_asks_for_two_permissions_and_no_webhook() {
        let m = manifest("acme/app", "sbr-app-0123abcd");
        assert_eq!(m["public"], false);
        assert_eq!(m["hook_attributes"]["active"], false);
        assert_eq!(
            m["default_permissions"],
            json!({"administration": "write", "actions": "read"})
        );
        assert_eq!(m["default_events"], json!([]));
        assert_eq!(m["redirect_url"], NEW_APP_PAGE);
        let name = app_name(&"n".repeat(40), "0123abcdef");
        assert!(name.len() <= 34 && name.ends_with("-0123abcd"), "{name}");
        assert_eq!(app_name("sbr-e2e", "0123abcdef"), "sbr-e2e-0123abcd");
        assert_eq!(app_name("sbr-", "0123abcdef"), "sbr-sbr--0123abcd");
    }

    #[test]
    fn the_manifest_page_posts_to_the_owners_settings() {
        let org = OwnerKind::Organization.new_app_url("acme");
        assert_eq!(
            org,
            "https://github.com/organizations/acme/settings/apps/new"
        );
        assert_eq!(
            OwnerKind::User.new_app_url("me"),
            "https://github.com/settings/apps/new"
        );
        let page = manifest_page(&org, &manifest("acme/app", "n"), "st4te");
        assert!(page.contains(
            "action=\"https://github.com/organizations/acme/settings/apps/new?state=st4te\""
        ));
        assert!(page.contains("name=\"manifest\" value=\"{&quot;default_events&quot;"));
        assert!(!page.contains("value=\"{\""));
        let url = data_url("<a href=\"x\">#&</a>");
        assert_eq!(
            url,
            "data:text/html;charset=utf-8,%3Ca%20href%3D%22x%22%3E%23%26%3C%2Fa%3E"
        );
    }

    #[test]
    fn finds_the_owners_kind() {
        let fake = Fake::new(|s| match s.url.as_str() {
            "https://api.github.com/users/acme" => respond(200, &[], r#"{"type":"Organization"}"#),
            "https://api.github.com/users/me" => respond(200, &[], r#"{"type":"User"}"#),
            _ => respond(404, &[], "{}"),
        });
        assert_eq!(owner_kind(&fake, "acme").unwrap(), OwnerKind::Organization);
        assert_eq!(owner_kind(&fake, "me").unwrap(), OwnerKind::User);
        assert!(owner_kind(&fake, "nobody").unwrap_err().contains("404"));
        assert!(
            fake.sent()
                .iter()
                .all(|s| s.header("authorization").is_none())
        );
    }

    #[test]
    fn takes_the_code_from_an_address_or_alone() {
        let code = |s: &str| parse_code(s, "st").map(|c| c.expose().to_string());
        assert_eq!(code("abc123").unwrap(), "abc123");
        assert_eq!(
            code(&format!("{NEW_APP_PAGE}?code=abc123&state=st")).unwrap(),
            "abc123"
        );
        assert_eq!(
            code(&format!(" {NEW_APP_PAGE}?state=st&code=a_1#x ")).unwrap(),
            "a_1"
        );
        assert!(
            code(&format!("{NEW_APP_PAGE}?code=abc"))
                .unwrap_err()
                .contains("no state")
        );
        assert!(
            code(&format!("{NEW_APP_PAGE}?code=abc&state=other"))
                .unwrap_err()
                .contains("state")
        );
        assert!(code(&format!("{NEW_APP_PAGE}?state=st")).is_err());
        assert!(code("../x").is_err());
        assert!(code("").is_err());
    }

    #[test]
    fn exchanges_the_code_for_the_apps_key() {
        let pem = test_pem();
        let body = json!({"id": 42, "slug": "sbr-app", "pem": pem, "client_secret": "cs"});
        let fake = Fake::new(move |s| match (s.method.as_str(), s.url.as_str()) {
            ("POST", "https://api.github.com/app-manifests/abc/conversions") => {
                respond(201, &[], &body.to_string())
            }
            _ => respond(404, &[], r#"{"message":"Not Found"}"#),
        });
        let created = convert(&fake, &Secret("abc".into())).unwrap();
        assert_eq!((created.id, created.slug.as_str()), (42, "sbr-app"));
        assert_eq!(created.pem.expose(), test_pem());
        assert!(fake.sent()[0].body.is_none());

        let err = convert(&fake, &Secret("zq9".into())).err().unwrap();
        assert!(err.contains("404") && !err.contains("zq9"), "{err}");
        let fake = Fake::new(|_| respond(201, &[], r#"{"id":1,"pem":"-----BEGIN secret"}"#));
        let err = convert(&fake, &Secret("abc".into())).err().unwrap();
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn waits_until_the_app_is_installed() {
        let calls = std::cell::Cell::new(0);
        let fake = Fake::new(move |_| {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                respond(404, &[], r#"{"message":"Not Found"}"#)
            } else {
                respond(200, &[], r#"{"id":77}"#)
            }
        });
        let app = App::new("42".into(), None, test_pem().as_bytes()).unwrap();
        let mut slept = Vec::new();
        let id = wait_installed(&fake, &app, "o/r", INSTALL_WAIT, "", |d| slept.push(d)).unwrap();
        assert_eq!(id, 77);
        assert_eq!(slept, [INSTALL_POLL, INSTALL_POLL]);
        assert!(
            fake.sent()
                .iter()
                .all(|s| s.url.ends_with("/repos/o/r/installation"))
        );

        let never = Fake::new(|_| respond(404, &[], r#"{"message":"Not Found"}"#));
        let err = wait_installed(&never, &app, "o/r", INSTALL_POLL * 2, "", |_| {}).unwrap_err();
        assert!(err.contains("not installed on o/r"), "{err}");
        assert_eq!(never.sent().len(), 3);

        // A dropped connection is tried again.
        let calls = std::cell::Cell::new(0);
        let flaky = Fake::failing(move |_| {
            calls.set(calls.get() + 1);
            if calls.get() < 2 {
                Err(crate::transport::NetworkError("reset".into()))
            } else {
                Ok(respond(200, &[], r#"{"id":5}"#))
            }
        });
        assert_eq!(
            wait_installed(&flaky, &app, "o/r", INSTALL_WAIT, "", |_| {}).unwrap(),
            5
        );
    }

    #[test]
    fn the_wait_stops_at_once_on_any_answer_but_404() {
        let app = App::new("42".into(), None, test_pem().as_bytes()).unwrap();
        for (status, expect) in [
            (401, "clock"),
            (403, "403 Resource not accessible"),
            (500, "500"),
        ] {
            let fake = Fake::new(move |_| {
                respond(status, &[], r#"{"message":"Resource not accessible"}"#)
            });
            let err = wait_installed(&fake, &app, "o/r", INSTALL_WAIT, "", |_| {
                panic!("slept after {status}")
            })
            .unwrap_err();
            assert!(err.contains(expect), "{status}: {err}");
            assert_eq!(fake.sent().len(), 1);
        }
    }

    #[test]
    fn a_reused_app_must_be_installed_on_the_repository() {
        let app = |installation| Auth::App {
            id: "42".into(),
            pem: Secret(test_pem()),
            installation,
            staged: None,
        };
        let installation = |auth: Auth| match auth {
            Auth::App { installation, .. } => installation,
            Auth::Pat(_) => panic!("an App"),
        };
        let on = |status: u16, body: &'static str| {
            Fake::new(move |s| {
                assert!(s.url.ends_with("/repos/o/new/installation"), "{}", s.url);
                respond(status, &[], body)
            })
        };
        // The stored installation covers the repository.
        let auth = check_installed(&on(200, r#"{"id":77}"#), app(Some(77)), "o/new").unwrap();
        assert_eq!(installation(auth), Some(77));
        // Another owner's installation: GitHub's answer wins.
        let auth = check_installed(&on(200, r#"{"id":88}"#), app(Some(77)), "o/new").unwrap();
        assert_eq!(installation(auth), Some(88));
        let auth = check_installed(&on(200, r#"{"id":88}"#), app(None), "o/new").unwrap();
        assert_eq!(installation(auth), Some(88));
        // Not installed there: refused before anything is written.
        let err = check_installed(
            &on(404, r#"{"message":"Not Found"}"#),
            app(Some(77)),
            "o/new",
        )
        .err()
        .unwrap();
        assert!(
            err.contains("GitHub App 42 (installation 77) is not installed on o/new")
                && err.contains("--new-app")
                && err.contains("Nothing was changed"),
            "{err}"
        );
        assert!(check_installed(&on(401, "{}"), app(Some(77)), "o/new").is_err());
        // A dropped connection keeps what the env file says.
        let down = Fake::failing(|_| Err(crate::transport::NetworkError("reset".into())));
        let auth = check_installed(&down, app(Some(77)), "o/new").unwrap();
        assert_eq!(installation(auth), Some(77));
        // A token has no installation to check.
        let fake = on(500, "{}");
        assert!(check_installed(&fake, Auth::Pat(Secret("t".into())), "o/new").is_ok());
        assert!(fake.sent().is_empty());
    }

    #[test]
    fn reads_an_env_file_as_systemd_does() {
        let vars = parse_env("# c\n\nA=1\n B = \"two\" \nC='3'\nnoequals\nD=\n");
        let pairs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (&**k, &**v)).collect();
        assert_eq!(pairs, [("A", "1"), ("B", "two"), ("C", "3"), ("D", "")]);
    }

    #[test]
    fn a_second_setup_reuses_the_app_in_the_env_file() {
        let env = |text: &str| Existing::Text(text.into());
        let ours = env("BOAT_API_KEY=bk\nGITHUB_APP_ID=42\n\
             GITHUB_APP_PRIVATE_KEY_FILE=/etc/sw-boat-runner/github-app.pem\n\
             GITHUB_APP_INSTALLATION_ID=77\n");
        let reuse = AppPlan::Reuse {
            id: "42".into(),
            key_file: KEY.into(),
            installation: Some(77),
        };
        assert_eq!(plan_app(false, None, &ours).unwrap(), reuse);
        assert_eq!(plan_app(true, None, &ours).unwrap(), AppPlan::Create);
        assert_eq!(
            plan_app(false, Some("7"), &ours).unwrap(),
            AppPlan::Shell("7".into())
        );
        assert_eq!(
            plan_app(false, None, &env("GITHUB_APP_ID=Iv1.ab\n")).unwrap(),
            AppPlan::Reuse {
                id: "Iv1.ab".into(),
                key_file: KEY.into(),
                installation: None,
            }
        );
        let pat = env("BOAT_API_KEY=bk\nGITHUB_TOKEN=t\n");
        assert_eq!(plan_app(false, None, &pat).unwrap(), AppPlan::Create);
        assert_eq!(
            plan_app(false, None, &Existing::Absent).unwrap(),
            AppPlan::Create
        );
        assert_eq!(
            plan_app(false, None, &Existing::Unreadable).unwrap(),
            AppPlan::Unknown
        );
        let bad = plan_app(false, None, &env("GITHUB_APP_ID=4 2\n")).unwrap_err();
        assert!(bad.contains("--new-app"), "{bad}");
        assert!(
            plan_app(
                false,
                None,
                &env("GITHUB_APP_ID=4\nGITHUB_APP_INSTALLATION_ID=x\n")
            )
            .is_err()
        );
        assert!(plan_app(false, Some("x;y"), &Existing::Absent).is_err());
    }

    #[test]
    fn app_ids_are_digits_or_a_client_id() {
        for ok in ["1", "123456", "Iv1.0123abcd", "Iv23liABC"] {
            assert!(check_app_id("id", ok).is_ok(), "{ok}");
        }
        for bad in ["", "Iv", "12a", "1 2", "Iv1/x", "x\nGITHUB_TOKEN=y", "-1"] {
            assert!(check_app_id("id", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_dry_run_says_where_the_app_comes_from() {
        let (what, auth) = dry_run_app(
            AppPlan::Reuse {
                id: "42".into(),
                key_file: KEY.into(),
                installation: Some(77),
            },
            "o/r",
        );
        assert!(what.starts_with("Would reuse GitHub App 42"), "{what}");
        assert!(matches!(auth, Auth::App { staged: None, .. }));
        let (what, _) = dry_run_app(AppPlan::Shell("7".into()), "o/r");
        assert!(what.starts_with("Would use GitHub App 7"), "{what}");
        let (what, _) = dry_run_app(AppPlan::Unknown, "o/r");
        assert!(what.contains("needs sudo"), "{what}");
        let (what, auth) = dry_run_app(AppPlan::Create, "o/r");
        assert!(
            what.starts_with("Would create a private GitHub App"),
            "{what}"
        );
        let Auth::App { staged, .. } = &auth else {
            panic!()
        };
        assert_eq!(
            staged.as_deref(),
            Some("/etc/sw-boat-runner/github-app.pem.<app id>")
        );
    }

    #[test]
    fn env_files_hold_the_keys_and_reject_systemd_syntax() {
        let boat = Secret("bk_1".into());
        let pat = env_file(&boat, &Auth::Pat(Secret("github_pat_1".into()))).unwrap();
        assert_eq!(
            pat.expose(),
            "BOAT_API_KEY=bk_1\nGITHUB_TOKEN=github_pat_1\n"
        );
        let app = Auth::App {
            id: "42".into(),
            pem: Secret(test_pem()),
            installation: Some(77),
            staged: None,
        };
        assert_eq!(
            env_file(&boat, &app).unwrap().expose(),
            "BOAT_API_KEY=bk_1\nGITHUB_APP_ID=42\n\
             GITHUB_APP_PRIVATE_KEY_FILE=/etc/sw-boat-runner/github-app.pem\n\
             GITHUB_APP_INSTALLATION_ID=77\n"
        );
        for bad in ["a b", "a\"b", "a\\b", "a\nb"] {
            let err = env_file(&Secret(bad.into()), &app).err().unwrap();
            assert!(
                err.starts_with("BOAT_API_KEY") && !err.contains(bad),
                "{err}"
            );
        }
        let bad_id = Auth::App {
            id: "42\nGITHUB_TOKEN=x".into(),
            pem: Secret(test_pem()),
            installation: None,
            staged: None,
        };
        assert!(
            env_file(&boat, &bad_id)
                .err()
                .unwrap()
                .contains("GITHUB_APP_ID")
        );
    }

    #[test]
    fn the_unit_is_install_shs() {
        let app = unit_file(true);
        assert!(
            app.contains("\nLoadCredential=github-app.pem:/etc/sw-boat-runner/github-app.pem\n")
        );
        assert!(app.contains(
            "\nExecStart=/bin/sh -c 'GITHUB_APP_PRIVATE_KEY_FILE=\"$${CREDENTIALS_DIRECTORY}/github-app.pem\" \
             exec /usr/local/bin/sw-boat-runner run /etc/sw-boat-runner/policy.toml'\n"
        ));
        let pat = unit_file(false);
        assert!(!pat.contains("LoadCredential"));
        assert!(pat.contains(
            "\nExecStart=/usr/local/bin/sw-boat-runner run /etc/sw-boat-runner/policy.toml\n"
        ));
        for unit in [app, pat] {
            for line in [
                "EnvironmentFile=/etc/sw-boat-runner/env",
                "DynamicUser=yes",
                "NoNewPrivileges=yes",
                "CapabilityBoundingSet=",
                "ProtectSystem=strict",
                "ProtectHome=yes",
                "PrivateTmp=yes",
                "PrivateDevices=yes",
                "WantedBy=multi-user.target",
            ] {
                assert!(unit.lines().any(|l| l == line), "{line}");
            }
        }
    }

    #[test]
    fn steps_show_no_secret() {
        let boat = Secret("bk_secret".into());
        let auth = Auth::App {
            id: "42".into(),
            pem: Secret(test_pem()),
            installation: None,
            staged: None,
        };
        let env = env_file(&boat, &auth).unwrap();
        let app_steps = steps("repo = \"a/b\"\n", &env, &auth, Path::new("/tmp/sbr"));
        let shown: Vec<String> = app_steps.iter().map(Step::describe).collect();
        let all = shown.join("\n");
        assert!(
            !all.contains("bk_secret") && !all.contains("PRIVATE KEY"),
            "{all}"
        );
        assert!(all.contains(
            "+ write /etc/sw-boat-runner/env (mode 600, root only): BOAT_API_KEY, \
             GITHUB_APP_ID, GITHUB_APP_PRIVATE_KEY_FILE, values hidden"
        ));
        assert!(
            all.contains(
                "+ write /etc/sw-boat-runner/github-app.pem (mode 600, root only): secret"
            )
        );
        assert!(
            all.contains("+ write /etc/sw-boat-runner/policy.toml (mode 644):\nrepo = \"a/b\"")
        );
        assert!(all.contains("+ install -m 755 /tmp/sbr /usr/local/bin/sw-boat-runner"));
        assert!(
            shown
                .last()
                .unwrap()
                .contains("systemctl --no-pager status")
        );

        // A new App's key moves into place right after the env, not before.
        let new = Auth::App {
            id: "42".into(),
            pem: Secret(test_pem()),
            installation: Some(77),
            staged: Some(staged_key("42")),
        };
        let env = env_file(&boat, &new).unwrap();
        let shown: Vec<String> = steps("", &env, &new, Path::new("/tmp/sbr"))
            .iter()
            .map(Step::describe)
            .collect();
        let at = |prefix: &str| shown.iter().position(|s| s.starts_with(prefix)).unwrap();
        assert_eq!(
            at("+ mv -f /etc/sw-boat-runner/github-app.pem.42 /etc/sw-boat-runner/github-app.pem"),
            at("+ write /etc/sw-boat-runner/env") + 1
        );
        assert!(at("+ mv") < at("+ systemctl restart"));
        assert!(
            !shown
                .iter()
                .any(|s| s.starts_with("+ write /etc/sw-boat-runner/github-app.pem"))
        );

        let pat = Auth::Pat(Secret("t".into()));
        let env = env_file(&boat, &pat).unwrap();
        let pat_steps = steps("", &env, &pat, Path::new("/tmp/sbr"));
        assert!(
            !pat_steps
                .iter()
                .any(|s| s.describe().contains("github-app.pem"))
        );
    }

    #[test]
    fn the_pages_link_is_short_and_carries_what_the_page_checks() {
        let state = "0123456789abcdef0123456789abcdef";
        let link = pages_link(
            "acme",
            &OwnerKind::Organization,
            "app.rs",
            state,
            "sbr-app.rs-01234567",
            None,
        );
        assert_eq!(
            link,
            "https://surely-works-llc.github.io/sw-boat-runner/new-app.html#owner=acme&kind=org\
             &repo=app.rs&state=0123456789abcdef0123456789abcdef&name=sbr-app.rs-01234567"
        );
        assert!(pages_link("me", &OwnerKind::User, "r", state, "n", None).contains("&kind=user&"));
        assert!(
            pages_link(
                "me",
                &OwnerKind::User,
                "r",
                state,
                "n",
                Some("http://127.0.0.1:5000/callback")
            )
            .ends_with("&name=n&redirect=http://127.0.0.1:5000/callback")
        );
        let name = app_name("app.rs", state);
        assert!(name.starts_with("sbr-app.rs-") && name.len() == "sbr-app.rs-".len() + 8);
    }

    /// docs/new-app.html builds the manifest in the browser; it must match `manifest`.
    #[test]
    fn the_pages_manifest_matches_setups() {
        let page = include_str!("../../docs/new-app.html");
        assert!(page.contains(&format!("const PAGE_URL = \"{NEW_APP_PAGE}\";")));
        assert!(page.contains(&format!("const HOMEPAGE = \"{HOMEPAGE}\";")));
        let m = manifest("acme/app", "n");
        let perms: Vec<String> = m["default_permissions"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| format!("\"{k}\": {v}"))
            .collect();
        let line = page
            .lines()
            .find(|l| l.starts_with("const PERMISSIONS = "))
            .unwrap();
        for p in &perms {
            assert!(line.contains(p.as_str()), "{p} not in {line}");
        }
        assert_eq!(line.matches(':').count(), perms.len(), "{line}");
        let keys: Vec<&String> = m.as_object().unwrap().keys().collect();
        for key in keys {
            assert!(page.contains(&format!("\"{key}\": ")), "{key}");
        }
        for s in [
            "\"active\": false",
            "\"public\": false",
            "\"default_events\": []",
            "\"redirect_url\": PAGE_URL",
            "\"sw-boat-runner for \" + owner + \"/\" + repo",
            "https://github.com/organizations/\" + owner + \"/settings/apps/new?state=",
            "\"https://github.com/settings/apps/new?state=\"",
        ] {
            assert!(page.contains(s), "{s}");
        }
        assert_eq!(m["description"], "sw-boat-runner for acme/app");
    }

    /// The page's CSP allows its one inline script by hash, so an edit must update it.
    #[test]
    fn the_pages_csp_hash_matches_its_script() {
        use base64::Engine;
        let page = include_str!("../../docs/new-app.html");
        let script = page
            .split_once("<script>")
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .unwrap()
            .0;
        let digest = ring::digest::digest(&ring::digest::SHA256, script.as_bytes());
        let hash = base64::engine::general_purpose::STANDARD.encode(digest);
        assert!(
            page.contains(&format!(
                "default-src 'none'; script-src 'sha256-{hash}'; style-src 'unsafe-inline'; \
                 form-action https://github.com; base-uri 'none'"
            )),
            "update the CSP hash in docs/new-app.html to sha256-{hash}"
        );
        assert_eq!(page.matches("<script").count(), 1);
    }

    #[test]
    fn prompts_take_the_default_on_enter() {
        assert_eq!(with_default("Repo", Some("a/b")), "Repo [a/b]");
        assert_eq!(with_default("Repo", None), "Repo");
        assert_eq!(
            accept("".into(), Some("a/b".into())).as_deref(),
            Some("a/b")
        );
        assert_eq!(
            accept(" c/d ".into(), Some("a/b".into())).as_deref(),
            Some("c/d")
        );
        assert_eq!(accept("".into(), None), None);
        assert_eq!(split_labels(DEFAULT_LABELS), ["self-hosted", "boat"]);
        assert_eq!(
            runs_on(&split_labels(DEFAULT_LABELS)),
            "runs-on: [self-hosted, boat]"
        );
    }

    #[test]
    fn finds_the_repository_in_a_github_remote() {
        for url in [
            "https://github.com/acme/app.git",
            "https://github.com/acme/app",
            "git@github.com:acme/app.git\n",
            "ssh://git@github.com/acme/app/",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("acme/app"), "{url}");
        }
        for url in [
            "https://gitlab.com/acme/app.git",
            "https://github.com/acme",
            "https://github.com/acme/app/tree/main",
            "git@github.com:acme/..",
        ] {
            assert_eq!(github_repo(url), None, "{url}");
        }
    }

    #[test]
    fn picks_a_snapshot_by_number_or_name() {
        let names: Vec<String> = ["ci-2", "ci-1", "7"].map(String::from).to_vec();
        let pick = |typed: &str| pick_snapshot(typed, &names, Some("ci-2".into()));
        assert_eq!(pick("").unwrap(), "ci-2");
        assert_eq!(pick("2").unwrap(), "ci-1");
        assert_eq!(pick("ci-1").unwrap(), "ci-1");
        assert_eq!(pick("other").unwrap(), "other");
        assert_eq!(pick("3").unwrap(), "7");
        assert_eq!(pick("7").unwrap(), "7");
        assert!(pick("9").unwrap_err().contains("1 to 3"));
        assert!(pick_snapshot("", &names, None).is_err());
    }
}
