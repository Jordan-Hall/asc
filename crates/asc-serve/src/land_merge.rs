//! Land content merge (PLAN U68, SPEC §8.5 step 3).
//!
//! [`ContentMerge`] is the [`OverlapPolicy`] U46 calls when a stack shares paths with what the
//! branch changed since the stack's base. A changeset whose paths do not intersect is rebased on
//! metadata alone. Otherwise its intersecting files are merged three-way (`land.contentMerge`,
//! default on), and the land answers `LAND_CONFLICT` with `Conflict` records only when
//!
//! * a text merge conflicts (the record carries `marked_id`, the diff3-style result), or
//! * an intersecting path matches `land.noMergeGlobs`, whose default names the lockfiles that
//!   must be regenerated rather than merged (`bun.lock`, `Cargo.lock`, `uv.lock`,
//!   `**/packages.lock.json`).
//!
//! With `land.contentMerge = false`, or a request with `no_content_merge`, the policy is
//! [`DisjointOnly`]: any shared path is a conflict and nothing is merged (`LAND_NEEDS_CONTENT_MERGE`
//! for the request flag, `LAND_CONFLICT` otherwise). The conflicting
//! changeset is the first one in the stack that cannot be rewritten; later ones are not tried,
//! because their parent would not exist.
//!
//! The merge reads whole files, so the policy reserves [`MERGE_TEXT_ESTIMATE`] bytes from the
//! text budget first. Stored objects are only the rewritten changesets and the marked files of a
//! conflict; GC removes the latter when nothing refers to them.

use async_trait::async_trait;
use asc_engine::land::NoMergeGlobs;
use asc_engine::merge::{ConflictStyle, MergeOptions};

use super::*;
use crate::idmode::Eng;
use crate::land::{
    DisjointOnly, LandError, LandOptions, MAX_CONFLICT_PATHS, OverlapPolicy, Rewrite,
};

/// The `land.noMergeGlobs` default (SPEC §8.5 step 3).
pub const DEFAULT_NO_MERGE_GLOBS: [&str; 4] =
    ["bun.lock", "Cargo.lock", "uv.lock", "**/packages.lock.json"];

/// Most `land.noMergeGlobs` entries.
pub const MAX_NO_MERGE_GLOBS: usize = 256;

/// The merge options of land and `Rebase`: conflict markers in diff3 style (SPEC §8.11), so an
/// agent sees the base text and `marked_id` equals `git merge-file --diff3`.
pub(crate) fn agent_merge_options() -> MergeOptions {
    MergeOptions {
        style: ConflictStyle::Diff3,
        ..MergeOptions::default()
    }
}

/// Why a land policy could not be built.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("too many land.noMergeGlobs entries")]
    TooMany,
    #[error("a land.noMergeGlobs entry is empty, too long or not valid")]
    BadGlob,
}

/// The content-merging land policy.
pub struct ContentMerge {
    enabled: bool,
    globs: Arc<NoMergeGlobs>,
    hooks: Arc<ReadHooks>,
}

impl ContentMerge {
    /// `enabled` is `land.contentMerge`; `patterns` is `land.noMergeGlobs`.
    pub fn new(
        hooks: Arc<ReadHooks>,
        enabled: bool,
        patterns: &[String],
    ) -> Result<Self, PolicyError> {
        if patterns.len() > MAX_NO_MERGE_GLOBS {
            return Err(PolicyError::TooMany);
        }
        if patterns
            .iter()
            .any(|g| g.is_empty() || g.len() > MAX_STRING || g.contains('\0'))
        {
            return Err(PolicyError::BadGlob);
        }
        let globs = NoMergeGlobs::new(patterns).map_err(|_| PolicyError::BadGlob)?;
        Ok(Self {
            enabled,
            globs: Arc::new(globs),
            hooks,
        })
    }

    /// The SPEC defaults: content merge on, the lockfile globs.
    pub fn with_defaults(hooks: Arc<ReadHooks>) -> Self {
        let patterns: Vec<String> = DEFAULT_NO_MERGE_GLOBS.map(String::from).to_vec();
        Self::new(hooks, true, &patterns).expect("the default globs are valid")
    }
}

#[async_trait]
impl OverlapPolicy for ContentMerge {
    async fn rewrite(
        &self,
        engine: &Arc<Eng>,
        stack: &[ChangesetId],
        tip: ChangesetId,
        opts: LandOptions,
    ) -> Result<Rewrite, LandError> {
        if !self.enabled || opts.no_content_merge {
            return DisjointOnly.rewrite(engine, stack, tip, opts).await;
        }
        let _text = self
            .hooks
            .text_budget
            .acquire(MERGE_TEXT_ESTIMATE)
            .await
            .map_err(|_| LandError::Internal("text memory budget exhausted".into()))?;
        let (engine, stack_ids, globs) = (engine.clone(), stack.to_vec(), self.globs.clone());
        let merged = on_blocking_thread(move || async move {
            engine
                .content_merge_land(&stack_ids, tip, &globs, &agent_merge_options())
                .await
        })
        .await
        .map_err(|e| LandError::internal(e.message()))?
        .map_err(LandError::internal)?;
        if !merged.conflicts.is_empty() {
            // The merge stops at the first changeset it cannot rewrite.
            let source = stack.get(merged.rewritten.len()).copied();
            let details = merged
                .conflicts
                .iter()
                .take(MAX_CONFLICT_PATHS)
                .map(|c| conflict_wire(source, c))
                .collect();
            return Err(LandError::Conflicts(details));
        }
        Ok(Rewrite {
            pairs: merged.rewritten,
            signatures_dropped: merged.signatures_dropped,
        })
    }
}

/// The one registration line of this unit (called from `lib.rs`): installs the policy with the
/// SPEC defaults over the pipeline's overlap hook.
pub fn register(ext: &Extensions, hooks: &Arc<ReadHooks>) {
    let policy = Arc::new(ContentMerge::with_defaults(hooks.clone()));
    assert!(
        ext.pipeline.set_overlap_policy(policy).is_ok(),
        "the land overlap policy is registered once"
    );
}

#[cfg(test)]
mod tests {
    use asc_core::path::RefName;
    use asc_rpc::auth::Principal;
    use asc_rpc::convert::ModeId;
    use asc_rpc::server::{RepoName, status_rpc_error};
    use uuid::Uuid;

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::update::ServerRefs;

    const LINES: &[u8] = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    const ONE: &[u8] = b"ONE\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    const TEN: &[u8] = b"1\n2\n3\n4\n5\n6\n7\n8\n9\nTEN\n";
    const BOTH: &[u8] = b"ONE\n2\n3\n4\n5\n6\n7\n8\n9\nTEN\n";

    fn ctx() -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: IdMode::Asc,
            principal: Principal { id: "alice".into() },
            op_id: Some(Uuid::new_v4()),
            client: None,
        }
    }

    fn update(
        expect: wire::ref_update::Expect,
        to: ChangesetId,
        mode: wire::UpdateMode,
        no_content_merge: bool,
    ) -> wire::UpdateRefsRequest {
        wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: "o/r".into(),
                id_mode: 0,
            }),
            updates: vec![wire::RefUpdate {
                name: "refs/heads/master".into(),
                expect: Some(expect),
                new: Some(wire::RefTarget::from(&RefTarget::Changeset(to))),
                delete: false,
            }],
            mode: mode as i32,
            no_content_merge,
            ..wire::UpdateRefsRequest::default()
        }
    }

    struct World {
        reg: Arc<Registry>,
        refs: ServerRefs,
        entry: Arc<RepoEntry>,
        base: ChangesetId,
    }

    /// `master` at a commit holding `files`, with the content-merge policy `policy` installed.
    async fn world(
        files: &[(&str, &[u8])],
        policy: impl Fn(Arc<ReadHooks>) -> ContentMerge,
    ) -> World {
        let reg = registry().await;
        let entry = repo(&reg).await;
        let pipeline = Arc::new(PipelineHooks::default());
        let hooks = Arc::new(ReadHooks::default());
        assert!(pipeline.set_overlap_policy(Arc::new(policy(hooks))).is_ok());
        let refs = ServerRefs::new(reg.clone(), pipeline);
        let base = commit(&entry, &[], files).await;
        refs.update_refs(
            &ctx(),
            update(
                wire::ref_update::Expect::Absent(true),
                base,
                wire::UpdateMode::FastForward,
                false,
            ),
        )
        .await
        .unwrap();
        World {
            reg,
            refs,
            entry,
            base,
        }
    }

    async fn land(
        w: &World,
        to: ChangesetId,
        no_content_merge: bool,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        w.refs
            .update_refs(
                &ctx(),
                update(
                    wire::ref_update::Expect::Any(true),
                    to,
                    wire::UpdateMode::Land,
                    no_content_merge,
                ),
            )
            .await
    }

    async fn tip(w: &World) -> ChangesetId {
        use asc_store::refs::Refs as _;
        let got = w
            .reg
            .refs()
            .get(w.entry.key, &RefName::new("refs/heads/master").unwrap())
            .await
            .unwrap()
            .unwrap();
        match got.target {
            RefTarget::Changeset(c) => c,
            RefTarget::Tag(_) => panic!("tag"),
        }
    }

    async fn file_at(w: &World, cs: ChangesetId, path: &str) -> Option<Vec<u8>> {
        let engine = w.entry.backend.engine();
        let root = engine.get_changeset(cs).await.unwrap().root;
        let e = engine
            .lookup(root, &RepoPath::new(path).unwrap())
            .await
            .unwrap()?;
        Some(
            engine
                .read_file(FileId::from_bytes(e.id), e.size)
                .await
                .unwrap()
                .to_vec(),
        )
    }

    fn defaults(hooks: Arc<ReadHooks>) -> ContentMerge {
        ContentMerge::with_defaults(hooks)
    }

    #[tokio::test]
    async fn overlapping_but_mergeable_lands_succeed() {
        let w = world(&[("f.txt", LINES)], defaults).await;
        let mine = commit(&w.entry, &[w.base], &[("f.txt", ONE)]).await;
        let theirs = commit(&w.entry, &[w.base], &[("f.txt", TEN)]).await;
        land(&w, theirs, false).await.unwrap();
        let done = land(&w, mine, false).await.unwrap();
        assert_eq!(done.rewritten.len(), 1);
        let head = tip(&w).await;
        assert_eq!(file_at(&w, head, "f.txt").await, Some(BOTH.to_vec()));
        let cs = w.entry.backend.engine().get_changeset(head).await.unwrap();
        assert_eq!(cs.parents, [theirs]);
    }

    #[tokio::test]
    async fn a_lockfile_overlap_is_a_conflict_with_records() {
        let w = world(&[("Cargo.lock", b"1\n2\n3\n"), ("x", b"x\n")], defaults).await;
        let mine = commit(&w.entry, &[w.base], &[("Cargo.lock", b"one\n2\n3\n")]).await;
        let theirs = commit(&w.entry, &[w.base], &[("Cargo.lock", b"1\n2\nthree\n")]).await;
        land(&w, theirs, false).await.unwrap();
        let before = tip(&w).await;
        let err = land(&w, mine, false).await.unwrap_err();
        let rpc = status_rpc_error(&err).unwrap();
        assert_eq!(rpc.code(), "LAND_CONFLICT");
        assert_eq!(rpc.info().paths, vec!["Cargo.lock".to_string()]);
        let [c] = rpc.info().conflicts.as_slice() else {
            panic!("one conflict record")
        };
        assert_eq!(c.path.to_string(), "Cargo.lock");
        assert_eq!(c.changeset, ModeId::Asc(mine));
        assert!(c.base.is_some() && c.ours.is_some() && c.theirs.is_some());
        assert!(c.marked.is_none(), "a lockfile is never merged");
        assert_eq!(tip(&w).await, before);
    }

    #[tokio::test]
    async fn a_text_conflict_carries_the_diff3_marked_file() {
        let w = world(&[("f.txt", LINES)], defaults).await;
        let mine = commit(
            &w.entry,
            &[w.base],
            &[("f.txt", b"1\n2\n3\n4\nmine\n6\n7\n8\n9\n10\n")],
        )
        .await;
        let theirs = commit(
            &w.entry,
            &[w.base],
            &[("f.txt", b"1\n2\n3\n4\ntheirs\n6\n7\n8\n9\n10\n")],
        )
        .await;
        land(&w, theirs, false).await.unwrap();
        let err = land(&w, mine, false).await.unwrap_err();
        let rpc = status_rpc_error(&err).unwrap();
        assert_eq!(rpc.code(), "LAND_CONFLICT");
        let [c] = rpc.info().conflicts.as_slice() else {
            panic!("one conflict record")
        };
        let Some(ModeId::Asc(marked)) = c.marked else {
            panic!("a marked file")
        };
        let got = w
            .entry
            .backend
            .engine()
            .read_file(marked, 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(got.to_vec()).unwrap();
        assert!(
            text.contains("<<<<<<< ours\n") && text.contains("||||||| base\n"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn disabling_the_content_merge_restores_path_disjoint_landing() {
        let off = |hooks| ContentMerge::new(hooks, false, &[]).unwrap();
        let w = world(&[("f.txt", LINES)], off).await;
        let mine = commit(&w.entry, &[w.base], &[("f.txt", ONE)]).await;
        let theirs = commit(&w.entry, &[w.base], &[("f.txt", TEN)]).await;
        land(&w, theirs, false).await.unwrap();
        let err = land(&w, mine, false).await.unwrap_err();
        assert_eq!(status_rpc_error(&err).unwrap().code(), "LAND_CONFLICT");
    }

    #[tokio::test]
    async fn a_request_can_opt_out_of_the_content_merge() {
        let w = world(&[("f.txt", LINES)], defaults).await;
        let mine = commit(&w.entry, &[w.base], &[("f.txt", ONE)]).await;
        let theirs = commit(&w.entry, &[w.base], &[("f.txt", TEN)]).await;
        land(&w, theirs, false).await.unwrap();
        let err = land(&w, mine, true).await.unwrap_err();
        assert_eq!(
            status_rpc_error(&err).unwrap().code(),
            "LAND_NEEDS_CONTENT_MERGE"
        );
    }

    #[test]
    fn bad_globs_are_refused() {
        let hooks = Arc::new(ReadHooks::default());
        for bad in [
            vec![String::new()],
            vec!["[".to_owned()],
            vec!["a\0b".to_owned()],
        ] {
            assert!(matches!(
                ContentMerge::new(hooks.clone(), true, &bad),
                Err(PolicyError::BadGlob)
            ));
        }
        let many = vec!["x".to_owned(); MAX_NO_MERGE_GLOBS + 1];
        assert!(matches!(
            ContentMerge::new(hooks, true, &many),
            Err(PolicyError::TooMany)
        ));
    }
}
