//! Crash safety of git import (SPEC §14.4 I1-I5, test `import_crash`): 200 seeded runs kill
//! the importer at a random write and rerun it; the store and git map must equal an
//! uninterrupted import byte for byte.

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::sync::Arc;

use asc_engine::Engine;
use asc_git::import::{ImportPolicy, Importer};
use asc_git::odb::Odb;
use asc_store::bucket::BucketObjects;
use fixtures::{KillObjects, Killer, MemMap, NoRefs, Repo, dump_store, repo_key, small_fixture};

const RUNS: u64 = 200;

/// One importer run over a shared store and map; `Err` means it was killed.
async fn run(
    repo: &Repo,
    store: &Arc<BucketObjects>,
    map: &MemMap,
    killer: &Arc<Killer>,
) -> Result<(), String> {
    let objects = Arc::new(KillObjects {
        inner: store.clone(),
        killer: killer.clone(),
    });
    let engine = Engine::new(objects.clone(), Arc::new(NoRefs), repo_key());
    let odb = Odb::open(repo.path()).expect("open odb");
    let policy = ImportPolicy {
        window: 3,
        batch: 2,
        workers: 2,
    };
    let mut importer = Importer::new(&engine, objects, &odb, map, policy);
    importer
        .import_commits(&repo.tips())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// xorshift64*: a seeded, dependency-free generator.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[tokio::test]
async fn killed_imports_rerun_to_identical_bytes() {
    let repo = small_fixture();

    // The reference: one uninterrupted import, counting its writes.
    let clean_store = Arc::new(BucketObjects::memory());
    let counter = Arc::new(Killer::new(usize::MAX));
    let clean_map = MemMap::new(counter.clone());
    run(&repo, &clean_store, &clean_map, &counter)
        .await
        .expect("clean import");
    let total = counter.writes();
    assert!(
        total > 50,
        "fixture should write many objects, wrote {total}"
    );
    let want_store = dump_store(&*clean_store).await;
    let want_map = clean_map.dump();

    let mut killed = 0;
    for seed in 1..=RUNS {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let kill_at = (next(&mut rng) % total as u64) as usize;

        let store = Arc::new(BucketObjects::memory());
        let killer = Arc::new(Killer::new(kill_at));
        let map = MemMap::new(killer.clone());
        if run(&repo, &store, &map, &killer).await.is_err() {
            killed += 1;
        }
        // The process restarts with nothing in memory and no fault.
        killer.set_limit(usize::MAX);
        run(&repo, &store, &map, &killer)
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: rerun failed: {e}"));

        assert_eq!(dump_store(&*store).await, want_store, "seed {seed}: store");
        assert_eq!(map.dump(), want_map, "seed {seed}: git map");
    }
    assert_eq!(killed, RUNS, "every seeded run must have been killed");
}
