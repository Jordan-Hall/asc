//! In-memory restack planner and executor (SPEC §20.1 D7, §4.4).
//!
//! [`Engine::plan_restack`] checks that a stack is a linear first-parent chain and fixes the
//! order of the rewrites. [`Engine::execute_restack`] rewrites each change onto the previous
//! result with [`Engine::rebase_metadata`] (or [`Engine::rebase_one`] for a merge): metadata-only when its paths are disjoint from what the
//! new base changed (no content reads), a content merge of the intersecting files otherwise.
//! Only a conflicted change stops the run; the caller materialises it in git's own
//! `rebase-merge/` state.
//!
//! The executor never moves a ref. It returns the rewrites and a [`RefTxn`] that the caller
//! applies through `Ctx::update_local_refs`. When `commit.gpgSign` or `-S` applies the caller
//! passes a [`CommitSigner`]; every rewritten commit is then signed, and a signing failure
//! pauses the run with no `RefTxn`.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use asc_core::codec::Enveloped;
use asc_core::git::{EXTRA_GIT_GPGSIG, export_commit, tree_oid};
use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::object::{Changeset, SortedMap};
use asc_core::path::RefName;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use std::sync::Arc;
use uuid::Uuid;

use super::MAX_STACK;
use crate::Engine;
use crate::derive::gitcommit::GitCommit;
use crate::merge::{self, Conflict, MergeOptions};
use crate::rebase::Rebased;

/// Longest signature block the executor accepts from a signer.
pub const MAX_SIGNATURE: usize = 64 * 1024;

/// Restack failures that are not a pause.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Merge(#[from] merge::Error),
    #[error(transparent)]
    Commit(#[from] crate::commit::Error),
    #[error(transparent)]
    Tree(#[from] crate::tree::Error),
    #[error(transparent)]
    Derive(#[from] crate::derive::Error),
    #[error(transparent)]
    Object(#[from] asc_core::object::ObjectError),
    #[error(transparent)]
    Git(#[from] asc_core::git::GitError),
    #[error(transparent)]
    Store(#[from] asc_store::objects::Error),
    #[error("stack of {0} changes exceeds the limit of {MAX_STACK}")]
    TooLong(usize),
    #[error("{0} is not the first-parent child of the change before it")]
    NotChain(ChangesetId),
    #[error("{0} appears twice in the stack")]
    Duplicate(ChangesetId),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A signing failure. The text is shown to the user and never parsed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SignError(pub String);

/// Signs the canonical bytes of a commit (without a `gpgsig` header) and returns the
/// signature block as git stores it in `gpgsig` (armor, no trailing newline needed).
#[async_trait]
pub trait CommitSigner: Send + Sync {
    async fn sign(&self, payload: &[u8]) -> std::result::Result<Vec<u8>, SignError>;
}

/// The changes to rewrite, oldest first, and the commit the first one moves onto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestackPlan {
    pub onto: ChangesetId,
    pub steps: Vec<ChangesetId>,
}

/// A ref that points at `from`, a change of the stack, and must follow its rewrite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefMove {
    pub name: RefName,
    pub from: ChangesetId,
}

/// Audit context of the returned [`RefTxn`].
#[derive(Clone, Debug)]
pub struct TxnMeta {
    pub actor: Actor,
    pub reason: String,
    pub op_id: Uuid,
}

/// One stored rewrite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewrite {
    pub old: ChangesetId,
    pub new: ChangesetId,
    /// A content merge ran; false for a metadata-only rebase or an unchanged change.
    pub merged: bool,
    pub signed: bool,
}

/// Why a run stopped before the end of the stack.
#[derive(Debug)]
pub enum Pause {
    Conflicts(Vec<Conflict>),
    /// The signer failed; the message is the signer's.
    Sign(String),
}

#[derive(Debug)]
pub enum Outcome {
    /// Every change is rewritten. `txn` is `None` when no ref has to move.
    Complete {
        rewrites: Vec<Rewrite>,
        txn: Option<RefTxn>,
    },
    /// `at` could not be rewritten. `rewrites` are the stored changes before it; no ref moves.
    Paused {
        rewrites: Vec<Rewrite>,
        at: ChangesetId,
        cause: Pause,
    },
}

/// A signature block as a `gpgsig` header value: no trailing newline.
fn trim_signature(sig: &[u8]) -> Option<Vec<u8>> {
    let end = sig
        .iter()
        .rposition(|&c| c != b'\n' && c != b'\r')
        .map_or(0, |i| i + 1);
    let sig = &sig[..end];
    (!sig.is_empty() && sig.len() <= MAX_SIGNATURE && !sig.contains(&0)).then(|| sig.to_vec())
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Checks that `stack` (oldest first) is a first-parent chain and orders the rewrites.
    /// Reads changesets only.
    pub async fn plan_restack(
        &self,
        stack: &[ChangesetId],
        onto: ChangesetId,
    ) -> Result<RestackPlan> {
        if stack.len() > MAX_STACK {
            return Err(Error::TooLong(stack.len()));
        }
        let mut seen = HashSet::new();
        let mut prev: Option<ChangesetId> = None;
        for &id in stack {
            if !seen.insert(id) {
                return Err(Error::Duplicate(id));
            }
            let cs = self.get_changeset(id).await?;
            if cs.parents.len() != 1 {
                return Err(merge::Error::NotLinear(id).into());
            }
            if let Some(p) = prev
                && cs.parents[0] != p
            {
                return Err(Error::NotChain(id));
            }
            prev = Some(id);
        }
        Ok(RestackPlan {
            onto,
            steps: stack.to_vec(),
        })
    }

    /// Stores `cs` without re-reading file content. Every caller derives `cs` from changesets
    /// that passed ingest validation, with `changes` copied or merge-checked, so the content
    /// check of `validate_changeset` would only repeat reads (SPEC §4.4: no content reads).
    async fn store_rewrite(&self, cs: Changeset) -> Result<ChangesetId> {
        let envelope = cs.to_envelope();
        let id = ChangesetId::of(&envelope);
        self.objects
            .put(&ObjKey::Changeset(id), encode_stored(&envelope))
            .await?;
        self.changesets.insert(id, Arc::new(cs));
        Ok(id)
    }

    /// `cs` with a `gpgsig` made by `signer` over its canonical commit bytes.
    async fn sign_changeset(
        &self,
        cs: &Changeset,
        signer: &dyn CommitSigner,
    ) -> Result<std::result::Result<Changeset, SignError>> {
        let root = self.get_tree(cs.root).await?;
        let mut parent_gits: Vec<GitOid> = Vec::with_capacity(cs.parents.len());
        for &p in &cs.parents {
            parent_gits.push(self.get_derived::<GitCommit>(p).await?.commit);
        }
        let payload = export_commit(cs, tree_oid(&root), &parent_gits)?;
        let raw = match signer.sign(&payload).await {
            Ok(raw) => raw,
            Err(e) => return Ok(Err(e)),
        };
        let Some(sig) = trim_signature(&raw) else {
            return Ok(Err(SignError(
                "the signer returned an empty, oversized or malformed signature".into(),
            )));
        };
        let mut extra: Vec<_> = cs
            .extra
            .as_slice()
            .iter()
            .filter(|(k, _)| k.as_slice() != EXTRA_GIT_GPGSIG)
            .cloned()
            .collect();
        extra.push((EXTRA_GIT_GPGSIG.to_vec(), sig));
        Ok(Ok(Changeset {
            extra: SortedMap::new(extra)?,
            ..cs.clone()
        }))
    }

    /// Rewrites `cs` onto `onto`. Metadata-only when disjoint (not yet stored), else a merge
    /// (stored by `rebase_one`).
    async fn rewrite_step(
        &self,
        cs: &Changeset,
        onto: ChangesetId,
        opts: &MergeOptions,
    ) -> Result<std::result::Result<(Changeset, bool), Vec<Conflict>>> {
        if let Some(rewritten) = self.rebase_metadata(cs, onto).await? {
            return Ok(Ok((rewritten, false)));
        }
        match self.rebase_one(cs, onto, opts, None).await? {
            Rebased::Done { id, merged } => {
                Ok(Ok(((*self.get_changeset(id).await?).clone(), merged)))
            }
            Rebased::Conflicts(c) => Ok(Err(c)),
        }
    }

    /// Runs `plan`. Stores every rewrite, signs it when `signer` is given, and returns the ref
    /// transaction for `moves`. Moves no ref itself.
    pub async fn execute_restack(
        &self,
        plan: &RestackPlan,
        moves: &[RefMove],
        signer: Option<&dyn CommitSigner>,
        opts: &MergeOptions,
        meta: &TxnMeta,
    ) -> Result<Outcome> {
        let mut rewrites: Vec<Rewrite> = Vec::with_capacity(plan.steps.len());
        let mut onto = plan.onto;
        for &old in &plan.steps {
            let cs = self.get_changeset(old).await?;
            if cs.parents.first() == Some(&onto) {
                // Already in place; a settled change keeps its identity and signature.
                rewrites.push(Rewrite {
                    old,
                    new: old,
                    merged: false,
                    signed: false,
                });
                continue;
            }
            let (mut rewritten, merged) = match self.rewrite_step(&cs, onto, opts).await? {
                Ok(done) => done,
                Err(c) => {
                    return Ok(Outcome::Paused {
                        rewrites,
                        at: old,
                        cause: Pause::Conflicts(c),
                    });
                }
            };
            if let Some(signer) = signer {
                match self.sign_changeset(&rewritten, signer).await? {
                    Ok(signed) => rewritten = signed,
                    Err(e) => {
                        return Ok(Outcome::Paused {
                            rewrites,
                            at: old,
                            cause: Pause::Sign(e.0),
                        });
                    }
                }
            }
            let new = self.store_rewrite(rewritten).await?;
            rewrites.push(Rewrite {
                old,
                new,
                merged,
                signed: signer.is_some(),
            });
            onto = new;
        }
        let txn = ref_txn(&rewrites, moves, meta);
        Ok(Outcome::Complete { rewrites, txn })
    }
}

/// One update per distinct ref whose target was rewritten.
fn ref_txn(rewrites: &[Rewrite], moves: &[RefMove], meta: &TxnMeta) -> Option<RefTxn> {
    let map: HashMap<ChangesetId, ChangesetId> = rewrites
        .iter()
        .filter(|r| r.old != r.new)
        .map(|r| (r.old, r.new))
        .collect();
    let mut names = HashSet::new();
    let updates: Vec<RefUpdate> = moves
        .iter()
        .filter(|m| names.insert(m.name.clone()))
        .filter_map(|m| {
            let new = *map.get(&m.from)?;
            Some(RefUpdate {
                name: m.name.clone(),
                expect: Expect::Value(RefTarget::Changeset(m.from)),
                new: Some(RefTarget::Changeset(new)),
                expires_at: None,
            })
        })
        .collect();
    (!updates.is_empty()).then(|| RefTxn {
        updates,
        actor: meta.actor.clone(),
        reason: meta.reason.clone(),
        op_id: meta.op_id,
    })
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    use bytes::Bytes;
    use asc_store::bucket::BucketObjects;
    use asc_store::key::KeyPrefix;
    use asc_store::refs::RepoKey;
    use futures::stream::BoxStream;

    use super::*;
    use crate::merge::testutil::{NoRefs, child, fixture};

    /// Counts reads of file and chunk content.
    struct Counting {
        inner: Arc<BucketObjects>,
        content_reads: AtomicUsize,
    }

    impl Counting {
        fn note(&self, key: &ObjKey) {
            if matches!(
                key,
                ObjKey::File(_) | ObjKey::Chunk(_) | ObjKey::ChunkList(_)
            ) {
                self.content_reads.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[async_trait]
    impl Objects for Counting {
        async fn get(&self, key: &ObjKey) -> asc_store::objects::Result<Option<Bytes>> {
            self.note(key);
            self.inner.get(key).await
        }
        async fn get_stored(&self, key: &ObjKey) -> asc_store::objects::Result<Option<Bytes>> {
            self.note(key);
            self.inner.get_stored(key).await
        }
        async fn get_range(
            &self,
            key: &ObjKey,
            r: Range<u64>,
        ) -> asc_store::objects::Result<Option<Bytes>> {
            self.note(key);
            self.inner.get_range(key, r).await
        }
        async fn put(&self, key: &ObjKey, stored: Bytes) -> asc_store::objects::Result<()> {
            self.inner.put(key, stored).await
        }
        async fn has_many(&self, keys: &[ObjKey]) -> asc_store::objects::Result<Vec<bool>> {
            self.inner.has_many(keys).await
        }
        async fn delete(&self, key: &ObjKey) -> asc_store::objects::Result<()> {
            self.inner.delete(key).await
        }
        fn list(
            &self,
            prefix: KeyPrefix,
        ) -> BoxStream<'_, asc_store::objects::Result<(ObjKey, SystemTime)>> {
            self.inner.list(prefix)
        }
    }

    struct FixedSigner(Vec<u8>);

    #[async_trait]
    impl CommitSigner for FixedSigner {
        async fn sign(&self, payload: &[u8]) -> std::result::Result<Vec<u8>, SignError> {
            assert!(!payload.windows(6).any(|w| w == b"gpgsig"));
            Ok(self.0.clone())
        }
    }

    struct FailingSigner;

    #[async_trait]
    impl CommitSigner for FailingSigner {
        async fn sign(&self, _: &[u8]) -> std::result::Result<Vec<u8>, SignError> {
            Err(SignError("gpg: no secret key".into()))
        }
    }

    const SIG: &[u8] = b"-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----\n";

    fn meta() -> TxnMeta {
        TxnMeta {
            actor: Actor {
                id: "u".into(),
                via: "test".into(),
            },
            reason: "restack".into(),
            op_id: Uuid::nil(),
        }
    }

    fn name(s: &str) -> RefName {
        RefName::new(s).unwrap()
    }

    /// A stack of `n` disjoint changes on `base`, and a target that touched only `b`.
    async fn disjoint_stack(
        n: usize,
    ) -> (
        crate::merge::testutil::TestEngine,
        Vec<ChangesetId>,
        ChangesetId,
    ) {
        let (e, base) = fixture().await;
        let mut stack = Vec::new();
        let mut at = base;
        for i in 0..n {
            let path = format!("f{i}");
            at = child(&e, at, &[(path.as_str(), "x\n")], Vec::new()).await;
            stack.push(at);
        }
        let target = child(&e, base, &[("b", "B\n")], Vec::new()).await;
        (e, stack, target)
    }

    fn counted(
        e: &crate::merge::testutil::TestEngine,
    ) -> (Engine<Counting, NoRefs>, Arc<Counting>) {
        let store = Arc::new(Counting {
            inner: e.objects.clone(),
            content_reads: AtomicUsize::new(0),
        });
        (
            Engine::new(store.clone(), Arc::new(NoRefs), RepoKey(1)),
            store,
        )
    }

    #[tokio::test]
    async fn fifty_disjoint_changes_restack_without_content_reads() {
        let (e, stack, target) = disjoint_stack(50).await;
        let (e, store) = counted(&e);
        let plan = e.plan_restack(&stack, target).await.unwrap();
        let moves = [RefMove {
            name: name("refs/heads/top"),
            from: stack[49],
        }];
        let out = e
            .execute_restack(&plan, &moves, None, &MergeOptions::default(), &meta())
            .await
            .unwrap();
        let Outcome::Complete { rewrites, txn } = out else {
            panic!("paused");
        };
        assert_eq!(store.content_reads.load(Ordering::SeqCst), 0);
        assert_eq!(rewrites.len(), 50);
        assert!(rewrites.iter().all(|r| !r.merged && !r.signed));
        let first = e.get_changeset(rewrites[0].new).await.unwrap();
        assert_eq!(first.parents, [target]);
        let last = e.get_changeset(rewrites[49].new).await.unwrap();
        assert_eq!(last.parents, [rewrites[48].new]);
        let txn = txn.unwrap();
        assert_eq!(txn.updates.len(), 1);
        assert_eq!(
            txn.updates[0].new,
            Some(RefTarget::Changeset(rewrites[49].new))
        );
        assert_eq!(
            txn.updates[0].expect,
            Expect::Value(RefTarget::Changeset(stack[49]))
        );
    }

    #[tokio::test]
    async fn signing_on_signs_every_rewrite_and_off_signs_none() {
        let (e, stack, target) = disjoint_stack(3).await;
        let plan = e.plan_restack(&stack, target).await.unwrap();
        let signer = FixedSigner(SIG.to_vec());
        let opts = MergeOptions::default();
        let Outcome::Complete { rewrites, .. } = e
            .execute_restack(&plan, &[], Some(&signer), &opts, &meta())
            .await
            .unwrap()
        else {
            panic!("paused");
        };
        for r in &rewrites {
            assert!(r.signed);
            let cs = e.get_changeset(r.new).await.unwrap();
            let sig = cs.extra.get(&EXTRA_GIT_GPGSIG.to_vec()).unwrap();
            assert_eq!(sig.as_slice(), SIG.strip_suffix(b"\n").unwrap());
        }
        // The signed chain is linked through the signed ids.
        let second = e.get_changeset(rewrites[1].new).await.unwrap();
        assert_eq!(second.parents, [rewrites[0].new]);
        let Outcome::Complete { rewrites, .. } = e
            .execute_restack(&plan, &[], None, &opts, &meta())
            .await
            .unwrap()
        else {
            panic!("paused");
        };
        for r in &rewrites {
            let cs = e.get_changeset(r.new).await.unwrap();
            assert!(cs.extra.get(&EXTRA_GIT_GPGSIG.to_vec()).is_none());
        }
    }

    #[tokio::test]
    async fn a_signing_failure_pauses_and_moves_no_ref() {
        let (e, stack, target) = disjoint_stack(3).await;
        let plan = e.plan_restack(&stack, target).await.unwrap();
        let moves = [RefMove {
            name: name("refs/heads/top"),
            from: stack[2],
        }];
        let out = e
            .execute_restack(
                &plan,
                &moves,
                Some(&FailingSigner),
                &MergeOptions::default(),
                &meta(),
            )
            .await
            .unwrap();
        match out {
            Outcome::Paused {
                rewrites,
                at,
                cause: Pause::Sign(msg),
            } => {
                assert!(rewrites.is_empty());
                assert_eq!(at, stack[0]);
                assert_eq!(msg, "gpg: no secret key");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_signature_is_a_signing_failure() {
        let (e, stack, target) = disjoint_stack(1).await;
        let plan = e.plan_restack(&stack, target).await.unwrap();
        let signer = FixedSigner(b"\n".to_vec());
        let out = e
            .execute_restack(&plan, &[], Some(&signer), &MergeOptions::default(), &meta())
            .await
            .unwrap();
        assert!(matches!(
            out,
            Outcome::Paused {
                cause: Pause::Sign(_),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_conflict_pauses_with_the_earlier_rewrites() {
        let (e, base) = fixture().await;
        let s1 = child(&e, base, &[("c", "C\n")], Vec::new()).await;
        let s2 = child(&e, s1, &[("a", "x\n2\n3\n4\n5\n")], Vec::new()).await;
        let target = child(&e, base, &[("a", "y\n2\n3\n4\n5\n")], Vec::new()).await;
        let plan = e.plan_restack(&[s1, s2], target).await.unwrap();
        let out = e
            .execute_restack(&plan, &[], None, &MergeOptions::default(), &meta())
            .await
            .unwrap();
        match out {
            Outcome::Paused {
                rewrites,
                at,
                cause: Pause::Conflicts(c),
            } => {
                assert_eq!(rewrites.len(), 1);
                assert_eq!(at, s2);
                assert_eq!(c.len(), 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn settled_changes_keep_their_identity_and_no_ref_moves() {
        let (e, base) = fixture().await;
        let s1 = child(&e, base, &[("c", "C\n")], Vec::new()).await;
        let plan = e.plan_restack(&[s1], base).await.unwrap();
        let moves = [RefMove {
            name: name("refs/heads/top"),
            from: s1,
        }];
        let out = e
            .execute_restack(&plan, &moves, None, &MergeOptions::default(), &meta())
            .await
            .unwrap();
        let Outcome::Complete { rewrites, txn } = out else {
            panic!("paused");
        };
        assert_eq!(rewrites[0].new, s1);
        assert!(txn.is_none());
    }

    #[tokio::test]
    async fn the_planner_rejects_broken_chains_and_duplicates() {
        let (e, base) = fixture().await;
        let a = child(&e, base, &[("c", "C\n")], Vec::new()).await;
        let b = child(&e, base, &[("b", "B\n")], Vec::new()).await;
        assert!(matches!(
            e.plan_restack(&[a, b], base).await,
            Err(Error::NotChain(_))
        ));
        assert!(matches!(
            e.plan_restack(&[a, a], base).await,
            Err(Error::Duplicate(_))
        ));
        assert!(matches!(
            e.plan_restack(&[base], a).await,
            Err(Error::Merge(merge::Error::NotLinear(_)))
        ));
    }
}
