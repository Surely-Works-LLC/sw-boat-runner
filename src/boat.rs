//! boat REST client: the sandbox calls the tick loop needs, nothing else.
//! Reference: https://docs.boat.dev/api/v1.md

use std::collections::BTreeMap;
use std::fmt;

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
    Unauthorized {
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
            Error::Unauthorized { code, message } => ("unauthorized".into(), code, message),
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
}

/// The labels a fork carries: its pool, and the job it is for unless it is a kept machine.
fn fork_labels(pool: &str, job: Option<JobKey>) -> BTreeMap<&'static str, String> {
    let mut labels = BTreeMap::from([(POOL_LABEL, pool.to_string())]);
    if let Some(job) = job {
        labels.insert(JOB_LABEL, format!("{}-{}", job.id, job.attempt));
    }
    labels
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
}

impl<'t> Client<'t> {
    pub(crate) fn new(t: &'t dyn Transport, key: String) -> Self {
        Client { t, key }
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

    /// Every sandbox of `pool`, in any state, following `pageInfo.nextCursor`.
    pub(crate) fn list(&self, pool: &str) -> Result<Vec<Sandbox>, Error> {
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
        let label = encode(&format!("{POOL_LABEL}={pool}"));
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut path = format!("/sandboxes?label={label}&limit={PAGE_LIMIT}");
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
            message: format!("more than {MAX_PAGES} pages of sandboxes for pool {pool}"),
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
            "labels": fork_labels(pool, job),
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
        401 | 403 => Error::Unauthorized { code, message },
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
            labels: fork_labels("ci", job)
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
}
