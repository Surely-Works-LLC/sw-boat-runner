//! boat REST client: the sandbox calls the tick loop needs, nothing else.
//! Reference: https://docs.boat.dev/api/v1.md

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::{Value, json};

use crate::model::{Cents, JobKey, MachineState};
use crate::transport::{Response, Transport, redact};

pub(crate) const BASE: &str = "https://boat.dev/api/v1";
/// Label every machine of a pool carries: `sw-boat-runner=<pool>`.
const POOL_LABEL: &str = "sw-boat-runner";
/// Label of a forked machine: `sw-boat-runner-job=<job id>-<attempt>`.
const JOB_LABEL: &str = "sw-boat-runner-job";
/// Label of the repository a machine serves: `sw-boat-runner-repo=<owner>/<name>`,
/// lowercased, as GitHub compares them. boat takes any label value up to 256 bytes, and
/// GitHub's names (39 + 1 + 100 bytes of `[A-Za-z0-9._-]`) fit. Machines made before 0.3.0
/// lack it; see `Client::list`.
const REPO_LABEL: &str = "sw-boat-runner-repo";
/// Label naming the daemon process that forked a machine, for `Peers`.
const DAEMON_LABEL: &str = "sw-boat-runner-daemon";
/// The longest `ttlSeconds` boat accepts. Expiry archives a machine; it does not delete it.
pub(crate) const MAX_TTL_S: u64 = 2_592_000;

/// `GET /sandboxes` pages of 200 (the documented maximum); a pool never gets near this.
const PAGE_LIMIT: u32 = 200;
const MAX_PAGES: usize = 20;
/// Wait used when a refusal carries no wait of its own.
const DEFAULT_RETRY_S: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// boat refused the call for now: a 429 (a start limit, a rate limit), or a 503
    /// `no_ready_machine`/`out_of_capacity`. A refused start is not counted; try again
    /// after `retry_after_s`.
    Refused {
        code: String,
        message: String,
        retry_after_s: u64,
    },
    /// 409: state conflict, idempotency in progress, key reused, or a `deploy_failed`
    /// create that boat cannot place.
    Conflict {
        code: String,
        message: String,
    },
    /// 402: the wallet needs billing; nothing will start until a human acts.
    BillingRequired,
    /// 401 or 403: bad key, suspended org, or a machine type the plan does not allow.
    /// `status` tells them apart: see `is_forbidden`.
    Unauthorized {
        status: u16,
        code: String,
        message: String,
    },
    NotFound,
    Network(String),
    Other {
        status: u16,
        code: String,
        message: String,
    },
}

impl Error {
    /// Errors that no later tick can fix.
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(self, Error::BillingRequired | Error::Unauthorized { .. })
    }

    /// A 403. On a read it is the account's (a suspended org, a key without access), but
    /// on a start it can be this request's alone: a `machine_type` the plan does not
    /// allow refuses one pool's starts and nothing else. The caller tells them apart by
    /// what it asked; a 403 for the whole account shows on the next tick's reads.
    pub(crate) fn is_forbidden(&self) -> bool {
        matches!(self, Error::Unauthorized { status: 403, .. })
    }

    /// Failures without a definite answer, so boat may have done what was asked: a
    /// transport error or timeout, a 5xx that is not a refusal, or a 2xx whose body did
    /// not parse. A 4xx is a refusal.
    pub(crate) fn may_have_applied(&self) -> bool {
        match self {
            Error::Network(_) => true,
            Error::Other { status, .. } => !(400..500).contains(status),
            _ => false,
        }
    }

    /// This error with every copy of `secret` in its text replaced, for an answer that
    /// might echo a credential back. Codes are boat's identifiers and stay as sent.
    fn redact(mut self, secret: &str) -> Self {
        match &mut self {
            Error::Refused { message, .. }
            | Error::Conflict { message, .. }
            | Error::Unauthorized { message, .. }
            | Error::Other { message, .. }
            | Error::Network(message) => redact(message, secret),
            Error::BillingRequired | Error::NotFound => {}
        }
        self
    }
}

impl fmt::Display for Error {
    /// `boat <what> (<code>)`, then boat's own words when it sent any.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (what, code, message) = match self {
            Error::BillingRequired => return write!(f, "boat billing required"),
            Error::NotFound => return write!(f, "boat: not found"),
            Error::Network(e) => return write!(f, "boat network error: {e}"),
            Error::Refused { code, message, .. } => ("refused the call".into(), code, message),
            Error::Conflict { code, message } => ("conflict".into(), code, message),
            Error::Unauthorized { code, message, .. } => ("unauthorized".into(), code, message),
            Error::Other {
                status,
                code,
                message,
            } => (format!("error {status}"), code, message),
        };
        write!(f, "boat {what} ({code})")?;
        if !message.is_empty() {
            write!(f, ": {message}")?;
        }
        if let Error::Refused { retry_after_s, .. } = self {
            write!(f, ", retry in {retry_after_s}s")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Sandbox {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// True while the sandbox holds its creator's personal logins (GitHub token, model
    /// logins, secrets). A create with `noEnv` withholds the account's secrets but keeps
    /// logins saved in the snapshot, so a snapshot of a sandbox made without `--no-env`
    /// gives every fork these logins. `None` when boat did not say.
    #[serde(default, rename = "holdsCreatorLogins")]
    pub holds_creator_logins: Option<bool>,
}

impl Sandbox {
    /// The job a machine was forked for, from its `JOB_LABEL`; `None` for a kept machine.
    pub(crate) fn job(&self) -> Option<JobKey> {
        let (id, attempt) = self.labels.get(JOB_LABEL)?.split_once('-')?;
        Some(JobKey {
            id: id.parse().ok()?,
            attempt: attempt.parse().ok()?,
        })
    }

    fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }
}

/// The value of `REPO_LABEL` for `repo`.
pub(crate) fn repo_label(repo: &str) -> String {
    repo.to_ascii_lowercase()
}

/// Whether a machine labelled `sw-boat-runner=<pool>` belongs to `repo`'s daemon: it has
/// `repo`'s `REPO_LABEL`, or none, as every machine made before 0.3.0. Those are adopted
/// so an upgrade does not orphan them; boat cannot add a label to a machine, so they keep
/// lacking it until they are deleted.
fn belongs(sb: &Sandbox, repo: &str) -> bool {
    sb.label(REPO_LABEL).is_none_or(|r| r == repo)
}

/// The labels a fork carries: its pool, its repository and daemon when the client has
/// them, and the job it is for unless it is a kept machine.
fn fork_labels(
    pool: &str,
    owner: Option<&Owner>,
    job: Option<JobKey>,
) -> BTreeMap<&'static str, String> {
    let mut labels = BTreeMap::from([(POOL_LABEL, pool.to_string())]);
    if let Some(owner) = owner {
        labels.insert(REPO_LABEL, owner.repo.clone());
        labels.insert(DAEMON_LABEL, owner.daemon.clone());
    }
    if let Some(job) = job {
        labels.insert(JOB_LABEL, format!("{}-{}", job.id, job.attempt));
    }
    labels
}

/// How much a `stray_report` line matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    /// Nothing to do.
    Info,
    /// Machines nothing deletes, or that two repositories' daemons may share.
    Warn,
}

/// What a listing of every daemon machine on the account says about the policy for `repo`
/// with `pools`: lines to print, none when all is well. Machines of a pool the policy no
/// longer has are never listed by `run` again, so nothing deletes them.
pub(crate) fn stray_report(repo: &str, pools: &[&str], all: &[Sandbox]) -> Vec<(Severity, String)> {
    let repo = repo_label(repo);
    let live = all
        .iter()
        .filter(|sb| machine_state(&sb.state) != MachineState::Gone);
    let mut ours: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut legacy: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut adopted: BTreeMap<&str, usize> = BTreeMap::new();
    // Pools of the policy that another repository's 0.3.0 daemon uses too.
    let mut shared: BTreeSet<&str> = BTreeSet::new();
    for sb in live {
        let Some(pool) = sb.label(POOL_LABEL) else {
            continue;
        };
        let known = pools.contains(&pool);
        match (sb.label(REPO_LABEL), known) {
            (Some(r), false) if r == repo => ours.entry(pool).or_default().push(&sb.id),
            (Some(r), true) if r != repo => {
                shared.insert(pool);
            }
            (None, false) => legacy.entry(pool).or_default().push(&sb.id),
            (None, true) => *adopted.entry(pool).or_default() += 1,
            _ => {}
        }
    }
    let delete = |ids: &[&str]| {
        ids.iter()
            .map(|id| format!("`boat delete {id} --yes`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = Vec::new();
    for (pool, ids) in &ours {
        out.push((
            Severity::Warn,
            format!(
                "pool {pool} is not in the policy but has {} machine(s) of {repo}: {}; \
                 nothing deletes them. Put the pool back and run `sw-boat-runner drain \
                 <policy.toml>`, or once their runners are done: {}",
                ids.len(),
                ids.join(", "),
                delete(ids),
            ),
        ));
    }
    // Unlabelled machines of a pool this policy does not have are almost always another
    // daemon's on the same account, so they get no delete or drain advice.
    for (pool, ids) in &legacy {
        let n = ids.len();
        let (machines, are, them) = if n == 1 {
            ("machine", "is", "it")
        } else {
            ("machines", "are", "them")
        };
        out.push((
            Severity::Info,
            format!(
                "pool {pool}: {n} {machines} from before 0.3.0 {are} not in this policy; \
                 another daemon on this boat account probably owns {them}; left alone"
            ),
        ));
    }
    // Machines without a repo label are a risk only when another repository may use the
    // same pool name on this account.
    let risk = !legacy.is_empty() || !shared.is_empty();
    for (pool, n) in &adopted {
        let line = if risk {
            (
                Severity::Warn,
                format!(
                    "pool {pool}: {n} machine(s) have no repo label (made before 0.3.0) and \
                     are adopted as {repo}'s, and this boat account has machines of another \
                     repository or pool that may share the name {pool}. A daemon older than \
                     0.3.0 filters on the pool label only, so it takes every machine of pool \
                     {pool}, labelled or not: upgrade every repository's daemon to 0.3.0, \
                     then drain pool {pool} (`sw-boat-runner drain <policy.toml>`) so all its \
                     machines carry the repo label"
                ),
            )
        } else {
            (
                Severity::Info,
                format!(
                    "pool {pool}: {n} machine(s) from before 0.3.0 have no repo label and are \
                     adopted as {repo}'s. That is harmless while only {repo} uses the pool \
                     name {pool} on this boat account; draining the pool to relabel them is \
                     optional"
                ),
            )
        };
        out.push(line);
    }
    out
}

/// The repository and process a client's machines belong to.
pub(crate) struct Owner {
    /// `REPO_LABEL`'s value.
    repo: String,
    /// `DAEMON_LABEL`'s value for the forks this process makes.
    daemon: String,
    peers: RefCell<Peers>,
}

/// Spots a second daemon on the same repository and pool: a machine that first shows up
/// in a listing after this process's first one, labelled by another daemon, was forked by
/// a daemon that is still running. A predecessor's last forks can show up for a request
/// timeout and a tick after it was stopped, so those first seen within `grace` of the
/// start are not counted. A second daemon that only uses kept machines forks nothing and
/// is not seen.
struct Peers {
    started: Instant,
    grace: Duration,
    /// Pools listed at least once.
    listed: HashSet<String>,
    /// Machines seen in any listing.
    seen: HashSet<String>,
    /// Daemons already warned about.
    warned: BTreeSet<String>,
}

impl Peers {
    fn note(&mut self, owner_daemon: &str, pool: &str, listed: &[Sandbox]) {
        let first = self.listed.insert(pool.to_string());
        let armed = self.started.elapsed() >= self.grace;
        for sb in listed {
            let new = self.seen.insert(sb.id.clone());
            let Some(daemon) = sb.label(DAEMON_LABEL) else {
                continue;
            };
            if first || !new || !armed || daemon == owner_daemon {
                continue;
            }
            if machine_state(&sb.state) == MachineState::Gone {
                continue;
            }
            if self.warned.insert(daemon.to_string()) {
                eprintln!(
                    "error: machine {} of pool {pool} was forked by another sw-boat-runner \
                     daemon ({daemon}) while this one ({owner_daemon}) runs. Two daemons on \
                     one repository and pool stop, delete and register runners on each \
                     other's machines: stop one of them",
                    sb.id
                );
            }
        }
    }
}

/// What `GET /limits` says about the next start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Quote {
    /// boat would take a start now: `canStart`, with a price. A null price means boat
    /// would refuse it even with `canStart` set.
    pub allows: bool,
    pub cents: Cents,
    /// At a price of 0, the fewest starts left among boat's minute, hour and day windows,
    /// if it sent them.
    pub free_left: Option<u32>,
}

/// Whole cents, at least 1 for any price above 0, so the 0 that marks an included start
/// never stands for a paid one.
fn cents(dollars: Option<f64>) -> Cents {
    dollars
        .filter(|&d| d > 0.0)
        .map_or(0, |d| ((d * 100.0).round() as Cents).max(1))
}

/// boat state → model state. boat's `archived` is a stop with the disk kept, so it maps to
/// `Stopped`; `cancelled` is a create or fork that got no machine, and `deleted` one that
/// is still listed after a delete. An unknown state counts as `Starting`: it holds a
/// `max_boxes` slot and draws no action until it settles.
pub(crate) fn machine_state(state: &str) -> MachineState {
    match state {
        "init" | "provisioning" | "provisioned" | "cloning" => MachineState::Starting,
        "ready" | "idle" | "running" => MachineState::Up,
        "archiving" => MachineState::Stopping,
        "archived" => MachineState::Stopped,
        "error" | "cancelled" | "deleted" => MachineState::Gone,
        _ => MachineState::Starting,
    }
}

/// `bx_` plus 8 characters of boat's id alphabet; never contains `-`.
pub(crate) fn is_sandbox_id(s: &str) -> bool {
    s.strip_prefix("bx_").is_some_and(|rest| {
        rest.len() == 8
            && rest
                .bytes()
                .all(|b| matches!(b, b'2'..=b'9' | b'a'..=b'h' | b'j' | b'k' | b'm' | b'n' | b'p'..=b'z'))
    })
}

pub(crate) struct Client<'t> {
    t: &'t dyn Transport,
    key: String,
    /// Set by `for_repo`: the daemon's machines, which `list` keeps and `fork` labels.
    owner: Option<Owner>,
}

impl<'t> Client<'t> {
    pub(crate) fn new(t: &'t dyn Transport, key: String) -> Self {
        Client {
            t,
            key,
            owner: None,
        }
    }

    /// A client for the daemon `daemon` of `repo`: `list` keeps only `repo`'s machines
    /// and `fork` labels new ones with both. A machine of another daemon that shows up more
    /// than `grace` after now is reported once on stderr; see `Peers`.
    pub(crate) fn for_repo(mut self, repo: &str, daemon: String, grace: Duration) -> Self {
        let peers = Peers {
            started: Instant::now(),
            grace,
            listed: HashSet::new(),
            seen: HashSet::new(),
            warned: BTreeSet::new(),
        };
        self.owner = Some(Owner {
            repo: repo_label(repo),
            daemon,
            peers: RefCell::new(peers),
        });
        self
    }

    /// The one request path: auth, status mapping, and the 2xx body read as `T`. An
    /// empty body reads as `null`. No error carries the API key.
    fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra: &[(&str, &str)],
    ) -> Result<T, Error> {
        let auth = format!("Bearer {}", self.key);
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Accept", "application/json"),
            ("User-Agent", "sw-boat-runner"),
        ];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        headers.extend_from_slice(extra);
        let bytes = body.map(|b| b.to_string().into_bytes());
        let url = format!("{BASE}{path}");
        let result = match self.t.send(method, &url, &headers, bytes.as_deref()) {
            Err(e) => Err(Error::Network(e.0)),
            Ok(resp) if !(200..300).contains(&resp.status) => Err(map_error(&resp)),
            Ok(resp) => {
                let body: &[u8] = if resp.body.is_empty() {
                    b"null"
                } else {
                    &resp.body
                };
                serde_json::from_slice(body).map_err(|e| Error::Other {
                    status: resp.status,
                    code: "bad_json".into(),
                    message: e.to_string(),
                })
            }
        };
        result.map_err(|e| e.redact(&self.key))
    }

    /// A call whose answer this client only needs to be readable JSON (or empty).
    fn send(&self, method: &str, path: &str, body: &Value) -> Result<(), Error> {
        self.call::<IgnoredAny>(method, path, Some(body), &[])
            .map(drop)
    }

    /// Every sandbox of `pool`, in any state, following `pageInfo.nextCursor`. With an
    /// owner, only the machines of its repository, and those with no repository label (see
    /// `belongs`); boat filters on the pool label, the rest is done here.
    pub(crate) fn list(&self, pool: &str) -> Result<Vec<Sandbox>, Error> {
        let mut listed = self.list_labelled(POOL_LABEL, pool)?;
        if let Some(owner) = &self.owner {
            listed.retain(|sb| belongs(sb, &owner.repo));
            owner.peers.borrow_mut().note(&owner.daemon, pool, &listed);
        }
        Ok(listed)
    }

    /// Every sandbox on the account with a pool label, of any repository and pool, for
    /// `stray_report`. boat has no filter on a label's key alone, so this reads the whole
    /// account's list.
    pub(crate) fn list_daemon_machines(&self) -> Result<Vec<Sandbox>, Error> {
        let mut all = self.list_query(None)?;
        all.retain(|sb| sb.labels.contains_key(POOL_LABEL));
        Ok(all)
    }

    /// Every sandbox labelled `key=value`, in any state, following `pageInfo.nextCursor`.
    pub(crate) fn list_labelled(&self, key: &str, value: &str) -> Result<Vec<Sandbox>, Error> {
        self.list_query(Some((key, value)))
    }

    /// Every sandbox, or those labelled `key=value`, following `pageInfo.nextCursor`.
    fn list_query(&self, label: Option<(&str, &str)>) -> Result<Vec<Sandbox>, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            sandboxes: Vec<Sandbox>,
            page_info: PageInfo,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            next_cursor: Option<String>,
            has_more: bool,
        }
        let filter = label.map_or(String::new(), |(key, value)| {
            format!("label={}&", encode(&format!("{key}={value}")))
        });
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut path = format!("/sandboxes?{filter}limit={PAGE_LIMIT}");
            if let Some(c) = &cursor {
                path.push_str("&cursor=");
                path.push_str(&encode(c));
            }
            let page: Page = self.call("GET", &path, None, &[])?;
            out.extend(page.sandboxes);
            match page.page_info.next_cursor {
                Some(c) if page.page_info.has_more => cursor = Some(c),
                _ => return Ok(out),
            }
        }
        Err(Error::Other {
            status: 0,
            code: "too_many_pages".into(),
            message: match label {
                Some((key, value)) => {
                    format!("more than {MAX_PAGES} pages of sandboxes labelled {key}={value}")
                }
                None => format!("more than {MAX_PAGES} pages of sandboxes"),
            },
        })
    }

    /// One sandbox, with `holdsCreatorLogins`.
    pub(crate) fn get(&self, id: &str) -> Result<Sandbox, Error> {
        #[derive(Deserialize)]
        struct Info {
            sandbox: Sandbox,
        }
        let info: Info = self.call("GET", &format!("/sandboxes/{id}"), None, &[])?;
        Ok(info.sandbox)
    }

    /// Resumes with `noEnv`: boat withholds the account's secrets and scrubs owner secrets
    /// the restored snapshot holds before the machine is reachable. One-way; the machine
    /// stays no-env.
    pub(crate) fn resume(&self, id: &str) -> Result<(), Error> {
        let body = json!({ "noEnv": true });
        self.send("POST", &format!("/sandboxes/{id}/resume"), &body)
    }

    /// Returns at once; 200 when already stopped, 202 while the stop runs.
    pub(crate) fn stop(&self, id: &str) -> Result<(), Error> {
        self.send("POST", &format!("/sandboxes/{id}/stop"), &json!({}))
    }

    /// Permanently deletes the sandbox and its disk.
    pub(crate) fn delete(&self, id: &str) -> Result<(), Error> {
        let path = format!("/sandboxes/{id}");
        self.call::<IgnoredAny>("DELETE", &path, None, &[("X-Ascii-Confirm-Delete", id)])
            .map(drop)
    }

    /// Starts a new machine from the saved snapshot, labelled with `pool` and, for a job
    /// fork, `job`. boat returns the first sandbox again for a repeat of the same key and
    /// body within 24 h, even a deleted one. `ttl_s` must be 1 to `MAX_TTL_S`. `noEnv`
    /// keeps the account's secrets out of the machine, because CI jobs run arbitrary code
    /// there. It does not scrub logins saved in the snapshot; `holds_creator_logins`
    /// reports those.
    pub(crate) fn fork(
        &self,
        snapshot: &str,
        machine_type: &str,
        pool: &str,
        job: Option<JobKey>,
        ttl_s: u64,
        idempotency_key: &str,
    ) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Created {
            sandbox: Sandbox,
        }
        assert!((1..=MAX_TTL_S).contains(&ttl_s), "ttl {ttl_s}");
        let body = json!({
            "from": snapshot,
            "type": machine_type,
            "labels": fork_labels(pool, self.owner.as_ref(), job),
            "ttlSeconds": ttl_s,
            "noEnv": true,
        });
        let key = [("Idempotency-Key", idempotency_key)];
        let created: Created = self.call("POST", "/sandboxes", Some(&body), &key)?;
        Ok(created.sandbox.id)
    }

    /// Writes a UTF-8 file under `/home/user` or `/tmp`; 200 means it is on disk. The
    /// content may be a credential, so no error quotes it.
    pub(crate) fn write_file(&self, id: &str, path: &str, content: &str) -> Result<(), Error> {
        let body = json!({ "path": path, "content": content, "encoding": "utf8" });
        self.send("PUT", &format!("/sandboxes/{id}/files"), &body)
            .map_err(|e| e.redact(content))
    }

    /// Starts `command` in the background. boat keeps every command in its history, so the
    /// command must never contain a secret.
    pub(crate) fn exec_detached(&self, id: &str, command: &str) -> Result<(), Error> {
        let body = json!({ "command": command, "detached": true });
        self.send("POST", &format!("/sandboxes/{id}/commands"), &body)
    }

    pub(crate) fn limits(&self) -> Result<Quote, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Limits {
            can_start: bool,
            /// 0 inside every limit, `None` when boat would refuse the start.
            #[serde(default)]
            next_start_dollars: Option<f64>,
            #[serde(default)]
            starts: Value,
        }
        let l: Limits = self.call("GET", "/limits", None, &[])?;
        let cents = cents(l.next_start_dollars);
        let free_left = ["minute", "hour", "day"]
            .iter()
            .filter_map(|w| l.starts.get(w)?.get("remaining")?.as_u64())
            .min()
            .filter(|_| cents == 0)
            .map(|left| u32::try_from(left).unwrap_or(u32::MAX));
        Ok(Quote {
            allows: l.can_start && l.next_start_dollars.is_some(),
            cents,
            free_left,
        })
    }
}

/// The disk a machine type gives its own files, in bytes (docs: machines). A sandbox or
/// saved snapshot larger than this does not start on that type: boat answers 409
/// `deploy_failed`.
pub(crate) fn disk_budget_bytes(machine_type: &str) -> Option<u64> {
    const GB: u64 = 1_000_000_000;
    match machine_type {
        "small" | "default" => Some(51 * GB),
        "large" => Some(111 * GB),
        "xlarge" => Some(251 * GB),
        _ => None,
    }
}

/// What `GET /sandboxes/{id}` says about a machine `snapshot` waits on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Detail {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub holds_creator_logins: Option<bool>,
    /// True once every file of the snapshot the machine came from is on its disk.
    #[serde(default)]
    pub hydrated: Option<bool>,
    #[serde(default)]
    pub hydration: Option<Hydration>,
    #[serde(default, rename = "type")]
    pub machine_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Hydration {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub percent: Option<u32>,
}

/// One entry of `GET /saved-snapshots`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedSnapshot {
    pub name: String,
    /// `saving`, `ready` or `failed`.
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub source_sandbox_id: Option<String>,
    /// Restored content size of the frozen state.
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// A detached command's state, from `GET /sandboxes/{id}/commands/{processId}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandStatus {
    pub running: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

/// A `POST /sandboxes` without `from`: a machine from boat's base image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewSandbox {
    pub machine_type: String,
    /// `None` sends `ttlSeconds: null`, which turns auto-stop off.
    pub ttl_s: Option<u64>,
    /// Withholds the account's secrets; a fresh sandbox then holds no creator logins.
    pub no_env: bool,
    pub labels: BTreeMap<&'static str, String>,
    pub name: String,
}

impl NewSandbox {
    fn body(&self) -> Value {
        if let Some(ttl) = self.ttl_s {
            assert!((1..=MAX_TTL_S).contains(&ttl), "ttl {ttl}");
        }
        json!({
            "type": self.machine_type,
            "ttlSeconds": self.ttl_s,
            "noEnv": self.no_env,
            "labels": self.labels,
            "name": self.name,
        })
    }
}

/// The calls `snapshot` makes, which the tick loop never does.
impl Client<'_> {
    /// `get` with the hydration progress and machine type.
    pub(crate) fn detail(&self, id: &str) -> Result<Detail, Error> {
        #[derive(Deserialize)]
        struct Info {
            sandbox: Detail,
        }
        let info: Info = self.call("GET", &format!("/sandboxes/{id}"), None, &[])?;
        Ok(info.sandbox)
    }

    /// Forks a running or stopped sandbox with `noEnv`, which scrubs the creator logins
    /// the source holds; the source is not changed. `machine_type` `None` keeps the
    /// source's. A repeat with the same key and body returns the first fork.
    pub(crate) fn fork_sandbox(
        &self,
        source: &str,
        machine_type: Option<&str>,
        labels: &BTreeMap<&str, String>,
        ttl_s: u64,
        idempotency_key: &str,
    ) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Created {
            sandbox: Sandbox,
        }
        assert!((1..=MAX_TTL_S).contains(&ttl_s), "ttl {ttl_s}");
        let mut body = json!({ "labels": labels, "ttlSeconds": ttl_s, "noEnv": true });
        if let Some(t) = machine_type {
            body["type"] = Value::from(t);
        }
        let key = [("Idempotency-Key", idempotency_key)];
        let path = format!("/sandboxes/{source}/fork");
        let created: Created = self.call("POST", &path, Some(&body), &key)?;
        Ok(created.sandbox.id)
    }

    /// Creates a sandbox from boat's base image. `ttl_s` `None` turns auto-stop off. A
    /// repeat with the same key and body returns the first sandbox.
    pub(crate) fn create_sandbox(
        &self,
        spec: &NewSandbox,
        idempotency_key: &str,
    ) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Created {
            sandbox: Sandbox,
        }
        let body = spec.body();
        let key = [("Idempotency-Key", idempotency_key)];
        let created: Created = self.call("POST", "/sandboxes", Some(&body), &key)?;
        Ok(created.sandbox.id)
    }

    /// Starts `command` detached and returns its process id for `command_status`. The
    /// command is kept in boat's history, so it must never contain a secret.
    pub(crate) fn exec_start(&self, id: &str, command: &str) -> Result<u64, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Started {
            process_id: u64,
        }
        let body = json!({ "command": command, "detached": true });
        let started: Started = self.call(
            "POST",
            &format!("/sandboxes/{id}/commands"),
            Some(&body),
            &[],
        )?;
        Ok(started.process_id)
    }

    pub(crate) fn command_status(&self, id: &str, process_id: u64) -> Result<CommandStatus, Error> {
        self.call(
            "GET",
            &format!("/sandboxes/{id}/commands/{process_id}"),
            None,
            &[],
        )
    }

    /// Saves the sandbox's current state under `name`; answers at once with `saving`.
    /// Reusing a name replaces that snapshot once the new save is ready.
    pub(crate) fn save_snapshot(&self, sandbox: &str, name: &str) -> Result<(), Error> {
        let body = json!({ "sandboxId": sandbox, "name": name });
        self.send("POST", "/saved-snapshots", &body)
    }

    /// Removes the saved snapshot called `name`; one already gone counts as removed.
    pub(crate) fn delete_saved_snapshot(&self, name: &str) -> Result<(), Error> {
        let path = format!("/saved-snapshots/{}", encode(name));
        match self.call::<IgnoredAny>("DELETE", &path, None, &[]) {
            Ok(_) | Err(Error::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Saved snapshots, newest first; only the one called `name` when given.
    pub(crate) fn saved_snapshots(&self, name: Option<&str>) -> Result<Vec<SavedSnapshot>, Error> {
        #[derive(Deserialize)]
        struct List {
            snapshots: Vec<SavedSnapshot>,
        }
        let path = match name {
            Some(n) => format!("/saved-snapshots?name={}", encode(n)),
            None => "/saved-snapshots".to_string(),
        };
        let list: List = self.call("GET", &path, None, &[])?;
        Ok(list.snapshots)
    }
}

/// Maps a non-2xx response. A 429 always sends `Retry-After`; a 503 puts
/// `retryAfterSeconds` in the body; `IP_RATE_LIMIT_EXCEEDED` uses `retryAfter`.
pub(crate) fn map_error(resp: &Response) -> Error {
    let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let text = |key: &str| {
        body.get(key)
            .or_else(|| body.get("error").and_then(|e| e.get(key)))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let code = text("code");
    // boat sends its human text as `message`, as `error` when that is a string, or as
    // `error.message` beside `error.code`.
    let message = Some(text("message"))
        .filter(|m| !m.is_empty())
        .or_else(|| {
            body.get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let retry_after_s = resp
        .number("retry-after")
        .or_else(|| body.get("retryAfterSeconds").and_then(Value::as_u64))
        .or_else(|| body.get("retryAfter").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_RETRY_S);
    match resp.status {
        429 => Error::Refused {
            code,
            message,
            retry_after_s,
        },
        503 if code == "no_ready_machine" || code == "out_of_capacity" => Error::Refused {
            code,
            message,
            retry_after_s,
        },
        409 => Error::Conflict { code, message },
        402 => Error::BillingRequired,
        401 | 403 => Error::Unauthorized {
            status: resp.status,
            code,
            message,
        },
        404 => Error::NotFound,
        status => Error::Other {
            status,
            code,
            message,
        },
    }
}

/// Percent-encodes a query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake::{Fake, respond};

    fn err(status: u16, headers: &[(&str, &str)], body: &str) -> Error {
        map_error(&respond(status, headers, body))
    }

    #[test]
    fn maps_errors() {
        assert_eq!(
            err(429, &[("Retry-After", "30")], r#"{"code":"limit_reached"}"#),
            Error::Refused {
                code: "limit_reached".into(),
                message: String::new(),
                retry_after_s: 30
            }
        );
        assert_eq!(
            err(
                503,
                &[],
                r#"{"ok":false,"code":"no_ready_machine","retryAfterSeconds":5}"#
            ),
            Error::Refused {
                code: "no_ready_machine".into(),
                message: String::new(),
                retry_after_s: 5
            }
        );
        assert_eq!(
            err(503, &[], r#"{"error":{"code":"out_of_capacity"}}"#),
            Error::Refused {
                code: "out_of_capacity".into(),
                message: String::new(),
                retry_after_s: DEFAULT_RETRY_S
            }
        );
        assert!(matches!(
            err(503, &[], r#"{"code":"browser_not_ready"}"#),
            Error::Other { status: 503, .. }
        ));
        assert_eq!(
            err(
                409,
                &[("Retry-After", "4")],
                r#"{"code":"sandbox_stopping"}"#
            ),
            Error::Conflict {
                code: "sandbox_stopping".into(),
                message: String::new(),
            }
        );
        assert!(err(402, &[], r#"{"code":"billing_required"}"#).is_fatal());
        assert!(err(401, &[], r#"{"code":"unauthorized"}"#).is_fatal());
        assert!(err(403, &[], r#"{"code":"org_suspended"}"#).is_fatal());
        assert!(err(403, &[], r#"{"code":"machine_type_not_allowed"}"#).is_forbidden());
        assert!(!err(401, &[], r#"{"code":"unauthorized"}"#).is_forbidden());
        assert!(!err(402, &[], r#"{"code":"billing_required"}"#).is_forbidden());
        assert_eq!(err(404, &[], ""), Error::NotFound);
        assert_eq!(
            err(500, &[], r#"{"code":"internal","message":"boom"}"#),
            Error::Other {
                status: 500,
                code: "internal".into(),
                message: "boom".into()
            }
        );
        assert!(!err(429, &[], "").is_fatal());
    }

    #[test]
    fn errors_keep_boats_message() {
        let deploy_failed = err(
            409,
            &[],
            r#"{"code":"deploy_failed","error":"This sandbox uses 92.8 GB of disk, which does not fit any available 'default' machine (largest budget: 76.0 GB).","status":409}"#,
        );
        assert_eq!(
            deploy_failed.to_string(),
            "boat conflict (deploy_failed): This sandbox uses 92.8 GB of disk, which does not \
             fit any available 'default' machine (largest budget: 76.0 GB)."
        );
        assert_eq!(
            err(
                409,
                &[],
                r#"{"error":{"code":"sandbox_stopping","message":"still stopping"}}"#
            ),
            Error::Conflict {
                code: "sandbox_stopping".into(),
                message: "still stopping".into(),
            }
        );
        assert_eq!(
            err(403, &[], r#"{"code":"org_suspended","message":"pay up"}"#).to_string(),
            "boat unauthorized (org_suspended): pay up"
        );
        assert_eq!(
            err(
                429,
                &[("Retry-After", "7")],
                r#"{"code":"limit_reached","error":"5 starts a minute"}"#
            )
            .to_string(),
            "boat refused the call (limit_reached): 5 starts a minute, retry in 7s"
        );
        assert_eq!(
            err(500, &[], r#"{"code":"internal","error":"boom"}"#).to_string(),
            "boat error 500 (internal): boom"
        );
        assert_eq!(
            err(409, &[], r#"{"code":"sandbox_stopping"}"#).to_string(),
            "boat conflict (sandbox_stopping)"
        );
    }

    #[test]
    fn only_answerless_failures_may_have_applied() {
        assert!(Error::Network("timed out".into()).may_have_applied());
        assert!(err(500, &[], r#"{"code":"internal"}"#).may_have_applied());
        assert!(err(503, &[], r#"{"code":"browser_not_ready"}"#).may_have_applied());
        // A create that boat accepted but answered with a body this client cannot read.
        for body in ["not json", r#"{"ok":true}"#] {
            let fake = Fake::new(move |_| respond(202, &[], body));
            let e = Client::new(&fake, "k".into())
                .fork("base", "large", "ci", None, 60, "key")
                .unwrap_err();
            assert!(e.may_have_applied(), "{e}");
        }
        assert!(!err(429, &[("Retry-After", "30")], "").may_have_applied());
        assert!(!err(503, &[], r#"{"code":"no_ready_machine"}"#).may_have_applied());
        assert!(!err(409, &[], r#"{"code":"idempotency_in_progress"}"#).may_have_applied());
        assert!(!err(422, &[], r#"{"code":"invalid_request"}"#).may_have_applied());
        assert!(!err(402, &[], "").may_have_applied());
    }

    #[test]
    fn maps_states() {
        assert_eq!(machine_state("cloning"), MachineState::Starting);
        assert_eq!(machine_state("ready"), MachineState::Up);
        assert_eq!(machine_state("running"), MachineState::Up);
        assert_eq!(machine_state("archiving"), MachineState::Stopping);
        assert_eq!(machine_state("archived"), MachineState::Stopped);
        assert_eq!(machine_state("cancelled"), MachineState::Gone);
        assert_eq!(machine_state("deleted"), MachineState::Gone);
        assert_eq!(machine_state("init"), MachineState::Starting);
        assert_eq!(machine_state("something_new"), MachineState::Starting);
    }

    #[test]
    fn checks_ids() {
        assert!(is_sandbox_id("bx_23456789"));
        assert!(is_sandbox_id("bx_abcdefgh"));
        assert!(!is_sandbox_id("bx_abcdefgi"));
        assert!(!is_sandbox_id("bx_1234567"));
        assert!(!is_sandbox_id("bx_2345678-"));
    }

    #[test]
    fn list_follows_cursor() {
        let fake = Fake::new(|s| {
            if s.url.contains("cursor=c%2F2") {
                respond(
                    200,
                    &[],
                    r#"{"sandboxes":[{"id":"bx_33333333","state":"archived"}],
                        "pageInfo":{"nextCursor":null,"hasMore":false,"limit":200}}"#,
                )
            } else {
                respond(
                    200,
                    &[],
                    r#"{"sandboxes":[{"id":"bx_22222222","state":"idle",
                        "labels":{"sw-boat-runner":"ci"}}],
                        "pageInfo":{"nextCursor":"c/2","hasMore":true,"limit":200}}"#,
                )
            }
        });
        let boat = Client::new(&fake, "boat_k".into());
        let got = boat.list("ci").unwrap();
        let ids: Vec<_> = got.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["bx_22222222", "bx_33333333"]);
        assert_eq!(got[0].labels["sw-boat-runner"], "ci");
        let sent = fake.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0].url,
            "https://boat.dev/api/v1/sandboxes?label=sw-boat-runner%3Dci&limit=200"
        );
        assert_eq!(
            sent[1].url,
            "https://boat.dev/api/v1/sandboxes?label=sw-boat-runner%3Dci&limit=200&cursor=c%2F2"
        );
        assert_eq!(sent[0].header("authorization"), Some("Bearer boat_k"));
    }

    /// A listing for pool `ci`: this repository's machine, one from before 0.3.0, another
    /// repository's, and this repository's in other case.
    fn shared_account() -> Fake {
        Fake::new(|_| {
            respond(
                200,
                &[],
                r#"{"sandboxes":[
                    {"id":"bx_22222222","state":"idle",
                     "labels":{"sw-boat-runner":"ci","sw-boat-runner-repo":"o/r"}},
                    {"id":"bx_33333333","state":"idle","labels":{"sw-boat-runner":"ci"}},
                    {"id":"bx_44444444","state":"idle",
                     "labels":{"sw-boat-runner":"ci","sw-boat-runner-repo":"o/other"}},
                    {"id":"bx_55555555","state":"idle",
                     "labels":{"sw-boat-runner":"ci","sw-boat-runner-repo":"o/r"}}],
                    "pageInfo":{"nextCursor":null,"hasMore":false}}"#,
            )
        })
    }

    /// Two repositories' daemons with the same pool name on one boat account each see
    /// only their own machines, and both adopt the ones made before the repo label.
    #[test]
    fn a_daemon_lists_only_its_repositorys_machines() {
        let fake = shared_account();
        let ids = |repo: &str| {
            let boat = Client::new(&fake, "k".into()).for_repo(repo, "d".into(), Duration::ZERO);
            let listed = boat.list("ci").unwrap();
            listed.into_iter().map(|sb| sb.id).collect::<Vec<_>>()
        };
        assert_eq!(ids("O/R"), ["bx_22222222", "bx_33333333", "bx_55555555"]);
        assert_eq!(ids("o/other"), ["bx_33333333", "bx_44444444"]);
        // Without an owner (snapshot, setup), every machine of the pool.
        assert_eq!(Client::new(&fake, "k".into()).list("ci").unwrap().len(), 4);
        // The account-wide listing has no label filter.
        let boat = Client::new(&fake, "k".into());
        assert_eq!(boat.list_daemon_machines().unwrap().len(), 4);
        let last = fake.sent().pop().unwrap();
        assert_eq!(last.url, "https://boat.dev/api/v1/sandboxes?limit=200");
    }

    #[test]
    fn a_daemons_forks_carry_its_repository_and_name() {
        let fake = Fake::new(|_| {
            respond(
                202,
                &[],
                r#"{"sandbox":{"id":"bx_44444444","state":"init"}}"#,
            )
        });
        let boat =
            Client::new(&fake, "k".into()).for_repo("Octo/Repo", "h-1-9".into(), Duration::ZERO);
        boat.fork("runner-base", "large", "ci", None, 3600, "key")
            .unwrap();
        let body: Value = serde_json::from_slice(fake.sent()[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body["labels"],
            json!({"sw-boat-runner":"ci","sw-boat-runner-repo":"octo/repo",
                   "sw-boat-runner-daemon":"h-1-9"})
        );
    }

    /// Pre-0.3.0 machines of a policy pool are only an info line, unless the account shows
    /// that another repository may use the same pool name.
    #[test]
    fn adopted_machines_warn_only_when_another_repository_may_share_the_pool() {
        let sb = |id: &str, pool: &str, repo: Option<&str>| {
            let mut labels = BTreeMap::from([(POOL_LABEL.to_string(), pool.to_string())]);
            if let Some(r) = repo {
                labels.insert(REPO_LABEL.into(), r.into());
            }
            Sandbox {
                id: id.into(),
                state: "idle".into(),
                labels,
                holds_creator_logins: None,
            }
        };
        let severities = |all: &[Sandbox]| {
            stray_report("O/R", &["ci"], all)
                .into_iter()
                .map(|(s, _)| s)
                .collect::<Vec<_>>()
        };
        let mine = [
            sb("bx_22222222", "ci", None),
            sb("bx_33333333", "ci", Some("o/r")),
        ];
        assert_eq!(severities(&mine), [Severity::Info]);
        let line = &stray_report("o/r", &["ci"], &mine)[0].1;
        assert!(
            line.contains("harmless") && line.contains("optional"),
            "{line}"
        );

        // Another repository's 0.3.0 daemon has a pool `ci` too.
        let shared = [mine[0].clone(), sb("bx_44444444", "ci", Some("o/other"))];
        assert_eq!(severities(&shared), [Severity::Warn]);
        let line = &stray_report("o/r", &["ci"], &shared)[0].1;
        assert!(line.contains("upgrade every repository's daemon"), "{line}");

        // Unlabelled machines of a pool this policy does not have: almost always another
        // daemon's, so only an info line with no delete command, but they may be a
        // daemon older than 0.3.0, so adopted machines warn.
        let unknown = [
            mine[0].clone(),
            sb("bx_55555555", "batch", None),
            sb("bx_66666666", "batch", None),
        ];
        assert_eq!(severities(&unknown), [Severity::Info, Severity::Warn]);
        let line = &stray_report("o/r", &["ci"], &unknown)[0].1;
        assert_eq!(
            line,
            "pool batch: 2 machines from before 0.3.0 are not in this policy; another daemon \
             on this boat account probably owns them; left alone"
        );
        assert!(!line.contains("bx_") && !line.contains("delete") && !line.contains("drain"));

        // Labelled with this repository in a pool the policy no longer has: surely ours,
        // so the delete and drain advice stays.
        let renamed = [sb("bx_77777777", "old", Some("o/r"))];
        let report = stray_report("o/r", &["ci"], &renamed);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].0, Severity::Warn);
        assert!(
            report[0].1.contains("`boat delete bx_77777777 --yes`")
                && report[0].1.contains("sw-boat-runner drain"),
            "{}",
            report[0].1
        );
        assert!(severities(&[]).is_empty());
    }

    /// A machine another daemon forked after this one's first listing is reported once;
    /// machines in the first listing, this daemon's own and pre-0.3.0 ones are not.
    #[test]
    fn spots_a_second_daemon_on_the_same_pool() {
        let sandbox = |id: &str, daemon: Option<&str>| {
            let mut labels = BTreeMap::from([(POOL_LABEL.to_string(), "ci".to_string())]);
            if let Some(d) = daemon {
                labels.insert(DAEMON_LABEL.into(), d.into());
            }
            Sandbox {
                id: id.into(),
                state: "idle".into(),
                labels,
                holds_creator_logins: None,
            }
        };
        let mut peers = Peers {
            started: Instant::now(),
            grace: Duration::ZERO,
            listed: HashSet::new(),
            seen: HashSet::new(),
            warned: BTreeSet::new(),
        };
        let old = sandbox("bx_22222222", Some("a-1-1"));
        peers.note("me", "ci", std::slice::from_ref(&old));
        assert!(peers.warned.is_empty());
        let later = [
            old,
            sandbox("bx_33333333", Some("me")),
            sandbox("bx_44444444", None),
        ];
        peers.note("me", "ci", &later);
        assert!(peers.warned.is_empty());
        let foreign = [
            sandbox("bx_55555555", Some("b-2-2")),
            sandbox("bx_66666666", Some("b-2-2")),
        ];
        peers.note("me", "ci", &foreign);
        assert_eq!(peers.warned.iter().collect::<Vec<_>>(), ["b-2-2"]);

        // Within the grace after a start, a predecessor's last fork is not counted.
        let mut fresh = Peers {
            grace: Duration::from_secs(3600),
            ..peers
        };
        fresh.warned.clear();
        fresh.note("me", "ci", &[sandbox("bx_77777777", Some("c-3-3"))]);
        assert!(fresh.warned.is_empty());
    }

    #[test]
    fn fork_sends_idempotency_key_and_labels() {
        let fake = Fake::new(|_| {
            respond(
                202,
                &[],
                r#"{"type":"sandbox.created","status":"provisioning",
                    "sandbox":{"id":"bx_44444444","state":"provisioning"}}"#,
            )
        });
        let boat = Client::new(&fake, "k".into());
        let job = JobKey { id: 7, attempt: 2 };
        let id = boat
            .fork("runner-base", "large", "ci", Some(job), 3600, "7-2")
            .unwrap();
        assert_eq!(id, "bx_44444444");
        let sent = fake.sent();
        assert_eq!(sent[0].method, "POST");
        assert_eq!(sent[0].url, "https://boat.dev/api/v1/sandboxes");
        assert_eq!(sent[0].header("idempotency-key"), Some("7-2"));
        let body: Value = serde_json::from_slice(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"from":"runner-base","type":"large","ttlSeconds":3600,"noEnv":true,
                   "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"7-2"}})
        );
    }

    #[test]
    fn resume_scrubs_owner_secrets() {
        let fake = Fake::new(|_| respond(202, &[], "{}"));
        Client::new(&fake, "k".into())
            .resume("bx_55555555")
            .unwrap();
        let sent = fake.sent();
        assert_eq!(
            sent[0].url,
            "https://boat.dev/api/v1/sandboxes/bx_55555555/resume"
        );
        assert_eq!(sent[0].body_text(), r#"{"noEnv":true}"#);
    }

    #[test]
    fn get_reads_holds_creator_logins() {
        let fake = Fake::new(|s| {
            let holds = match s.url.as_str() {
                u if u.ends_with("bx_22222222") => r#","holdsCreatorLogins":true"#,
                u if u.ends_with("bx_33333333") => r#","holdsCreatorLogins":false"#,
                _ => "",
            };
            let body = format!(
                r#"{{"ok":true,"type":"sandbox.info","sandbox":{{"id":"bx_22222222",
                    "state":"idle","labels":{{"sw-boat-runner":"ci"}}{holds}}}}}"#
            );
            respond(200, &[], &body)
        });
        let boat = Client::new(&fake, "k".into());
        let holds = |id| boat.get(id).unwrap().holds_creator_logins;
        assert_eq!(holds("bx_22222222"), Some(true));
        assert_eq!(holds("bx_33333333"), Some(false));
        assert_eq!(holds("bx_44444444"), None);
        assert_eq!(
            fake.sent()[0].url,
            "https://boat.dev/api/v1/sandboxes/bx_22222222"
        );
        assert_eq!(fake.sent()[0].method, "GET");
    }

    #[test]
    fn delete_confirms_id() {
        let fake = Fake::new(|_| respond(202, &[], "{}"));
        Client::new(&fake, "k".into())
            .delete("bx_55555555")
            .unwrap();
        let sent = fake.sent();
        assert_eq!(sent[0].method, "DELETE");
        assert_eq!(
            sent[0].header("x-ascii-confirm-delete"),
            Some("bx_55555555")
        );
    }

    #[test]
    fn limits_report_the_fewest_starts_left() {
        let fake = Fake::new(|_| {
            respond(
                200,
                &[],
                r#"{"canStart":true,"nextStartDollars":0,"starts":{"unlimited":false,
                    "minute":{"limit":10,"used":3,"remaining":7},
                    "hour":{"limit":50,"used":46,"remaining":4},
                    "day":{"limit":150,"used":47,"remaining":103}}}"#,
            )
        });
        let limits = Client::new(&fake, "k".into()).limits().unwrap();
        assert_eq!(limits.free_left, Some(4));
    }

    /// Windows are reported only for included starts: past them the price is not known.
    #[test]
    fn limits_report_no_free_starts_at_a_price() {
        let fake = Fake::new(|_| {
            respond(
                200,
                &[],
                r#"{"canStart":true,"nextStartDollars":0.03,
                    "starts":{"day":{"remaining":3}}}"#,
            )
        });
        let quote = Client::new(&fake, "k".into()).limits().unwrap();
        assert_eq!(
            quote,
            Quote {
                allows: true,
                cents: 3,
                free_left: None
            }
        );
    }

    #[test]
    fn limits_null_price() {
        let fake = Fake::new(|_| respond(200, &[], r#"{"canStart":true,"nextStartDollars":null}"#));
        let limits = Client::new(&fake, "k".into()).limits().unwrap();
        // boat would refuse a start it cannot price, even with `canStart` set.
        assert!(!limits.allows);
        assert_eq!(limits.cents, 0);
        assert_eq!(limits.free_left, None);
    }

    /// Only a start boat quotes at exactly 0 is included; a fraction of a cent is paid.
    #[test]
    fn cents_never_round_a_paid_start_to_free() {
        assert_eq!(cents(Some(0.29)), 29);
        assert_eq!(cents(Some(0.07)), 7);
        assert_eq!(cents(Some(0.004)), 1);
        assert_eq!(cents(Some(0.0)), 0);
        assert_eq!(cents(Some(-1.0)), 0);
        assert_eq!(cents(None), 0);
    }

    #[test]
    fn job_labels_round_trip() {
        let sandbox = |job: Option<JobKey>| Sandbox {
            id: "bx_22222222".into(),
            state: "idle".into(),
            labels: fork_labels("ci", None, job)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            holds_creator_logins: None,
        };
        let job = JobKey { id: 9, attempt: 3 };
        assert_eq!(sandbox(Some(job)).job(), Some(job));
        assert_eq!(sandbox(None).job(), None);
        let mut bad = sandbox(None);
        bad.labels.insert(JOB_LABEL.into(), "9".into());
        assert_eq!(bad.job(), None);
    }

    #[test]
    fn fork_sandbox_scrubs_logins_and_keeps_the_source() {
        let fake = Fake::new(|_| {
            respond(
                202,
                &[],
                r#"{"ok":true,"type":"sandbox.forking","id":"bx_44444444",
                    "sandbox":{"id":"bx_44444444","state":"provisioning"}}"#,
            )
        });
        let boat = Client::new(&fake, "k".into());
        let labels = BTreeMap::from([("sw-boat-runner-snapshot", "ci-1".to_string())]);
        let id = boat
            .fork_sandbox("bx_22222222", Some("large"), &labels, 600, "key-1")
            .unwrap();
        assert_eq!(id, "bx_44444444");
        boat.fork_sandbox("bx_22222222", None, &labels, 600, "key-2")
            .unwrap();
        let sent = fake.sent();
        assert_eq!(
            sent[0].url,
            "https://boat.dev/api/v1/sandboxes/bx_22222222/fork"
        );
        assert_eq!(sent[0].header("idempotency-key"), Some("key-1"));
        let body = |i: usize| -> Value {
            serde_json::from_slice(sent[i].body.as_deref().unwrap()).unwrap()
        };
        assert_eq!(
            body(0),
            json!({"labels":{"sw-boat-runner-snapshot":"ci-1"},"ttlSeconds":600,
                   "noEnv":true,"type":"large"})
        );
        assert_eq!(body(1).get("type"), None);
    }

    #[test]
    fn detail_reads_hydration() {
        let fake = Fake::new(|_| {
            respond(
                200,
                &[],
                r#"{"ok":true,"sandbox":{"id":"bx_22222222","state":"running","type":"large",
                    "holdsCreatorLogins":false,"hydrated":false,
                    "hydration":{"phase":"hydrating","entriesDone":5,"entriesTotal":10,"percent":50}}}"#,
            )
        });
        let d = Client::new(&fake, "k".into())
            .detail("bx_22222222")
            .unwrap();
        assert_eq!(d.hydrated, Some(false));
        assert_eq!(d.hydration.unwrap().percent, Some(50));
        assert_eq!(d.machine_type.as_deref(), Some("large"));
        assert_eq!(d.holds_creator_logins, Some(false));
    }

    #[test]
    fn exec_starts_detached_and_polls() {
        let fake = Fake::new(|s| {
            if s.method == "POST" {
                respond(
                    200,
                    &[],
                    r#"{"ok":true,"type":"command.started","success":true,"processId":17,
                        "pid":4242,"command":"du","startedAt":"2026-11-10T00:00:00Z"}"#,
                )
            } else {
                respond(
                    200,
                    &[],
                    r#"{"ok":true,"success":true,"processId":17,"status":"exited",
                        "running":false,"exitCode":0,"stdout":"a\tb\n","stderr":""}"#,
                )
            }
        });
        let boat = Client::new(&fake, "k".into());
        assert_eq!(boat.exec_start("bx_22222222", "du").unwrap(), 17);
        let st = boat.command_status("bx_22222222", 17).unwrap();
        assert!(!st.running);
        assert_eq!(st.exit_code, Some(0));
        assert_eq!(st.stdout, "a\tb\n");
        let sent = fake.sent();
        assert_eq!(sent[0].body_text(), r#"{"command":"du","detached":true}"#);
        assert_eq!(
            sent[1].url,
            "https://boat.dev/api/v1/sandboxes/bx_22222222/commands/17"
        );
    }

    #[test]
    fn creates_a_sandbox_from_the_base_image() {
        let fake = Fake::new(|_| {
            respond(
                201,
                &[],
                r#"{"ok":true,"sandbox":{"id":"bx_44444444","state":"provisioning"}}"#,
            )
        });
        let boat = Client::new(&fake, "k".into());
        let spec = NewSandbox {
            machine_type: "small".into(),
            ttl_s: None,
            no_env: false,
            labels: BTreeMap::from([("a", "b".to_string())]),
            name: "n".into(),
        };
        assert_eq!(boat.create_sandbox(&spec, "idem").unwrap(), "bx_44444444");
        let sent = fake.sent();
        assert_eq!(sent[0].url, "https://boat.dev/api/v1/sandboxes");
        assert_eq!(sent[0].header("idempotency-key"), Some("idem"));
        assert_eq!(
            sent[0].body_text(),
            r#"{"labels":{"a":"b"},"name":"n","noEnv":false,"ttlSeconds":null,"type":"small"}"#
        );
    }

    #[test]
    fn saves_and_lists_saved_snapshots() {
        let fake = Fake::new(|s| {
            if s.method == "POST" {
                respond(
                    202,
                    &[],
                    r#"{"ok":true,"status":"saving","snapshot":{"name":"ci-1",
                        "status":"saving","sourceSandboxId":"bx_44444444",
                        "createdAt":"2026-11-10T00:00:00Z"}}"#,
                )
            } else {
                respond(
                    200,
                    &[],
                    r#"{"ok":true,"type":"snapshot.saved.list","snapshots":[{"name":"ci-1",
                        "status":"ready","sourceSandboxId":"bx_44444444","sizeBytes":15900000000,
                        "createdAt":"2026-11-10T00:00:00Z"}]}"#,
                )
            }
        });
        let boat = Client::new(&fake, "k".into());
        boat.save_snapshot("bx_44444444", "ci-1").unwrap();
        let list = boat.saved_snapshots(Some("ci-1")).unwrap();
        assert_eq!(list[0].size_bytes, Some(15_900_000_000));
        assert_eq!(list[0].status, "ready");
        boat.saved_snapshots(None).unwrap();
        let sent = fake.sent();
        assert_eq!(
            sent[0].body_text(),
            r#"{"name":"ci-1","sandboxId":"bx_44444444"}"#
        );
        assert_eq!(
            sent[1].url,
            "https://boat.dev/api/v1/saved-snapshots?name=ci-1"
        );
        assert_eq!(sent[2].url, "https://boat.dev/api/v1/saved-snapshots");
    }

    #[test]
    fn disk_budgets() {
        assert_eq!(disk_budget_bytes("default"), Some(51_000_000_000));
        assert_eq!(disk_budget_bytes("xlarge"), Some(251_000_000_000));
        assert_eq!(disk_budget_bytes("huge"), None);
    }
}
