//! Changeset rebase (SPEC §4.4).
//!
//! A changeset whose paths are disjoint from what the new base changed is rebased on metadata
//! alone: same `changes`, new parent, root rebuilt with `apply` (no content reads). Otherwise the
//! intersecting files are merged by [`Engine::merge_trees`]. A rewritten changeset drops
//! `git.gpgsig` and `git.commit`, because a raw body names the old parents; the caller re-signs
//! when `commit.gpgSign` or `-S` applies.

use asc_core::codec::Enveloped;
use asc_core::id::ChangesetId;
use asc_core::object::{Change, Changeset, SortedMap};
use asc_core::path::RepoPath;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use std::sync::Arc;

use crate::Engine;
use crate::commit::PathPolicy;
use crate::land::{NoMergeGlobs, PathSet};
use crate::merge::{Conflict, ConflictKind, Error, MergeOptions, Result, Stages};

/// `extra` keys a rewrite removes.
const DROPPED_EXTRA: [&[u8]; 2] = [b"git.gpgsig", b"git.commit"];

/// The result of rebasing one changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rebased {
    /// The stored rewritten changeset; `merged` is false for a metadata-only rebase.
    Done { id: ChangesetId, merged: bool },
    /// The changeset cannot be rewritten without resolving these.
    Conflicts(Vec<Conflict>),
}

/// The only parent of a changeset, or `None` for a root or a merge.
fn single_parent(cs: &Changeset) -> Option<ChangesetId> {
    match cs.parents.as_slice() {
        [p] => Some(*p),
        _ => None,
    }
}

fn kept_extra(cs: &Changeset) -> Result<SortedMap<Vec<u8>, Vec<u8>>> {
    let kept = cs
        .extra
        .as_slice()
        .iter()
        .filter(|(k, _)| !DROPPED_EXTRA.contains(&k.as_slice()))
        .cloned()
        .collect();
    Ok(SortedMap::new(kept)?)
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Paths of `cs` that conflict with what `onto` changed since `cs`'s parent.
    async fn overlap(
        &self,
        parent: ChangesetId,
        cs: &Changeset,
        onto: ChangesetId,
    ) -> Result<Vec<(RepoPath, RepoPath)>> {
        let theirs = self.paths_since(parent, onto).await?;
        Ok(PathSet::from_changes(&cs.changes).conflicts(&theirs))
    }

    /// Validates and stores a rewritten changeset.
    async fn put_rewritten(&self, cs: Changeset) -> Result<ChangesetId> {
        self.validate_changeset(
            &cs,
            PathPolicy::Native {
                windows_safe: false,
            },
        )
        .await?;
        let envelope = cs.to_envelope();
        let id = ChangesetId::of(&envelope);
        self.objects
            .put(&ObjKey::Changeset(id), encode_stored(&envelope))
            .await?;
        self.changesets.insert(id, Arc::new(cs));
        Ok(id)
    }

    /// `cs` moved onto `onto` with its `changes` unchanged. The caller has checked
    /// disjointness.
    async fn metadata_rewrite(&self, cs: &Changeset, onto: ChangesetId) -> Result<Changeset> {
        let onto_root = self.get_changeset(onto).await?.root;
        let changes: Vec<(RepoPath, Change)> = cs.changes.as_slice().to_vec();
        let root = self.apply(onto_root, &changes).await?;
        Ok(Changeset {
            parents: vec![onto],
            root,
            author: cs.author.clone(),
            committer: cs.committer.clone(),
            message: cs.message.clone(),
            extra: kept_extra(cs)?,
            changes: cs.changes.clone(),
        })
    }

    /// Metadata-only rebase of `cs` onto `onto` (not stored). `None` when the paths of `cs`
    /// intersect what `onto` changed since its parent, or when `cs` is a root or a merge.
    pub async fn rebase_metadata(
        &self,
        cs: &Changeset,
        onto: ChangesetId,
    ) -> Result<Option<Changeset>> {
        let Some(parent) = single_parent(cs) else {
            return Ok(None);
        };
        if !self.overlap(parent, cs, onto).await?.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.metadata_rewrite(cs, onto).await?))
    }

    /// Rebases and stores `cs` onto `onto`: metadata-only when disjoint, else merging the
    /// intersecting files. With `no_merge`, an intersecting path matching it is a conflict
    /// without a merge attempt (SPEC §8.5 step 3).
    pub(crate) async fn rebase_one(
        &self,
        cs: &Changeset,
        onto: ChangesetId,
        opts: &MergeOptions,
        no_merge: Option<&NoMergeGlobs>,
    ) -> Result<Rebased> {
        let Some(parent) = single_parent(cs) else {
            return Err(Error::NotLinear(cs.id()));
        };
        let pairs = self.overlap(parent, cs, onto).await?;
        if pairs.is_empty() {
            let rewritten = self.metadata_rewrite(cs, onto).await?;
            let id = self.put_rewritten(rewritten).await?;
            return Ok(Rebased::Done { id, merged: false });
        }
        let parent_root = self.get_changeset(parent).await?.root;
        let onto_root = self.get_changeset(onto).await?.root;
        if let Some(globs) = no_merge {
            let mut hits = Vec::new();
            for (a, b) in &pairs {
                if globs.is_match(a) || globs.is_match(b) {
                    hits.push(Conflict {
                        path: a.clone(),
                        kind: ConflictKind::Content,
                        stages: Stages {
                            base: self.lookup(parent_root, a).await?,
                            ours: self.lookup(onto_root, a).await?,
                            theirs: self.lookup(cs.root, a).await?,
                        },
                        marked_id: None,
                    });
                }
            }
            if !hits.is_empty() {
                return Ok(Rebased::Conflicts(hits));
            }
        }
        let outcome = self
            .merge_trees(parent_root, onto_root, cs.root, opts)
            .await?;
        if !outcome.is_clean() {
            return Ok(Rebased::Conflicts(outcome.conflicts));
        }
        let mut changes = self.leafdiff(onto_root, outcome.root, None).await?;
        for (path, change) in &mut changes {
            if let Change::Write { copy_from, .. } = change
                && let Some(Change::Write { copy_from: old, .. }) = cs.changes.get(path)
            {
                copy_from.clone_from(old);
            }
        }
        let rewritten = Changeset {
            parents: vec![onto],
            root: outcome.root,
            author: cs.author.clone(),
            committer: cs.committer.clone(),
            message: cs.message.clone(),
            extra: kept_extra(cs)?,
            changes: SortedMap::new(changes)?,
        };
        let id = self.put_rewritten(rewritten).await?;
        Ok(Rebased::Done { id, merged: true })
    }

    /// Rebases and stores one changeset onto `onto`.
    pub async fn rebase_changeset(
        &self,
        cs: ChangesetId,
        onto: ChangesetId,
        opts: &MergeOptions,
    ) -> Result<Rebased> {
        let cs = self.get_changeset(cs).await?;
        self.rebase_one(&cs, onto, opts, None).await
    }

    /// Rebases `stack` (oldest first) onto `onto`, each changeset onto the previous result.
    ///
    /// One result per changeset. After a failure the rest are `Error::Blocked`: their parent
    /// does not exist.
    pub async fn rebase_stack(
        &self,
        stack: &[ChangesetId],
        onto: ChangesetId,
        opts: &MergeOptions,
    ) -> Vec<Result<ChangesetId>> {
        let mut out = Vec::with_capacity(stack.len());
        let mut onto = Some(onto);
        for &id in stack {
            let Some(base) = onto else {
                out.push(Err(Error::Blocked));
                continue;
            };
            match self.rebase_changeset(id, base, opts).await {
                Ok(Rebased::Done { id: new, .. }) => {
                    out.push(Ok(new));
                    onto = Some(new);
                }
                Ok(Rebased::Conflicts(c)) => {
                    out.push(Err(Error::Conflicts(c)));
                    onto = None;
                }
                Err(e) => {
                    out.push(Err(e));
                    onto = None;
                }
            }
        }
        out
    }
}

// The tests sit at module level so their paths read `rebase::drops_gpgsig` and so on.

#[cfg(test)]
use crate::merge::testutil::{child, commit, files, fixture, p, read};

#[cfg(test)]
#[tokio::test]
async fn drops_gpgsig() {
    let (e, base) = fixture().await;
    let extra = vec![
        (b"asc.snapshot".to_vec(), Vec::new()),
        (
            b"git.gpgsig".to_vec(),
            b"-----BEGIN PGP SIGNATURE-----".to_vec(),
        ),
    ];
    let signed = child(&e, base, &[("a", "1\n2\n3\n4\n5\n6\n")], extra).await;
    let target = child(&e, base, &[("b", "B\n")], Vec::new()).await;
    let mut cs = (*e.get_changeset(signed).await.unwrap()).clone();
    let raw = [(b"git.commit".to_vec(), b"tree ...".to_vec())];
    let mut all = cs.extra.as_slice().to_vec();
    all.extend(raw);
    cs.extra = SortedMap::new(all).unwrap();
    let got = e.rebase_metadata(&cs, target).await.unwrap().unwrap();
    let keys: Vec<_> = got
        .extra
        .as_slice()
        .iter()
        .map(|(k, _)| k.clone())
        .collect();
    assert_eq!(keys, [b"asc.snapshot".to_vec()]);
    assert_eq!(got.parents, [target]);
    assert_eq!(got.author, cs.author);
    assert_eq!(got.message, cs.message);
    assert_eq!(got.changes, cs.changes);
}

#[cfg(test)]
#[tokio::test]
async fn metadata_rebase_rebuilds_the_root_without_reading_content() {
    let (e, base) = fixture().await;
    let mine = child(&e, base, &[("a", "1\n2\n3\n4\n5\n6\n")], Vec::new()).await;
    let target = child(&e, base, &[("b", "B\n")], Vec::new()).await;
    let cs = e.get_changeset(mine).await.unwrap();
    let got = e.rebase_metadata(&cs, target).await.unwrap().unwrap();
    let want = files(
        &e,
        &[("a", "1\n2\n3\n4\n5\n6\n"), ("b", "B\n"), ("c", "c\n")],
    )
    .await;
    assert_eq!(got.root, want);
    // The rewritten changeset is a valid child of the target.
    e.validate_changeset(
        &got,
        PathPolicy::Native {
            windows_safe: false,
        },
    )
    .await
    .unwrap();
}

#[cfg(test)]
#[tokio::test]
async fn metadata_rebase_refuses_intersecting_roots_and_merges() {
    let (e, base) = fixture().await;
    let mine = child(&e, base, &[("a", "1\n2\n3\n4\n5\n6\n")], Vec::new()).await;
    let touches_a = child(&e, base, &[("a", "0\n1\n2\n3\n4\n5\n")], Vec::new()).await;
    let cs = e.get_changeset(mine).await.unwrap();
    assert!(e.rebase_metadata(&cs, touches_a).await.unwrap().is_none());
    let root = e.get_changeset(base).await.unwrap();
    assert!(e.rebase_metadata(&root, base).await.unwrap().is_none());
    let tree = files(&e, &[("m", "m\n")]).await;
    let merge = commit(&e, &[mine, touches_a], tree, "merge").await;
    let merge = e.get_changeset(merge).await.unwrap();
    assert!(e.rebase_metadata(&merge, base).await.unwrap().is_none());
}

#[cfg(test)]
#[tokio::test]
async fn stack_rebases_through_content_merges() {
    let (e, base) = fixture().await;
    let s1 = child(&e, base, &[("a", "ONE\n2\n3\n4\n5\n")], Vec::new()).await;
    let s2 = child(&e, s1, &[("c", "C\n")], Vec::new()).await;
    let target = child(&e, base, &[("a", "1\n2\n3\n4\nFIVE\n")], Vec::new()).await;
    let got = e
        .rebase_stack(&[s1, s2], target, &MergeOptions::default())
        .await;
    let ids: Vec<_> = got.into_iter().map(|r| r.unwrap()).collect();
    let tip = e.get_changeset(ids[1]).await.unwrap();
    assert_eq!(tip.parents, [ids[0]]);
    let first = e.get_changeset(ids[0]).await.unwrap();
    assert_eq!(first.parents, [target]);
    assert_eq!(
        read(&e, tip.root, "a").await,
        Some(b"ONE\n2\n3\n4\nFIVE\n".to_vec())
    );
    assert_eq!(read(&e, tip.root, "c").await, Some(b"C\n".to_vec()));
    // The merged changeset records exactly what it changed relative to its new parent.
    let paths: Vec<_> = first
        .changes
        .as_slice()
        .iter()
        .map(|(p, _)| p.to_string())
        .collect();
    assert_eq!(paths, ["a"]);
}

#[cfg(test)]
#[tokio::test]
async fn a_conflict_blocks_the_rest_of_the_stack() {
    let (e, base) = fixture().await;
    let s1 = child(&e, base, &[("a", "x\n2\n3\n4\n5\n")], Vec::new()).await;
    let s2 = child(&e, s1, &[("c", "C\n")], Vec::new()).await;
    let target = child(&e, base, &[("a", "y\n2\n3\n4\n5\n")], Vec::new()).await;
    let got = e
        .rebase_stack(&[s1, s2], target, &MergeOptions::default())
        .await;
    match &got[0] {
        Err(Error::Conflicts(c)) => {
            assert_eq!(c.len(), 1);
            assert_eq!(c[0].path, p("a"));
            assert!(c[0].marked_id.is_some());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(got[1], Err(Error::Blocked)));
}
