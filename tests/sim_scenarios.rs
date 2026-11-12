//! Hand-written whole-loop scenarios on the stateful fake (tests/sim). Each drives the
//! real `run::Loop` with `decide::decide` over virtual time.

mod sim;

use sim::*;

const T0: u64 = 20_000 * DAY_S + 3_600;

fn world(knobs: Knobs) -> World {
    World::new(T0, knobs, 1)
}

#[test]
fn fork_pool_serves_every_job_and_deletes_its_forks() {
    let mut w = world(Knobs::default());
    w.add_run(
        "o/r",
        T0 + 120,
        &[(large(), 60), (large(), 300), (large(), 30)],
    );
    let mut s = Sim::new(w, vec![one_pool("fork", "max_boxes = 4")]);
    s.run_for(1_800);
    s.settle(3_600);
    s.end_checks();
    s.drain_checks(0, 0);
    s.assert_clean(&[]);
    let w = s.world();
    assert_eq!(w.stats.creates, 3, "{:?}", w.stats);
    assert!(w.jobs.iter().all(|j| j.status == JobStatus::Done));
}

#[test]
fn warm_pool_stops_idle_machines_and_resumes_them() {
    let mut w = world(Knobs::default());
    w.add_run("o/r", T0 + 60, &[(large(), 60)]);
    w.add_run("o/r", T0 + 1_500, &[(large(), 60)]);
    let cfg = one_pool("warm", "max_boxes = 2\nidle_stop_s = 120\nmin_up_s = 0");
    let mut s = Sim::new(w, vec![cfg]);
    s.run_for(3_000);
    s.assert_clean(&[]);
    let w = s.world();
    assert_eq!((w.stats.creates, w.stats.resumes, w.stats.stops), (1, 1, 2));
    assert!(w.jobs.iter().all(|j| j.status == JobStatus::Done));
}

/// Two daemons on one boat account, for one repository with pools of different names, and
/// for two repositories with pools of different names: neither touches the other's
/// machines, even while one is down.
#[test]
fn two_daemons_with_different_pool_names_keep_to_their_own_machines() {
    for other_repo in ["o/r", "o/other"] {
        let mut w = world(Knobs {
            list_lag_s: 10,
            ..Knobs::default()
        });
        let b_labels = || vec!["self-hosted".to_string(), "b".to_string()];
        w.add_run("o/r", T0 + 30, &[(large(), 300), (large(), 120)]);
        w.add_run(other_repo, T0 + 60, &[(b_labels(), 300), (b_labels(), 60)]);
        let a = one_pool(
            "hybrid",
            "max_boxes = 2
idle_stop_s = 60
min_up_s = 0",
        );
        let b = policy(&format!(
            "repo = \"{other_repo}\"\n[[pool]]\nname = \"b\"\nlabels = [\"self-hosted\", \"b\"]\n\
             snapshot = \"snap\"\npreset = \"hybrid\"\nmax_boxes = 2\nidle_stop_s = 60\nmin_up_s = 0\n"
        ));
        let mut s = Sim::new(w, vec![a, b]);
        s.run_for(900);
        s.kill(0);
        s.world_mut()
            .add_run(other_repo, s.now, &[(b_labels(), 60)]);
        s.run_for(900);
        s.settle(4 * 3_600);
        s.end_checks();
        s.assert_clean(&[]);
        let w = s.world();
        assert!(w.stats.creates >= 2, "{other_repo}: {:?}", w.stats);
    }
}
