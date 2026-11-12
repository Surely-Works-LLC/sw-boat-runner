//! A one-pool policy and a fake boat and GitHub that the loop's tests share.

use std::cell::Cell;

use super::{HTTP_TIMEOUT, Loop};
use crate::github;
use crate::model::{Action, Config, JobKey, Placement, Pool, UnixSecs};
use crate::transport::Response;
use crate::transport::fake::{Fake, Reply, Sent, respond};

pub(super) const BOAT: &str = "https://boat.dev/api/v1";
pub(super) const GH: &str = "https://api.github.com/repos/o/r/actions";
pub(super) const BOAT_KEY: &str = "boat-key-0123456789";
pub(super) const GH_TOKEN: &str = "gh-token-0123456789";
pub(super) const NOW: UnixSecs = 1_000;
/// A process started this long before `NOW` has waited out the restart wait.
pub(super) const STARTED: UnixSecs = NOW - HTTP_TIMEOUT.as_secs();

pub(super) fn config() -> Config {
    Config {
        repo: "o/r".into(),
        tick_s: 15,
        pools: vec![Pool {
            name: "ci".into(),
            labels: vec!["self-hosted".into(), "large".into()],
            machine_type: "large".into(),
            snapshot: "runner-base".into(),
            placement: vec![Placement::Fork],
            min_up: 0,
            max_boxes: 4,
            slots_per_box: 2,
            idle_stop_s: 600,
            min_up_s: 0,
            register_timeout_s: 300,
            fork_ttl_s: 3_600,
            paid_start_cents_per_day: 100,
        }],
    }
}

/// A loop whose restart wait is over by `NOW`.
pub(super) fn started<'a>(cfg: &'a Config, fake: &'a Fake) -> Loop<'a> {
    Loop::new(cfg, fake, BOAT_KEY.into(), gh_auth(), STARTED)
}

/// `GH_TOKEN` as a personal access token.
pub(super) fn gh_auth() -> github::Auth {
    github::Auth::Token(GH_TOKEN.into())
}

/// Three machines of pool `ci` (one up, one stopped, one job fork), two of its runners and
/// one of another pool, job 70 queued in run 5, and starts at 25 cents.
pub(super) fn reply(s: &Sent) -> Response {
    let url = s.url.as_str();
    let body = match (s.method.as_str(), url) {
        ("GET", u) if u.starts_with(&format!("{BOAT}/sandboxes?")) => {
            r#"{"sandboxes":[
                {"id":"bx_22222222","state":"idle","labels":{"sw-boat-runner":"ci"}},
                {"id":"bx_33333333","state":"archived","labels":{"sw-boat-runner":"ci"}},
                {"id":"bx_44444444","state":"running",
                 "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"7-2"}}],
               "pageInfo":{"nextCursor":null,"hasMore":false,"limit":200}}"#
        }
        ("GET", u) if u.starts_with(&format!("{GH}/runners?")) => {
            r#"{"total_count":3,"runners":[
                {"id":11,"name":"sbr-ci-bx_22222222-0-900","status":"online","busy":false},
                {"id":12,"name":"sbr-ci-bx_44444444-0-900","status":"offline","busy":false},
                {"id":13,"name":"sbr-other-bx_44444444-0-900","status":"online","busy":true}]}"#
        }
        ("GET", u) if u.contains("status=queued") => {
            r#"{"workflow_runs":[{"id":5,"run_attempt":1}]}"#
        }
        ("GET", u) if u.contains("status=in_progress") => r#"{"workflow_runs":[]}"#,
        ("GET", u) if u.contains("/runs/5/jobs") => {
            r#"{"jobs":[{"id":70,"run_attempt":1,"status":"queued","labels":["self-hosted","large"]}]}"#
        }
        ("GET", u) if u.starts_with(&format!("{BOAT}/sandboxes/bx_")) => {
            let id = u.rsplit('/').next().unwrap_or_default();
            let body = format!(
                r#"{{"sandbox":{{"id":"{id}","state":"idle","holdsCreatorLogins":false}}}}"#
            );
            return respond(200, &[], &body);
        }
        ("GET", u) if u.ends_with("/v1/limits") => r#"{"canStart":true,"nextStartDollars":0.25}"#,
        ("POST", u) if u.ends_with("/generate-jitconfig") => {
            return respond(
                201,
                &[],
                r#"{"runner":{"id":20},"encoded_jit_config":"JITSECRET"}"#,
            );
        }
        ("POST", u) if u.ends_with("/v1/sandboxes") => {
            r#"{"type":"sandbox.created","sandbox":{"id":"bx_55555555","state":"init"}}"#
        }
        ("DELETE", u) if u.starts_with(GH) => return respond(204, &[], ""),
        _ => "{}",
    };
    respond(200, &[], body)
}

pub(super) fn requests(fake: &Fake) -> Vec<String> {
    fake.sent()
        .iter()
        .map(|s| format!("{} {}", s.method, s.url))
        .collect()
}

pub(super) fn forks(fake: &Fake) -> Vec<Sent> {
    fake.sent()
        .into_iter()
        .filter(|s| s.method == "POST" && s.url == format!("{BOAT}/sandboxes"))
        .collect()
}

fn count(fake: &Fake, suffix: &str) -> usize {
    requests(fake)
        .iter()
        .filter(|r| r.ends_with(suffix))
        .count()
}

pub(super) fn registers(fake: &Fake) -> usize {
    count(fake, "/generate-jitconfig")
}

pub(super) fn resumes(fake: &Fake) -> usize {
    count(fake, "/resume")
}

/// A fork in pool `ci`: for job `id`, attempt 1, or a kept machine for `None`.
pub(super) fn fork(job: Option<u64>) -> Action {
    let job = job.map(|id| JobKey { id, attempt: 1 });
    Action::Fork {
        pool: "ci".into(),
        job,
    }
}

pub(super) fn resume(machine: &str) -> Action {
    let machine = machine.into();
    Action::Resume { machine }
}

pub(super) fn delete(machine: &str) -> Action {
    let machine = machine.into();
    Action::Delete { machine }
}

pub(super) fn stop(machine: &str) -> Action {
    let machine = machine.into();
    Action::Stop { machine }
}

pub(super) fn unregister(runner: &str) -> Action {
    let runner = runner.into();
    Action::Unregister { runner }
}

pub(super) fn register(machine: &str, slot: u32) -> Action {
    let (pool, machine) = ("ci".into(), machine.into());
    Action::Register {
        pool,
        machine,
        slot,
    }
}

/// One job fork `bx_66666666` for queued job 70 and no runners, with starts at 25 cents.
/// boat lists it in `states[i]` on the i-th tick, and in the last state after that.
pub(super) fn one_fork(states: &'static [&'static str]) -> Fake {
    let listing = move |t: usize| {
        let state = states[(t - 1).min(states.len() - 1)];
        format!(
            r#"{{"id":"bx_66666666","state":"{state}",
                 "labels":{{"sw-boat-runner":"ci","sw-boat-runner-job":"70-1"}}}}"#
        )
    };
    lagging_creates(listing, |_| JOB_70, |_| None, "0.25")
}

/// boat lists `listing(t)` and GitHub queues jobs `jobs(t)` on the t-th tick (from 1);
/// starts are included, and every create answers `bx_55555555`.
pub(super) fn lagging(listing: fn(usize) -> &'static str, jobs: fn(usize) -> &'static str) -> Fake {
    lagging_creates(listing, jobs, |_| None, "0")
}

/// As `lagging`, but a create on tick t gets `create(t)` when that is `Some`, and every
/// start is quoted at `dollars`.
pub(super) fn lagging_creates<L: AsRef<str>>(
    listing: impl Fn(usize) -> L + 'static,
    jobs: fn(usize) -> &'static str,
    create: fn(usize) -> Option<Reply>,
    dollars: &'static str,
) -> Fake {
    let ticks = Cell::new(0);
    Fake::failing(move |s| {
        let url = s.url.as_str();
        if s.method == "POST" && url == format!("{BOAT}/sandboxes") {
            if let Some(r) = create(ticks.get()) {
                return r;
            }
        }
        Ok(
            if s.method == "GET" && url.starts_with(&format!("{BOAT}/sandboxes?")) {
                ticks.set(ticks.get() + 1);
                let body = format!(
                    r#"{{"sandboxes":[{}],
                   "pageInfo":{{"nextCursor":null,"hasMore":false,"limit":200}}}}"#,
                    listing(ticks.get()).as_ref()
                );
                respond(200, &[], &body)
            } else if s.method == "GET" && url.starts_with(&format!("{GH}/runners?")) {
                respond(200, &[], r#"{"total_count":0,"runners":[]}"#)
            } else if s.method == "GET" && url.contains("/runs/5/jobs") {
                respond(200, &[], &format!(r#"{{"jobs":[{}]}}"#, jobs(ticks.get())))
            } else if s.method == "GET" && url.ends_with("/v1/limits") {
                let body = format!(r#"{{"canStart":true,"nextStartDollars":{dollars}}}"#);
                respond(200, &[], &body)
            } else {
                reply(s)
            },
        )
    })
}

pub(super) const JOB_70: &str =
    r#"{"id":70,"run_attempt":1,"status":"queued","labels":["self-hosted","large"]}"#;
pub(super) const JOBS_70_71: &str = r#"{"id":70,"run_attempt":1,"status":"queued","labels":["self-hosted","large"]},
    {"id":71,"run_attempt":1,"status":"queued","labels":["self-hosted","large"]}"#;
pub(super) const LISTED_55: &str = r#"{"id":"bx_55555555","state":"provisioning",
    "labels":{"sw-boat-runner":"ci","sw-boat-runner-job":"70-1"}}"#;

/// One answer of a fake that lists `listing` in pool `ci`, no runners, and jobs `jobs` in
/// run 5, with included starts; anything else as `reply`.
pub(super) fn lagging_reply(s: &Sent, listing: &str, jobs: &str) -> Response {
    let url = s.url.as_str();
    if s.method == "GET" && url.starts_with(&format!("{BOAT}/sandboxes?")) {
        let body = format!(
            r#"{{"sandboxes":[{listing}],
               "pageInfo":{{"nextCursor":null,"hasMore":false,"limit":200}}}}"#
        );
        respond(200, &[], &body)
    } else if s.method == "GET" && url.starts_with(&format!("{GH}/runners?")) {
        respond(200, &[], r#"{"total_count":0,"runners":[]}"#)
    } else if s.method == "GET" && url.contains("/runs/5/jobs") {
        respond(200, &[], &format!(r#"{{"jobs":[{jobs}]}}"#))
    } else if s.method == "GET" && url.ends_with("/v1/limits") {
        respond(200, &[], r#"{"canStart":true,"nextStartDollars":0}"#)
    } else {
        reply(s)
    }
}
