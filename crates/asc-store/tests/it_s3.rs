//! S3 conformance against MinIO and Garage (`docker compose -f docker-compose.it.yml up -d minio garage`).
//!
//! MinIO uses the compose credentials and the pre-created bucket `asc-it` (override with
//! `ASC_IT_MINIO_{ENDPOINT,BUCKET,USER,PASSWORD}`). Garage needs `ASC_IT_GARAGE_KEY` and
//! `ASC_IT_GARAGE_SECRET` (plus optional `ASC_IT_GARAGE_{ENDPOINT,BUCKET,REGION}`) and is skipped without them.
#![cfg(feature = "it-s3")]

use std::sync::Arc;

use bytes::Bytes;
use asc_core::id::FileId;
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::conformance::run_objects_suite;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::s3::{MULTIPART_THRESHOLD, Resilient};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

struct Target {
    endpoint: String,
    bucket: String,
    region: String,
    user: String,
    pass: String,
}

impl Target {
    fn minio() -> Self {
        Self {
            endpoint: env("ASC_IT_MINIO_ENDPOINT", "http://127.0.0.1:9000"),
            bucket: env("ASC_IT_MINIO_BUCKET", "asc-it"),
            region: "us-east-1".into(),
            user: env("ASC_IT_MINIO_USER", "asctest"),
            pass: env("ASC_IT_MINIO_PASSWORD", "asctest-secret"),
        }
    }

    fn garage() -> Option<Self> {
        Some(Self {
            endpoint: env("ASC_IT_GARAGE_ENDPOINT", "http://127.0.0.1:3900"),
            bucket: env("ASC_IT_GARAGE_BUCKET", "asc-it"),
            region: env("ASC_IT_GARAGE_REGION", "garage"),
            user: std::env::var("ASC_IT_GARAGE_KEY").ok()?,
            pass: std::env::var("ASC_IT_GARAGE_SECRET").ok()?,
        })
    }

    /// The production wrapper over a path-style S3 store, and a run-unique key root.
    fn store(&self) -> (Arc<dyn ObjectStore>, Path) {
        let s3 = AmazonS3Builder::new()
            .with_endpoint(&self.endpoint)
            .with_allow_http(true)
            .with_bucket_name(&self.bucket)
            .with_region(&self.region)
            .with_access_key_id(&self.user)
            .with_secret_access_key(&self.pass)
            .with_retry(object_store::RetryConfig {
                max_retries: 0,
                ..Default::default()
            })
            .build()
            .unwrap();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Path::from(format!("it-{n:x}-{}", std::process::id()));
        (Arc::new(Resilient::new(s3)), root)
    }
}

async fn conformance(t: Target) {
    let (store, root) = t.store();
    let b = BucketObjects::new(store.clone(), root.clone());
    // A repeated put of a valid value is `Ok` and leaves it intact.
    let key = ObjKey::File(FileId::of(b"probe"));
    b.put(&key, encode_stored(b"probe")).await.unwrap();
    b.put(&key, encode_stored(b"probe")).await.unwrap();
    assert_eq!(&b.get(&key).await.unwrap().unwrap()[..], b"probe");
    run_objects_suite(&b, |k, v: Bytes| {
        let (store, root) = (store.clone(), root.clone());
        async move {
            let p = Path::from(format!("{root}/{}", k.path()));
            store.put(&p, PutPayload::from_bytes(v)).await.unwrap();
        }
    })
    .await;
}

async fn multipart(t: Target) {
    let (store, root) = t.store();
    let p = Path::from(format!("{root}/big"));
    let big = Bytes::from(vec![3u8; MULTIPART_THRESHOLD + 1024]);
    store
        .put(&p, PutPayload::from_bytes(big.clone()))
        .await
        .unwrap();
    let got = store.get(&p).await.unwrap().bytes().await.unwrap();
    assert!(got == big);
}

#[tokio::test]
#[ignore = "needs MinIO from docker-compose.it.yml"]
async fn minio_conformance() {
    conformance(Target::minio()).await;
}

#[tokio::test]
#[ignore = "needs MinIO from docker-compose.it.yml"]
async fn minio_large_value_is_multipart() {
    multipart(Target::minio()).await;
}

#[tokio::test]
#[ignore = "needs Garage from docker-compose.it.yml"]
async fn garage_conformance() {
    match Target::garage() {
        Some(t) => conformance(t).await,
        None => eprintln!("skipped: set ASC_IT_GARAGE_KEY and ASC_IT_GARAGE_SECRET"),
    }
}
