//! S3 conformance against MinIO and Garage (`docker compose -f docker-compose.it.yml up -d minio garage`).
//!
//! MinIO uses the compose credentials and the pre-created bucket `asc-it` (override with
//! `ASC_IT_MINIO_{ENDPOINT,BUCKET,USER,PASSWORD}`). Garage defaults to the fixed key made by deploy/garage-init.sh; override with `ASC_IT_GARAGE_KEY` and
//! `ASC_IT_GARAGE_SECRET` (plus optional `ASC_IT_GARAGE_{ENDPOINT,BUCKET,REGION}`).
#![cfg(feature = "it-s3")]

use std::sync::Arc;

use asc_core::id::FileId;
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::conformance::run_objects_suite;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::s3::{MULTIPART_THRESHOLD, Resilient};
use bytes::Bytes;
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
            user: env("ASC_IT_GARAGE_KEY", "GK0123456789abcdef01234567"),
            pass: env(
                "ASC_IT_GARAGE_SECRET",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
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
    let (store, root) = t.open(false);
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

impl Target {
    /// The same store through the production entry point: an `s3://` URL with `endpoint`, `region`
    /// and `path_style`, static credentials, and the create fallback on (or off for `strict`).
    fn open(&self, strict: bool) -> (Arc<dyn ObjectStore>, Path) {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut url = url::Url::parse(&format!("s3://{}/it-open-{n:x}", self.bucket)).unwrap();
        url.query_pairs_mut()
            .append_pair("endpoint", &self.endpoint)
            .append_pair("region", &self.region)
            .append_pair("path_style", "true");
        let creds = Some(asc_store::s3::S3Credentials {
            key_id: self.user.clone(),
            secret: self.pass.clone(),
        });
        if strict {
            asc_store::s3::open_strict(&url, creds).unwrap()
        } else {
            asc_store::s3::open_with(&url, creds).unwrap()
        }
    }
}

/// `open_with` against Garage: path-style, region `garage`, then put, get, ranged get, list,
/// copy and delete through the wrapper.
async fn open_roundtrip(t: Target) {
    let (store, root) = t.open(false);
    let a = root.clone().join("a");
    let b = root.clone().join("b");
    store
        .put(&a, PutPayload::from_static(b"0123456789"))
        .await
        .unwrap();
    assert_eq!(
        &store.get(&a).await.unwrap().bytes().await.unwrap()[..],
        b"0123456789"
    );
    assert_eq!(&store.get_range(&a, 2..5).await.unwrap()[..], b"234");
    store.copy(&a, &b).await.unwrap();
    assert_eq!(
        &store.get(&b).await.unwrap().bytes().await.unwrap()[..],
        b"0123456789"
    );
    let listed: Vec<_> = futures::TryStreamExt::try_collect(store.list(Some(&root)))
        .await
        .unwrap();
    assert_eq!(listed.len(), 2, "{listed:?}");
    store.delete(&a).await.unwrap();
    store.delete(&b).await.unwrap();
    assert!(matches!(
        store.get(&a).await,
        Err(object_store::Error::NotFound { .. })
    ));
}

/// More than one `ListObjectsV2` page (S3 pages hold at most 1000 keys).
async fn list_paginates(t: Target) {
    use futures::{StreamExt, TryStreamExt};
    let (store, root) = t.open(false);
    futures::stream::iter(0..1200)
        .map(|i| {
            let (store, p) = (store.clone(), root.clone().join(format!("k{i:05}")));
            async move { store.put(&p, PutPayload::from_static(b"x")).await }
        })
        .buffer_unordered(16)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let listed: Vec<_> = store.list(Some(&root)).try_collect().await.unwrap();
    assert_eq!(listed.len(), 1200);
    let mut names: Vec<_> = listed.iter().map(|m| m.location.to_string()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 1200);
}

#[tokio::test]
#[ignore = "needs Garage from docker-compose.it.yml"]
async fn garage_open_roundtrip() {
    open_roundtrip(Target::garage().unwrap()).await;
}

#[tokio::test]
#[ignore = "needs Garage from docker-compose.it.yml"]
async fn garage_large_value_is_multipart() {
    multipart(Target::garage().unwrap()).await;
}

#[tokio::test]
#[ignore = "needs Garage from docker-compose.it.yml"]
async fn garage_list_paginates() {
    list_paginates(Target::garage().unwrap()).await;
}

/// A store that ignores `If-None-Match` lets a repeated `Create` through. The strict store reports
/// that honestly (no silent downgrade), so the probe can refuse `BucketRefs`; the default store
/// keeps working for content-addressed keys.
#[tokio::test]
#[ignore = "needs Garage from docker-compose.it.yml"]
async fn garage_strict_store_does_not_downgrade() {
    let t = Target::garage().unwrap();
    let (strict, root) = t.open(true);
    let r = asc_store::probe::probe(&*strict, &root).await;
    eprintln!("garage strict probe: {} {}", r.create_exclusive, r.detail);
    assert!(!r.detail.is_empty());
    let (loose, root) = t.open(false);
    let key = root.join("create-twice");
    let create = || {
        loose.put_opts(
            &key,
            PutPayload::from_static(b"v"),
            object_store::PutOptions {
                mode: object_store::PutMode::Create,
                ..Default::default()
            },
        )
    };
    let first = create().await;
    let second = create().await;
    eprintln!("garage loose Create twice: {first:?} then {second:?}");
    first.unwrap();
    // Either exclusive (AlreadyExists) or ignored (Ok): never an unrelated error.
    assert!(matches!(
        second,
        Ok(_) | Err(object_store::Error::AlreadyExists { .. })
    ));
}
