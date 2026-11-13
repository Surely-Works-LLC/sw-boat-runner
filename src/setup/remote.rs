//! `setup --remote`: setup run from a laptop or workstation. It asks the usual
//! questions, creates the GitHub App here (the browser comes back to a one-shot listener
//! on 127.0.0.1, or the code is pasted), creates a small always-on boat machine, and
//! installs the daemon on it with the release's install.sh, which runs `setup` there.
//!
//! The secrets (the boat key, the App's private key or a personal token) reach the
//! machine only as the stdin of a `boat ssh` command, which boat does not keep, into
//! files under a `umask 077` directory that the install deletes. They are never in a
//! command line, an `exec` string or the boat API's command history.

use std::collections::BTreeMap;
use std::io::{BufRead, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use super::*;
use crate::boat::{self, NewSandbox};
use crate::snapshot::{Io, Terminal};

/// How long the listener waits for the browser to come back from GitHub.
const CALLBACK_WAIT: Duration = Duration::from_secs(15 * 60);
const MACHINE_TYPE: &str = "small";
const MACHINE_NAME: &str = "sw-boat-runner";
/// Label holding this run's id, so a machine whose create got no answer can be found.
const RUN_LABEL: &str = "sw-boat-runner-daemon-run";
/// Label marking the daemon machine of one repository, with `repo_key` as its value.
const HOST_LABEL: &str = "sw-boat-runner-host";
/// The directory on the machine that holds the secrets until install.sh has run.
const STAGE: &str = "$HOME/.sbr-setup";

/// Where the GitHub access comes from.
#[derive(Debug, PartialEq, Eq)]
enum GitHub {
    Pat,
    /// `GITHUB_APP_ID` and `GITHUB_APP_PRIVATE_KEY_FILE` in this shell.
    Shell(String),
    Create,
}

pub(super) fn run(opts: &Options) -> Result<(), String> {
    let git_ref = release_ref()?;
    let source = Source::from_env()?;
    let github = github_plan(opts)?;
    if !opts.dry_run {
        boat_cli_ready()?;
    }
    // As soon as the repository is known, before the boat key, the other prompts and the
    // App: two daemons for one repository would compete for its jobs, and whoever has one
    // hears so before pasting anything. A dry run asks no secret: the key only lists
    // snapshots when it is in the environment.
    let mut replaced = Vec::new();
    let mut boat_key = None;
    let policy_text = choose_policy(opts, &mut boat_key, |repo, key| {
        if opts.dry_run {
            return Ok(());
        }
        let (found, k) = check_daemons(
            repo,
            opts.replace,
            boat_list,
            || secret("BOAT_API_KEY", "boat API key"),
            |k| labelled_daemons(k, repo),
        )?;
        replaced = found;
        *key = Some(k);
        Ok(())
    })?;
    let cfg = policy::parse(&policy_text).map_err(|e| format!("policy: {e}"))?;
    println!("Policy:\n\n{policy_text}");
    show_check(&policy_text)?;
    if opts.dry_run {
        print!(
            "{}",
            plan(
                &cfg.repo,
                &github,
                &git_ref,
                opts.paste || opts.offline_link,
                opts.replace
            )
        );
        return Ok(());
    }
    let boat_key = boat_key.expect("asked above");

    let t = Ureq::new(Duration::from_secs(30));
    let (auth, local_key) = match &github {
        GitHub::Pat => {
            let token = secret(
                "GITHUB_TOKEN",
                &format!(
                    "GitHub fine-grained token for {} (Administration write, Actions read)",
                    cfg.repo
                ),
            )?;
            (Auth::Pat(token), None)
        }
        GitHub::Shell(id) => (shell_app(id.clone(), false)?, None),
        GitHub::Create => {
            let (auth, path) = create_app_here(&t, &cfg.repo, opts)?;
            (auth, Some(path))
        }
    };
    let retry_hint = match (&auth, &local_key) {
        (Auth::App { id, .. }, Some(path)) => format!(
            "\nThe app's key is at {}: run `GITHUB_APP_ID={id} GITHUB_APP_PRIVATE_KEY_FILE={} \
             sw-boat-runner setup --remote` to try again with the same app.",
            path.display(),
            path.display()
        ),
        _ => String::new(),
    };

    let mut io = Terminal::new();
    let boat = boat::Client::new(&t, boat_key.expose().to_string());
    let id = create_machine(&boat, &cfg.repo, &mut io).map_err(|e| format!("{e}{retry_hint}"))?;
    println!("Machine {id} ({MACHINE_TYPE}, no auto-stop).");
    let files = match stage(&id, &policy_text, &boat_key, &auth, &mut io) {
        Ok(files) => files,
        Err(e) => {
            // Nothing runs on it yet: delete it, with whatever secret reached it.
            delete_machine(&boat, &id, &mut io);
            return Err(format!("{e}{retry_hint}"));
        }
    };
    println!("+ boat ssh {id} <install.sh at {git_ref}, then `sw-boat-runner setup` there>\n");
    let script = install_script(&source.repo, &git_ref, &auth, &files);
    match boat_ssh(&id, &script, None) {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "the install on {id} failed (above). {}{retry_hint}",
                kept(&id)
            ));
        }
        Err(e) => return Err(format!("{e}. {}{retry_hint}", kept(&id))),
    }
    if let Some(path) = &local_key {
        // The machine has the key now; the copy here is no longer needed.
        if std::fs::remove_file(path).is_ok() {
            println!(
                "Removed the local copy of the app's key, {}.",
                path.display()
            );
        }
    }
    if !replaced.is_empty() {
        replace_old(&boat, &id, &replaced, &mut io);
    }
    println!(
        "\nThe daemon runs on boat machine {id}.\n\
         Logs:   boat ssh {id} journalctl -u {SERVICE} -f\n\
         Doctor: boat ssh {id} sudo {BIN} doctor\n\n\
         Point jobs at the pool with this line in the workflow:\n\n    {}\n",
        runs_on(&cfg.pools[0].labels)
    );
    Ok(())
}

/// The release the machine installs: `SBR_REF`, else this binary's version.
fn release_ref() -> Result<String, String> {
    let r = given(&None, "SBR_REF").unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    // It goes into a URL and a shell command on the machine.
    if r.is_empty()
        || r.starts_with(['-', '.', '/'])
        || r.contains("..")
        || !r
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(format!("SBR_REF {r:?} is not a tag, commit or branch name"));
    }
    Ok(r)
}

/// The same choice as `setup`, without an earlier setup's env file to reuse.
fn github_plan(opts: &Options) -> Result<GitHub, String> {
    let shell_app_id = std::env::var("GITHUB_APP_ID")
        .ok()
        .filter(|v| !v.is_empty());
    note_ignored_token(opts.pat);
    match (opts.pat, shell_app_id) {
        (true, Some(_)) => Err("set GITHUB_TOKEN or GITHUB_APP_*, not both".into()),
        (true, None) => Ok(GitHub::Pat),
        (false, Some(_)) if opts.new_app => {
            Err("--new-app creates a GitHub App: unset GITHUB_APP_ID".into())
        }
        (false, Some(id)) => {
            check_app_id("GITHUB_APP_ID", &id)?;
            Ok(GitHub::Shell(id))
        }
        (false, None) => Ok(GitHub::Create),
    }
}

/// The boat CLI must be installed and signed in: the secrets go over `boat ssh`.
fn boat_cli_ready() -> Result<(), String> {
    let out = Command::new("boat")
        .args(["status", "--json"])
        .env("BOAT_NO_UPDATE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| {
            format!("--remote needs the boat CLI (https://docs.boat.dev/cli-reference): {e}")
        })?;
    let status: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    if status["account"]["signedIn"] != Value::Bool(true) {
        return Err(
            "--remote copies the keys over `boat ssh`: sign the boat CLI in first (boat login), \
             to the account BOAT_API_KEY belongs to"
                .into(),
        );
    }
    Ok(())
}

/// What `--remote --dry-run` prints after the policy.
fn plan(repo: &str, github: &GitHub, git_ref: &str, paste: bool, replace: bool) -> String {
    let (app_step, file) = match github {
        GitHub::Pat => (
            "ask for GITHUB_TOKEN without echo".to_string(),
            "github-token",
        ),
        GitHub::Shell(id) => (
            format!(
                "use GitHub App {id} from GITHUB_APP_ID, with the key at GITHUB_APP_PRIVATE_KEY_FILE"
            ),
            "app.pem",
        ),
        GitHub::Create => (
            format!(
                "create a private GitHub App for {repo} (Administration write, Actions read, no \
                 webhook) in the browser; {}, exchange the code here, keep the key in \
                 ~/.config/sw-boat-runner (mode 600) until the machine has it, and wait until \
                 the app is installed on {repo}",
                if paste {
                    "you paste the code GitHub returns"
                } else {
                    "GitHub sends the browser back to http://127.0.0.1:<random port>/callback, \
                     where setup listens for 15 minutes (or you paste the address it ends on)"
                }
            ),
            "app.pem",
        ),
    };
    let existing = if replace {
        "delete it once doctor finds no FAIL on the new machine (--replace)"
    } else {
        "stop, unless --replace"
    };
    format!(
        "Would, from this computer:\n\n\
         0. look with `boat list --all --json` for a boat machine that already runs the \
            daemon for {repo} and {existing}; then ask for BOAT_API_KEY without echo\n\
         1. {app_step}\n\
         2. create an always-on boat {MACHINE_TYPE} machine called `{MACHINE_NAME} {repo}` (no auto-stop, \
            not noEnv) and wait until it runs\n\
         3. write {STAGE}/{{policy.toml,boat-key,{file}}} on it (directory mode 700) through \
            the stdin of `boat ssh`, never in a command\n\
         4. boat ssh <id>: download examples/always-on/install.sh at {git_ref}, which checks \
            the release binary and runs `sw-boat-runner setup --policy policy.toml`: it writes \
            /etc/sw-boat-runner, enables the {SERVICE} service and runs doctor, whose output \
            shows here; then delete {STAGE}\n\
         5. print the machine id and `boat ssh <id> journalctl -u {SERVICE} -f`\n\n\
         SBR_REF picks another release. If step 3 fails the machine is deleted; if step 4 \
         fails it is kept for a look, with `boat delete <id> --yes` to remove it.\n"
    )
}

/// The redirect address the listener on `port` serves. docs/new-app.html takes exactly
/// what `is_loopback_redirect` takes.
fn loopback_redirect(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// `^http://127\.0\.0\.1:[0-9]{2,5}/callback$`, the pattern docs/new-app.html checks.
fn is_loopback_redirect(url: &str) -> bool {
    url.strip_prefix("http://127.0.0.1:")
        .and_then(|rest| rest.strip_suffix("/callback"))
        .is_some_and(|port| {
            (2..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Creates the App from this computer and waits until it is installed. GitHub shows the
/// key once, so it is also kept in a mode 600 file here until the machine has it.
fn create_app_here(
    t: &dyn Transport,
    repo: &str,
    opts: &Options,
) -> Result<(Auth, PathBuf), String> {
    let (owner, name) = repo.split_once('/').expect("a checked repo");
    let kind = owner_kind(t, owner)?;
    let state = uuid::Uuid::new_v4().simple().to_string();
    let app_name = app_name(name, &state);
    let listener = if opts.paste || opts.offline_link {
        None
    } else {
        match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => Some(l),
            Err(e) => {
                println!("Could not listen on 127.0.0.1 ({e}); you will paste the code instead.");
                None
            }
        }
    };
    let port = listener
        .as_ref()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port());
    // The page refuses any other redirect, so check here what it will check.
    let redirect = port
        .map(loopback_redirect)
        .filter(|r| is_loopback_redirect(r));
    let link = if opts.offline_link {
        let page = manifest_page(&kind.new_app_url(owner), &manifest(repo, &app_name), &state);
        data_url(&page)
    } else {
        pages_link(owner, &kind, name, &state, &app_name, redirect.as_deref())
    };
    let opened = !opts.offline_link && open_browser(&link);
    println!(
        "Create the GitHub App: {} check what the page lists and press its button.\n\
         GitHub then asks you to confirm the app.\n\n{link}\n",
        if opened {
            "your browser opens this link (open it by hand if it does not);"
        } else {
            "open this link in your browser,"
        }
    );
    let code = match (listener, port, given(&None, "SBR_APP_CODE")) {
        (_, _, Some(pasted)) => parse_code(&pasted, &state)?,
        (Some(listener), Some(port), None) => {
            println!(
                "Waiting up to {} min for GitHub to send the browser back to {}.\n\
                 If the browser is on another computer, the page there does not load: paste \
                 its address\n(it starts with {}?code=), or just the code, here and press \
                 Enter.",
                CALLBACK_WAIT.as_secs() / 60,
                loopback_redirect(port),
                loopback_redirect(port)
            );
            let pasted = read_pastes();
            match wait_for_code(&listener, port, &state, CALLBACK_WAIT, &pasted)? {
                Some(code) => code,
                None => {
                    println!(
                        "No answer in {} min. If GitHub created the app, the browser shows an \
                         address starting with {}?code=: paste it here (or just the code).",
                        CALLBACK_WAIT.as_secs() / 60,
                        loopback_redirect(port)
                    );
                    wait_for_paste(&pasted, &state)?
                }
            }
        }
        _ => {
            println!(
                "GitHub then opens {}?code=...; copy the whole address it shows.\n",
                source().new_app_page
            );
            let pasted = secret("SBR_APP_CODE", "Paste the address (or just the code)")?;
            parse_code(pasted.expose(), &state)?
        }
    };
    let created = convert(t, &code)?;
    let id = created.id.to_string();
    App::new(id.clone(), None, created.pem.expose().as_bytes())?;
    let path = keep_key(&id, &created.pem)?;
    println!(
        "Created app {} (id {id}); its key is at {} until the machine has it.",
        created.slug,
        path.display()
    );
    let note = print_install_link(repo, &kind, &created.slug);
    let app = App::new(id.clone(), None, created.pem.expose().as_bytes())?;
    println!(
        "Waiting until the app is installed on {repo} (up to {} min)...",
        INSTALL_WAIT.as_secs() / 60
    );
    let installation = wait_installed(t, &app, repo, INSTALL_WAIT, &note, std::thread::sleep)
        .map_err(|e| {
            format!(
                "{e}\nInstall the app, then run `GITHUB_APP_ID={id} GITHUB_APP_PRIVATE_KEY_FILE={} \
                 sw-boat-runner setup --remote`.",
                path.display()
            )
        })?;
    println!("Installed (installation {installation}).");
    Ok((
        Auth::App {
            id,
            pem: created.pem,
            installation: Some(installation),
            staged: None,
        },
        path,
    ))
}

/// `open` on macOS, `xdg-open` elsewhere; false when neither ran.
fn open_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Writes the key to `~/.config/sw-boat-runner/github-app-<id>.pem`, mode 600.
fn keep_key(id: &str, pem: &Secret) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let dir = PathBuf::from(home).join(".config/sw-boat-runner");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("github-app-{id}.pem"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(pem.expose().as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// What one request to the listener asks for.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    /// `GET /callback` with this run's state and a code.
    Code(String),
    /// `GET /callback` without this run's state, or without a code.
    Refused(String),
    /// Anything else, such as the browser's `/favicon.ico`.
    NotFound,
}

/// Reads a request's head: the browser's `GET /callback?code=...&state=...` with a `Host`
/// of this listener. The code counts only with the state this setup sent.
fn parse_callback(head: &str, port: u16, state: &str) -> Callback {
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (Some("GET"), Some(target)) = (first.next(), first.next()) else {
        return Callback::NotFound;
    };
    let Some(("/callback", query)) = target.split_once('?') else {
        return Callback::NotFound;
    };
    let host = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim().to_string());
    if host.as_deref() != Some(format!("127.0.0.1:{port}").as_str()) {
        return Callback::Refused("the request is not for this listener".into());
    }
    match parse_code(&format!("/callback?{query}"), state) {
        Ok(code) => Callback::Code(code.expose().to_string()),
        Err(e) => Callback::Refused(e),
    }
}

/// Lines typed on stdin, read on a thread of their own so the wait for the browser can
/// take a pasted code at the same time. After each line the thread waits for `more`:
/// `true` reads another line, `false` (or a dropped sender) ends the thread. When the
/// browser wins, the thread stays blocked in its read until the process ends, so nothing
/// after the wait may read stdin.
struct Pasted {
    lines: Receiver<String>,
    more: Sender<bool>,
}

fn read_pastes() -> Pasted {
    let (line_tx, lines) = mpsc::channel();
    let (more, more_rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line_tx.send(line).is_err() || !more_rx.recv().unwrap_or(false) {
                return;
            }
        }
    });
    Pasted { lines, more }
}

/// The code in one pasted line, if it holds one; a wrong paste says why. Either way it
/// tells the reader whether to read another line.
fn take_paste(pasted: &Pasted, line: &str, state: &str) -> Option<Secret> {
    let line = line.trim();
    let code = if line.is_empty() {
        None
    } else {
        parse_code(line, state)
            .map_err(|e| println!("{e}. Paste the whole address, or just the code."))
            .ok()
    };
    let _ = pasted.more.send(code.is_none());
    code
}

/// Waits for a pasted code alone, after the listener gave up.
fn wait_for_paste(pasted: &Pasted, state: &str) -> Result<Secret, String> {
    loop {
        let line = pasted
            .lines
            .recv()
            .map_err(|_| "no code: stdin closed before one was pasted".to_string())?;
        if let Some(code) = take_paste(pasted, &line, state) {
            return Ok(code);
        }
    }
}

/// Serves the listener until a request or a pasted line brings the code (`Some`) or
/// `limit` passes (`None`). Other requests get an error page, a wrong paste a message,
/// and the wait goes on.
fn wait_for_code(
    listener: &TcpListener,
    port: u16,
    state: &str,
    limit: Duration,
    pasted: &Pasted,
) -> Result<Option<Secret>, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener: {e}"))?;
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match pasted.lines.try_recv() {
            Ok(line) => {
                if let Some(code) = take_paste(pasted, &line, state) {
                    return Ok(Some(code));
                }
                continue;
            }
            // No line yet, or stdin closed: the browser may still come back.
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = answer(stream, port, state) {
                    return Ok(Some(code));
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200))
            }
            Err(e) => return Err(format!("listener: {e}")),
        }
    }
    Ok(None)
}

/// Answers one connection; the code when it brought one.
fn answer(mut stream: TcpStream, port: u16, state: &str) -> Option<Secret> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while head.len() < 8192 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let (status, text, code) = match parse_callback(&String::from_utf8_lossy(&head), port, state) {
        Callback::Code(code) => (
            "200 OK",
            "GitHub created the app. You can close this tab and go back to the terminal."
                .to_string(),
            Some(Secret(code)),
        ),
        Callback::Refused(why) => (
            "400 Bad Request",
            format!("sw-boat-runner setup did not take this: {why}."),
            None,
        ),
        Callback::NotFound => ("404 Not Found", "Not found.".to_string(), None),
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>sw-boat-runner setup</title>\
         <p style=\"font:16px system-ui,sans-serif;margin:2rem\">{text}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    code
}

/// Creates the always-on machine. It is not `noEnv`: it runs only the daemon, no CI job.
fn create_machine(boat: &boat::Client, repo: &str, io: &mut dyn Io) -> Result<String, String> {
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let spec = NewSandbox {
        machine_type: MACHINE_TYPE.into(),
        ttl_s: None,
        no_env: false,
        labels: BTreeMap::from([(RUN_LABEL, run_id.clone()), (HOST_LABEL, repo_key(repo))]),
        name: format!("{MACHINE_NAME} {repo}"),
    };
    let key = format!("sw-boat-runner-daemon-{run_id}");
    println!("+ create a boat {MACHINE_TYPE} machine with no auto-stop");
    let id =
        match crate::snapshot::retry(io, "create machine", |_| boat.create_sandbox(&spec, &key)) {
            Ok(id) => id,
            Err(e) => {
                crate::snapshot::delete_labelled(boat, RUN_LABEL, &run_id, io);
                return Err(e);
            }
        };
    if let Err(e) = crate::snapshot::wait_until_up(boat, &id, io) {
        delete_machine(boat, &id, io);
        return Err(e);
    }
    Ok(id)
}

fn delete_machine(boat: &boat::Client, id: &str, io: &mut dyn Io) {
    match crate::snapshot::cleanup_retry(io, "delete machine", |_| boat.delete(id)) {
        Ok(()) => println!("Deleted machine {id}."),
        Err(e) => println!(
            "Could not delete machine {id} ({e}); it may hold the staged keys in {STAGE}. \
             `boat delete {id} --yes` removes it."
        ),
    }
}

/// What every failure after the machine exists ends with.
fn kept(id: &str) -> String {
    format!(
        "The machine {id} is kept, and may still hold the staged keys in {STAGE}: `boat ssh \
         {id}` to look, `boat delete {id} --yes` to remove it."
    )
}

/// The `HOST_LABEL` value for `repo`: hex, which boat takes as a label value, of the
/// repository's name without case, as GitHub compares it.
fn repo_key(repo: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, repo.to_lowercase().as_bytes());
    digest.as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A machine that already runs the daemon for the repository.
#[derive(Debug, PartialEq, Eq)]
struct Daemon {
    id: String,
    state: String,
}

/// `boat list --all --json`, from the boat CLI, which is signed in before setup asks for
/// any key.
fn boat_list() -> Result<Value, String> {
    let out = Command::new("boat")
        .args(["list", "--all", "--json"])
        .env("BOAT_NO_UPDATE", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("boat list: {e}"))?;
    let listed: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    if !out.status.success() || !listed["sandboxes"].is_array() {
        return Err(format!(
            "`boat list --all --json` failed ({}), so setup cannot tell whether a daemon \
             already runs for the repository; fix that and try again",
            out.status
        ));
    }
    Ok(listed)
}

/// The machines boat's label search finds with `HOST_LABEL` for `repo`, through the API.
fn labelled_daemons(key: &Secret, repo: &str) -> Result<Vec<Daemon>, String> {
    let t = Ureq::new(Duration::from_secs(30));
    let boat = boat::Client::new(&t, key.expose().to_string());
    let labelled = boat
        .list_labelled(HOST_LABEL, &repo_key(repo))
        .map_err(|e| format!("look for a daemon machine for {repo}: {e}"))?;
    Ok(labelled
        .into_iter()
        .filter(|m| !gone(&m.state))
        .map(|m| Daemon {
            id: m.id,
            state: m.state,
        })
        .collect())
}

/// The daemon machines `setup --remote` made for `repo`, and the boat key, asked for
/// only once the CLI's list (`list`) shows none, or `--replace` will take their place.
/// They are found by `HOST_LABEL`, or, for those made before the label, by the name it
/// gives them. Only when the list is cut short does the key's label search (`labelled`)
/// look further.
fn check_daemons(
    repo: &str,
    replace: bool,
    list: impl FnOnce() -> Result<Value, String>,
    ask_key: impl FnOnce() -> Result<Secret, String>,
    labelled: impl FnOnce(&Secret) -> Result<Vec<Daemon>, String>,
) -> Result<(Vec<Daemon>, Secret), String> {
    let listed = list()?;
    let mut found = daemons_in(&listed, repo);
    if !replace && !found.is_empty() {
        refuse_second_daemon(repo, &found, replace)?;
    }
    let key = ask_key()?;
    if listed["pageInfo"]["hasMore"] == Value::Bool(true) {
        for d in labelled(&key)? {
            if !found.iter().any(|f| f.id == d.id) {
                found.push(d);
            }
        }
    }
    refuse_second_daemon(repo, &found, replace)?;
    Ok((found, key))
}

/// A machine that no longer runs anything.
fn gone(state: &str) -> bool {
    matches!(state, "deleted" | "cancelled")
}

fn daemons_in(listed: &Value, repo: &str) -> Vec<Daemon> {
    let key = repo_key(repo);
    let name = format!("{MACHINE_NAME} {repo}");
    listed["sandboxes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            m["labels"][HOST_LABEL].as_str() == Some(key.as_str())
                || m["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&name))
        })
        .filter_map(|m| {
            let state = m["state"].as_str().unwrap_or("unknown").to_string();
            Some(Daemon {
                id: m["id"].as_str()?.to_string(),
                state,
            })
            .filter(|d| !gone(&d.state))
        })
        .collect()
}

/// Without `--replace`, an existing daemon machine for the repository stops setup.
fn refuse_second_daemon(repo: &str, found: &[Daemon], replace: bool) -> Result<(), String> {
    let Some(first) = found.first() else {
        return Ok(());
    };
    let list: Vec<String> = found
        .iter()
        .map(|d| format!("{} ({})", d.id, d.state))
        .collect();
    if replace {
        println!(
            "Will delete {} once doctor finds no FAIL on the new machine (--replace).",
            list.join(", ")
        );
        return Ok(());
    }
    let id = &first.id;
    Err(format!(
        "boat machine {} already runs the daemon for {repo}, and a second one would compete \
         for its jobs. Keep it (`boat ssh {id} sudo {BIN} doctor` checks it; to change the \
         policy, edit {POLICY} there and restart the service), delete it first with `boat \
         delete {id} --yes`, or pass --replace to delete it once doctor finds no FAIL on \
         the new machine.",
        list.join(", ")
    ))
}

/// With `--replace`: deletes the old daemon machines once doctor finds no FAIL on the new
/// one. An active service is not enough: a daemon that keeps failing stays active.
fn replace_old(boat: &boat::Client, id: &str, old: &[Daemon], io: &mut dyn Io) {
    println!("+ boat ssh {id} sudo {BIN} doctor   (before deleting the old machine)");
    let healthy = boat_ssh(id, &doctor_quietly(), None);
    if !matches!(healthy, Ok(true)) {
        let ids: Vec<&str> = old.iter().map(|d| d.id.as_str()).collect();
        println!(
            "doctor found a problem on {id}, so {} kept running: check \
             `boat ssh {id} sudo {BIN} doctor`, then delete one of the two with \
             `boat delete <id> --yes`.",
            ids.join(", ")
        );
        return;
    }
    for d in old {
        delete_machine(boat, &d.id, io);
    }
}

/// Doctor on the new machine, its output dropped: it already showed during the install,
/// and any FAIL exits non-zero.
fn doctor_quietly() -> String {
    format!("sudo {BIN} doctor >/dev/null 2>&1")
}

/// The files `stage` writes under `STAGE`.
struct Staged {
    secret_file: &'static str,
}

/// Writes the policy and the secrets on the machine, each through the stdin of a `boat
/// ssh` command that holds no secret.
fn stage(
    id: &str,
    policy_text: &str,
    boat_key: &Secret,
    auth: &Auth,
    io: &mut dyn Io,
) -> Result<Staged, String> {
    // sshd may take a moment after boat reports the machine up.
    let mut tries = 0;
    while !boat_ssh(id, "true", None)? {
        tries += 1;
        if tries >= 6 {
            return Err(format!("`boat ssh {id}` does not connect"));
        }
        io.sleep(Duration::from_secs(5));
        if io.interrupted() {
            return Err("interrupted".into());
        }
    }
    let (secret_file, secret) = match auth {
        Auth::Pat(token) => ("github-token", token),
        Auth::App { pem, .. } => ("app.pem", pem),
    };
    let files: [(&str, &[u8]); 3] = [
        ("policy.toml", policy_text.as_bytes()),
        ("boat-key", boat_key.expose().as_bytes()),
        (secret_file, secret.expose().as_bytes()),
    ];
    for (name, content) in files {
        println!("+ boat ssh {id} <write {STAGE}/{name} from stdin>");
        if !boat_ssh(id, &write_script(name), Some(content))? {
            return Err(format!("could not write {STAGE}/{name} on {id}"));
        }
    }
    Ok(Staged { secret_file })
}

/// Writes stdin to `STAGE/name`, readable by the machine's user only.
fn write_script(name: &str) -> String {
    format!("umask 077 && mkdir -p \"{STAGE}\" && cat > \"{STAGE}/{name}\"")
}

/// Runs install.sh at `git_ref` with the staged files; the secrets come from the files,
/// never from this text, and the directory is deleted whatever happens.
fn install_script(source: &str, git_ref: &str, auth: &Auth, files: &Staged) -> String {
    let github = match auth {
        // install.sh takes the token only when asked to.
        Auth::Pat(_) => format!(
            "GITHUB_TOKEN=$(cat {}) || exit 1\nexport GITHUB_TOKEN SBR_PAT=1\n",
            files.secret_file
        ),
        Auth::App {
            id, installation, ..
        } => {
            // A boat machine comes with its creator's GITHUB_TOKEN. It stays for install.sh,
            // whose `gh attestation verify` needs a signed-in gh; install.sh unsets it
            // before `setup`, which runs without --pat.
            let mut s = format!(
                "export GITHUB_APP_ID={id} GITHUB_APP_PRIVATE_KEY_FILE=\"$d/{}\"\n",
                files.secret_file
            );
            if let Some(i) = installation {
                s += &format!("export GITHUB_APP_INSTALLATION_ID={i}\n");
            }
            s
        }
    };
    format!(
        "set -u
d=\"{STAGE}\"
trap 'rm -rf \"$d\"' EXIT
cd \"$d\" || exit 1
curl --proto '=https' -fsSL -o install.sh https://raw.githubusercontent.com/{source}/{git_ref}/examples/always-on/install.sh || exit 1
BOAT_API_KEY=$(cat boat-key) || exit 1
export BOAT_API_KEY SBR_REF={git_ref} SBR_UPSTREAM={source}
{github}bash install.sh policy.toml
"
    )
}

/// `boat ssh <id> <script>` with `input` on its stdin (or none), its output shown here.
/// True when the command succeeded.
fn boat_ssh(id: &str, script: &str, input: Option<&[u8]>) -> Result<bool, String> {
    let mut child = Command::new("boat")
        .args(["ssh", id, script])
        .env("BOAT_NO_UPDATE", "1")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .spawn()
        .map_err(|e| format!("boat ssh {id}: {e}"))?;
    if let Some(bytes) = input {
        let mut stdin = child.stdin.take().expect("piped");
        if let Err(e) = stdin.write_all(bytes) {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("boat ssh {id}: writing its input: {e}"));
        }
        // Dropped here, so the remote `cat` sees the end of its input.
    }
    let status = child.wait().map_err(|e| format!("boat ssh {id}: {e}"))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "0123456789abcdef0123456789abcdef";

    /// A `Pasted` fed by the test instead of stdin, and what the waits answered.
    fn pastes() -> (Pasted, Sender<String>, Receiver<bool>) {
        let (tx, lines) = mpsc::channel();
        let (more, more_rx) = mpsc::channel();
        (Pasted { lines, more }, tx, more_rx)
    }

    fn head(target: &str, host: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nAccept: text/html\r\n\r\n")
    }

    #[test]
    fn the_listener_takes_the_code_only_with_this_runs_state() {
        let ok = head(
            &format!("/callback?code=abc_1&state={STATE}"),
            "127.0.0.1:5000",
        );
        assert_eq!(
            parse_callback(&ok, 5000, STATE),
            Callback::Code("abc_1".into())
        );
        let swapped = head(
            &format!("/callback?state={STATE}&code=abc"),
            "127.0.0.1:5000",
        );
        assert_eq!(
            parse_callback(&swapped, 5000, STATE),
            Callback::Code("abc".into())
        );
        for bad in [
            head("/callback?code=abc&state=other", "127.0.0.1:5000"),
            head("/callback?code=abc", "127.0.0.1:5000"),
            head(&format!("/callback?state={STATE}"), "127.0.0.1:5000"),
            head(
                &format!("/callback?code=../x&state={STATE}"),
                "127.0.0.1:5000",
            ),
            // Another host name for the same address, as DNS rebinding would send.
            head(
                &format!("/callback?code=abc&state={STATE}"),
                "evil.example:5000",
            ),
            head(
                &format!("/callback?code=abc&state={STATE}"),
                "127.0.0.1:5001",
            ),
        ] {
            assert!(
                matches!(parse_callback(&bad, 5000, STATE), Callback::Refused(_)),
                "{bad}"
            );
        }
        for other in [
            head("/favicon.ico", "127.0.0.1:5000"),
            head("/callback", "127.0.0.1:5000"),
            head("/other?code=abc", "127.0.0.1:5000"),
            format!("POST /callback?code=abc&state={STATE} HTTP/1.1\r\n\r\n"),
            String::new(),
        ] {
            assert_eq!(
                parse_callback(&other, 5000, STATE),
                Callback::NotFound,
                "{other}"
            );
        }
    }

    #[test]
    fn the_listener_answers_one_browser_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            let get = |target: String| {
                let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(s, "{}", head(&target, &format!("127.0.0.1:{port}"))).unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).unwrap();
                out
            };
            let a = get("/favicon.ico".into());
            let b = get("/callback?code=abc&state=wrong".into());
            let c = get(format!("/callback?code=c0de&state={STATE}"));
            (a, b, c)
        });
        let (pasted, _tx, _) = pastes();
        let code = wait_for_code(&listener, port, STATE, Duration::from_secs(20), &pasted)
            .unwrap()
            .unwrap();
        assert_eq!(code.expose(), "c0de");
        let (a, b, c) = client.join().unwrap();
        assert!(a.starts_with("HTTP/1.1 404"), "{a}");
        assert!(
            b.starts_with("HTTP/1.1 400") && b.contains("state differs"),
            "{b}"
        );
        assert!(
            c.starts_with("HTTP/1.1 200") && c.contains("close this tab"),
            "{c}"
        );
        assert!(!c.contains("c0de"));
        // Nothing comes: the wait ends with no code.
        let idle = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = idle.local_addr().unwrap().port();
        assert!(
            wait_for_code(&idle, port, STATE, Duration::from_millis(300), &pasted)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_pasted_code_ends_the_wait_for_the_browser() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (pasted, tx, more) = pastes();
        for line in [
            "\n".to_string(),
            "http://127.0.0.1:1/callback?code=abc&state=other\n".to_string(),
            format!("http://127.0.0.1:{port}/callback?code=p4ste&state={STATE}\n"),
        ] {
            tx.send(line).unwrap();
        }
        let code = wait_for_code(&listener, port, STATE, Duration::from_secs(20), &pasted)
            .unwrap()
            .unwrap();
        assert_eq!(code.expose(), "p4ste");
        // Read on after the blank line and the wrong state; stop after the code.
        assert_eq!(more.try_iter().collect::<Vec<_>>(), [true, true, false]);

        // After the listener gives up, only a paste ends the wait.
        tx.send("c0de\n".into()).unwrap();
        assert_eq!(wait_for_paste(&pasted, STATE).unwrap().expose(), "c0de");
        drop(tx);
        assert!(
            wait_for_paste(&pasted, STATE)
                .err()
                .unwrap()
                .contains("stdin closed")
        );
    }

    #[test]
    fn finds_the_daemon_machine_for_the_repository() {
        let key = repo_key("acme/app");
        assert_eq!(key, repo_key("Acme/App"));
        assert_ne!(key, repo_key("acme/app2"));
        assert!(key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit()));
        let listed = serde_json::json!({
            "pageInfo": {"hasMore": false},
            "sandboxes": [
                {"id": "bx_aaaaaaaa", "state": "idle", "name": "renamed",
                 "labels": {HOST_LABEL: key}},
                // Made before the label: found by name.
                {"id": "bx_bbbbbbbb", "state": "stopped", "name": "sw-boat-runner acme/app",
                 "labels": {}},
                {"id": "bx_cccccccc", "state": "deleted", "name": "sw-boat-runner acme/app"},
                {"id": "bx_dddddddd", "state": "idle", "name": "sw-boat-runner acme/app2"},
                {"id": "bx_eeeeeeee", "state": "idle", "name": "acme-tests",
                 "labels": {"sw-boat-runner": "acme"}},
                // A pool fork with the daemon-identity label of #36: not a host.
                {"id": "bx_ffffffff", "state": "idle", "name": "acme-ci",
                 "labels": {"sw-boat-runner-daemon": key}},
            ]
        });
        let found = daemons_in(&listed, "acme/app");
        assert_eq!(
            found,
            [
                Daemon {
                    id: "bx_aaaaaaaa".into(),
                    state: "idle".into()
                },
                Daemon {
                    id: "bx_bbbbbbbb".into(),
                    state: "stopped".into()
                },
            ]
        );
        assert!(daemons_in(&listed, "acme/other").is_empty());

        let err = refuse_second_daemon("acme/app", &found, false).unwrap_err();
        assert!(
            err.starts_with("boat machine bx_aaaaaaaa (idle), bx_bbbbbbbb (stopped) already runs")
                && err.contains("`boat delete bx_aaaaaaaa --yes`")
                && err.contains("--replace"),
            "{err}"
        );
        assert!(refuse_second_daemon("acme/app", &found, true).is_ok());
        assert!(refuse_second_daemon("acme/app", &[], false).is_ok());
    }

    /// `check_daemons` with `listed` from the CLI and `labelled` from the API; what it
    /// returned, whether it asked for the key, and whether it used the API.
    fn check(
        listed: Value,
        replace: bool,
        labelled: Vec<Daemon>,
    ) -> (Result<Vec<Daemon>, String>, bool, bool) {
        let mut asked = false;
        let mut searched = false;
        let got = check_daemons(
            "acme/app",
            replace,
            || Ok(listed),
            || {
                asked = true;
                Ok(Secret("key".into()))
            },
            |k| {
                assert_eq!(k.expose(), "key");
                searched = true;
                Ok(labelled)
            },
        )
        .map(|(found, _)| found);
        (got, asked, searched)
    }

    #[test]
    fn an_existing_daemon_stops_setup_before_the_boat_key() {
        let daemon = |id: &str| Daemon {
            id: id.into(),
            state: "idle".into(),
        };
        let one = serde_json::json!({
            "pageInfo": {"hasMore": false},
            "sandboxes": [{"id": "bx_aaaaaaaa", "state": "idle", "name": "sw-boat-runner acme/app"}]
        });
        let (got, asked, searched) = check(one.clone(), false, vec![]);
        assert!(got.unwrap_err().contains("already runs the daemon"));
        assert!(!asked && !searched, "the key was asked for");
        // Even when the list is cut short, what it shows is enough to stop.
        let mut cut = one.clone();
        cut["pageInfo"]["hasMore"] = Value::Bool(true);
        let (got, asked, _) = check(cut, false, vec![]);
        assert!(got.is_err() && !asked);

        // --replace goes on, asks for the key, and replaces what it found.
        let (got, asked, searched) = check(one, true, vec![]);
        assert_eq!(got.unwrap(), [daemon("bx_aaaaaaaa")]);
        assert!(asked && !searched);

        // None listed: the key, and no API search while the list is whole.
        let none = serde_json::json!({"pageInfo": {"hasMore": false}, "sandboxes": []});
        let (got, asked, searched) = check(none, false, vec![daemon("bx_zzzzzzzz")]);
        assert_eq!(got.unwrap(), []);
        assert!(asked && !searched);

        // A list cut short: the label search with the key looks further.
        let more = serde_json::json!({"pageInfo": {"hasMore": true}, "sandboxes": []});
        let (got, asked, searched) = check(more.clone(), false, vec![daemon("bx_zzzzzzzz")]);
        assert!(
            got.unwrap_err()
                .starts_with("boat machine bx_zzzzzzzz (idle)")
        );
        assert!(asked && searched);
        let (got, _, searched) = check(more, false, vec![]);
        assert!(got.unwrap().is_empty() && searched);

        // A failed list stops before the key.
        let mut asked = false;
        let got = check_daemons(
            "acme/app",
            false,
            || Err("boat list failed".into()),
            || {
                asked = true;
                Ok(Secret("key".into()))
            },
            |_| Ok(vec![]),
        );
        assert!(got.is_err() && !asked);
    }

    #[test]
    fn replace_waits_for_doctor_not_just_an_active_service() {
        assert_eq!(
            doctor_quietly(),
            "sudo /usr/local/bin/sw-boat-runner doctor >/dev/null 2>&1"
        );
    }

    #[test]
    fn a_kept_machine_says_how_to_delete_it() {
        let k = kept("bx_aaaaaaaa");
        assert!(
            k.contains("$HOME/.sbr-setup") && k.contains("`boat delete bx_aaaaaaaa --yes`"),
            "{k}"
        );
    }

    #[test]
    fn redirects_are_loopback_callbacks_only() {
        assert!(is_loopback_redirect(&loopback_redirect(54321)));
        assert!(is_loopback_redirect(&loopback_redirect(80)));
        for bad in [
            "http://127.0.0.1:5/callback",
            "http://127.0.0.1:123456/callback",
            "https://127.0.0.1:5000/callback",
            "http://localhost:5000/callback",
            "http://127.0.0.1:5000/callback?x=1",
            "http://127.0.0.1:5000/callback/",
            "http://127.0.0.1:50a0/callback",
            "http://127.0.0.1.evil.com:5000/callback",
        ] {
            assert!(!is_loopback_redirect(bad), "{bad}");
        }
        // The page checks the same pattern.
        let page = include_str!("../../docs/new-app.html");
        assert!(page.contains(r"const LOOPBACK = /^http:\/\/127\.0\.0\.1:[0-9]{2,5}\/callback$/;"));
    }

    #[test]
    fn secrets_go_over_stdin_never_in_a_command() {
        let app = Auth::App {
            id: "42".into(),
            pem: Secret("-----BEGIN PRIVATE KEY-----".into()),
            installation: Some(77),
            staged: None,
        };
        let s = install_script(
            "Surely-Works-LLC/sw-boat-runner",
            "v0.3.0",
            &app,
            &Staged {
                secret_file: "app.pem",
            },
        );
        assert!(s.contains(
            "https://raw.githubusercontent.com/Surely-Works-LLC/sw-boat-runner/v0.3.0/examples/always-on/install.sh"
        ));
        assert!(s.contains(
            "export BOAT_API_KEY SBR_REF=v0.3.0 SBR_UPSTREAM=Surely-Works-LLC/sw-boat-runner\n"
        ));
        assert!(s.contains("BOAT_API_KEY=$(cat boat-key)"));
        assert!(s.contains(
            "export GITHUB_APP_ID=42 GITHUB_APP_PRIVATE_KEY_FILE=\"$d/app.pem\"\n\
             export GITHUB_APP_INSTALLATION_ID=77\n"
        ));
        // The machine's GITHUB_TOKEN stays for install.sh's attestation check; install.sh
        // unsets it for `setup` itself.
        assert!(!s.contains("unset"), "{s}");
        assert!(s.contains("trap 'rm -rf \"$d\"' EXIT"));
        assert!(s.ends_with("bash install.sh policy.toml\n"));
        assert!(!s.contains("PRIVATE KEY"));
        let pat = install_script(
            "acme/sw-boat-runner",
            "main",
            &Auth::Pat(Secret("ghp_x".into())),
            &Staged {
                secret_file: "github-token",
            },
        );
        assert!(pat.contains("GITHUB_TOKEN=$(cat github-token)") && !pat.contains("ghp_x"));
        assert!(pat.contains("export GITHUB_TOKEN SBR_PAT=1\n"), "{pat}");
        // A fork installs from itself, and says so to the machine.
        assert!(pat.contains(
            "https://raw.githubusercontent.com/acme/sw-boat-runner/main/examples/always-on/install.sh"
        ));
        assert!(pat.contains("SBR_UPSTREAM=acme/sw-boat-runner\n"), "{pat}");
        assert!(!s.contains("SBR_PAT"), "{s}");
        assert_eq!(
            write_script("boat-key"),
            "umask 077 && mkdir -p \"$HOME/.sbr-setup\" && cat > \"$HOME/.sbr-setup/boat-key\""
        );
    }

    #[test]
    fn the_release_ref_is_a_plain_name() {
        // SAFETY of the test: only this test reads or writes SBR_REF in this process.
        let check = |r: &str| {
            unsafe { std::env::set_var("SBR_REF", r) };
            let got = release_ref();
            unsafe { std::env::remove_var("SBR_REF") };
            got
        };
        assert_eq!(check("v0.3.0").unwrap(), "v0.3.0");
        assert_eq!(check("feature/x").unwrap(), "feature/x");
        assert_eq!(check("0123abc").unwrap(), "0123abc");
        for bad in ["v1;rm -rf", "-x", "../x", "a b", "$(id)", "a'b"] {
            assert!(check(bad).is_err(), "{bad}");
        }
        assert_eq!(
            release_ref().unwrap(),
            format!("v{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn a_dry_run_prints_the_plan() {
        let p = plan("acme/app", &GitHub::Create, "v0.3.0", false, false);
        assert!(
            p.contains("1. create a private GitHub App for acme/app"),
            "{p}"
        );
        assert!(p.contains("http://127.0.0.1:<random port>/callback"), "{p}");
        assert!(
            p.contains("2. create an always-on boat small machine"),
            "{p}"
        );
        assert!(
            p.contains("$HOME/.sbr-setup/{policy.toml,boat-key,app.pem}"),
            "{p}"
        );
        assert!(p.contains("install.sh at v0.3.0"), "{p}");
        assert!(p.contains("journalctl -u sw-boat-runner -f"), "{p}");
        assert!(
            p.contains(
                "0. look with `boat list --all --json` for a boat machine that already runs \
                 the daemon for acme/app and stop, unless --replace; then ask for BOAT_API_KEY"
            ),
            "{p}"
        );
        assert!(plan("acme/app", &GitHub::Create, "v0.3.0", false, true).contains("(--replace)"));
        let pasted = plan("acme/app", &GitHub::Create, "v0.3.0", true, false);
        assert!(pasted.contains("you paste the code") && !pasted.contains("127.0.0.1"));
        let pat = plan("acme/app", &GitHub::Pat, "main", false, false);
        assert!(
            pat.contains("GITHUB_TOKEN") && pat.contains("github-token"),
            "{pat}"
        );
        let shell = plan("acme/app", &GitHub::Shell("7".into()), "main", false, false);
        assert!(shell.contains("use GitHub App 7"), "{shell}");
    }
}
