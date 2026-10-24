use std::sync::Arc;

use async_trait::async_trait;
use asc_core::content::{Body, ingest};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};

use super::*;
use crate::diff::Status;

struct NoRefs;

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

type TestEngine = Engine<BucketObjects, NoRefs>;

fn engine() -> TestEngine {
    Engine::new(
        Arc::new(BucketObjects::memory()),
        Arc::new(NoRefs),
        RepoKey(1),
    )
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

async fn tree_of(e: &TestEngine, files: &[(&str, &str)]) -> TreeId {
    let mut changes = Vec::new();
    for (path, content) in files {
        let got = ingest(std::io::Cursor::new(content.as_bytes())).unwrap();
        let Body::Inline(b) = got.body else {
            panic!("small file is inline")
        };
        e.objects
            .put(&ObjKey::File(got.file), encode_stored(&b))
            .await
            .unwrap();
        changes.push((
            p(path),
            Change::write(Kind::File, got.file, got.size, got.git, None).unwrap(),
        ));
    }
    e.apply(TestEngine::empty_tree_id(), &changes)
        .await
        .unwrap()
}

fn paths(changes: &[TreeChange]) -> Vec<String> {
    changes
        .iter()
        .map(|c| String::from_utf8(c.path.as_bytes().to_vec()).unwrap())
        .collect()
}

async fn inter(e: &TestEngine, a: Side, b: Side) -> Vec<TreeChange> {
    e.interdiff(a, b, None, &DiffOptions::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn same_base_reports_what_the_amend_changed() {
    let e = engine();
    let base = tree_of(&e, &[("a", "1\n"), ("b", "1\n")]).await;
    let v1 = tree_of(&e, &[("a", "1\n"), ("b", "1\n"), ("c", "new\n")]).await;
    let v2 = tree_of(&e, &[("a", "2\n"), ("b", "1\n"), ("c", "new\n")]).await;
    let got = inter(&e, Side { root: v1, base }, Side { root: v2, base }).await;
    assert_eq!(paths(&got), ["a"]);
    assert_eq!(got[0].status, Status::Modified);
}

#[tokio::test]
async fn identical_versions_have_no_interdiff() {
    let e = engine();
    let base = tree_of(&e, &[("a", "1\n")]).await;
    let v = tree_of(&e, &[("a", "2\n")]).await;
    let s = Side { root: v, base };
    assert!(inter(&e, s, s).await.is_empty());
}

#[tokio::test]
async fn a_rebase_onto_a_moved_trunk_leaves_the_trunk_changes_out() {
    let e = engine();
    // v1 was made on base1; trunk then changed `t` and `u`, and v2 is v1 rebased with an edit to `a`.
    let base1 = tree_of(&e, &[("a", "1\n"), ("t", "old\n"), ("u", "old\n")]).await;
    let v1 = tree_of(&e, &[("a", "mine\n"), ("t", "old\n"), ("u", "old\n")]).await;
    let base2 = tree_of(&e, &[("a", "1\n"), ("t", "new\n"), ("u", "new\n")]).await;
    let v2 = tree_of(&e, &[("a", "mine2\n"), ("t", "new\n"), ("u", "new\n")]).await;
    let got = inter(
        &e,
        Side {
            root: v1,
            base: base1,
        },
        Side {
            root: v2,
            base: base2,
        },
    )
    .await;
    assert_eq!(paths(&got), ["a"]);
}

#[tokio::test]
async fn a_clean_rebase_has_an_empty_interdiff() {
    let e = engine();
    let base1 = tree_of(&e, &[("a", "1\n"), ("t", "old\n")]).await;
    let v1 = tree_of(&e, &[("a", "mine\n"), ("t", "old\n")]).await;
    let base2 = tree_of(&e, &[("a", "1\n"), ("t", "new\n")]).await;
    let v2 = tree_of(&e, &[("a", "mine\n"), ("t", "new\n")]).await;
    let got = inter(
        &e,
        Side {
            root: v1,
            base: base1,
        },
        Side {
            root: v2,
            base: base2,
        },
    )
    .await;
    assert!(got.is_empty(), "{:?}", paths(&got));
}

#[tokio::test]
async fn a_dropped_edit_still_shows() {
    let e = engine();
    let base = tree_of(&e, &[("a", "1\n"), ("b", "1\n")]).await;
    let v1 = tree_of(&e, &[("a", "x\n"), ("b", "y\n")]).await;
    let v2 = tree_of(&e, &[("a", "x\n"), ("b", "1\n")]).await;
    let got = inter(&e, Side { root: v1, base }, Side { root: v2, base }).await;
    assert_eq!(paths(&got), ["b"]);
}

#[tokio::test]
async fn the_pathspec_narrows_the_result() {
    let e = engine();
    let base = tree_of(&e, &[("a", "1\n"), ("b", "1\n")]).await;
    let v1 = tree_of(&e, &[("a", "1\n"), ("b", "1\n")]).await;
    let v2 = tree_of(&e, &[("a", "2\n"), ("b", "2\n")]).await;
    let spec = Pathspec::parse(["b"]).unwrap();
    let got = e
        .interdiff(
            Side { root: v1, base },
            Side { root: v2, base },
            Some(&spec),
            &DiffOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(paths(&got), ["b"]);
}

#[tokio::test]
async fn a_patch_over_the_path_limit_is_refused() {
    let e = engine();
    let base1 = tree_of(&e, &[]).await;
    let base2 = tree_of(&e, &[("t", "x\n")]).await;
    let v1 = tree_of(&e, &[("a", "1\n"), ("b", "1\n"), ("c", "1\n")]).await;
    let v2 = tree_of(&e, &[("t", "x\n"), ("a", "2\n")]).await;
    let err = e
        .interdiff_limited(
            Side {
                root: v1,
                base: base1,
            },
            Side {
                root: v2,
                base: base2,
            },
            None,
            &DiffOptions::default(),
            2,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::TooLarge(2)), "{err}");
}
