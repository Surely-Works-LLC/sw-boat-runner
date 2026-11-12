//! One whole-loop test per known bug, each asserting the behaviour the fix should give.
//! They fail on v0.2.0, so each is `#[ignore]`d with the fix it waits for; the PR that
//! fixes a bug removes its `#[ignore]`. Run them with `cargo test --test sim_known_bugs
//! -- --ignored`.

mod sim;

use serde_json::json;
use sim::*;
use sw_boat_runner::github::parse_runner_name;
use sw_boat_runner::model::JobKey;

/// 01:00 UTC, so a fresh daemon's assumed-spent budget does not matter.
const T0: u64 = 20_000 * DAY_S + 3_600;

fn world(knobs: Knobs) -> World {
    World::new(T0, knobs, 7)
}

fn deletes_of(w: &World, id: &str) -> usize {
    let line = format!("DELETE /sandboxes/{id}");
    w.log.iter().filter(|r| r.line() == line).count()
}

/// A fatal answer to one machine's delete must not stop the deletes of the others: every
/// tick the daemon exits at the first machine and the second is never deleted.
#[test]
#[ignore = "known bug: fatal errors stop cleanup (src/run fix)"]
fn a_fatal_error_on_one_delete_does_not_stop_the_others() {
    let mut w = world(Knobs::default());
    let job = |id| Some(JobKey { id, attempt: 1 });
    let a = w.seed_machine(0, "ci", job(901), BoxState::Up);
    let b = w.seed_machine(0, "ci", job(902), BoxState::Up);
    let (first, second) = if a < b { (a, b) } else { (b, a) };
    // boat refuses the first machine's delete for good, as for a plan or permission
    // problem with that one machine.
    let target = format!("DELETE /sandboxes/{first}");
    w.rules.push(Rule::new(
        move |r| r.line() == target,
        Effect::Status(403, json!({"code": "forbidden"})),
        u32::MAX,
    ));
    // A fresh process counts Up forks as used, so both are due for deletion at once.
    let mut s = Sim::new(w, vec![one_pool("fork", "max_boxes = 4")]);
    s.run_for(300);
    let w = s.world();
    assert!(deletes_of(&w, &first) > 0);
    assert_eq!(
        deletes_of(&w, &second),
        1,
        "the second fork was never deleted; exits: {}",
        w.stats.exits
    );
}

/// A register whose jit config write fails leaves a `Registered` runner that holds the
/// slot until `register_timeout_s`; the job waits that long for a slot that is free.
#[test]
#[ignore = "known bug: register failure leaks a slot (src/run fix)"]
fn a_failed_register_frees_its_slot_at_once() {
    let mut w = world(Knobs::default());
    w.seed_machine(0, "ci", None, BoxState::Up);
    let job = w.add_run("o/r", T0, &[(large(), 60)])[0];
    w.rules.push(Rule::new(
        |r| r.method == "PUT" && r.url.ends_with("/files"),
        Effect::Status(500, json!({"code": "internal"})),
        1,
    ));
    let cfg = one_pool("warm", "max_boxes = 1\nregister_timeout_s = 900");
    let mut s = Sim::warmed(w, vec![cfg]);
    s.run_for(1_200);
    let w = s.world();
    let started = w.jobs[job].started_at.expect("the job ran") - T0;
    assert!(started <= 120, "the job waited {started}s for its slot");
}

/// A kept machine that holds its creator's logins gets no runner until it is stopped and
/// resumed, but a pool with demand never stops it, so with `max_boxes = 1` the pool never
/// runs a job again.
#[test]
#[ignore = "known bug: logins block a pool (src/run + decide fix)"]
fn a_kept_machine_with_logins_does_not_block_its_pool() {
    let mut w = world(Knobs::default());
    let id = w.seed_machine(0, "ci", None, BoxState::Up);
    w.sandboxes
        .iter_mut()
        .find(|s| s.id == id)
        .unwrap()
        .holds_logins = true;
    let job = w.add_run("o/r", T0, &[(large(), 60)])[0];
    let cfg = one_pool("warm", "max_boxes = 1\nidle_stop_s = 60\nmin_up_s = 0");
    let mut s = Sim::warmed(w, vec![cfg]);
    s.run_for(3_600);
    let w = s.world();
    assert!(
        w.jobs[job].started_at.is_some(),
        "the job never ran; requests to {id}: {:?}",
        w.log
            .iter()
            .filter(|r| r.url.contains(&id) && r.method != "GET")
            .map(Req::line)
            .collect::<Vec<_>>()
    );
}

/// A resume whose answer is lost is not remembered as in flight, so while boat still lists
/// the machine stopped the daemon sends the resume again.
#[test]
#[ignore = "known bug: a resume timeout isn't marked in flight (src/run fix)"]
fn a_resume_whose_answer_is_lost_is_not_sent_again() {
    let knobs = Knobs {
        list_lag_s: 30,
        ..Knobs::default()
    };
    let mut w = world(knobs);
    let id = w.seed_machine(0, "ci", None, BoxState::Stopped { since: T0 - DAY_S });
    w.add_run("o/r", T0, &[(large(), 60), (large(), 60)]);
    w.rules
        .push(Rule::new(|r| r.url.ends_with("/resume"), Effect::Lost, 1));
    let cfg = one_pool("warm", "max_boxes = 2");
    let mut s = Sim::warmed(w, vec![cfg]);
    s.run_for(600);
    let w = s.world();
    let resumes = w
        .log
        .iter()
        .filter(|r| r.line() == format!("POST /sandboxes/{id}/resume"))
        .count();
    assert_eq!(resumes, 1, "resumes of {id}");
    s.assert_clean(&[]);
}

/// The daemon stamps what it does with the tick's start time, not the time it sends the
/// request. With slow requests the stamp is behind: a runner's `registered_at` is older
/// than its registration, so `register_timeout_s` runs out early.
///
/// The fix has to read time from a clock the simulation controls for this test to pass
/// (`Loop::tick`'s `now` today).
#[test]
#[ignore = "known bug: timing uses tick start (src/run fix; needs a clock seam the sim can drive)"]
fn a_runner_is_stamped_with_the_time_it_is_registered() {
    let knobs = Knobs {
        latency_s: 5,
        ..Knobs::default()
    };
    let mut w = world(knobs);
    w.add_run("o/r", T0, &[(large(), 60), (large(), 60), (large(), 60)]);
    let mut s = Sim::warmed(w, vec![one_pool("fork", "max_boxes = 4")]);
    s.run_for(600);
    let w = s.world();
    let registers: Vec<(u64, u64)> = w
        .log
        .iter()
        .filter(|r| r.url.ends_with("/generate-jitconfig"))
        .map(|r| {
            let name = r.body["name"].as_str().unwrap_or_default();
            (r.at, parse_runner_name(name).map_or(0, |n| n.registered_at))
        })
        .collect();
    assert!(!registers.is_empty());
    for (sent, stamped) in registers {
        assert!(
            sent.abs_diff(stamped) <= 5,
            "registered at {sent} but stamped {stamped}"
        );
    }
}

/// More than 100 queued runs: GitHub lists the newest 100 on the first page, and the
/// daemon reads only that page, so the oldest runs' jobs are not seen.
#[test]
#[ignore = "known bug: pagination over 100 (github.rs fix)"]
fn more_than_100_queued_runs_are_all_seen() {
    let mut w = world(Knobs::default());
    for i in 0..150 {
        w.add_run("o/r", T0 - 600 + i, &[(large(), 600)]);
    }
    let mut s = Sim::warmed(w, vec![one_pool("fork", "max_boxes = 200")]);
    s.step();
    let w = s.world();
    let forked: std::collections::BTreeSet<_> =
        w.sandboxes.iter().filter_map(|sb| sb.job()).collect();
    assert_eq!(forked.len(), 150, "forks after the first tick");
}

/// A run with more than 100 jobs (a matrix): the daemon reads only the first page of its
/// jobs.
#[test]
#[ignore = "known bug: pagination over 100 (github.rs fix)"]
fn more_than_100_jobs_in_a_run_are_all_seen() {
    let mut w = world(Knobs::default());
    let jobs: Vec<_> = (0..150).map(|_| (large(), 600)).collect();
    w.add_run("o/r", T0 - 60, &jobs);
    let mut s = Sim::warmed(w, vec![one_pool("fork", "max_boxes = 200")]);
    s.step();
    let w = s.world();
    let forked = w.sandboxes.iter().filter(|sb| sb.job().is_some()).count();
    assert_eq!(forked, 150, "forks after the first tick");
}

/// A tick that skips its observe to stay in the GitHub budget still drops the ETag cache,
/// so the next observe pays full price for lists that did not change.
#[test]
#[ignore = "known bug: an ETag cache drop on skipped ticks (github.rs fix)"]
fn a_skipped_tick_keeps_the_etag_cache() {
    let knobs = Knobs {
        // Under the daemon's reserve of 500, so it observes once every 300 s.
        gh_rate_limit: 400,
        ..Knobs::default()
    };
    let w = world(knobs);
    let mut s = Sim::warmed(w, vec![one_pool("fork", "max_boxes = 2")]);
    s.run_for(1_200);
    let w = s.world();
    let gh_gets: Vec<&Req> = w
        .log
        .iter()
        .filter(|r| !r.is_boat() && r.method == "GET")
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut unconditional = Vec::new();
    for r in &gh_gets {
        if !seen.insert(r.url.clone()) && r.header("if-none-match").is_none() {
            unconditional.push(format!("{} {}", r.at - T0, r.line()));
        }
    }
    assert!(gh_gets.len() > 3, "{} GitHub reads", gh_gets.len());
    assert!(
        unconditional.is_empty(),
        "repeat reads without If-None-Match: {unconditional:?}"
    );
}

/// Two daemons for two repositories on one boat account, both with a pool named `ci`:
/// boat's label is only the pool name, so each lists, and deletes, the other's machines.
#[test]
#[ignore = "fixed by pool-identity PR (cross-repo label collision)"]
fn two_repos_with_the_same_pool_name_keep_to_their_own_machines() {
    let mut w = world(Knobs::default());
    w.add_run("o/r", T0, &[(large(), 300), (large(), 300)]);
    w.add_run("o/other", T0 + 30, &[(large(), 300), (large(), 300)]);
    let a = one_pool("fork", "max_boxes = 4");
    let mut b = one_pool("fork", "max_boxes = 4");
    b.repo = "o/other".into();
    let mut s = Sim::warmed(w, vec![a, b]);
    s.run_for(1_800);
    s.settle(3_600);
    s.end_checks();
    s.assert_clean(&[]);
}

/// Two daemons for one repository on one boat account, with one pool each of the same
/// name (for example an old and a new host that share a policy during a move): once one
/// stops ticking for a while, the other manages its machines.
#[test]
#[ignore = "fixed by pool-identity PR (two daemons on one pool)"]
fn two_daemons_on_one_repo_and_pool_name_keep_to_their_own_machines() {
    let mut w = world(Knobs::default());
    w.add_run("o/r", T0, &[(large(), 300), (large(), 300)]);
    let a = one_pool("warm", "max_boxes = 2\nidle_stop_s = 60\nmin_up_s = 0");
    let b = a.clone();
    let mut s = Sim::warmed(w, vec![a, b]);
    s.run_for(300);
    // Daemon 0's host goes down for a while; its machines are still its own.
    s.kill(0);
    s.world_mut().add_run("o/r", s.now + 30, &[(large(), 60)]);
    s.run_for(1_500);
    let foreign = s.world().violations_of(Kind::Foreign).len();
    assert_eq!(foreign, 0, "{:?}", s.violations_except(&[]));
}

/// `drain` right after the daemon forked a machine that boat does not list yet reports
/// 0 and leaves the machine running.
#[test]
#[ignore = "known bug: drain under list lag (src/run fix)"]
fn drain_finds_a_machine_boat_does_not_list_yet() {
    let knobs = Knobs {
        list_lag_s: 30,
        ..Knobs::default()
    };
    let mut w = world(knobs);
    w.add_run("o/r", T0, &[(large(), 600)]);
    let mut s = Sim::warmed(w, vec![one_pool("fork", "max_boxes = 2")]);
    s.step();
    assert_eq!(s.world().stats.creates, 1);
    // The operator stops the daemon and drains at once.
    s.drain_checks(0, 0);
    let v = s.world().violations_of(Kind::Drain).len();
    assert_eq!(v, 0, "{:?}", s.violations_except(&[]));
}
