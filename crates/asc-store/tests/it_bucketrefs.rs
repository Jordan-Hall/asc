//! `BucketRefs` and the probe against MinIO and Garage (`docker compose -f docker-compose.it.yml up -d minio garage`).
//!
//! Uses the same environment overrides as `it_s3.rs`. MinIO must pass the probe and the CAS race;
//! Garage v2 is expected to fail the probe (SPEC §3.4).
#![cfg(feature = "it-s3")]

use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_store::bucketrefs::BucketRefs;
use asc_store::probe::probe;
use asc_store::refs::{Actor, Expect, RefError, Refs, RepoKey};
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use uuid::Uuid;

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// A raw S3 store (no create fallback) and a run-unique key root.
fn store(
    endpoint: &str,
    bucket: &str,
    region: &str,
    user: &str,
    pass: &str,
) -> (Arc<dyn ObjectStore>, Path) {
    let s3 = AmazonS3Builder::new()
        .with_endpoint(endpoint)
        .with_allow_http(true)
        .with_bucket_name(bucket)
        .with_region(region)
        .with_access_key_id(user)
        .with_secret_access_key(pass)
        .build()
        .unwrap();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = Path::from(format!("it-refs-{n:x}-{}", std::process::id()));
    (Arc::new(s3), root)
}

fn minio() -> (Arc<dyn ObjectStore>, Path) {
    store(
        &env("ASC_IT_MINIO_ENDPOINT", "http://127.0.0.1:9000"),
        &env("ASC_IT_MINIO_BUCKET", "asc-it"),
        "us-east-1",
        &env("ASC_IT_MINIO_USER", "asctest"),
        &env("ASC_IT_MINIO_PASSWORD", "asctest-secret"),
    )
}

fn actor() -> Actor {
    Actor {
        id: "it".into(),
        via: "test".into(),
    }
}

fn tgt(s: &str) -> RefTarget {
    RefTarget::Changeset(ChangesetId::of(s.as_bytes()))
}

#[tokio::test]
#[ignore = "needs MinIO from docker-compose.it.yml"]
async fn minio_probe_passes() {
    let (s, root) = minio();
    let r = probe(&*s, &root).await;
    assert!(r.create_exclusive, "{}", r.detail);
}

#[tokio::test]
#[ignore = "needs Garage from docker-compose.it.yml"]
async fn garage_probe_is_reported() {
    // Garage v2 is expected to fail; either way the probe must answer without panicking.
    let (s, root) = store(
        &env("ASC_IT_GARAGE_ENDPOINT", "http://127.0.0.1:3900"),
        &env("ASC_IT_GARAGE_BUCKET", "asc-it"),
        &env("ASC_IT_GARAGE_REGION", "garage"),
        &env("ASC_IT_GARAGE_KEY", "GK0123456789abcdef01234567"),
        &env(
            "ASC_IT_GARAGE_SECRET",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
    );
    let r = probe(&*s, &root).await;
    eprintln!("garage probe: {} {}", r.create_exclusive, r.detail);
    assert!(!r.detail.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs MinIO from docker-compose.it.yml"]
async fn minio_cas_race_has_one_winner() {
    let (s, root) = minio();
    let refs = BucketRefs::new(s, root);
    let name = RefName::new("refs/heads/race").unwrap();
    let mut tasks = Vec::new();
    for i in 0..16 {
        let (refs, name) = (refs.clone(), name.clone());
        tasks.push(tokio::spawn(async move {
            refs.cas(
                &name,
                Expect::Absent,
                Some(tgt(&i.to_string())),
                &actor(),
                Uuid::now_v7(),
            )
            .await
        }));
    }
    let mut wins = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(1) => wins += 1,
            Err(RefError::Conflict(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(wins, 1);
    let got = refs.get(RepoKey(1), &name).await.unwrap().unwrap();
    assert_eq!(got.version, 1);
    assert_eq!(refs.list(RepoKey(1), "refs/").await.unwrap().len(), 1);
}
