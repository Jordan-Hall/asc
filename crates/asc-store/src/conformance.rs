//! Reusable `Objects` conformance suite (feature `testkit`).

use std::future::Future;

use asc_core::git::{ObjectKind, object_oid};
use asc_core::id::{ChunkId, FileId};
use bytes::Bytes;

use crate::codec::{decode_stored_capped, encode_stored};
use crate::key::{KeyPrefix, ObjKey};
use crate::objects::{Error, Objects, max_decoded};

/// Exercises any [`Objects`] implementation.
///
/// `tamper(key, stored)` must overwrite the backing value for `key` with `stored`
/// without going through `Objects`, so corruption detection can be checked.
pub async fn run_objects_suite<O, F, Fut>(objs: &O, tamper: F)
where
    O: Objects,
    F: Fn(ObjKey, Bytes) -> Fut,
    Fut: Future<Output = ()>,
{
    let text = b"conformance payload 0123456789".repeat(40);
    let file = ObjKey::File(FileId::of(&text));
    let missing = ObjKey::File(FileId::of(b"never stored"));

    // absent
    assert!(objs.get(&file).await.unwrap().is_none());
    assert!(objs.get_stored(&file).await.unwrap().is_none());
    assert_eq!(
        objs.has_many(&[file, missing]).await.unwrap(),
        [false, false]
    );

    // put / get / stored bytes are untouched
    let stored = encode_stored(&text);
    objs.put(&file, stored.clone()).await.unwrap();
    assert_eq!(objs.get(&file).await.unwrap().unwrap(), text.as_slice());
    assert_eq!(objs.get_stored(&file).await.unwrap().unwrap(), stored);
    assert_eq!(
        decode_stored_capped(&stored, max_decoded(file.class())).unwrap(),
        text.as_slice()
    );
    assert_eq!(
        objs.has_many(&[missing, file, file]).await.unwrap(),
        [false, true, true]
    );
    assert!(objs.has_many(&[]).await.unwrap().is_empty());

    // idempotent put keeps the original value
    objs.put(&file, stored.clone()).await.unwrap();
    assert_eq!(objs.get_stored(&file).await.unwrap().unwrap(), stored);

    // ranges
    assert_eq!(
        objs.get_range(&file, 3..10).await.unwrap().unwrap(),
        &text[3..10]
    );
    let n = text.len() as u64;
    assert_eq!(
        objs.get_range(&file, n - 2..n + 50).await.unwrap().unwrap(),
        &text[text.len() - 2..]
    );
    assert!(objs.get_range(&missing, 0..1).await.unwrap().is_none());
    assert!(matches!(
        objs.get_range(&ObjKey::Tag(asc_core::id::TagId::of(b"t")), 0..1)
            .await,
        Err(Error::RangeUnsupported(_))
    ));

    // listing
    let chunk_bytes = b"a chunk".to_vec();
    let chunk = ObjKey::Chunk(ChunkId::of(&chunk_bytes));
    objs.put(&chunk, encode_stored(&chunk_bytes)).await.unwrap();
    let listed = collect_keys(objs, KeyPrefix::all()).await;
    assert!(
        listed.contains(&file) && listed.contains(&chunk),
        "{listed:?}"
    );
    let only_chunks = collect_keys(objs, KeyPrefix::class(chunk.class())).await;
    assert_eq!(only_chunks, [chunk]);

    // corruption: bytes that do not hash to the key
    tamper(file, encode_stored(b"evil bytes")).await;
    assert!(matches!(objs.get(&file).await, Err(Error::Corrupt(k)) if k == file));
    assert!(matches!(
        objs.get_range(&file, 0..4).await,
        Err(Error::Corrupt(_))
    ));
    assert_eq!(
        objs.get_stored(&file).await.unwrap().unwrap(),
        encode_stored(b"evil bytes")
    );
    // a damaged zstd frame is not silently accepted either
    tamper(chunk, Bytes::from_static(&[1, 0xde, 0xad])).await;
    assert!(objs.get(&chunk).await.is_err());

    // repair: a verified put overwrites the corrupt value
    tamper(file, encode_stored(b"evil bytes")).await;
    assert!(matches!(objs.get(&file).await, Err(Error::Corrupt(_))));
    objs.put(&file, stored.clone()).await.unwrap();
    assert_eq!(objs.get(&file).await.unwrap().unwrap(), text.as_slice());
    assert_eq!(objs.get_stored(&file).await.unwrap().unwrap(), stored);

    // put verifies: bytes that do not hash to the key are rejected and not written
    let fresh = ObjKey::File(FileId::of(b"fresh"));
    assert!(matches!(
        objs.put(&fresh, encode_stored(b"not fresh")).await,
        Err(Error::Corrupt(k)) if k == fresh
    ));
    assert!(objs.get_stored(&fresh).await.unwrap().is_none());

    // oversize zstd frame (File cap is 4 MiB) is rejected on put and on get
    let big = vec![0u8; 4 * 1024 * 1024 + 1];
    let big_key = ObjKey::File(FileId::of(&big));
    let big_stored = encode_stored(&big);
    assert!(matches!(
        objs.put(&big_key, big_stored.clone()).await,
        Err(Error::Codec(..))
    ));
    tamper(big_key, big_stored).await;
    assert!(matches!(objs.get(&big_key).await, Err(Error::Codec(..))));
    objs.delete(&big_key).await.unwrap();

    // git tree override must hash to its oid
    let tree_body = b"100644 a\0aaaaaaaaaaaaaaaaaaaa".to_vec();
    let good = ObjKey::GitTreeOverride(object_oid(ObjectKind::Tree, &tree_body));
    objs.put(&good, encode_stored(&tree_body)).await.unwrap();
    assert_eq!(
        objs.get(&good).await.unwrap().unwrap(),
        tree_body.as_slice()
    );
    let bad = ObjKey::GitTreeOverride(asc_core::id::GitOid::from_bytes([9; 20]));
    assert!(matches!(
        objs.put(&bad, encode_stored(&tree_body)).await,
        Err(Error::Corrupt(_))
    ));
    tamper(bad, encode_stored(&tree_body)).await;
    assert!(matches!(objs.get(&bad).await, Err(Error::Corrupt(_))));

    // delete
    objs.delete(&file).await.unwrap();
    objs.delete(&file).await.unwrap();
    assert_eq!(objs.has_many(&[file]).await.unwrap(), [false]);
}

async fn collect_keys<O: Objects>(objs: &O, prefix: KeyPrefix) -> Vec<ObjKey> {
    use futures::TryStreamExt;
    let items: Vec<_> = objs.list(prefix).try_collect().await.unwrap();
    items.into_iter().map(|(k, _)| k).collect()
}
