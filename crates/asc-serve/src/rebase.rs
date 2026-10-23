//! Server `Rebase` for agents (PLAN U68, SPEC §8.11).
//!
//! `Rebase(base, head, onto, resolutions, dry_run, update_ref, expect, mode)` replays the
//! changesets of `base..head` (oldest first, which must be linear) onto `onto`. A changeset whose
//! paths do not intersect what `onto` changed is rebased on metadata; otherwise its intersecting
//! files are merged three-way with diff3 markers, so `Conflict.marked_id` equals
//! `git merge-file --diff3`. The marked file is stored and readable with `ReadFile`.
//!
//! * A conflict is an answer, not an error: the response lists the unresolved `Conflict`
//!   records of the first changeset that cannot be rewritten and no ref moves.
//! * An agent sends the same request again with `Resolution { changeset, path, op }` entries.
//!   `changeset` is the id of the changeset in `base..head` (the old id) and `op` is a `FileOp`
//!   in the `Commit` vocabulary, applied over the merged tree of that changeset. The call
//!   succeeds when every conflict has a resolution. A resolution that names a changeset that
//!   replayed without conflict is refused (`INVALID_ARGUMENT`), so a typo never goes unnoticed.
//!   A resolved changeset keeps its author, committer and message; its `copy_from` annotations
//!   are not carried.
//! * `dry_run` (the default of the client tools) stores the rewritten changesets, which nothing
//!   refers to, and moves no ref; it answers with the id the rebase would produce.
//! * With `update_ref` the result goes through `UpdateRefs` in `mode` `FAST_FORWARD` or `LAND`
//!   (hooks run once). `expect` follows the `Commit` rule: absent means the ref must not exist.
//!   The call is idempotent by `x-asc-op-id`: a repeat after a successful ref move returns the
//!   stored answer. Without `update_ref` the replay is deterministic, so a repeat stores the
//!   same ids, and the result is ingested.
//!
//! The service has already checked write access on the repository and the `x-asc-op-id` header.
//! Whole-file merge buffers are reserved from the text budget first. The resolution list, the
//! inline bytes it carries and the stack are bounded by constants below.

use std::collections::BTreeMap;

use asc_core::change::RewriteOp;
use asc_engine::graph::LogQuery;
use asc_engine::rebase::Rebased;
use asc_rpc::server::{RebaseHandler, RepoName};

use super::land_merge::agent_merge_options;
use super::*;
use crate::update::Rewrites;

/// Most resolutions one `Rebase` carries.
pub const MAX_RESOLUTIONS: usize = 10_000;
/// Most inline bytes all `put_content` resolutions of one call carry together.
pub const MAX_RESOLUTION_BYTES: usize = 64 << 20;
/// Most changesets one `Rebase` replays; a longer stack needs a client rebase.
pub const MAX_RANGE: usize = crate::land::MAX_STACK;

/// `extra` keys a rewrite removes: a signature or raw body names the old parents.
const DROPPED_EXTRA: [&[u8]; 2] = [b"git.gpgsig", b"git.commit"];

type Pairs = Vec<(ChangesetId, ChangesetId)>;

/// The request's resolutions, by `(changeset, path)`; the replay removes the ones it consumes.
struct Resolutions {
    ops: BTreeMap<(ChangesetId, RepoPath), wire::FileOp>,
}

impl Resolutions {
    fn parse(list: &[wire::Resolution]) -> Result<Self, Status> {
        if list.len() > MAX_RESOLUTIONS {
            return Err(invalid("too many resolutions"));
        }
        let mut bytes = 0usize;
        let mut ops = BTreeMap::new();
        for r in list {
            let changeset =
                ChangesetId::from_slice(&r.changeset).map_err(|e| invalid(e.to_string()))?;
            let path = op_path(&r.path)?;
            let mut op =
                r.op.clone()
                    .ok_or_else(|| invalid("a resolution needs an operation"))?;
            if !op.path.is_empty() && op.path != r.path {
                return Err(invalid("a resolution operation names another path"));
            }
            op.path = r.path.clone();
            if let Some(wire::file_op::Op::PutContent(data)) = &op.op {
                bytes = bytes.saturating_add(data.len());
                if bytes > MAX_RESOLUTION_BYTES {
                    return Err(invalid("resolutions carry too much inline content"));
                }
            }
            if ops.insert((changeset, path.clone()), op).is_some() {
                return Err(invalid(format!("{path} is resolved twice")));
            }
        }
        Ok(Self { ops })
    }

    fn has(&self, changeset: ChangesetId, path: &RepoPath) -> bool {
        self.ops.contains_key(&(changeset, path.clone()))
    }

    /// Removes and returns every resolution of `changeset`.
    fn take(&mut self, changeset: ChangesetId) -> Vec<wire::FileOp> {
        let keys: Vec<_> = self
            .ops
            .keys()
            .filter(|(c, _)| *c == changeset)
            .cloned()
            .collect();
        keys.iter().filter_map(|k| self.ops.remove(k)).collect()
    }

    /// The first resolution nothing consumed, if any.
    fn unused(&self) -> Option<(ChangesetId, &RepoPath)> {
        self.ops.keys().next().map(|(c, p)| (*c, p))
    }
}

/// How one changeset replayed.
enum Replayed {
    Done(ChangesetId),
    Conflicts(Vec<wire::Conflict>),
}

fn wire_pairs(pairs: &[(ChangesetId, ChangesetId)]) -> Vec<wire::Rewrite> {
    pairs
        .iter()
        .map(|(old, new)| wire::Rewrite {
            old: Bytes::copy_from_slice(old.as_bytes()),
            new: Bytes::copy_from_slice(new.as_bytes()),
        })
        .collect()
}

fn kept_extra(extra: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    extra
        .iter()
        .filter(|(k, _)| !DROPPED_EXTRA.contains(&k.as_slice()))
        .cloned()
        .collect()
}

impl ServerMutations {
    pub(crate) async fn rebase_inner(
        &self,
        ctx: &CallCtx,
        req: wire::RebaseRequest,
    ) -> Result<wire::RebaseResponse, Status> {
        let o = self.open(ctx).await?;
        check_update_ref(&req.update_ref)?;
        if req.head.is_empty() || req.head.len() > MAX_STRING || req.base.len() > MAX_STRING {
            return Err(invalid("a rebase names a base and a head"));
        }
        if req.onto.len() > MAX_STRING {
            return Err(invalid("onto is too long"));
        }
        let mode = match wire::UpdateMode::try_from(req.mode) {
            Ok(m @ (wire::UpdateMode::FastForward | wire::UpdateMode::Land)) => m,
            _ => return Err(invalid("mode must be FAST_FORWARD or LAND")),
        };
        let mut resolutions = Resolutions::parse(&req.resolutions)?;
        let writes = !req.dry_run;
        if writes && !req.update_ref.is_empty() {
            // Validate before any write; `UpdateRefs` checks the name again.
            asc_core::path::RefName::new(req.update_ref.as_bytes())
                .map_err(|e| invalid(e.to_string()))?;
            if let Some(done) = self.replayed_rebase(&o, ctx, &req.update_ref).await? {
                return Ok(done);
            }
        }
        let base = self.rev(&o, &req.base).await?;
        let onto = self.rev(&o, &req.onto).await?;
        let head = self.rebase_head(&o, ctx, &req).await?;
        // Without a ref the fence spans the object writes and the ingest. With one, `UpdateRefs`
        // takes it itself. A dry run keeps nothing.
        let _gc = if writes && req.update_ref.is_empty() {
            Some(self.fence().await?)
        } else {
            None
        };
        let stack = self.rebase_range(&o, base, head).await?;
        if stack.is_empty() {
            // Nothing between base and head: the rebase is `onto` itself.
            return Ok(wire::RebaseResponse {
                result: Bytes::copy_from_slice(onto.as_bytes()),
                ..wire::RebaseResponse::default()
            });
        }
        let _text = self.budget(MERGE_TEXT_ESTIMATE).await?;

        let mut pairs: Pairs = Vec::with_capacity(stack.len());
        let mut at = onto;
        for old in stack {
            match self.replay_one(&o, old, at, &mut resolutions).await? {
                Replayed::Done(id) => {
                    pairs.push((old, id));
                    at = id;
                }
                Replayed::Conflicts(conflicts) => {
                    return Ok(wire::RebaseResponse {
                        conflicts,
                        ..wire::RebaseResponse::default()
                    });
                }
            }
        }
        if let Some((changeset, path)) = resolutions.unused() {
            return Err(invalid(format!(
                "the resolution of {path} in {changeset} matches no conflict"
            )));
        }
        let mut rewritten = wire_pairs(&pairs);
        let mut result = at;
        if writes {
            let done = self
                .publish_rebase(&o, ctx, &req, mode, result, &pairs)
                .await?;
            result = done.0;
            rewritten = done.1;
        }
        Ok(wire::RebaseResponse {
            result: Bytes::copy_from_slice(result.as_bytes()),
            rewritten,
            conflicts: Vec::new(),
        })
    }

    /// The changeset `head` names; a head in another repository is resolved there and its
    /// closure copied here (`CopyClosure` checks its own rights on both sides).
    async fn rebase_head(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        req: &wire::RebaseRequest,
    ) -> Result<ChangesetId, Status> {
        let Some(head_repo) = req.head_repo.as_ref() else {
            return self.rev(o, &req.head).await;
        };
        let name = RepoName::parse(&head_repo.name)?;
        if name == ctx.repo {
            return self.rev(o, &req.head).await;
        }
        let source = self.registry.open(&name).await.map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &source);
        let spec = ids.rev_in(&req.head, &source.default_branch).await?;
        let head = source
            .backend
            .engine()
            .resolve(std::slice::from_ref(&spec))
            .await
            .map_err(graph_status)?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))?;
        MutationsHandler::copy_closure(
            self,
            ctx,
            wire::CopyClosureRequest {
                from: Some(wire::RepoRef {
                    name: name.to_string(),
                    id_mode: IdMode::Asc as i32,
                }),
                to: Some(wire::RepoRef {
                    name: ctx.repo.to_string(),
                    id_mode: IdMode::Asc as i32,
                }),
                changesets: vec![Bytes::copy_from_slice(head.as_bytes())],
            },
        )
        .await?;
        Ok(head)
    }

    /// The changesets of `base..head`, oldest first. They must form one chain ending at `head`.
    async fn rebase_range(
        &self,
        o: &Opened,
        base: ChangesetId,
        head: ChangesetId,
    ) -> Result<Vec<ChangesetId>, Status> {
        let q = LogQuery {
            include: vec![head],
            exclude: vec![base],
            limit: Some(MAX_RANGE + 1),
            ..LogQuery::default()
        };
        let mut entries = Vec::new();
        let mut page = None;
        loop {
            let got = o
                .engine
                .log(&q, page.as_ref())
                .await
                .map_err(graph_status)?;
            entries.extend(got.entries);
            if entries.len() > MAX_RANGE {
                return Err(Status::failed_precondition(
                    "too many changesets to rebase in one step",
                ));
            }
            match got.next {
                Some(next) => page = Some(next),
                None => break,
            }
        }
        // Newest first; each is the only child of the next one down.
        let linear = entries.first().map(|e| e.id).is_none_or(|id| id == head)
            && entries.iter().enumerate().all(|(i, e)| {
                e.parents.len() == 1
                    && entries
                        .get(i + 1)
                        .is_none_or(|next| next.id == e.parents[0])
            });
        if !linear {
            return Err(Status::failed_precondition(
                "a rebase needs linear history between base and head",
            ));
        }
        Ok(entries.iter().rev().map(|e| e.id).collect())
    }

    /// Replays `old` onto `onto`, applying the resolutions when it conflicts.
    async fn replay_one(
        &self,
        o: &Opened,
        old: ChangesetId,
        onto: ChangesetId,
        resolutions: &mut Resolutions,
    ) -> Result<Replayed, Status> {
        let opts = agent_merge_options();
        let engine = o.engine.clone();
        let step =
            on_blocking_thread(
                move || async move { engine.rebase_changeset(old, onto, &opts).await },
            )
            .await?
            .map_err(merge_status)?;
        let conflicts = match step {
            Rebased::Done { id, .. } => return Ok(Replayed::Done(id)),
            Rebased::Conflicts(conflicts) => conflicts,
        };
        let unresolved: Vec<wire::Conflict> = conflicts
            .iter()
            .filter(|c| !resolutions.has(old, &c.path))
            .take(MAX_CONFLICTS)
            .map(|c| conflict_wire(Some(old), c))
            .collect();
        if !unresolved.is_empty() {
            return Ok(Replayed::Conflicts(unresolved));
        }
        self.apply_resolved(o, old, onto, resolutions)
            .await
            .map(Replayed::Done)
    }

    /// Stores `old` rebased onto `onto` with the merged tree corrected by the resolutions of `old`.
    async fn apply_resolved(
        &self,
        o: &Opened,
        old: ChangesetId,
        onto: ChangesetId,
        resolutions: &mut Resolutions,
    ) -> Result<ChangesetId, Status> {
        let cs = o.engine.get_changeset(old).await.map_err(commit_status)?;
        let [parent] = cs.parents.as_slice() else {
            return Err(Status::failed_precondition(
                "a rebase needs linear history between base and head",
            ));
        };
        let parent_root = o
            .engine
            .get_changeset(*parent)
            .await
            .map_err(commit_status)?
            .root;
        let onto_root = o
            .engine
            .get_changeset(onto)
            .await
            .map_err(commit_status)?
            .root;
        let (engine, theirs, opts) = (o.engine.clone(), cs.root, agent_merge_options());
        let merged = on_blocking_thread(move || async move {
            engine
                .merge_trees(parent_root, onto_root, theirs, &opts)
                .await
        })
        .await?
        .map_err(merge_status)?;
        let ops = resolutions.take(old);
        let planned = self.plan_ops(o, merged.root, &ops).await?;
        let root = o.apply_changes(merged.root, planned.changes).await?;
        o.create(NewChangeset {
            parents: vec![onto],
            author: cs.author.clone(),
            committer: cs.committer.clone(),
            message: cs.message.clone(),
            extra: kept_extra(cs.extra.as_slice()),
            root,
            copies: planned.copies,
        })
        .await
    }

    /// Makes `result` visible and returns the id the ref ended at with every rewrite.
    /// Without a ref: pipeline step 4, then the observer hears about `pairs`. With one: `UpdateRefs` moves it, and the step-8 transaction tells the observer.
    async fn publish_rebase(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        req: &wire::RebaseRequest,
        mode: wire::UpdateMode,
        result: ChangesetId,
        pairs: &[(ChangesetId, ChangesetId)],
    ) -> Result<(ChangesetId, Vec<wire::Rewrite>), Status> {
        if req.update_ref.is_empty() {
            self.ingest_ids(o, &[result]).await?;
            let refs = self.registry.refs();
            let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
            self.registry.guard(&mut tx, &o.entry).await?;
            self.pipeline
                .observer()
                .on_rewritten(&mut tx, o.entry.key, pairs, RewriteOp::Rebase)
                .await
                .map_err(|e| internal("on_rewritten", &e))?;
            tx.commit().await.map_err(|e| internal("commit", &e))?;
            return Ok((result, wire_pairs(pairs)));
        }
        let expect = match req.expect.as_ref() {
            Some(t) => wire::ref_update::Expect::Value(wire::RefTarget::clone(t)),
            None => wire::ref_update::Expect::Absent(true),
        };
        let update = wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: ctx.repo.to_string(),
                id_mode: IdMode::Asc as i32,
            }),
            updates: vec![wire::RefUpdate {
                name: req.update_ref.clone(),
                expect: Some(expect),
                new: Some(wire::RefTarget::from(&RefTarget::Changeset(result))),
                delete: false,
            }],
            mode: mode as i32,
            hooks: wire::HookMode::Run as i32,
            ..wire::UpdateRefsRequest::default()
        };
        // A LAND request finds its own rewrites, so it takes none.
        let rewrites = (mode == wire::UpdateMode::FastForward).then_some(Rewrites {
            pairs,
            op: RewriteOp::Rebase,
        });
        let rewrites_listed = rewrites.is_some();
        let done = self.refs.update_rewriting(ctx, update, rewrites).await?;
        match done.results.as_slice() {
            [r] => match r.new.as_ref().map(RefTarget::try_from) {
                Some(Ok(RefTarget::Changeset(c))) => {
                    // A fast-forward lists the rewrites it was given; a land lists its own,
                    // after ours.
                    let mut all = if rewrites_listed {
                        Vec::new()
                    } else {
                        wire_pairs(pairs)
                    };
                    all.extend(done.rewritten);
                    Ok((c, all))
                }
                _ => Err(internal("update_refs", &"unexpected result")),
            },
            _ => Err(internal("update_refs", &"unexpected result")),
        }
    }

    /// The answer of a previous attempt of this `op_id` that moved `update_ref`, if any.
    async fn replayed_rebase(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        update_ref: &str,
    ) -> Result<Option<wire::RebaseResponse>, Status> {
        let Some(op) = ctx.op_id else { return Ok(None) };
        let Some(done) = self.refs.stored(&o.entry, op).await? else {
            return Ok(None);
        };
        match done.results.as_slice() {
            [r] if r.name == update_ref => match r.new.as_ref().map(RefTarget::try_from) {
                Some(Ok(RefTarget::Changeset(c))) => Ok(Some(wire::RebaseResponse {
                    result: Bytes::copy_from_slice(c.as_bytes()),
                    rewritten: done.rewritten,
                    conflicts: Vec::new(),
                })),
                _ => Err(invalid("op id was used for another operation")),
            },
            _ => Err(invalid("op id was used for another operation")),
        }
    }
}

#[async_trait]
impl RebaseHandler for ServerMutations {
    async fn rebase(
        &self,
        ctx: &CallCtx,
        req: wire::RebaseRequest,
    ) -> Result<wire::RebaseResponse, Status> {
        self.rebase_inner(ctx, req).await
    }
}

/// The one registration line of this unit (called from `lib.rs`): the `rebase` group, served by
/// the mutations handler.
pub fn register(ext: &Extensions, handler: &Arc<ServerMutations>) {
    ext.handlers
        .set_rebase(handler.clone())
        .expect("the rebase handler group is registered once");
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Stdio;

    use asc_git::realgit::RealGit;

    use super::super::fx::*;
    use super::*;
    use crate::ingest::testutil::{put_file, sig as stored_sig};

    const LINES: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";

    fn with_line(n: usize, text: &str) -> String {
        LINES
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{}\n", if i + 1 == n { text } else { l }))
            .collect()
    }

    type Ops<'a> = &'a [(&'a str, Option<&'a str>)];

    async fn make(f: &Fx, parents: &[ChangesetId], message: &str, ops: Ops<'_>) -> ChangesetId {
        let engine = f.entry.backend.engine();
        let base = match parents.first() {
            Some(&p) => engine.get_changeset(p).await.unwrap().root,
            None => Eng::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (path, content) in ops {
            let change = match content {
                Some(c) => put_file(&f.entry, c.as_bytes()).await,
                None => Change::Delete,
            };
            changes.push((RepoPath::new(*path).unwrap(), change));
        }
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        let root = engine.apply(base, &changes).await.unwrap();
        engine
            .create_changeset(
                NewChangeset {
                    parents: parents.to_vec(),
                    author: stored_sig(),
                    committer: stored_sig(),
                    message: message.as_bytes().to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    fn req(base: ChangesetId, head: ChangesetId, onto: ChangesetId) -> wire::RebaseRequest {
        wire::RebaseRequest {
            repo: repo_ref(),
            base: base.to_string(),
            head: head.to_string(),
            onto: onto.to_string(),
            ..wire::RebaseRequest::default()
        }
    }

    fn resolution(cs: ChangesetId, path: &str, content: &str) -> wire::Resolution {
        wire::Resolution {
            changeset: Bytes::copy_from_slice(cs.as_bytes()),
            path: Bytes::copy_from_slice(path.as_bytes()),
            op: Some(put(path, content.as_bytes())),
        }
    }

    /// `base` with a stack of two changesets (`a` edits line 1 of f.txt, `b` adds b.txt) and a
    /// target `onto` that edits line 10 of f.txt.
    async fn stack(f: &Fx) -> (ChangesetId, ChangesetId, ChangesetId, ChangesetId) {
        let base = make(
            f,
            &[],
            "base",
            &[("f.txt", Some(LINES)), ("g.txt", Some("g\n"))],
        )
        .await;
        let a = make(f, &[base], "a", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let b = make(f, &[a], "b", &[("b.txt", Some("b\n"))]).await;
        let onto = make(
            f,
            &[base],
            "onto",
            &[("f.txt", Some(&with_line(10, "TEN")))],
        )
        .await;
        (base, a, b, onto)
    }

    #[tokio::test]
    async fn an_overlapping_but_mergeable_stack_replays() {
        let f = fx().await;
        let (base, a, b, onto) = stack(&f).await;
        let mut q = req(base, b, onto);
        q.dry_run = true;
        let got = f.m.rebase(&ctx(), q).await.unwrap();
        assert!(got.conflicts.is_empty());
        assert_eq!(got.rewritten.len(), 2);
        let result = id(&got.result);
        let want = with_line(1, "ONE").replace("10\n", "TEN\n");
        assert_eq!(read(&f, result, "f.txt").await, Some(want.into_bytes()));
        assert_eq!(read(&f, result, "b.txt").await, Some(b"b\n".to_vec()));
        let _ = a;
        assert_eq!(ref_target(&f, "refs/heads/out").await, None);
    }

    #[tokio::test]
    async fn an_empty_range_answers_onto() {
        let f = fx().await;
        let (base, _, _, onto) = stack(&f).await;
        let got = f.m.rebase(&ctx(), req(base, base, onto)).await.unwrap();
        assert_eq!(id(&got.result), onto);
        assert!(got.conflicts.is_empty() && got.rewritten.is_empty());
    }

    async fn conflicting(f: &Fx) -> (ChangesetId, ChangesetId, ChangesetId) {
        let base = make(f, &[], "base", &[("f.txt", Some(LINES))]).await;
        let mine = make(
            f,
            &[base],
            "mine",
            &[("f.txt", Some(&with_line(5, "mine")))],
        )
        .await;
        let onto = make(
            f,
            &[base],
            "onto",
            &[("f.txt", Some(&with_line(5, "theirs")))],
        )
        .await;
        (base, mine, onto)
    }

    fn git_out(dir: &Path, args: &[&str]) -> Vec<u8> {
        let real = RealGit::resolve(None, None).expect("git is installed");
        let mut cmd = real.command().expect("a git command");
        let out = cmd
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("git runs");
        // `merge-file` exits with the number of conflicts.
        assert!(
            out.status.success() || args.contains(&"merge-file"),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    #[tokio::test]
    async fn a_conflict_is_reported_with_marked_content_equal_to_git_merge_file() {
        let f = fx().await;
        let (base, mine, onto) = conflicting(&f).await;
        let mut q = req(base, mine, onto);
        q.dry_run = true;
        let got = f.m.rebase(&ctx(), q).await.unwrap();
        assert!(got.result.is_empty() && got.rewritten.is_empty());
        let [c] = got.conflicts.as_slice() else {
            panic!("one conflict, got {:?}", got.conflicts)
        };
        assert_eq!(c.path.as_ref(), b"f.txt");
        assert_eq!(id(&c.changeset), mine);
        assert_eq!(c.kind, wire::ConflictKind::Content as i32);
        assert!(!c.base_id.is_empty() && !c.ours_id.is_empty() && !c.theirs_id.is_empty());
        let marked = f
            .entry
            .backend
            .engine()
            .read_file(FileId::from_slice(&c.marked_id).unwrap(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        assert!(
            String::from_utf8_lossy(&marked).contains("|||||||"),
            "diff3 style"
        );
        if RealGit::resolve(None, None).is_err() {
            eprintln!("git is not installed; skipping the comparison");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for (name, cs) in [("ours", onto), ("base", base), ("theirs", mine)] {
            let bytes = read(&f, cs, "f.txt").await.unwrap();
            std::fs::write(dir.path().join(name), bytes).unwrap();
        }
        let want = git_out(
            dir.path(),
            &[
                "merge-file",
                "-p",
                "--diff3",
                "-L",
                "ours",
                "-L",
                "base",
                "-L",
                "theirs",
                "ours",
                "base",
                "theirs",
            ],
        );
        assert_eq!(marked, want);
    }

    #[tokio::test]
    async fn resolutions_settle_the_conflict_and_the_ref_moves_once() {
        let f = fx().await;
        let (base, mine, onto) = conflicting(&f).await;
        let tail = make(&f, &[mine], "tail", &[("t.txt", Some("t\n"))]).await;
        let mut q = req(base, tail, onto);
        q.update_ref = "refs/heads/rebased".into();
        // Without a resolution nothing moves.
        let first = f.m.rebase(&ctx(), q.clone()).await.unwrap();
        assert_eq!(first.conflicts.len(), 1);
        assert_eq!(ref_target(&f, "refs/heads/rebased").await, None);

        q.resolutions = vec![resolution(mine, "f.txt", "resolved\n")];
        q.dry_run = false;
        let c = ctx();
        let got = f.m.rebase(&c, q.clone()).await.unwrap();
        assert!(got.conflicts.is_empty());
        assert_eq!(got.rewritten.len(), 2);
        let result = id(&got.result);
        assert_eq!(
            ref_target(&f, "refs/heads/rebased").await,
            Some(RefTarget::Changeset(result))
        );
        assert_eq!(
            read(&f, result, "f.txt").await,
            Some(b"resolved\n".to_vec())
        );
        assert_eq!(read(&f, result, "t.txt").await, Some(b"t\n".to_vec()));
        // The first rewritten changeset sits on `onto` and keeps the message.
        let first_new = id(&got.rewritten[0].new);
        let new = f
            .entry
            .backend
            .engine()
            .get_changeset(first_new)
            .await
            .unwrap();
        assert_eq!(new.parents, [onto]);
        assert_eq!(new.message, b"mine");

        // The same op id again returns the stored answer.
        let rows = count(&f, "SELECT COUNT(*) FROM ref_log").await;
        let again = f.m.rebase(&c, q).await.unwrap();
        assert_eq!(again.result, got.result);
        assert_eq!(again.rewritten, got.rewritten);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, rows);
    }

    #[tokio::test]
    async fn a_resolution_that_matches_no_conflict_is_refused() {
        let f = fx().await;
        let (base, a, b, onto) = stack(&f).await;
        let mut q = req(base, b, onto);
        q.dry_run = true;
        q.resolutions = vec![resolution(a, "f.txt", "x\n")];
        let e = f.m.rebase(&ctx(), q).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn bad_requests_are_refused_before_any_write() {
        let f = fx().await;
        let (base, mine, onto) = conflicting(&f).await;
        let ok = req(base, mine, onto);
        let cases: Vec<(&str, wire::RebaseRequest)> = vec![
            (
                "force",
                wire::RebaseRequest {
                    mode: wire::UpdateMode::Force as i32,
                    ..ok.clone()
                },
            ),
            (
                "no head",
                wire::RebaseRequest {
                    head: String::new(),
                    ..ok.clone()
                },
            ),
            (
                "resolution without op",
                wire::RebaseRequest {
                    resolutions: vec![wire::Resolution {
                        changeset: Bytes::copy_from_slice(mine.as_bytes()),
                        path: Bytes::from_static(b"f.txt"),
                        op: None,
                    }],
                    ..ok.clone()
                },
            ),
            (
                "resolution with a bad changeset id",
                wire::RebaseRequest {
                    resolutions: vec![wire::Resolution {
                        changeset: Bytes::from_static(b"short"),
                        path: Bytes::from_static(b"f.txt"),
                        op: Some(put("f.txt", b"x")),
                    }],
                    ..ok.clone()
                },
            ),
            (
                "resolution for an unsafe path",
                wire::RebaseRequest {
                    resolutions: vec![resolution(mine, "../x", "x")],
                    ..ok.clone()
                },
            ),
            (
                "duplicate resolutions",
                wire::RebaseRequest {
                    resolutions: vec![
                        resolution(mine, "f.txt", "x"),
                        resolution(mine, "f.txt", "y"),
                    ],
                    ..ok.clone()
                },
            ),
            (
                "too many resolutions",
                wire::RebaseRequest {
                    resolutions: (0..=MAX_RESOLUTIONS)
                        .map(|i| resolution(mine, &format!("p{i}"), "x"))
                        .collect(),
                    ..ok.clone()
                },
            ),
        ];
        for (what, q) in cases {
            let e = f.m.rebase(&ctx(), q).await.unwrap_err();
            assert!(
                matches!(e.code(), tonic::Code::InvalidArgument),
                "{what}: {e:?}"
            );
        }
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn a_non_linear_range_is_a_failed_precondition() {
        let f = fx().await;
        let base = make(&f, &[], "base", &[("a", Some("a\n"))]).await;
        let left = make(&f, &[base], "left", &[("l", Some("l\n"))]).await;
        let right = make(&f, &[base], "right", &[("r", Some("r\n"))]).await;
        let merge = make(&f, &[left, right], "merge", &[("m", Some("m\n"))]).await;
        let e =
            f.m.rebase(&ctx(), req(base, merge, base))
                .await
                .unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn a_read_only_mirror_takes_no_rebase() {
        let f = fx().await;
        let (base, mine, onto) = conflicting(&f).await;
        make_mirror(&f).await;
        let e = f.m.rebase(&ctx(), req(base, mine, onto)).await.unwrap_err();
        assert_eq!(code(&e), "MIRROR_MODE");
    }
}
