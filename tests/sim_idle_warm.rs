//! Whole-loop checks of `idle_warm` on the stateful fake (tests/sim): a warm uses a spare
//! machine and nothing else. Each scenario runs twice over the same world, without and
//! with `idle_warm`, and every request that changes a machine or a runner must go out at
//! the same second with the same target, and every job start at the same second; only
//! the warms and their cancels differ. So the pool falls to `min_up` after a burst and
//! grows for the next one exactly as it would without warms.

mod sim;

use serde_json::json;
use sim::*;
use sw_boat_runner::model::Config;

const T0: u64 = 20_000 * DAY_S + 3_600;

/// A deployed pool's shape, names aside: one machine kept up, room for twelve, one slot
/// each, two standby runners and a ten-minute idle stop, ticking every 15 s.
const POOL: &str = "min_up = 1\nmax_boxes = 12\nslots_per_box = 1\nstandby = 2\nidle_stop_s = 600";

fn cfg(pool: &str, interval_s: Option<u64>) -> Config {
    let warm = interval_s.map_or(String::new(), |i| {
        format!(
            "[pool.idle_warm]\nargv = [\"./warm\"]\ncwd = \"/home/user/work\"\n\
             interval_s = {i}\ntimeout_s = 600"
        )
    });
    one_pool("warm", &format!("{pool}\n{warm}"))
}

/// `n` jobs of a few minutes each, of different lengths.
fn burst(n: usize) -> Vec<(Vec<String>, u64)> {
    (0..n)
        .map(|i| (large(), 120 + 45 * (i as u64 % 7)))
        .collect()
}

/// What one run did.
struct Outcome {
    /// Each request that changes a machine or a runner, as `second METHOD path [name]`.
    actions: Vec<String>,
    /// Each job's start, in seconds after `T0`.
    starts: Vec<Option<u64>>,
    /// The pool's machines up or starting after each tick, by tick.
    up: Vec<usize>,
    stats: Stats,
    warms: Vec<WarmProc>,
    /// The cancels the daemon sent, as (seconds after `T0`, machine).
    cancels: Vec<(u64, String)>,
}

fn is_warm(r: &Req) -> bool {
    let command = r.body["command"].as_str().unwrap_or_default();
    command.contains("SBR_WARM=") || command.starts_with(": > /tmp/sbr-warm-")
}

fn is_cancel(r: &Req) -> bool {
    let command = r.body["command"].as_str().unwrap_or_default();
    r.method == "POST" && command.starts_with(": > /tmp/sbr-warm-")
}

/// Runs `world` for `secs` under `cfg`, calling `each` before every tick.
fn run(world: World, cfg: Config, secs: u64, mut each: impl FnMut(&mut Sim)) -> Outcome {
    let mut s = Sim::with_state(world, vec![cfg]);
    let mut up = Vec::new();
    while s.now < T0 + secs {
        each(&mut s);
        s.step();
        let w = s.world();
        up.push(w.sandboxes.iter().filter(|b| b.state.holds_box()).count());
    }
    s.assert_clean(&[]);
    let w = s.world();
    let actions = w
        .log
        .iter()
        .filter(|r| r.method != "GET" && !is_warm(r))
        .map(|r| {
            let name = r.body["name"].as_str().filter(|_| !r.is_boat());
            let path = r.body["path"].as_str();
            let at = r.at - T0;
            format!("{at} {} {}", r.line(), name.or(path).unwrap_or_default())
        })
        .collect();
    Outcome {
        actions,
        starts: w
            .jobs
            .iter()
            .map(|j| j.started_at.map(|t| t - T0))
            .collect(),
        up,
        stats: w.stats.clone(),
        warms: w.warms.clone(),
        cancels: w
            .log
            .iter()
            .filter(|r| is_cancel(r))
            .map(|r| {
                (
                    r.at - T0,
                    r.line().split('/').nth(2).unwrap_or_default().into(),
                )
            })
            .collect(),
    }
}

/// The run without `idle_warm` and the run with it, over the same world.
fn twice(
    pool: &str,
    interval_s: u64,
    world: impl Fn() -> World,
    secs: u64,
    each: impl Fn(&mut Sim),
) -> (Outcome, Outcome) {
    let without = run(world(), cfg(pool, None), secs, &each);
    let with = run(world(), cfg(pool, Some(interval_s)), secs, &each);
    (without, with)
}

/// Every machine and runner action, and every job start, at the same second.
fn assert_same(without: &Outcome, with: &Outcome, case: &str) {
    let first = without
        .actions
        .iter()
        .zip(&with.actions)
        .position(|(a, b)| a != b);
    assert!(
        first.is_none() && without.actions.len() == with.actions.len(),
        "{case}: the actions differ from #{first:?}:\nwithout {:?}\nwith    {:?}",
        first.map(|i| &without.actions[i..(i + 6).min(without.actions.len())]),
        first.map(|i| &with.actions[i..(i + 6).min(with.actions.len())]),
    );
    assert_eq!(without.starts, with.starts, "{case}: job starts");
    assert_eq!(without.up, with.up, "{case}: machines up by tick");
}

/// Warms on one machine start at least `interval_s` apart, and no two of the pool run
/// at once.
fn assert_paced(warms: &[WarmProc], interval_s: u64, case: &str) {
    for (i, a) in warms.iter().enumerate() {
        let end = a.ended.map_or(u64::MAX, |(t, _)| t);
        for b in &warms[i + 1..] {
            assert!(b.started >= end, "{case}: two warms at once: {a:?} {b:?}");
            if b.machine == a.machine {
                let gap = b.started - a.started;
                assert!(
                    gap >= interval_s,
                    "{case}: warms {gap}s apart on {}",
                    a.machine
                );
            }
        }
    }
}

/// Bursts of up to `max_boxes` jobs, each followed by silence long enough for the pool to
/// fall to `min_up`: the machines up, the stops and the resumes and forks of the next
/// burst are the same tick for tick with warms as without, whether warms take a minute
/// or end at once, run into idle stops, or recur on machines that stay up.
#[test]
fn warms_change_no_stop_and_no_start_across_bursts() {
    let deployed = POOL;
    // Machines stay up long enough for several warms each, and none gets a standby runner.
    let long_idle = "min_up = 1\nmax_boxes = 12\nslots_per_box = 1\nidle_stop_s = 1800";
    let cases = [
        ("deployed, warms of 60 s", deployed, 300, 60),
        ("deployed, warms that end at once", deployed, 300, 0),
        ("long idle stop, no standby", long_idle, 300, 60),
        ("long idle stop, shortest interval", long_idle, 106, 0),
    ];
    let mut stopped_mid_warm = 0;
    let mut cancelled_on_resume = 0;
    for (name, pool, interval_s, warm_s) in cases {
        for n in [1, 5, 12] {
            let case = format!("{name}, {n} jobs");
            let world = || {
                let knobs = Knobs {
                    warm_s,
                    ..Knobs::default()
                };
                let mut w = World::new(T0, knobs, 1);
                w.add_run("o/r", T0 + 900, &burst(n));
                w.add_run("o/r", T0 + 6_300, &burst(n));
                w
            };
            let (without, with) = twice(pool, interval_s, world, 10_800, |_| {});
            assert_same(&without, &with, &case);
            // One job takes the kept machine's standby runner, which stays, so nothing warms.
            assert_eq!(with.stats.warms == 0, n == 1 && pool == deployed, "{case}");
            assert_paced(&with.warms, interval_s, &case);
            // Down to `min_up` before the second burst and at the end.
            for at in [6_300, 10_800] {
                let tick = (at / 15) as usize - 1;
                assert_eq!(with.up[tick], 1, "{case}: machines up at {at}s");
            }
            assert!(
                with.starts.iter().all(Option::is_some),
                "{case}: a job never ran"
            );
            stopped_mid_warm += with.stats.warm_stops;
            cancelled_on_resume += with.cancels.len();
        }
    }
    // The cases reach the corners this test is for.
    assert!(stopped_mid_warm > 0, "no idle stop came during a warm");
    assert!(
        cancelled_on_resume > 0,
        "no resumed machine had its warm cancelled"
    );
}

/// A quiet pool at `min_up` with standby runners keeps one on its machine for good, so
/// that machine never warms. Without standby runners it warms once every `interval_s`.
#[test]
fn a_quiet_pool_warms_only_without_standby_runners() {
    let quiet_s = 6 * 3_600;
    let world = || World::new(T0, Knobs::default(), 1);
    let (without, with) = twice(POOL, 300, world, quiet_s, |_| {});
    assert_same(&without, &with, "standby 2");
    assert_eq!(with.stats.warms, 0, "{:?}", with.stats);
    assert_eq!(with.stats.registers, 1, "{:?}", with.stats);
    // A fresh daemon waits out its restart window (90 s) before its first fork.
    assert!(with.up.iter().skip(7).all(|&n| n == 1), "{:?}", with.up);

    let no_standby = "min_up = 1\nmax_boxes = 12\nslots_per_box = 1\nidle_stop_s = 600";
    let (without, with) = twice(no_standby, 300, world, quiet_s, |_| {});
    assert_same(&without, &with, "standby 0");
    assert_eq!(with.stats.registers, 0, "{:?}", with.stats);
    assert_paced(&with.warms, 300, "standby 0");
    // The kept machine is up about two minutes in; its first warm is due 300 s after the
    // daemon first sees it up, and one every 300 s after that, each on the tick that
    // reaches it.
    let n = with.stats.warms;
    assert!((quiet_s / 315..=quiet_s / 300).contains(&n), "{n} warms");
}

/// A job that finds the only spare machine warming cancels the warm and registers on the
/// same tick it would without warms. A cancel that fails delays the register a tick.
#[test]
fn a_job_cancels_a_running_warm_and_starts_on_time() {
    let pool = "min_up = 1\nmax_boxes = 2\nslots_per_box = 1\nidle_stop_s = 3600";
    let world = |fails: u32| {
        move || {
            let knobs = Knobs {
                warm_s: 600,
                ..Knobs::default()
            };
            let mut w = World::new(T0, knobs, 1);
            w.seed_machine(0, "ci", None, BoxState::Up);
            w.seed_machine(0, "ci", None, BoxState::Stopped { since: T0 - 600 });
            w.add_run("o/r", T0 + 300, &burst(1));
            w.rules.push(Rule::new(
                is_cancel,
                Effect::Status(500, json!({"code": "internal"})),
                fails,
            ));
            w
        }
    };
    let (without, with) = twice(pool, 120, world(0), 1_200, |_| {});
    assert_same(&without, &with, "cancel confirmed");
    assert_eq!(with.stats.warm_cancels, 1, "{:?}", with.stats);
    assert_eq!(with.stats.resumes, 0, "{:?}", with.stats);

    let (_, failed) = twice(pool, 120, world(1), 1_200, |_| {});
    let on_time = without.starts[0].unwrap();
    assert_eq!(failed.starts[0], Some(on_time + 15), "{:?}", failed.stats);
    assert_eq!(failed.stats.resumes, 0, "{:?}", failed.stats);
}

/// A cancel to `machine` went out within `within_s` after `at` (seconds after `T0`).
fn cancelled(o: &Outcome, machine: &str, at: u64, within_s: u64) -> bool {
    o.cancels
        .iter()
        .any(|(t, m)| m == machine && (at..=at + within_s).contains(t))
}

/// A restart in the middle of a warm, with the warm file kept, cancels that warm before
/// anything else runs on its machine, and so does a launch whose answer boat lost.
/// Neither moves a stop or a start.
#[test]
fn a_restart_or_a_lost_launch_mid_warm_changes_no_action() {
    let world = |lost: u32| {
        let mut w = World::new(T0, Knobs::default(), 1);
        w.add_run("o/r", T0 + 900, &burst(12));
        w.add_run("o/r", T0 + 6_300, &burst(12));
        // boat carries out these launches, but their answers are lost.
        w.rules.push(Rule::new(
            |r| is_warm(r) && !is_cancel(r),
            Effect::Lost,
            lost,
        ));
        w
    };
    // Three restarts, each while a warm runs; the run without warms restarts then too.
    let mut at = Vec::new();
    let with = run(world(0), cfg(POOL, Some(300)), 10_800, |s| {
        let warming = s.world().warms.iter().find(|w| w.ended.is_none()).cloned();
        if let Some(w) = warming.filter(|_| at.len() < 3) {
            at.push((s.now, w.machine));
            s.restart(0);
        }
    });
    let without = run(world(0), cfg(POOL, None), 10_800, |s| {
        if at.iter().any(|(t, _)| *t == s.now) {
            s.restart(0);
        }
    });
    assert_same(&without, &with, "restarts mid-warm");
    assert_eq!(at.len(), 3);
    for (t, machine) in &at {
        let t = t - T0;
        assert!(
            cancelled(&with, machine, t, 120),
            "restart at {t}s: {:?}",
            with.cancels
        );
    }

    let (without, with) = twice(POOL, 300, || world(4), 10_800, |_| {});
    assert_same(&without, &with, "lost launches");
    for w in &with.warms[..4] {
        let t = w.started - T0;
        assert!(
            cancelled(&with, &w.machine, t, 120),
            "lost launch at {t}s: {:?}",
            with.cancels
        );
    }
    assert!(with.stats.warm_cancels >= 1, "{:?}", with.stats);
}

/// A cancel that keeps failing holds the job on its machine, with no other machine
/// started for it, until the warm's own `timeout_s` ends it; the register follows on the
/// next tick.
#[test]
fn a_cancel_that_keeps_failing_holds_the_job_until_the_warm_times_out() {
    let pool = "min_up = 1\nmax_boxes = 2\nslots_per_box = 1\nidle_stop_s = 3600";
    let knobs = Knobs {
        warm_s: 3_600,
        ..Knobs::default()
    };
    let mut w = World::new(T0, knobs, 1);
    w.seed_machine(0, "ci", None, BoxState::Up);
    w.seed_machine(0, "ci", None, BoxState::Stopped { since: T0 - 600 });
    w.add_run("o/r", T0 + 300, &burst(1));
    w.rules.push(Rule::new(
        is_cancel,
        Effect::Status(500, json!({"code": "internal"})),
        u32::MAX,
    ));
    let with = run(w, cfg(pool, Some(120)), 3_600, |_| {});
    let warm = &with.warms[0];
    assert_eq!(
        warm.ended.map(|(t, end)| (t - warm.started, end)),
        Some((600, WarmEnd::TimedOut))
    );
    let ended = warm.ended.unwrap().0 - T0;
    let started = with.starts[0].expect("the job ran");
    assert!(
        (ended..=ended + 30).contains(&started),
        "warm ended {ended}s, job started {started}s"
    );
    assert_eq!(with.stats.resumes, 0, "{:?}", with.stats);
}
