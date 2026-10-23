//! `Fork` and `CopyClosure` copy no derived index data (SPEC §3.3, §23.5, PLAN I15).

use asc_core::id::FileId;
use asc_serve::codeintel::backfill::{copy_closure, is_copied};
use asc_serve::migrate;
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;

fn derived(name: &'static str, version: u32, n: u8) -> ObjKey {
    ObjKey::Derived {
        name,
        version,
        id: [n; 32],
    }
}

#[tokio::test]
async fn codeintel_fork_copies_the_content_closure_and_no_index_objects_or_rows() {
    let src = BucketObjects::memory();
    let dst = BucketObjects::memory();
    let content = [ObjKey::File(FileId::of(b"fn main() {}"))];
    let index = [
        derived("tdocs", 1, 1),
        derived("tags", 1, 2),
        derived("chunks", 1, 3),
        derived("ciseg", 2, 4),
        derived("civec", 1, 5),
        derived("ciidx", 1, 6),
    ];
    src.put(&content[0], encode_stored(b"fn main() {}"))
        .await
        .unwrap();
    for k in &index {
        src.put(k, encode_stored(b"[]")).await.unwrap();
    }
    let mut keys = content.to_vec();
    keys.extend(index);

    // The source namespace is indexed.
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ci_docs (ns, file, ord, size, flags, versions) VALUES ($1, $2, 0, 1, 0, $3)",
    )
    .bind(b"src".to_vec())
    .bind(vec![1u8; 32])
    .bind(vec![0u8; 12])
    .execute(&pool)
    .await
    .unwrap();

    assert!(keys.iter().filter(|k| is_copied(k)).count() == 1);
    assert_eq!(copy_closure(&src, &dst, &keys).await.unwrap(), 1);
    let present = dst.has_many(&keys).await.unwrap();
    assert!(present[0]);
    assert!(
        present[1..].iter().all(|p| !p),
        "a derived object was copied"
    );

    for (table, sql) in [
        (
            "ci_docs",
            "SELECT CAST(COUNT(*) AS BIGINT) FROM ci_docs WHERE ns = $1",
        ),
        (
            "ci_doc_langs",
            "SELECT CAST(COUNT(*) AS BIGINT) FROM ci_doc_langs WHERE ns = $1",
        ),
        (
            "ci_chunks",
            "SELECT CAST(COUNT(*) AS BIGINT) FROM ci_chunks WHERE ns = $1",
        ),
        (
            "ci_jobs",
            "SELECT CAST(COUNT(*) AS BIGINT) FROM ci_jobs WHERE ns = $1",
        ),
        (
            "ci_manifest",
            "SELECT CAST(COUNT(*) AS BIGINT) FROM ci_manifest WHERE ns = $1",
        ),
    ] {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(b"dst".to_vec())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "{table} has rows for the fork");
    }
}

#[tokio::test]
async fn codeintel_fork_missing_source_object_fails_the_copy() {
    let (src, dst) = (BucketObjects::memory(), BucketObjects::memory());
    let key = ObjKey::File(FileId::of(b"absent"));
    assert!(copy_closure(&src, &dst, &[key]).await.is_err());
}
