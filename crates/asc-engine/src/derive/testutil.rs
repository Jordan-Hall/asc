//! Shared helpers for derive tests.

use std::io::Cursor;
use std::sync::Arc;

use asc_core::content::{Body, ingest};
use asc_core::id::ChangesetId;
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use async_trait::async_trait;

use crate::Engine;
use crate::commit::{NewChangeset, PathPolicy};

pub struct NoRefs;

#[async_trait]
impl Refs for NoRefs {
    async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
        Ok(None)
    }
    async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
        Ok(Vec::new())
    }
    async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
        Err(RefError::Corrupt("unused".into()))
    }
    async fn log(
        &self,
        _: RepoKey,
        _: &RefName,
        _: u32,
    ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
        Ok(Vec::new())
    }
}

pub type TestEngine = Engine<BucketObjects, NoRefs>;

pub fn engine() -> TestEngine {
    Engine::new(
        Arc::new(BucketObjects::memory()),
        Arc::new(NoRefs),
        RepoKey(1),
    )
}

async fn put_file(e: &TestEngine, content: &[u8]) -> Change {
    let got = ingest(Cursor::new(content)).unwrap();
    let Body::Inline(b) = got.body else {
        unreachable!("test files are small")
    };
    e.objects
        .put(&ObjKey::File(got.file), encode_stored(&b))
        .await
        .unwrap();
    Change::write(Kind::File, got.file, got.size, got.git, None).unwrap()
}

/// A changeset on `parents` whose root holds `f.txt` = `content` and `d/g.txt`.
pub async fn commit_on(e: &TestEngine, parents: &[ChangesetId], content: &[u8]) -> ChangesetId {
    let f = put_file(e, content).await;
    let g = put_file(e, b"fixed").await;
    let root = e
        .apply(
            TestEngine::empty_tree_id(),
            &[
                (RepoPath::new("f.txt").unwrap(), f),
                (RepoPath::new("d/g.txt").unwrap(), g),
            ],
        )
        .await
        .unwrap();
    let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap();
    e.create_changeset(
        NewChangeset {
            parents: parents.to_vec(),
            author: sig.clone(),
            committer: sig,
            message: b"msg\n".to_vec(),
            extra: Vec::new(),
            root,
            copies: Vec::new(),
        },
        PathPolicy::Native { windows_safe: true },
    )
    .await
    .unwrap()
}
