//! SPEC 17.2: the scenarios on many seeds, replay, and proof that each invariant has teeth.

use asc_sim::{Bugs, Plan, Scenario, run_one, run_plan};

#[test]
fn seeds_pass_every_scenario() {
    let plan = Plan::from_env();
    let reports = match run_plan(&plan) {
        Ok(r) => r,
        Err(f) => panic!("{f}"),
    };
    assert_eq!(reports.len(), plan.seeds.len() * plan.scenarios.len());
}

#[test]
fn a_seed_replays_identically() {
    for sc in Scenario::ALL {
        for seed in [3, 11, 42] {
            let a = run_one(sc, seed, Bugs::default()).expect("run a");
            let b = run_one(sc, seed, Bugs::default()).expect("run b");
            assert_eq!(
                a.trace,
                b.trace,
                "{} seed {seed} is not deterministic",
                sc.name()
            );
            assert_eq!(a.steps, b.steps);
        }
    }
}

#[test]
fn different_seeds_take_different_paths() {
    let a = run_one(Scenario::ConcurrentLand, 1, Bugs::default()).expect("run");
    let b = run_one(Scenario::ConcurrentLand, 2, Bugs::default()).expect("run");
    assert_ne!(a.trace, b.trace);
}

#[test]
fn faults_and_crashes_actually_happen() {
    let mut crashes = 0;
    let mut unavailable = 0;
    let mut lost = 0;
    let mut conflicts = 0;
    for seed in 1..=20 {
        let r = run_one(Scenario::ConcurrentLand, seed, Bugs::default()).expect("run");
        crashes += r.crashes;
        unavailable += r.unavailable;
        lost += r.lost;
        conflicts += r.conflicts;
        assert!(r.landed >= 20);
    }
    assert!(crashes > 20, "crash points never fired: {crashes}");
    assert!(unavailable > 20, "no UNAVAILABLE injected");
    assert!(lost > 5, "no lost response injected");
    assert!(conflicts > 20, "concurrent CAS never conflicted");
}

fn caught(sc: Scenario, bugs: Bugs, needle: &str) {
    let mut hit = false;
    for seed in 1..=60 {
        if let Err(f) = run_one(sc, seed, bugs) {
            assert!(
                f.message.contains(needle),
                "seed {seed} failed for another reason: {f}"
            );
            assert!(f.to_string().contains(&format!("ASC_SIM_SEED={seed}")));
            hit = true;
            break;
        }
    }
    assert!(hit, "{} with {bugs:?} was never caught", sc.name());
}

#[test]
fn a_retry_without_dedupe_is_caught() {
    let bugs = Bugs {
        no_dedupe: true,
        ..Bugs::default()
    };
    caught(Scenario::ConcurrentLand, bugs, "op_id");
}

#[test]
fn a_missing_reconciler_is_caught() {
    let bugs = Bugs {
        skip_reconcile: true,
        ..Bugs::default()
    };
    caught(Scenario::OutboxRestart, bugs, "post_receive");
}

#[test]
fn a_gc_that_ignores_min_age_is_caught() {
    let bugs = Bugs {
        gc_ignores_age: true,
        ..Bugs::default()
    };
    caught(Scenario::GcTraffic, bugs, "");
}

#[test]
fn writing_a_changeset_before_its_closure_is_refused() {
    use asc_sim::faults::FaultPlan;
    use asc_sim::model::{Cs, Fail, SimKey};
    use asc_sim::world::{Role, World};
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let cs = Cs {
        parent: None,
        files: vec![(0, [9; 32])],
        accepted: vec![],
        nonce: 1,
    };
    let mut w = World::new(1, &[Role::Primary], FaultPlan::default(), Bugs::default());
    let res = rt.block_on(w.put_obj(SimKey::Cs(cs.id()), &cs.encode()));
    assert!(matches!(res, Err(Fail::Violation(m)) if m.contains("I1")));
}

#[test]
fn a_cache_without_coalescing_stampedes() {
    let bugs = Bugs {
        no_coalesce: true,
        ..Bugs::default()
    };
    caught(Scenario::CacheStampede, bugs, "upstream requests");
}

#[test]
fn a_skewed_request_with_an_effect_is_caught() {
    let bugs = Bugs {
        skew_has_effect: true,
        ..Bugs::default()
    };
    caught(Scenario::VersionSkew, bugs, "version-skewed");
}

#[test]
fn the_cache_sheds_instead_of_stampeding() {
    for seed in 1..=10 {
        let r = run_one(Scenario::CacheStampede, seed, Bugs::default()).expect("run");
        assert!(
            r.upstream_calls < 300 * 40,
            "{} upstream calls",
            r.upstream_calls
        );
    }
}
