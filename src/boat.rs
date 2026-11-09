//! boat REST client: the sandbox calls the tick loop needs, nothing else.
//! Reference: https://docs.boat.dev/api/v1.md

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::MachineState;
use crate::run::{Response, Transport};

pub const BASE: &str = "https://boat.dev/api/v1";
/// Label every machine of a pool carries: `sw-boat-runner=<pool>`.
pub const POOL_LABEL: &str = "sw-boat-runner";
/// Label of a forked machine: `sw-boat-runner-job=<job id>-<attempt>`.
pub const JOB_LABEL: &str = "sw-boat-runner-job";

/// `GET /sandboxes` pages of 200 (the documented maximum); a pool never gets near this.
const PAGE_LIMIT: u32 = 200;
const MAX_PAGES: usize = 20;
/// Wait used when a refusal carries no wait of its own.
const DEFAULT_RETRY_S: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// boat refused to start a machine (429, or 503 `no_ready_machine`/`out_of_capacity`).
    /// The start is not counted; try again after `retry_after_s`.
    RefusedStart {
        code: String,
        retry_after_s: u64,
    },
    /// 409: state conflict, idempotency in progress or key reused.
    Conflict {
        code: String,
    },
    /// 402: the wallet needs billing; nothing will start until a human acts.
    BillingRequired,
    /// 401 or 403: bad key, suspended org, or a machine type the plan does not allow.
    Unauthorized {
        code: String,
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
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::BillingRequired | Error::Unauthorized { .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::RefusedStart {
                code,
                retry_after_s,
            } => write!(
                f,
                "boat refused the start ({code}), retry in {retry_after_s}s"
            ),
            Error::Conflict { code } => write!(f, "boat conflict ({code})"),
            Error::BillingRequired => write!(f, "boat billing required"),
            Error::Unauthorized { code } => write!(f, "boat unauthorized ({code})"),
            Error::NotFound => write!(f, "boat: not found"),
            Error::Network(e) => write!(f, "boat network error: {e}"),
            Error::Other {
                status,
                code,
                message,
            } => write!(f, "boat error {status} ({code}): {message}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Sandbox {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub can_start: bool,
    /// 0 inside every limit, `None` when boat would refuse the start.
    #[serde(default)]
    pub next_start_dollars: Option<f64>,
}

/// boat state → model state. boat's `archived` is a stop with the disk kept, so it maps to
/// `Stopped`; `cancelled` is a create or fork that got no machine. An unknown state counts
/// as `Starting`: it holds a `max_boxes` slot and draws no action until it settles.
pub fn machine_state(state: &str) -> MachineState {
    match state {
        "init" | "provisioning" | "provisioned" | "cloning" => MachineState::Starting,
        "ready" | "idle" | "running" => MachineState::Up,
        "archiving" => MachineState::Stopping,
        "archived" => MachineState::Stopped,
        "error" | "cancelled" => MachineState::Gone,
        _ => MachineState::Starting,
    }
}

/// `bx_` plus 8 characters of boat's id alphabet; never contains `-`.
pub fn is_sandbox_id(s: &str) -> bool {
    s.strip_prefix("bx_").is_some_and(|rest| {
        rest.len() == 8
            && rest
                .bytes()
                .all(|b| matches!(b, b'2'..=b'9' | b'a'..=b'h' | b'j' | b'k' | b'm' | b'n' | b'p'..=b'z'))
    })
}

pub struct Client<'t> {
    t: &'t dyn Transport,
    key: String,
}

impl<'t> Client<'t> {
    pub fn new(t: &'t dyn Transport, key: String) -> Self {
        Client { t, key }
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra: &[(&str, &str)],
    ) -> Result<Value, Error> {
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
        let resp = self
            .t
            .send(method, &url, &headers, bytes.as_deref())
            .map_err(|e| Error::Network(e.0))?;
        if !(200..300).contains(&resp.status) {
            return Err(map_error(&resp));
        }
        if resp.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&resp.body).map_err(|e| Error::Other {
            status: resp.status,
            code: "bad_json".into(),
            message: e.to_string(),
        })
    }

    /// Every sandbox of `pool`, in any state, following `pageInfo.nextCursor`.
    pub fn list(&self, pool: &str) -> Result<Vec<Sandbox>, Error> {
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
            let page: Page = parse(self.call("GET", &path, None, &[])?)?;
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

    pub fn resume(&self, id: &str) -> Result<(), Error> {
        self.call(
            "POST",
            &format!("/sandboxes/{id}/resume"),
            Some(&json!({})),
            &[],
        )
        .map(drop)
    }

    /// Returns at once; 200 when already stopped, 202 while the stop runs.
    pub fn stop(&self, id: &str) -> Result<(), Error> {
        self.call(
            "POST",
            &format!("/sandboxes/{id}/stop"),
            Some(&json!({})),
            &[],
        )
        .map(drop)
    }

    /// Permanently deletes the sandbox and its disk.
    pub fn delete(&self, id: &str) -> Result<(), Error> {
        self.call(
            "DELETE",
            &format!("/sandboxes/{id}"),
            None,
            &[("X-Ascii-Confirm-Delete", id)],
        )
        .map(drop)
    }

    /// Starts a new machine from the saved snapshot. boat returns the first sandbox again
    /// for a repeat of the same key and body within 24 h, so a crash cannot fork twice.
    pub fn create_from_snapshot(
        &self,
        snapshot: &str,
        machine_type: &str,
        labels: &BTreeMap<String, String>,
        ttl_s: u64,
        idempotency_key: &str,
    ) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Created {
            sandbox: Sandbox,
        }
        let body = json!({
            "from": snapshot,
            "type": machine_type,
            "labels": labels,
            // boat accepts 1..=2592000; expiry stops (archives) the machine, it does not delete it.
            "ttlSeconds": ttl_s.clamp(1, 2_592_000),
        });
        let created: Created = parse(self.call(
            "POST",
            "/sandboxes",
            Some(&body),
            &[("Idempotency-Key", idempotency_key)],
        )?)?;
        Ok(created.sandbox.id)
    }

    /// Writes a UTF-8 file under `/home/user` or `/tmp`; 200 means it is on disk.
    pub fn write_file(&self, id: &str, path: &str, content: &str) -> Result<(), Error> {
        let body = json!({ "path": path, "content": content, "encoding": "utf8" });
        self.call("PUT", &format!("/sandboxes/{id}/files"), Some(&body), &[])
            .map(drop)
    }

    /// Starts `command` in the background. boat keeps every command in its history, so the
    /// command must never contain a secret.
    pub fn exec_detached(&self, id: &str, command: &str) -> Result<(), Error> {
        let body = json!({ "command": command, "detached": true });
        self.call(
            "POST",
            &format!("/sandboxes/{id}/commands"),
            Some(&body),
            &[],
        )
        .map(drop)
    }

    pub fn limits(&self) -> Result<Limits, Error> {
        parse(self.call("GET", "/limits", None, &[])?)
    }
}

fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T, Error> {
    serde_json::from_value(v).map_err(|e| Error::Other {
        status: 0,
        code: "bad_json".into(),
        message: e.to_string(),
    })
}

/// Maps a non-2xx response. A 429 always sends `Retry-After`; a 503 puts
/// `retryAfterSeconds` in the body; `IP_RATE_LIMIT_EXCEEDED` uses `retryAfter`.
pub fn map_error(resp: &Response) -> Error {
    let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let text = |key: &str| {
        body.get(key)
            .or_else(|| body.get("error").and_then(|e| e.get(key)))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let code = text("code");
    let retry_after_s = resp
        .header("retry-after")
        .and_then(|v| v.trim().parse().ok())
        .or_else(|| body.get("retryAfterSeconds").and_then(Value::as_u64))
        .or_else(|| body.get("retryAfter").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_RETRY_S);
    match resp.status {
        429 => Error::RefusedStart {
            code,
            retry_after_s,
        },
        503 if code == "no_ready_machine" || code == "out_of_capacity" => Error::RefusedStart {
            code,
            retry_after_s,
        },
        409 => Error::Conflict { code },
        402 => Error::BillingRequired,
        401 | 403 => Error::Unauthorized { code },
        404 => Error::NotFound,
        status => Error::Other {
            status,
            code,
            message: text("message"),
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
    use crate::run::fake::{Fake, respond};

    fn err(status: u16, headers: &[(&str, &str)], body: &str) -> Error {
        map_error(&respond(status, headers, body))
    }

    #[test]
    fn maps_errors() {
        assert_eq!(
            err(429, &[("Retry-After", "30")], r#"{"code":"limit_reached"}"#),
            Error::RefusedStart {
                code: "limit_reached".into(),
                retry_after_s: 30
            }
        );
        assert_eq!(
            err(
                503,
                &[],
                r#"{"ok":false,"code":"no_ready_machine","retryAfterSeconds":5}"#
            ),
            Error::RefusedStart {
                code: "no_ready_machine".into(),
                retry_after_s: 5
            }
        );
        assert_eq!(
            err(503, &[], r#"{"error":{"code":"out_of_capacity"}}"#),
            Error::RefusedStart {
                code: "out_of_capacity".into(),
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
                code: "sandbox_stopping".into()
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
    fn maps_states() {
        assert_eq!(machine_state("cloning"), MachineState::Starting);
        assert_eq!(machine_state("ready"), MachineState::Up);
        assert_eq!(machine_state("running"), MachineState::Up);
        assert_eq!(machine_state("archiving"), MachineState::Stopping);
        assert_eq!(machine_state("archived"), MachineState::Stopped);
        assert_eq!(machine_state("cancelled"), MachineState::Gone);
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
        let labels = BTreeMap::from([
            (POOL_LABEL.to_string(), "ci".to_string()),
            (JOB_LABEL.to_string(), "7-2".to_string()),
        ]);
        let id = boat
            .create_from_snapshot("runner-base", "large", &labels, 0, "7-2")
            .unwrap();
        assert_eq!(id, "bx_44444444");
        let sent = fake.sent();
        assert_eq!(sent[0].method, "POST");
        assert_eq!(sent[0].url, "https://boat.dev/api/v1/sandboxes");
        assert_eq!(sent[0].header("idempotency-key"), Some("7-2"));
        let body: Value = serde_json::from_slice(sent[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"from":"runner-base","type":"large","ttlSeconds":1,
                   "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"7-2"}})
        );
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
    fn limits_null_price() {
        let fake = Fake::new(|_| respond(200, &[], r#"{"canStart":true,"nextStartDollars":null}"#));
        let limits = Client::new(&fake, "k".into()).limits().unwrap();
        assert!(limits.can_start);
        assert_eq!(limits.next_start_dollars, None);
    }
}
