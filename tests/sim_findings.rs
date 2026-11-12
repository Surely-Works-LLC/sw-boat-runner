//! Bugs the stateful simulation found beyond the known list, each shrunk to a minimal
//! deterministic scenario. They fail on v0.2.0 and are `#[ignore]`d until fixed; run them
//! with `cargo test --test sim_findings -- --ignored`.
//!
//! Two of them hold only if boat or GitHub behave in a way neither documents (named in
//! the test); they pin what the daemon assumes.

mod sim;

use std::rc::Rc;

use sim::*;

const T0: u64 = 20_000 * DAY_S + 3_600;

/// The first start of a tick goes out at the quote the observe read, unchecked, and an
/// accepted start adds that quote to today's spend. If anyone else on the boat account
/// starts a machine in between (a person, another daemon, another CI system) and so uses
/// up the included starts, boat charges the paid price, the daemon books 0, and later
/// starts the same day go past `paid_start_cents_per_day`.
///
/// Here: cap 10 cents, one included start an hour, then 5 cents. Someone else takes the
/// included start just before the daemon's first fork after midnight; the daemon then
/// makes two more paid starts it believes fit the cap: 15 cents against a cap of 10.
#[test]
#[ignore = "new bug: start price moved between observe and act is booked at the old price (src/run/act.rs)"]
fn a_start_charged_above_its_quote_counts_against_the_cap() {
    // 23:50 UTC: a fresh daemon books its whole cap as spent until midnight.
    let start = 20_001 * DAY_S - 600;
    let pricing = Pricing {
        included_per_hour: 1,
        paid_per_hour: 10,
        paid_cents: 5,
        can_start: true,
    };
    let mut w = World::new(
        start,
        Knobs {
            pricing,
            ..Knobs::default()
        },
        3,
    );
    let midnight = 20_001 * DAY_S;
    w.add_run("o/r", midnight + 60, &[(large(), 900)]);
    w.add_run("o/r", midnight + 600, &[(large(), 900), (large(), 900)]);
    w.rules.push(Rule::new(
        |r| r.method == "POST" && r.url == format!("{BOAT}/sandboxes"),
        Effect::Then(Rc::new(|w: &mut World| {
            // Another user of the account starts a machine first.
            w.take_start(usize::MAX, "someone-else");
        })),
        1,
    ));
    let cfg = one_pool("fork", "max_boxes = 4\npaid_start_cents_per_day = 10");
    let mut s = Sim::new(w, vec![cfg]);
    s.run_for(1_800);
    let paid = s.world().stats.paid_cents;
    let spend = s.world().violations_of(Kind::Spend).len();
    assert_eq!(
        spend,
        0,
        "{} cents paid: {:?}",
        paid,
        s.violations_except(&[])
    );
}

/// A job's fork whose job is cancelled before its runner took it keeps an idle runner,
/// and `decide` deletes a fork only when its runner is gone or `fork_ttl_s` has passed.
/// With nothing else queued the fork stays up for the whole TTL (an hour by default):
/// one machine-hour per cancelled job, which `cancel-in-progress` makes common.
#[test]
#[ignore = "new bug: a cancelled job's fork idles until fork_ttl_s (src/decide.rs)"]
fn a_fork_whose_job_was_cancelled_is_deleted_soon() {
    let mut w = World::new(
        T0,
        Knobs {
            boot_s: 20,
            runner_online_s: 60,
            ..Knobs::default()
        },
        5,
    );
    let job = w.add_run("o/r", T0, &[(large(), 300)])[0];
    // After its fork registered a runner, before the runner is online.
    w.jobs[job].cancel_at = Some(T0 + 50);
    let cfg = one_pool("fork", "max_boxes = 2\nregister_timeout_s = 900");
    let mut s = Sim::warmed(w, vec![cfg]);
    s.run_for(1_800);
    let w = s.world();
    let fork = w
        .sandboxes
        .iter()
        .find(|s| s.job().is_some())
        .expect("a fork");
    let deleted = fork
        .history
        .iter()
        .find(|(_, st)| *st == "deleted")
        .map(|(t, _)| t - T0);
    assert!(
        deleted.is_some_and(|t| t <= 900),
        "fork {} still {:?} 30 minutes after its job was cancelled",
        fork.id,
        fork.state
    );
}

/// Assumes GitHub's runner list can miss a runner registered more than one tick ago.
/// A kept slot whose new runner is not listed yet looks free, so the next tick registers
/// another runner on it, and the start command's `pkill` of the slot's runner kills the
/// first one while it runs a job.
#[test]
#[ignore = "new bug (if GitHub's runner list lags a tick): slot reuse kills a busy runner (src/run/script.rs, decide.rs)"]
fn a_slot_whose_runner_is_not_listed_yet_is_not_reused() {
    let mut w = World::new(
        T0,
        Knobs {
            runner_list_lag_s: 30,
            runner_online_s: 5,
            ..Knobs::default()
        },
        9,
    );
    w.seed_machine(0, "ci", None, BoxState::Up);
    w.add_run("o/r", T0, &[(large(), 120), (large(), 120)]);
    let cfg = one_pool("warm", "max_boxes = 1\nslots_per_box = 1");
    let mut s = Sim::warmed(w, vec![cfg]);
    s.run_for(600);
    s.assert_clean(&[]);
}

/// Assumes boat's listing can be older than the one before it (read replicas). A fork
/// leaves the daemon's in-flight memory the first time a listing shows it; a later, older
/// listing that does not show it yet makes the daemon fork the same job again, past
/// `max_boxes`.
#[test]
#[ignore = "new bug (if boat's listing is not monotonic): a fork that drops out of a listing is forked again (src/run/observe.rs)"]
fn a_fork_missing_from_an_older_listing_is_not_forked_again() {
    let mut w = World::new(T0, Knobs::default(), 11);
    w.add_run("o/r", T0, &[(large(), 600)]);
    let lag =
        |s: u64| -> Effect { Effect::Then(Rc::new(move |w: &mut World| w.knobs.list_lag_s = s)) };
    let listing = |at: u64| {
        move |r: &Req| {
            r.method == "GET" && r.url.starts_with(&format!("{BOAT}/sandboxes?")) && r.at == at
        }
    };
    // Tick 2 lists the fork; tick 3's listing comes from a replica a minute behind.
    w.rules.push(Rule::new(listing(T0 + 30), lag(60), 1));
    w.rules.push(Rule::new(listing(T0 + 45), lag(0), 1));
    let cfg = one_pool("fork", "max_boxes = 1");
    let mut s = Sim::warmed(w, vec![cfg]);
    s.run_for(120);
    s.assert_clean(&[]);
}
