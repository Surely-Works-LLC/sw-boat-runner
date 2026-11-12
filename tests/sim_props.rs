//! Whole-loop property tests: proptest picks a policy, boat's and GitHub's behaviour, a
//! workload and a fault schedule, then the real `run::Loop` runs over virtual time
//! against the stateful fake in tests/sim, and the world checks the invariants.
//!
//! `SIM_CASES` sets the number of cases (default 100) and `SIM_SEED` proptest's seed
//! (default fixed, so CI runs the same cases every time; `SIM_SEED=random` draws one).
//! A failure prints the shrunk `Case`; `run_case` replays it.
//!
//! Two behaviours of the world stay off here because the daemon is known to break under
//! them (tests/sim_findings.rs): a boat listing older than the one before it
//! (`list_jitter_s`) and a GitHub runner list that lags more than a tick
//! (`runner_list_lag_s` above 15 s). The known bugs in tests/sim_known_bugs.rs are each a
//! delay or a corner these profiles reach rarely, so the properties pass with them.

mod sim;

use proptest::prelude::*;
use proptest::test_runner::{Config as PtConfig, RngSeed};
use sim::*;

/// Simulated time: jobs arrive in the first `ACTIVE_S`, then `SETTLE_S` without faults.
const ACTIVE_S: u64 = 2_700;
/// The most the settle phase may take; it ends as soon as nothing is left to do.
const SETTLE_S: u64 = 4 * 3_600;

#[derive(Clone, Debug)]
pub struct PoolCase {
    preset: &'static str,
    max_boxes: u32,
    slots: u32,
    min_up: u32,
    idle_stop_s: u64,
    min_up_s: u64,
    register_timeout_s: u64,
    cap: u32,
    second_pool: bool,
}

#[derive(Clone, Debug)]
pub struct JobCase {
    at: u64,
    labels: u8,
    run_s: u64,
    cancel_after: Option<u64>,
}

/// The behaviours of boat and GitHub that the TLA spec abstracts away.
#[derive(Clone, Debug)]
pub struct WorldCase {
    list_jitter_s: u64,
    runner_list_lag_s: u64,
    runner_linger_s: u64,
    runner_death: f64,
    disk_keep_s: u64,
    /// A later job of a run (`needs:`) is queued this long after the first.
    stagger_s: u64,
}

fn world_case() -> impl Strategy<Value = WorldCase> {
    (
        // `list_jitter_s` and a runner list lag over a tick: see the module comment.
        prop::sample::select(vec![0u64]),
        prop::sample::select(vec![0u64, 0, 10]),
        prop::sample::select(vec![0u64, 30]),
        prop::sample::select(vec![0.0, 0.0, 0.01]),
        prop::sample::select(vec![14 * DAY_S, 1_800]),
        prop::sample::select(vec![0u64, 0, 90]),
    )
        .prop_map(|(j, rl, linger, death, disk, stagger)| WorldCase {
            list_jitter_s: j,
            runner_list_lag_s: rl,
            runner_linger_s: linger,
            runner_death: death,
            disk_keep_s: disk,
            stagger_s: stagger,
        })
}

#[derive(Clone, Debug)]
pub struct Case {
    seed: u64,
    /// Seconds after 00:00 UTC the run starts; some runs cross midnight.
    time_of_day: u64,
    pool: PoolCase,
    boot_s: u64,
    list_lag_s: u64,
    runner_online_s: u64,
    page_size: usize,
    included_per_hour: u32,
    paid_per_hour: u32,
    paid_cents: u32,
    runs: Vec<Vec<JobCase>>,
    faults: Faults,
    restarts: Vec<u64>,
    world: WorldCase,
    /// End with `drain` (true) or by killing the daemon (false).
    drain_at_end: bool,
}

fn labels(kind: u8) -> Vec<String> {
    let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    match kind {
        0..=2 => l(&["self-hosted", "large"]),
        3 => l(&["self-hosted", "LARGE"]),
        4 => l(&["self-hosted", "large", "gpu"]),
        _ => l(&["ubuntu-latest"]),
    }
}

fn pool_case() -> impl Strategy<Value = PoolCase> {
    (
        prop::sample::select(vec!["warm", "fork", "hybrid", "pool"]),
        1u32..=4,
        1u32..=2,
        prop::sample::select(vec![0u32, 0, 1, 2]),
        prop::sample::select(vec![30u64, 120, 600]),
        prop::sample::select(vec![0u64, 60]),
        prop::sample::select(vec![120u64, 900]),
        prop::sample::select(vec![0u32, 3, 20, 1_000]),
        any::<bool>(),
    )
        .prop_map(
            |(preset, max_boxes, slots, min_up, idle_stop_s, min_up_s, rt, cap, second)| {
                let min_up = match preset {
                    "fork" => 0,
                    "pool" => min_up.clamp(1, max_boxes),
                    _ => min_up.min(max_boxes),
                };
                PoolCase {
                    preset,
                    max_boxes,
                    slots,
                    min_up,
                    idle_stop_s,
                    min_up_s,
                    register_timeout_s: rt,
                    cap,
                    second_pool: second,
                }
            },
        )
}

fn job_case() -> impl Strategy<Value = JobCase> {
    (
        0u64..ACTIVE_S,
        0u8..6,
        prop::sample::select(vec![5u64, 30, 120, 600]),
        prop::option::weighted(0.15, 0u64..600),
    )
        .prop_map(|(at, labels, run_s, cancel_after)| JobCase {
            at,
            labels,
            run_s,
            cancel_after,
        })
}

/// Probabilities per request: 0, rare, or frequent.
fn p() -> impl Strategy<Value = f64> {
    prop::sample::select(vec![0.0, 0.0, 0.005, 0.02])
}

/// Faults that no known bug turns into a violation.
fn safe_faults() -> impl Strategy<Value = Faults> {
    (p(), p(), p(), p(), p(), p(), p(), p()).prop_map(
        |(boat_net, boat_5xx, boat_429, boat_lost, gh_net, gh_5xx, gh_429, crash)| Faults {
            crash: crash / 2.0,
            boat_net,
            boat_5xx,
            boat_429,
            boat_lost,
            gh_net,
            gh_5xx,
            gh_429,
            ..Faults::none()
        },
    )
}

/// Every fault the fake has, the fatal ones and lost GitHub answers among them.
fn all_faults() -> impl Strategy<Value = Faults> {
    (safe_faults(), p(), p(), p(), p()).prop_map(|(f, boat_auth, gh_auth, gh_lost, boot)| Faults {
        boat_auth: boat_auth / 5.0,
        gh_auth: gh_auth / 5.0,
        gh_lost,
        boot_error: boot,
        ..f
    })
}

fn case(faults: BoxedStrategy<Faults>) -> impl Strategy<Value = Case> {
    (
        (
            any::<u64>(),
            prop::sample::select(vec![3_600u64, 43_200, DAY_S - 1_800]),
            pool_case(),
            prop::sample::select(vec![5u64, 20, 60]),
            prop::sample::select(vec![0u64, 0, 10, 45]),
            prop::sample::select(vec![5u64, 30]),
            prop::sample::select(vec![1usize, 2, 200]),
        ),
        (
            prop::sample::select(vec![0u32, 2, 1_000]),
            prop::sample::select(vec![0u32, 3, 100]),
            prop::sample::select(vec![1u32, 3, 7]),
            prop::collection::vec(prop::collection::vec(job_case(), 1..4), 0..8),
            faults,
            prop::collection::vec(0u64..(ACTIVE_S / 15), 0..3),
            any::<bool>(),
            world_case(),
        ),
    )
        .prop_map(
            |(
                (seed, time_of_day, pool, boot_s, list_lag_s, runner_online_s, page_size),
                (included, paid_per_hour, paid_cents, runs, faults, restarts, drain_at_end, world),
            )| Case {
                world,
                seed,
                time_of_day,
                pool,
                boot_s,
                list_lag_s,
                runner_online_s,
                page_size,
                included_per_hour: included,
                paid_per_hour,
                paid_cents,
                runs,
                faults,
                restarts,
                drain_at_end,
            },
        )
}

fn policy_of(p: &PoolCase) -> String {
    let mut text = format!(
        "repo = \"o/r\"\ntick_s = 15\n[[pool]]\nname = \"ci\"\n\
         labels = [\"self-hosted\", \"large\"]\nmachine_type = \"large\"\nsnapshot = \"snap\"\n\
         preset = \"{}\"\nmax_boxes = {}\nslots_per_box = {}\nmin_up = {}\nidle_stop_s = {}\n\
         min_up_s = {}\nregister_timeout_s = {}\nfork_ttl_s = 3600\npaid_start_cents_per_day = {}\n",
        p.preset,
        p.max_boxes,
        p.slots,
        p.min_up,
        p.idle_stop_s,
        p.min_up_s,
        p.register_timeout_s,
        p.cap
    );
    if p.second_pool {
        text.push_str(
            "[[pool]]\nname = \"gpu\"\nlabels = [\"self-hosted\", \"large\", \"gpu\"]\n\
             machine_type = \"large\"\nsnapshot = \"snap\"\npreset = \"fork\"\nmax_boxes = 2\n\
             register_timeout_s = 120\npaid_start_cents_per_day = 5\n",
        );
    }
    text
}

/// Builds and runs one case, returning the finished simulation.
pub fn run_case(c: &Case) -> Sim {
    let start = 20_000 * DAY_S + c.time_of_day;
    let knobs = Knobs {
        boot_s: c.boot_s,
        list_lag_s: c.list_lag_s,
        runner_online_s: c.runner_online_s,
        page_size: c.page_size,
        list_jitter_s: c.world.list_jitter_s,
        runner_list_lag_s: c.world.runner_list_lag_s,
        runner_linger_s: c.world.runner_linger_s,
        runner_death: c.world.runner_death,
        disk_keep_s: c.world.disk_keep_s,
        pricing: Pricing {
            included_per_hour: c.included_per_hour,
            paid_per_hour: c.paid_per_hour,
            paid_cents: c.paid_cents,
            can_start: true,
        },
        ..Knobs::default()
    };
    let mut world = World::new(start, knobs, c.seed);
    world.keep_log = false;
    world.faults = c.faults.clone();
    for run in &c.runs {
        let at = start + run[0].at;
        let jobs: Vec<(Vec<String>, u64)> =
            run.iter().map(|j| (labels(j.labels), j.run_s)).collect();
        let idx = world.add_run("o/r", at, &jobs);
        for (n, (i, j)) in idx.into_iter().zip(run).enumerate() {
            let at = at + n as u64 * c.world.stagger_s;
            world.jobs[i].created_at = at;
            world.jobs[i].cancel_at = j.cancel_after.map(|s| at + s);
        }
    }
    let cfg = policy(&policy_of(&c.pool));
    let mut s = Sim::new(world, vec![cfg]);
    let ticks = ACTIVE_S / s.tick_s;
    for t in 0..ticks {
        if c.restarts.contains(&t) {
            s.restart(0);
        }
        s.step();
    }
    s.settle(SETTLE_S);
    s.end_checks();
    if c.drain_at_end {
        // `drain` right after a start misses the machine boat has not listed yet (a known
        // bug, tests/sim_known_bugs.rs), so wait out the listing lag first.
        s.drain_checks(0, c.list_lag_s + c.world.list_jitter_s + 15);
    } else {
        s.abandon_checks();
    }
    s
}

fn cases() -> u32 {
    std::env::var("SIM_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}

fn seed() -> RngSeed {
    match std::env::var("SIM_SEED").ok().as_deref() {
        Some("random") => RngSeed::Random,
        Some(v) => RngSeed::Fixed(v.parse().expect("SIM_SEED is a number or random")),
        None => RngSeed::Fixed(0x5b_0a7),
    }
}

fn config() -> PtConfig {
    PtConfig {
        cases: cases(),
        rng_seed: seed(),
        failure_persistence: None,
        ..PtConfig::default()
    }
}

/// Kinds a fault this profile injects may cause without any bug: a fatal answer makes
/// the daemon exit, and the supervisor in `Sim` starts it again.
const EXPECTED: [Kind; 1] = [Kind::Exit];

proptest! {
    #![proptest_config(config())]

    /// Every invariant, under faults the known bugs do not turn into violations.
    #[test]
    fn invariants_hold(c in case(safe_faults().boxed())) {
        let s = run_case(&c);
        let v = s.violations_except(&EXPECTED);
        prop_assert!(v.is_empty(), "{}\nstats {:?}", v.iter().take(12).map(|v| v.to_string()).collect::<Vec<_>>().join("\n"), s.world().stats);
    }
}

proptest! {
    #![proptest_config(config())]

    /// As `invariants_hold`, with every fault: fatal answers, lost GitHub answers and
    /// machines that boot into `error`.
    #[test]
    fn every_fault(c in case(all_faults().boxed())) {
        let s = run_case(&c);
        let v = s.violations_except(&EXPECTED);
        prop_assert!(v.is_empty(), "{}\nstats {:?}", v.iter().take(12).map(|v| v.to_string()).collect::<Vec<_>>().join("\n"), s.world().stats);
    }
}
