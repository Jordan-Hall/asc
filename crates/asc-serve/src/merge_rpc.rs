//! Server-side `Merge`: all styles, `dry_run`, protected globs and cross-repository heads
//! (PLAN U48b, SPEC §8.7).
//!
//! * `MERGE` makes a two-parent changeset (`[base, head]`) whose tree is the three-way merge of
//!   `base` and `head` over their merge base. `SQUASH` makes one changeset on `base` with the same
//!   tree. `REBASE` rewrites each changeset of `base..head` onto `base` (metadata only where the
//!   paths are disjoint, a content merge of the intersecting files otherwise) and answers with the
//!   new tip. `REBASE_MERGE` rebases and then merges the new tip into `base`, so the result is a
//!   merge changeset `[base, tip]`. `FAST_FORWARD_ONLY` answers with `head` itself, and refuses
//!   anything else.
//! * A conflict is an answer, not an error: the response lists the paths, no changeset is made
//!   and no ref moves. `dry_run` runs the same computation and stops there; it makes no changeset
//!   and moves no ref (a rebase still stores the rewritten changesets and a content merge still
//!   stores its marked files; both are unreferenced objects that GC removes).
//! * `protected_globs` name paths a merge may not touch. A merge whose `head` changed such a path
//!   since the merge base reports it in `changed_protected` and writes nothing, `dry_run` or not.
//! * With an empty `update_ref` the result goes through pipeline step 4 only
//!   ([`ServerMutations::ingest_ids`]). With one, the ref moves through the full `UpdateRefs`
//!   pipeline as a compare-and-set against `base`, and the step-8 transaction also tells the
//!   [`IngestObserver`](crate::ingest::IngestObserver) about the rewrites. Without a ref the
//!   observer hears about them in a transaction of its own.
//! * A head in another repository is read after the service checked read access to it. Its closure
//!   is copied with `CopyClosure` first.
//!
//! The service has already checked write access on the repository and the `x-asc-op-id` header.
//! Every whole-file buffer a content merge needs is reserved from the
//! [`TextBudget`](crate::TextBudget) before the merge starts, every list is bounded by a constant
//! below, and errors never carry request bytes beyond paths.

use asc_core::change::RewriteOp;
use asc_engine::graph::LogQuery;
use asc_engine::history::LogEntry;
use asc_engine::land::NoMergeGlobs;
use asc_engine::rebase::Rebased;
use asc_rpc::server::RepoName;

use super::*;
use crate::update::Rewrites;

/// Most protected globs one `Merge` may carry.
pub const MAX_PROTECTED_GLOBS: usize = 256;
/// Most protected paths one response names.
pub const MAX_PROTECTED_REPORTED: usize = 10_000;
/// Most changesets a `SQUASH` or `REBASE` covers; a longer range needs a client rebase.
pub const MAX_RANGE: usize = crate::land::MAX_STACK;

/// `(old, new)` changeset ids.
type Pairs = Vec<(ChangesetId, ChangesetId)>;

/// The changesets a merge rewrote.
enum Rewritten {
    None,
    /// Every source maps to the one squashed result.
    Squashed(Vec<ChangesetId>),
    /// Each changeset maps to its rebased copy.
    Rebased(Pairs),
}

impl Rewritten {
    fn pairs(self, result: ChangesetId) -> Pairs {
        match self {
            Self::None => Vec::new(),
            Self::Squashed(sources) => sources.into_iter().map(|s| (s, result)).collect(),
            Self::Rebased(pairs) => pairs,
        }
    }
}

/// What the merge computation decided, before anything is made visible.
enum Computed {
    /// There is nothing to merge: the result changes no file.
    Empty,
    /// The paths that do not merge.
    Conflicts(Vec<wire::Conflict>),
    /// An existing changeset becomes the result (`FAST_FORWARD_ONLY`).
    Existing(ChangesetId),
    /// The rewritten tip becomes the result (`REBASE`); the rewrites are stored.
    Tip(ChangesetId, Pairs),
    /// A new changeset on `parents` with this tree.
    Tree {
        root: TreeId,
        parents: Vec<ChangesetId>,
        rewritten: Rewritten,
        op: RewriteOp,
        /// The message the request leaves out.
        message: Vec<u8>,
    },
}

fn globs_of(patterns: &[String]) -> Result<Option<NoMergeGlobs>, Status> {
    if patterns.len() > MAX_PROTECTED_GLOBS {
        return Err(invalid("too many protected globs"));
    }
    if patterns
        .iter()
        .any(|g| g.is_empty() || g.len() > MAX_STRING || g.contains('\0'))
    {
        return Err(invalid("a protected glob is empty or too long"));
    }
    if patterns.is_empty() {
        return Ok(None);
    }
    NoMergeGlobs::new(patterns)
        .map(Some)
        .map_err(|_| invalid("a protected glob is not valid"))
}

/// Whether `path` is protected. A path that is not UTF-8 cannot be matched by a glob, so it
/// counts as protected: an unreadable name never slips past the check.
fn is_protected(globs: &NoMergeGlobs, path: &RepoPath) -> bool {
    std::str::from_utf8(path.as_bytes()).is_err() || globs.is_match(path)
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

fn merge_message(head: ChangesetId, base: ChangesetId) -> Vec<u8> {
    format!("Merge changeset {head} into {base}\n").into_bytes()
}

fn conflicts_of(source: ChangesetId, found: &[Conflict]) -> Vec<wire::Conflict> {
    found
        .iter()
        .take(MAX_CONFLICTS)
        .map(|c| conflict_wire(Some(source), c))
        .collect()
}

impl ServerMutations {
    pub(crate) async fn merge_inner(
        &self,
        ctx: &CallCtx,
        req: wire::MergeRequest,
    ) -> Result<wire::MergeResponse, Status> {
        let o = self.open(ctx).await?;
        let style =
            wire::MergeStyle::try_from(req.style).map_err(|_| invalid("unknown merge style"))?;
        check_update_ref(&req.update_ref)?;
        check_message(&req.message)?;
        if req.sign {
            // Signing needs a server key (SPEC §8.5 step 5: there is no server signature).
            return Err(Status::unimplemented("this server does not sign commits"));
        }
        if req.head.is_empty() || req.head.len() > MAX_STRING || req.base.len() > MAX_STRING {
            return Err(invalid("a merge names a base and a head"));
        }
        let globs = globs_of(&req.protected_globs)?;
        let writes = !req.dry_run;
        if writes && !req.update_ref.is_empty() {
            // Validate before any write; `UpdateRefs` checks the name again.
            asc_core::path::RefName::new(req.update_ref.as_bytes())
                .map_err(|e| invalid(e.to_string()))?;
            if let Some(done) = self.replayed_merge(&o, ctx, &req.update_ref).await? {
                return Ok(done);
            }
        }
        let base = self.rev(&o, &req.base).await?;
        let head = self.merge_head(&o, ctx, &req).await?;
        // Without a ref the fence spans the object writes and the ingest. With one, `UpdateRefs`
        // takes it itself (the lock is not re-entrant under a waiting sweep). A dry run keeps
        // nothing.
        let _gc = if writes && req.update_ref.is_empty() {
            Some(self.fence().await?)
        } else {
            None
        };

        // `head` already in `base`: nothing to merge.
        if head == base
            || o.engine
                .is_ancestor(head, base)
                .await
                .map_err(graph_status)?
        {
            return Ok(wire::MergeResponse {
                is_empty: true,
                ..wire::MergeResponse::default()
            });
        }
        let fast_forward = o
            .engine
            .is_ancestor(base, head)
            .await
            .map_err(graph_status)?;
        if style == wire::MergeStyle::FastForwardOnly && !fast_forward {
            return Err(Status::failed_precondition("not a fast-forward"));
        }
        let mb = if fast_forward {
            base
        } else {
            *o.engine
                .merge_base(base, &[head], false)
                .await
                .map_err(graph_status)?
                .first()
                .ok_or_else(|| Status::failed_precondition("the histories have no common base"))?
        };

        let protected = match &globs {
            Some(globs) => self.changed_protected(&o, globs, mb, head).await?,
            None => Vec::new(),
        };
        if writes && !protected.is_empty() {
            return Ok(wire::MergeResponse {
                changed_protected: protected,
                ..wire::MergeResponse::default()
            });
        }
        let base_cs = o.engine.get_changeset(base).await.map_err(commit_status)?;
        let head_cs = o.engine.get_changeset(head).await.map_err(commit_status)?;
        // Signatures are checked before anything is stored. A squash is by default the head's
        // author; the other styles need none (a rebase keeps the rewritten ones).
        let makes_changeset = matches!(
            style,
            wire::MergeStyle::Merge | wire::MergeStyle::Squash | wire::MergeStyle::RebaseMerge
        );
        let signed = if writes && makes_changeset {
            let default_author =
                (style == wire::MergeStyle::Squash).then(|| head_cs.author.clone());
            Some(signatures(
                req.author.as_ref(),
                req.committer.as_ref(),
                default_author,
            )?)
        } else {
            None
        };

        let computed = self
            .compute(&o, style, (base, &base_cs), (head, &head_cs), mb)
            .await?;
        let answer = |r: wire::MergeResponse| wire::MergeResponse {
            changed_protected: protected.clone(),
            ..r
        };
        match computed {
            Computed::Empty => Ok(answer(wire::MergeResponse {
                is_empty: true,
                ..wire::MergeResponse::default()
            })),
            Computed::Conflicts(details) => Ok(answer(wire::MergeResponse {
                conflicts: details.iter().map(|c| c.path.clone()).collect(),
                conflict_details: details,
                ..wire::MergeResponse::default()
            })),
            _ if !writes => Ok(answer(wire::MergeResponse::default())),
            Computed::Existing(id) => {
                self.publish_merge(&o, ctx, &req, base, id, &[], RewriteOp::Rebase)
                    .await?;
                Ok(wire::MergeResponse {
                    result: Bytes::copy_from_slice(id.as_bytes()),
                    ..wire::MergeResponse::default()
                })
            }
            Computed::Tip(tip, pairs) => {
                self.publish_merge(&o, ctx, &req, base, tip, &pairs, RewriteOp::Rebase)
                    .await?;
                Ok(wire::MergeResponse {
                    result: Bytes::copy_from_slice(tip.as_bytes()),
                    rewritten: wire_pairs(&pairs),
                    ..wire::MergeResponse::default()
                })
            }
            Computed::Tree {
                root,
                parents,
                rewritten,
                op,
                message,
            } => {
                let (author, committer) =
                    signed.ok_or_else(|| internal("merge", &"a new changeset has no signature"))?;
                let message = if req.message.is_empty() {
                    message
                } else {
                    req.message.to_vec()
                };
                let id = o
                    .create(NewChangeset {
                        parents,
                        author,
                        committer,
                        message,
                        extra: Vec::new(),
                        root,
                        copies: Vec::new(),
                    })
                    .await?;
                let pairs = rewritten.pairs(id);
                self.publish_merge(&o, ctx, &req, base, id, &pairs, op)
                    .await?;
                Ok(wire::MergeResponse {
                    result: Bytes::copy_from_slice(id.as_bytes()),
                    rewritten: wire_pairs(&pairs),
                    ..wire::MergeResponse::default()
                })
            }
        }
    }

    /// The changeset `head` names. A head in another repository is resolved there, then its
    /// closure is copied here (`CopyClosure`, which checks its own rights on both sides).
    async fn merge_head(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        req: &wire::MergeRequest,
    ) -> Result<ChangesetId, Status> {
        let Some(head_repo) = req.head_repo.as_ref() else {
            return self.rev(o, &req.head).await;
        };
        let name = RepoName::parse(&head_repo.name)?;
        if name == ctx.repo {
            return self.rev(o, &req.head).await;
        }
        let source = self.registry.open(&name).await.map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &source).for_viewer(&ctx.principal);
        let head = ids
            .resolve(&req.head, &source.default_branch, graph_status)
            .await?
            .ok_or_else(|| Status::not_found(format!("unknown revision {:?}", req.head)))?;
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

    /// The paths `head` changed since `mb` that match `globs`, sorted, at most
    /// [`MAX_PROTECTED_REPORTED`].
    async fn changed_protected(
        &self,
        o: &Opened,
        globs: &NoMergeGlobs,
        mb: ChangesetId,
        head: ChangesetId,
    ) -> Result<Vec<Bytes>, Status> {
        let changed = o.engine.diff(mb, head, &[]).await.map_err(graph_status)?;
        let mut out: Vec<Bytes> = changed
            .iter()
            .filter(|(p, _)| is_protected(globs, p))
            .map(|(p, _)| Bytes::copy_from_slice(p.as_bytes()))
            .collect();
        out.sort();
        out.truncate(MAX_PROTECTED_REPORTED);
        Ok(out)
    }

    /// The changesets `head` has that `base` does not, newest first.
    async fn range(
        &self,
        o: &Opened,
        base: ChangesetId,
        head: ChangesetId,
    ) -> Result<Vec<LogEntry>, Status> {
        let q = LogQuery {
            include: vec![head],
            exclude: vec![base],
            limit: Some(MAX_RANGE + 1),
            ..LogQuery::default()
        };
        let mut out = Vec::new();
        let mut page = None;
        loop {
            let got = o
                .engine
                .log(&q, page.as_ref())
                .await
                .map_err(graph_status)?;
            out.extend(got.entries);
            if out.len() > MAX_RANGE {
                return Err(Status::failed_precondition(
                    "too many changesets to merge in one step",
                ));
            }
            match got.next {
                Some(next) => page = Some(next),
                None => return Ok(out),
            }
        }
    }

    /// Runs the style: the merged tree and what must be written for it.
    async fn compute(
        &self,
        o: &Opened,
        style: wire::MergeStyle,
        (base, base_cs): (ChangesetId, &asc_core::object::Changeset),
        (head, head_cs): (ChangesetId, &asc_core::object::Changeset),
        mb: ChangesetId,
    ) -> Result<Computed, Status> {
        use wire::MergeStyle as S;
        if style == S::FastForwardOnly {
            return Ok(Computed::Existing(head));
        }
        // Content merges and rebases read whole files: reserve the memory first.
        let _text = self.budget(MERGE_TEXT_ESTIMATE).await?;
        match style {
            S::Merge | S::Squash => {
                let mb_root = o
                    .engine
                    .get_changeset(mb)
                    .await
                    .map_err(commit_status)?
                    .root;
                let (ours, theirs) = (base_cs.root, head_cs.root);
                let opts = MergeOptions::default();
                let engine = o.engine.clone();
                let merged = on_blocking_thread(move || async move {
                    engine.merge_trees(mb_root, ours, theirs, &opts).await
                })
                .await?
                .map_err(merge_status)?;
                if !merged.conflicts.is_empty() {
                    return Ok(Computed::Conflicts(conflicts_of(head, &merged.conflicts)));
                }
                if merged.root == base_cs.root {
                    return Ok(Computed::Empty);
                }
                if style == S::Merge {
                    return Ok(Computed::Tree {
                        root: merged.root,
                        parents: vec![base, head],
                        rewritten: Rewritten::None,
                        op: RewriteOp::New,
                        message: merge_message(head, base),
                    });
                }
                let sources = self.range(o, base, head).await?;
                Ok(Computed::Tree {
                    root: merged.root,
                    parents: vec![base],
                    rewritten: Rewritten::Squashed(sources.iter().map(|e| e.id).collect()),
                    op: RewriteOp::Squash,
                    message: head_cs.message.clone(),
                })
            }
            S::Rebase | S::RebaseMerge => {
                let stack = self.range(o, base, head).await?;
                // Newest first; each is the only child of the next one down.
                let linear = stack.first().map(|e| e.id) == Some(head)
                    && stack.iter().enumerate().all(|(i, e)| {
                        e.parents.len() == 1
                            && stack.get(i + 1).is_none_or(|next| next.id == e.parents[0])
                    });
                if !linear {
                    return Err(Status::failed_precondition(
                        "a rebase needs linear history between base and head",
                    ));
                }
                let ids: Vec<ChangesetId> = stack.iter().rev().map(|e| e.id).collect();
                let opts = MergeOptions::default();
                let engine = o.engine.clone();
                let done = on_blocking_thread(move || async move {
                    let mut onto = base;
                    let mut pairs = Vec::with_capacity(ids.len());
                    for old in ids {
                        match engine.rebase_changeset(old, onto, &opts).await? {
                            Rebased::Done { id, .. } => {
                                pairs.push((old, id));
                                onto = id;
                            }
                            Rebased::Conflicts(c) => return Ok(Err((old, c))),
                        }
                    }
                    Ok::<_, merge::Error>(Ok(pairs))
                })
                .await?
                .map_err(merge_status)?;
                let pairs = match done {
                    Err((old, conflicts)) => {
                        return Ok(Computed::Conflicts(conflicts_of(old, &conflicts)));
                    }
                    Ok(pairs) => pairs,
                };
                let Some(&(_, tip)) = pairs.last() else {
                    return Ok(Computed::Empty);
                };
                let tip_root = o
                    .engine
                    .get_changeset(tip)
                    .await
                    .map_err(commit_status)?
                    .root;
                if tip_root == base_cs.root {
                    return Ok(Computed::Empty);
                }
                if style == S::Rebase {
                    return Ok(Computed::Tip(tip, pairs));
                }
                Ok(Computed::Tree {
                    root: tip_root,
                    parents: vec![base, tip],
                    rewritten: Rewritten::Rebased(pairs),
                    op: RewriteOp::Rebase,
                    message: merge_message(head, base),
                })
            }
            S::FastForwardOnly => Ok(Computed::Existing(head)),
        }
    }

    /// Makes `id` visible. Without a ref: pipeline step 4, then the observer hears about
    /// `pairs`. With one: `UpdateRefs` moves it as a compare-and-set against `base`, and the
    /// step-8 transaction tells the observer.
    #[allow(clippy::too_many_arguments)]
    async fn publish_merge(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        req: &wire::MergeRequest,
        base: ChangesetId,
        id: ChangesetId,
        pairs: &[(ChangesetId, ChangesetId)],
        op: RewriteOp,
    ) -> Result<(), Status> {
        if req.update_ref.is_empty() {
            self.ingest_ids(o, &[id]).await?;
            if pairs.is_empty() {
                return Ok(());
            }
            let refs = self.registry.refs();
            let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
            self.registry.guard(&mut tx, &o.entry).await?;
            self.pipeline
                .observer()
                .on_rewritten(&mut tx, o.entry.key, pairs, op)
                .await
                .map_err(|e| internal("on_rewritten", &e))?;
            return tx.commit().await.map_err(|e| internal("commit", &e));
        }
        let expect = self.ref_expectation(o, ctx, &req.update_ref, base).await?;
        let update = wire::UpdateRefsRequest {
            repo: Some(wire::RepoRef {
                name: ctx.repo.to_string(),
                id_mode: IdMode::Asc as i32,
            }),
            updates: vec![wire::RefUpdate {
                name: req.update_ref.clone(),
                expect: Some(expect),
                new: Some(wire::RefTarget::from(&RefTarget::Changeset(id))),
                delete: false,
            }],
            mode: wire::UpdateMode::FastForward as i32,
            hooks: wire::HookMode::Run as i32,
            ..wire::UpdateRefsRequest::default()
        };
        let done = self
            .refs
            .update_rewriting(ctx, update, Some(Rewrites { pairs, op }))
            .await?;
        match done.results.as_slice() {
            [r] if r.new.as_ref().map(RefTarget::try_from)
                == Some(Ok(RefTarget::Changeset(id))) =>
            {
                Ok(())
            }
            _ => Err(internal("update_refs", &"unexpected result")),
        }
    }

    /// The answer of a previous attempt of this `op_id` that moved `update_ref`, if any.
    async fn replayed_merge(
        &self,
        o: &Opened,
        ctx: &CallCtx,
        update_ref: &str,
    ) -> Result<Option<wire::MergeResponse>, Status> {
        let Some(op) = ctx.op_id else { return Ok(None) };
        let Some(done) = self.refs.stored(&o.entry, op).await? else {
            return Ok(None);
        };
        match done.results.as_slice() {
            [r] if r.name == update_ref => match r.new.as_ref().map(RefTarget::try_from) {
                Some(Ok(RefTarget::Changeset(c))) => Ok(Some(wire::MergeResponse {
                    result: Bytes::copy_from_slice(c.as_bytes()),
                    rewritten: done.rewritten,
                    ..wire::MergeResponse::default()
                })),
                _ => Err(invalid("op id was used for another operation")),
            },
            _ => Err(invalid("op id was used for another operation")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Stdio;
    use std::sync::Mutex;

    use asc_core::object::Changeset;
    use asc_git::realgit::RealGit;

    use super::super::fx::*;
    use super::*;
    use crate::ingest::testutil::{put_file, sig as stored_sig};
    use crate::ingest::{IngestObserver, ObserverError};

    const LINES: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";

    /// `LINES` with line `n` (1-based) replaced.
    fn with_line(n: usize, text: &str) -> String {
        LINES
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{}\n", if i + 1 == n { text } else { l }))
            .collect()
    }

    type Ops<'a> = &'a [(&'a str, Option<&'a str>)];

    /// A stored changeset on `parents` with `ops` applied over the first parent's tree
    /// (`None` deletes).
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

    /// `R`: the common root of most tests.
    async fn root(f: &Fx) -> ChangesetId {
        make(
            f,
            &[],
            "root",
            &[("f.txt", Some(LINES)), ("g.txt", Some("g\n"))],
        )
        .await
    }

    fn req(base: ChangesetId, head: ChangesetId, style: wire::MergeStyle) -> wire::MergeRequest {
        wire::MergeRequest {
            repo: repo_ref(),
            base: base.to_string(),
            head: head.to_string(),
            style: style as i32,
            author: Some(sig()),
            committer: Some(sig()),
            ..wire::MergeRequest::default()
        }
    }

    async fn cs(f: &Fx, id: ChangesetId) -> Arc<Changeset> {
        f.entry.backend.engine().get_changeset(id).await.unwrap()
    }

    fn result_of(r: &wire::MergeResponse) -> ChangesetId {
        assert!(!r.result.is_empty(), "no result: {r:?}");
        id(&r.result)
    }

    fn pairs_of(r: &wire::MergeResponse) -> Pairs {
        r.rewritten
            .iter()
            .map(|w| (id(&w.old), id(&w.new)))
            .collect()
    }

    /// Records what the observer hears about rewrites.
    #[derive(Default)]
    struct Spy(Mutex<Vec<(Pairs, RewriteOp)>>);

    #[async_trait]
    impl IngestObserver for Spy {
        async fn on_rewritten(
            &self,
            _tx: &mut sqlx::Transaction<'_, sqlx::any::Any>,
            _repo: asc_store::refs::RepoKey,
            pairs: &[(ChangesetId, ChangesetId)],
            kind: RewriteOp,
        ) -> Result<(), ObserverError> {
            self.0.lock().unwrap().push((pairs.to_vec(), kind));
            Ok(())
        }
    }

    /// Fails inside the transaction that moves the ref.
    struct Failing;

    #[async_trait]
    impl IngestObserver for Failing {
        async fn on_rewritten(
            &self,
            _tx: &mut sqlx::Transaction<'_, sqlx::any::Any>,
            _repo: asc_store::refs::RepoKey,
            _pairs: &[(ChangesetId, ChangesetId)],
            _kind: RewriteOp,
        ) -> Result<(), ObserverError> {
            Err(ObserverError::Failed("refused".into()))
        }
    }

    // ------------------------------------------------------------------ the styles

    #[tokio::test]
    async fn merge_makes_a_two_parent_changeset_with_the_merged_tree() {
        let f = fx().await;
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let theirs = make(
            &f,
            &[r],
            "theirs",
            &[("g.txt", Some("G\n")), ("h.txt", Some("h\n"))],
        )
        .await;
        let got =
            f.m.merge_inner(&ctx(), req(ours, theirs, wire::MergeStyle::Merge))
                .await
                .unwrap();
        let made = result_of(&got);
        assert!(got.conflicts.is_empty() && got.rewritten.is_empty() && !got.is_empty);
        let c = cs(&f, made).await;
        assert_eq!(c.parents, [ours, theirs]);
        assert_eq!(c.committer.name(), b"Al");
        assert_eq!(
            read(&f, made, "f.txt").await,
            Some(with_line(1, "ONE").into_bytes())
        );
        assert_eq!(read(&f, made, "g.txt").await, Some(b"G\n".to_vec()));
        assert_eq!(read(&f, made, "h.txt").await, Some(b"h\n".to_vec()));
        // Changes are recorded against the first parent only.
        let paths: Vec<_> = c
            .changes
            .as_slice()
            .iter()
            .map(|(p, _)| p.to_string())
            .collect();
        assert_eq!(paths, ["g.txt", "h.txt"]);
        // Pipeline step 4 only: the closure is ingested, no ref, log or outbox row.
        assert_eq!(graph_rows(&f).await, 4);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM refs").await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM outbox").await, 0);
        // A message in the request wins over the default.
        let mut named = req(ours, theirs, wire::MergeStyle::Merge);
        named.message = Bytes::from_static(b"Merge PR 7\n");
        let again = f.m.merge_inner(&ctx(), named).await.unwrap();
        assert_eq!(cs(&f, result_of(&again)).await.message, b"Merge PR 7\n");
    }

    #[tokio::test]
    async fn squash_makes_one_changeset_on_base_and_maps_every_source_to_it() {
        let f = fx().await;
        let spy = Arc::new(Spy::default());
        f.pipeline.set_observer(spy.clone()).ok().unwrap();
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let t2 = make(&f, &[t1], "t2", &[("h.txt", Some("h\n"))]).await;
        let mut q = req(ours, t2, wire::MergeStyle::Squash);
        q.author = None;
        let got = f.m.merge_inner(&ctx(), q).await.unwrap();
        let made = result_of(&got);
        let c = cs(&f, made).await;
        assert_eq!(c.parents, [ours]);
        // By default the head's author and message.
        assert_eq!(c.author.name(), b"A");
        assert_eq!(c.committer.name(), b"Al");
        assert_eq!(c.message, b"t2");
        for (p, want) in [("g.txt", "G\n"), ("h.txt", "h\n")] {
            assert_eq!(read(&f, made, p).await, Some(want.as_bytes().to_vec()));
        }
        let mut pairs = pairs_of(&got);
        pairs.sort();
        let mut want = vec![(t1, made), (t2, made)];
        want.sort();
        assert_eq!(pairs, want);
        // The observer heard the same pairs, once.
        let heard = spy.0.lock().unwrap();
        assert_eq!(heard.len(), 1);
        assert_eq!(heard[0].1, RewriteOp::Squash);
        let mut heard_pairs = heard[0].0.clone();
        heard_pairs.sort();
        assert_eq!(heard_pairs, want);
    }

    #[tokio::test]
    async fn rebase_rewrites_each_changeset_onto_base() {
        let f = fx().await;
        let spy = Arc::new(Spy::default());
        f.pipeline.set_observer(spy.clone()).ok().unwrap();
        let r = root(&f).await;
        let main = make(&f, &[r], "main", &[("m.txt", Some("m\n"))]).await;
        let f1 = make(&f, &[r], "f1", &[("x.txt", Some("x\n"))]).await;
        let f2 = make(&f, &[f1], "f2", &[("y.txt", Some("y\n"))]).await;
        let got =
            f.m.merge_inner(&ctx(), req(main, f2, wire::MergeStyle::Rebase))
                .await
                .unwrap();
        let tip = result_of(&got);
        let pairs = pairs_of(&got);
        assert_eq!(pairs.len(), 2);
        assert_eq!((pairs[0].0, pairs[1].0), (f1, f2));
        assert_eq!(pairs[1].1, tip);
        let (n1, n2) = (cs(&f, pairs[0].1).await, cs(&f, tip).await);
        assert_eq!(n1.parents, [main]);
        assert_eq!(n2.parents, [pairs[0].1]);
        assert_eq!(
            (n1.message.as_slice(), n2.message.as_slice()),
            (&b"f1"[..], &b"f2"[..])
        );
        for p in ["m.txt", "x.txt", "y.txt", "g.txt"] {
            assert!(read(&f, tip, p).await.is_some(), "{p}");
        }
        // The rewritten changesets are in the graph with their ancestors.
        assert_eq!(graph_rows(&f).await, 4);
        assert_eq!(
            spy.0.lock().unwrap().as_slice(),
            [(pairs, RewriteOp::Rebase)]
        );
    }

    #[tokio::test]
    async fn rebase_merges_the_files_both_sides_changed() {
        let f = fx().await;
        let r = root(&f).await;
        let main = make(&f, &[r], "main", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let f1 = make(&f, &[r], "f1", &[("f.txt", Some(&with_line(10, "TEN")))]).await;
        let got =
            f.m.merge_inner(&ctx(), req(main, f1, wire::MergeStyle::Rebase))
                .await
                .unwrap();
        let tip = result_of(&got);
        let both = with_line(1, "ONE").replace("10\n", "TEN\n");
        assert_eq!(read(&f, tip, "f.txt").await, Some(both.into_bytes()));
    }

    #[tokio::test]
    async fn rebase_conflicts_are_an_answer_that_makes_nothing_visible() {
        let f = fx().await;
        let r = root(&f).await;
        let main = make(&f, &[r], "main", &[("f.txt", Some(&with_line(1, "A")))]).await;
        let f1 = make(&f, &[r], "f1", &[("f.txt", Some(&with_line(1, "B")))]).await;
        let mut q = req(main, f1, wire::MergeStyle::Rebase);
        q.update_ref = "refs/heads/master".into();
        let got = f.m.merge_inner(&ctx(), q).await.unwrap();
        assert!(got.result.is_empty() && got.rewritten.is_empty());
        assert_eq!(got.conflicts, [Bytes::from_static(b"f.txt")]);
        assert_eq!(got.conflict_details[0].changeset.as_ref(), f1.as_bytes());
        assert_eq!(graph_rows(&f).await, 0);
        assert_eq!(ref_target(&f, "refs/heads/master").await, None);
    }

    #[tokio::test]
    async fn a_rebase_needs_linear_history() {
        let f = fx().await;
        let r = root(&f).await;
        let main = make(&f, &[r], "main", &[("m.txt", Some("m\n"))]).await;
        let a = make(&f, &[r], "a", &[("a.txt", Some("a\n"))]).await;
        let b = make(&f, &[r], "b", &[("b.txt", Some("b\n"))]).await;
        let m = make(&f, &[a, b], "merge", &[]).await;
        for style in [wire::MergeStyle::Rebase, wire::MergeStyle::RebaseMerge] {
            let e =
                f.m.merge_inner(&ctx(), req(main, m, style))
                    .await
                    .unwrap_err();
            assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{style:?}");
        }
        assert_eq!(graph_rows(&f).await, 0);
    }

    #[tokio::test]
    async fn rebase_merge_rebases_then_merges_the_new_tip() {
        let f = fx().await;
        let spy = Arc::new(Spy::default());
        f.pipeline.set_observer(spy.clone()).ok().unwrap();
        let r = root(&f).await;
        let main = make(&f, &[r], "main", &[("m.txt", Some("m\n"))]).await;
        let f1 = make(&f, &[r], "f1", &[("x.txt", Some("x\n"))]).await;
        let f2 = make(&f, &[f1], "f2", &[("y.txt", Some("y\n"))]).await;
        let got =
            f.m.merge_inner(&ctx(), req(main, f2, wire::MergeStyle::RebaseMerge))
                .await
                .unwrap();
        let made = result_of(&got);
        let pairs = pairs_of(&got);
        assert_eq!(pairs.len(), 2);
        let tip = pairs[1].1;
        assert_ne!(tip, made);
        let c = cs(&f, made).await;
        assert_eq!(c.parents, [main, tip]);
        assert_eq!(c.root, cs(&f, tip).await.root);
        assert_eq!(cs(&f, tip).await.parents, [pairs[0].1]);
        assert_eq!(
            spy.0.lock().unwrap().as_slice(),
            [(pairs, RewriteOp::Rebase)]
        );
    }

    #[tokio::test]
    async fn fast_forward_only_answers_with_head_and_refuses_anything_else() {
        let f = fx().await;
        let r = root(&f).await;
        let f1 = make(&f, &[r], "f1", &[("x.txt", Some("x\n"))]).await;
        let f2 = make(&f, &[f1], "f2", &[("y.txt", Some("y\n"))]).await;
        let mut q = req(r, f2, wire::MergeStyle::FastForwardOnly);
        q.update_ref = "refs/heads/master".into();
        let got = f.m.merge_inner(&ctx(), q).await.unwrap();
        assert_eq!(result_of(&got), f2);
        assert!(got.rewritten.is_empty());
        assert_eq!(
            ref_target(&f, "refs/heads/master").await,
            Some(RefTarget::Changeset(f2))
        );
        // No new changeset: the closure of f2 and nothing else.
        assert_eq!(graph_rows(&f).await, 3);

        let main = make(&f, &[r], "main", &[("m.txt", Some("m\n"))]).await;
        for dry_run in [false, true] {
            let mut q = req(main, f2, wire::MergeStyle::FastForwardOnly);
            q.dry_run = dry_run;
            let e = f.m.merge_inner(&ctx(), q).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::FailedPrecondition);
        }
    }

    #[tokio::test]
    async fn nothing_to_merge_is_reported_as_empty() {
        let f = fx().await;
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        // Head already in base, and head equal to base.
        for (base, head) in [(ours, r), (ours, ours)] {
            for style in [
                wire::MergeStyle::Merge,
                wire::MergeStyle::Squash,
                wire::MergeStyle::Rebase,
                wire::MergeStyle::FastForwardOnly,
            ] {
                let got =
                    f.m.merge_inner(&ctx(), req(base, head, style))
                        .await
                        .unwrap();
                assert!(got.is_empty && got.result.is_empty(), "{style:?}");
            }
        }
        // A squash whose tree equals base's changes nothing.
        let same = make(&f, &[r], "same", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        assert_ne!(same, ours);
        let mut q = req(ours, same, wire::MergeStyle::Squash);
        q.update_ref = "refs/heads/master".into();
        let got = f.m.merge_inner(&ctx(), q).await.unwrap();
        assert!(got.is_empty && got.result.is_empty());
        assert_eq!(ref_target(&f, "refs/heads/master").await, None);
        assert_eq!(graph_rows(&f).await, 0);
    }

    // ------------------------------------------------------------------ dry run

    /// A base, ours and theirs that conflict in three ways and merge cleanly in three others.
    async fn conflicting(f: &Fx) -> (ChangesetId, ChangesetId, ChangesetId) {
        let r = make(
            f,
            &[],
            "base",
            &[
                ("a.txt", Some(LINES)),
                ("b.txt", Some(LINES)),
                ("c.txt", Some("keep\n")),
                ("d.txt", Some(LINES)),
                ("dir/e.txt", Some(LINES)),
            ],
        )
        .await;
        let ours = make(
            f,
            &[r],
            "ours",
            &[
                ("a.txt", Some(&with_line(1, "ours"))),
                ("b.txt", Some(&with_line(10, "ours"))),
                ("d.txt", Some(&with_line(5, "ours"))),
                ("both.txt", Some("ours\n")),
                ("dir/e.txt", Some(&with_line(10, "ours"))),
            ],
        )
        .await;
        let theirs = make(
            f,
            &[r],
            "theirs",
            &[
                ("a.txt", Some(&with_line(1, "theirs"))),
                ("b.txt", Some(&with_line(1, "theirs"))),
                ("c.txt", None),
                ("d.txt", None),
                ("both.txt", Some("theirs\n")),
            ],
        )
        .await;
        (r, ours, theirs)
    }

    fn git_cmd(dir: &Path, args: &[&str]) -> std::process::Command {
        let real = RealGit::resolve(None, None).expect("git is installed");
        let mut cmd = real.command().expect("a git command");
        cmd.arg("-C")
            .arg(dir)
            .args(["-c", "core.quotepath=off", "-c", "core.autocrlf=false"])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x");
        cmd
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = git_cmd(dir, args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("git runs");
        // `merge-tree` exits 1 when the merge conflicts.
        assert!(
            out.status.success() || args.contains(&"merge-tree"),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf-8")
    }

    /// Writes the files of a changeset into a git work tree, removing what it lacks.
    async fn checkout(f: &Fx, dir: &Path, c: ChangesetId) {
        let engine = f.entry.backend.engine();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
        }
        let root = cs(f, c).await.root;
        let mut stack = vec![(root, String::new())];
        while let Some((tree, prefix)) = stack.pop() {
            for e in engine.get_tree(tree).await.unwrap().entries() {
                let name = format!("{prefix}{}", String::from_utf8_lossy(e.name.as_bytes()));
                if e.kind == Kind::Dir {
                    stack.push((TreeId::from_bytes(e.id), format!("{name}/")));
                } else {
                    let data = engine
                        .read_file(FileId::from_bytes(e.id), e.size)
                        .await
                        .unwrap();
                    let at = dir.join(&name);
                    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
                    std::fs::write(at, data).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn a_dry_run_reports_the_conflicts_git_merge_tree_reports() {
        if RealGit::resolve(None, None).is_err() {
            eprintln!("git is not installed; skipping");
            return;
        }
        let f = fx().await;
        let (r, ours, theirs) = conflicting(&f).await;

        // git's answer for the same three trees.
        let dir = tempfile::tempdir().unwrap();
        git_out(dir.path(), &["init", "-q", "-b", "base"]);
        for (branch, c) in [("base", r), ("ours", ours), ("theirs", theirs)] {
            if branch != "base" {
                git_out(dir.path(), &["checkout", "-q", "-B", branch, "base"]);
            }
            checkout(&f, dir.path(), c).await;
            git_out(dir.path(), &["add", "-A"]);
            git_out(dir.path(), &["commit", "-q", "--allow-empty", "-m", branch]);
        }
        let merged = git_out(
            dir.path(),
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "ours",
                "theirs",
            ],
        );
        let mut git_paths: Vec<String> = merged
            .lines()
            .skip(1)
            .take_while(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        git_paths.sort();
        assert_eq!(
            git_paths,
            ["a.txt", "both.txt", "d.txt"],
            "the oracle: {merged}"
        );

        for style in [wire::MergeStyle::Merge, wire::MergeStyle::Squash] {
            let mut q = req(ours, theirs, style);
            q.dry_run = true;
            q.update_ref = "refs/heads/master".into();
            let got = f.m.merge_inner(&ctx(), q).await.unwrap();
            let mut paths: Vec<String> = got
                .conflicts
                .iter()
                .map(|p| String::from_utf8(p.to_vec()).unwrap())
                .collect();
            paths.sort();
            assert_eq!(paths, git_paths, "{style:?}");
            let kinds: Vec<_> = got
                .conflict_details
                .iter()
                .map(|c| (String::from_utf8_lossy(&c.path).into_owned(), c.kind()))
                .collect();
            assert_eq!(
                kinds,
                [
                    ("a.txt".to_owned(), wire::ConflictKind::Content),
                    ("both.txt".to_owned(), wire::ConflictKind::AddAdd),
                    ("d.txt".to_owned(), wire::ConflictKind::ModifyDelete),
                ],
                "{style:?}"
            );
            assert_eq!(got.conflict_details[0].marked_id.len(), 32);
            assert!(got.result.is_empty());
        }
        // Nothing was made visible: no changeset, no ref, no log.
        assert_eq!(graph_rows(&f).await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM refs").await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 0);
    }

    #[tokio::test]
    async fn a_clean_dry_run_makes_nothing() {
        let f = fx().await;
        let spy = Arc::new(Spy::default());
        f.pipeline.set_observer(spy.clone()).ok().unwrap();
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let t2 = make(&f, &[t1], "t2", &[("h.txt", Some("h\n"))]).await;
        for style in [
            wire::MergeStyle::Merge,
            wire::MergeStyle::Squash,
            wire::MergeStyle::Rebase,
            wire::MergeStyle::RebaseMerge,
        ] {
            let mut q = req(ours, t2, style);
            q.dry_run = true;
            q.update_ref = "refs/heads/master".into();
            // A dry run needs no signatures.
            q.author = None;
            q.committer = None;
            let got = f.m.merge_inner(&ctx(), q).await.unwrap();
            assert_eq!(got, wire::MergeResponse::default(), "{style:?}");
        }
        assert_eq!(graph_rows(&f).await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM refs").await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM op_results").await, 0);
        assert!(spy.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn real_merge_conflicts_make_no_changeset_and_move_no_ref() {
        let f = fx().await;
        let (_, ours, theirs) = conflicting(&f).await;
        for style in [wire::MergeStyle::Merge, wire::MergeStyle::Squash] {
            let mut q = req(ours, theirs, style);
            q.update_ref = "refs/heads/master".into();
            let got = f.m.merge_inner(&ctx(), q).await.unwrap();
            assert_eq!(got.conflicts.len(), 3, "{style:?}");
            assert!(got.result.is_empty() && !got.is_empty);
            assert_eq!(
                got.conflict_details[0].changeset.as_ref(),
                theirs.as_bytes()
            );
        }
        assert_eq!(graph_rows(&f).await, 0);
        assert_eq!(ref_target(&f, "refs/heads/master").await, None);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 0);
    }

    // ------------------------------------------------------------------ protected globs

    #[tokio::test]
    async fn protected_paths_are_reported_by_a_dry_run_and_stop_a_real_merge() {
        let f = fx().await;
        let r = make(
            &f,
            &[],
            "root",
            &[
                ("f.txt", Some(LINES)),
                ("Cargo.lock", Some("lock\n")),
                ("gone.lock", Some("lock\n")),
            ],
        )
        .await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let theirs = make(
            &f,
            &[r],
            "theirs",
            &[
                ("Cargo.lock", Some("new lock\n")),
                ("gone.lock", None),
                ("secrets/key", Some("k\n")),
                ("src/x.rs", Some("x\n")),
            ],
        )
        .await;
        let globs = vec!["*.lock".to_owned(), "secrets/**".to_owned()];
        let want = vec![
            Bytes::from_static(b"Cargo.lock"),
            Bytes::from_static(b"gone.lock"),
            Bytes::from_static(b"secrets/key"),
        ];

        let mut dry = req(ours, theirs, wire::MergeStyle::Merge);
        dry.protected_globs = globs.clone();
        dry.dry_run = true;
        let got = f.m.merge_inner(&ctx(), dry).await.unwrap();
        assert_eq!(got.changed_protected, want);
        assert!(got.conflicts.is_empty() && got.result.is_empty());

        for style in [
            wire::MergeStyle::Merge,
            wire::MergeStyle::Squash,
            wire::MergeStyle::Rebase,
            wire::MergeStyle::RebaseMerge,
        ] {
            let mut q = req(ours, theirs, style);
            q.protected_globs = globs.clone();
            q.update_ref = "refs/heads/master".into();
            let got = f.m.merge_inner(&ctx(), q).await.unwrap();
            assert_eq!(got.changed_protected, want, "{style:?}");
            assert!(got.result.is_empty(), "{style:?}");
        }
        assert_eq!(graph_rows(&f).await, 0);
        assert_eq!(ref_target(&f, "refs/heads/master").await, None);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 0);

        // Globs that match nothing the head changed let the merge through.
        let mut q = req(ours, theirs, wire::MergeStyle::Merge);
        q.protected_globs = vec!["docs/**".into()];
        let got = f.m.merge_inner(&ctx(), q).await.unwrap();
        assert!(got.changed_protected.is_empty());
        result_of(&got);
    }

    #[test]
    fn a_name_no_glob_can_read_counts_as_protected() {
        let globs = NoMergeGlobs::new(&["docs/**"]).unwrap();
        assert!(is_protected(&globs, &RepoPath::new("docs/a.md").unwrap()));
        assert!(!is_protected(&globs, &RepoPath::new("src/a.rs").unwrap()));
        if let Ok(odd) = RepoPath::new(&b"src/\xff"[..]) {
            assert!(is_protected(&globs, &odd));
        }
    }

    // ------------------------------------------------------------------ refs, observer, replay

    /// In a branchless repository the caller's `refs/heads/feat` is stored as
    /// `refs/users/<owner>/heads/feat`; the merge's compare-and-set reads that stored name.
    #[tokio::test]
    async fn a_second_merge_onto_an_existing_user_branch_in_a_branchless_repo_advances_it() {
        let f = fx().await;
        make_branchless(&f).await;
        let (alice, bob) = ("5:alice", "6:bob");
        let (a_ref, b_ref) = ("refs/users/5/heads/feat", "refs/users/6/heads/feat");
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let t2 = make(&f, &[r], "t2", &[("h.txt", Some("h\n"))]).await;
        let mut q = req(ours, t1, wire::MergeStyle::Squash);
        q.update_ref = "refs/heads/feat".into();
        let a1 = result_of(&f.m.merge_inner(&ctx_as(alice), q.clone()).await.unwrap());
        let b1 = result_of(&f.m.merge_inner(&ctx_as(bob), q).await.unwrap());
        assert_eq!(ref_target(&f, a_ref).await, Some(RefTarget::Changeset(a1)));
        assert_eq!(ref_target(&f, b_ref).await, Some(RefTarget::Changeset(b1)));
        assert_eq!(ref_target(&f, "refs/heads/feat").await, None);

        // The ref exists, so the expectation is its value and the merge onto it advances it.
        let mut next = req(a1, t2, wire::MergeStyle::Squash);
        next.update_ref = "refs/heads/feat".into();
        let a2 = result_of(&f.m.merge_inner(&ctx_as(alice), next).await.unwrap());
        assert_eq!(ref_target(&f, a_ref).await, Some(RefTarget::Changeset(a2)));
        // Bob's same-named branch is untouched, and a stale base is still refused for him.
        assert_eq!(ref_target(&f, b_ref).await, Some(RefTarget::Changeset(b1)));
        let mut stale = req(ours, t2, wire::MergeStyle::Squash);
        stale.update_ref = "refs/heads/feat".into();
        let e = f.m.merge_inner(&ctx_as(bob), stale).await.unwrap_err();
        assert!(
            matches!(code(&e).as_str(), "REF_CONFLICT" | "NOT_FAST_FORWARD"),
            "{e:?}"
        );
        assert_eq!(ref_target(&f, b_ref).await, Some(RefTarget::Changeset(b1)));
        assert_eq!(ref_target(&f, a_ref).await, Some(RefTarget::Changeset(a2)));
    }

    #[tokio::test]
    async fn with_a_ref_the_pipeline_moves_it_and_tells_the_observer_in_that_transaction() {
        let f = fx().await;
        let spy = Arc::new(Spy::default());
        f.pipeline.set_observer(spy.clone()).ok().unwrap();
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let mut q = req(ours, t1, wire::MergeStyle::Squash);
        q.update_ref = "refs/heads/master".into();
        let got = f.m.merge_inner(&ctx(), q).await.unwrap();
        let made = result_of(&got);
        assert_eq!(
            ref_target(&f, "refs/heads/master").await,
            Some(RefTarget::Changeset(made))
        );
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 1);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM outbox").await, 1);
        assert_eq!(pairs_of(&got), [(t1, made)]);
        assert_eq!(
            spy.0.lock().unwrap().as_slice(),
            [(vec![(t1, made)], RewriteOp::Squash)]
        );

        // The ref now names `made`; merging onto the stale `ours` is refused and changes nothing.
        let t2 = make(&f, &[r], "t2", &[("h.txt", Some("h\n"))]).await;
        let mut stale = req(ours, t2, wire::MergeStyle::Squash);
        stale.update_ref = "refs/heads/master".into();
        let e = f.m.merge_inner(&ctx(), stale).await.unwrap_err();
        assert!(
            matches!(code(&e).as_str(), "REF_CONFLICT" | "NOT_FAST_FORWARD"),
            "{e:?}"
        );
        assert_eq!(
            ref_target(&f, "refs/heads/master").await,
            Some(RefTarget::Changeset(made))
        );
        assert_eq!(spy.0.lock().unwrap().len(), 1);

        // Naming the ref as base merges onto its current value.
        let mut next = req(made, t2, wire::MergeStyle::Squash);
        next.base = "refs/heads/master".into();
        next.update_ref = "refs/heads/master".into();
        let got = f.m.merge_inner(&ctx(), next).await.unwrap();
        assert_eq!(
            ref_target(&f, "refs/heads/master").await,
            Some(RefTarget::Changeset(result_of(&got)))
        );
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 2);
    }

    #[tokio::test]
    async fn a_failing_observer_rolls_the_ref_move_back() {
        let f = fx().await;
        f.pipeline.set_observer(Arc::new(Failing)).ok().unwrap();
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let mut q = req(ours, t1, wire::MergeStyle::Squash);
        q.update_ref = "refs/heads/master".into();
        let e = f.m.merge_inner(&ctx(), q).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Internal);
        assert!(!e.message().contains("refused"), "{e:?}");
        assert_eq!(ref_target(&f, "refs/heads/master").await, None);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM op_results").await, 0);
    }

    #[tokio::test]
    async fn without_a_ref_a_failing_observer_is_an_error() {
        let f = fx().await;
        f.pipeline.set_observer(Arc::new(Failing)).ok().unwrap();
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let e =
            f.m.merge_inner(&ctx(), req(ours, t1, wire::MergeStyle::Squash))
                .await
                .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Internal);
    }

    #[tokio::test]
    async fn a_retry_with_the_same_op_id_answers_the_same_and_moves_nothing_twice() {
        let f = fx().await;
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let mut q = req(ours, t1, wire::MergeStyle::Squash);
        q.update_ref = "refs/heads/master".into();
        let c = ctx();
        let first = f.m.merge_inner(&c, q.clone()).await.unwrap();
        let second = f.m.merge_inner(&c, q).await.unwrap();
        assert_eq!(result_of(&first), result_of(&second));
        assert_eq!(pairs_of(&first), pairs_of(&second));
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 1);
        // The same op id for another ref is refused.
        let mut other = req(ours, t1, wire::MergeStyle::Squash);
        other.update_ref = "refs/heads/other".into();
        let e = f.m.merge_inner(&c, other).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    // ------------------------------------------------------------------ text budget

    #[tokio::test]
    async fn content_merges_reserve_text_memory_first() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct Count(AtomicU64);
        #[async_trait]
        impl crate::TextBudget for Count {
            async fn acquire(
                &self,
                bytes: u64,
            ) -> Result<crate::TextGuard, crate::TextBudgetExhausted> {
                self.0.fetch_add(bytes, Ordering::SeqCst);
                Ok(crate::TextGuard::unlimited())
            }
        }
        struct Never;
        #[async_trait]
        impl crate::TextBudget for Never {
            async fn acquire(
                &self,
                _: u64,
            ) -> Result<crate::TextGuard, crate::TextBudgetExhausted> {
                Err(crate::TextBudgetExhausted)
            }
        }
        let styles = [
            wire::MergeStyle::Merge,
            wire::MergeStyle::Squash,
            wire::MergeStyle::Rebase,
            wire::MergeStyle::RebaseMerge,
        ];
        let counted = Arc::new(Count(AtomicU64::new(0)));
        let f = fx().await;
        f.hooks.text_budget.set(counted.clone()).ok().unwrap();
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        for style in styles {
            let before = counted.0.load(Ordering::SeqCst);
            f.m.merge_inner(&ctx(), req(ours, t1, style)).await.unwrap();
            assert_eq!(
                counted.0.load(Ordering::SeqCst) - before,
                MERGE_TEXT_ESTIMATE,
                "{style:?}"
            );
        }
        // A fast-forward reads no file.
        let before = counted.0.load(Ordering::SeqCst);
        f.m.merge_inner(&ctx(), req(r, t1, wire::MergeStyle::FastForwardOnly))
            .await
            .unwrap();
        assert_eq!(counted.0.load(Ordering::SeqCst), before);

        let g = fx().await;
        g.hooks.text_budget.set(Arc::new(Never)).ok().unwrap();
        let r = root(&g).await;
        let ours = make(&g, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&g, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        for style in styles {
            let e =
                g.m.merge_inner(&ctx(), req(ours, t1, style))
                    .await
                    .unwrap_err();
            assert_eq!(e.code(), tonic::Code::ResourceExhausted, "{style:?}");
        }
        assert_eq!(graph_rows(&g).await, 0);
    }

    // ------------------------------------------------------------------ bad requests

    #[tokio::test]
    async fn malformed_requests_are_refused_before_anything_is_stored() {
        let f = fx().await;
        let r = root(&f).await;
        let ours = make(&f, &[r], "ours", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let edit = |g: fn(&mut wire::MergeRequest)| {
            let mut q = req(ours, t1, wire::MergeStyle::Merge);
            g(&mut q);
            q
        };
        let cases: Vec<(&str, wire::MergeRequest, &str)> = vec![
            ("style", edit(|q| q.style = 99), "InvalidArgument"),
            ("head", edit(|q| q.head.clear()), "InvalidArgument"),
            (
                "long head",
                edit(|q| q.head = "a".repeat(MAX_STRING + 1)),
                "InvalidArgument",
            ),
            (
                "long message",
                edit(|q| q.message = Bytes::from(vec![b'm'; MAX_MESSAGE_BYTES + 1])),
                "InvalidArgument",
            ),
            ("sign", edit(|q| q.sign = true), "Unimplemented"),
            (
                "ref name",
                edit(|q| q.update_ref = "refs/heads/..".into()),
                "InvalidArgument",
            ),
            (
                "long ref",
                edit(|q| q.update_ref = "r".repeat(MAX_STRING + 1)),
                "InvalidArgument",
            ),
            (
                "many globs",
                edit(|q| q.protected_globs = vec!["a".into(); MAX_PROTECTED_GLOBS + 1]),
                "InvalidArgument",
            ),
            (
                "bad glob",
                edit(|q| q.protected_globs = vec!["[".into()]),
                "InvalidArgument",
            ),
            (
                "empty glob",
                edit(|q| q.protected_globs = vec![String::new()]),
                "InvalidArgument",
            ),
            (
                "long glob",
                edit(|q| q.protected_globs = vec!["a".repeat(MAX_STRING + 1)]),
                "InvalidArgument",
            ),
            (
                "no signature",
                edit(|q| {
                    q.author = None;
                    q.committer = None;
                }),
                "InvalidArgument",
            ),
            (
                "unknown head",
                edit(|q| q.head = "refs/heads/nope".into()),
                "NotFound",
            ),
            (
                "bad head repo",
                edit(|q| {
                    q.head_repo = Some(wire::RepoRef {
                        name: "no-slash".into(),
                        id_mode: 0,
                    });
                }),
                "InvalidArgument",
            ),
        ];
        for (what, q, want) in cases {
            let e = f.m.merge_inner(&ctx(), q).await.unwrap_err();
            assert_eq!(format!("{:?}", e.code()), want, "{what}: {e:?}");
        }
        assert_eq!(graph_rows(&f).await, 0);
        assert_eq!(count(&f, "SELECT COUNT(*) FROM ref_log").await, 0);
    }

    #[tokio::test]
    async fn a_mirror_takes_no_merge() {
        let f = fx().await;
        let r = root(&f).await;
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        make_mirror(&f).await;
        let e =
            f.m.merge_inner(&ctx(), req(r, t1, wire::MergeStyle::Merge))
                .await
                .unwrap_err();
        assert_eq!(code(&e), "MIRROR_MODE");
    }

    // ------------------------------------------------------------------ other repositories

    #[tokio::test]
    async fn a_head_in_another_repository_goes_through_copy_closure_first() {
        use crate::ingest::{Ingestor, NoObserver};

        let named = |n: &str| {
            Some(wire::RepoRef {
                name: n.into(),
                id_mode: IdMode::Asc as i32,
            })
        };
        let f = fx().await;
        let other = f
            .registry
            .create(&RepoName::parse("o/other").unwrap(), &Default::default())
            .await
            .unwrap();
        let ingest = |e: &Arc<crate::registry::RepoEntry>, ids: &[ChangesetId]| {
            let e = e.clone();
            let targets: Vec<RefTarget> = ids.iter().map(|&c| RefTarget::Changeset(c)).collect();
            let registry = f.registry.clone();
            async move {
                Ingestor::new(registry.pool(), registry.refs(), &e, true)
                    .ingest(&targets, &NoObserver)
                    .await
                    .unwrap();
            }
        };
        let r = root(&f).await;
        ingest(&f.entry, &[r]).await;

        // A head that the other repository never ingested is not copied from it, and nothing
        // is written here.
        let rows = graph_rows(&f).await;
        let loose = crate::ingest::testutil::commit(&other, &[], &[("z.txt", b"z\n")]).await;
        let mut q = req(r, loose, wire::MergeStyle::Merge);
        q.head_repo = Some(wire::RepoRef {
            name: "o/other".into(),
            id_mode: IdMode::Asc as i32,
        });
        let e = f.m.merge_inner(&ctx(), q.clone()).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound, "{e:?}");
        assert_eq!(graph_rows(&f).await, rows);

        // A head whose history shares nothing here is copied, then refused by the merge itself.
        ingest(&other, &[loose]).await;
        let e = f.m.merge_inner(&ctx(), q.clone()).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{e:?}");
        assert!(graph_rows(&f).await > rows, "the closure was copied first");

        // A head built on a changeset both repositories have: the closure is copied here and
        // the merge uses the copy.
        let other_ctx = CallCtx {
            repo: RepoName::parse("o/other").unwrap(),
            ..ctx()
        };
        crate::admin::copy_closure(
            &f.registry,
            &f.pipeline,
            &other_ctx,
            wire::CopyClosureRequest {
                from: named("o/r"),
                to: named("o/other"),
                changesets: vec![Bytes::copy_from_slice(r.as_bytes())],
            },
        )
        .await
        .unwrap();
        let theirs = crate::ingest::testutil::commit(&other, &[r], &[("z.txt", b"z\n")]).await;
        ingest(&other, &[theirs]).await;
        assert!(
            f.entry
                .backend
                .engine()
                .get_changeset(theirs)
                .await
                .is_err(),
            "not here yet"
        );
        let mut q = req(r, theirs, wire::MergeStyle::FastForwardOnly);
        q.head_repo = Some(wire::RepoRef {
            name: "o/other".into(),
            id_mode: IdMode::Asc as i32,
        });
        assert_eq!(
            result_of(&f.m.merge_inner(&ctx(), q.clone()).await.unwrap()),
            theirs
        );
        assert_eq!(cs(&f, theirs).await.parents, vec![r]);

        // A head that does not exist there, and a repository that does not exist.
        let mut q2 = q.clone();
        q2.head = "refs/heads/nope".into();
        let e = f.m.merge_inner(&ctx(), q2).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);
        q.head_repo = Some(wire::RepoRef {
            name: "o/missing".into(),
            id_mode: 0,
        });
        let e = f.m.merge_inner(&ctx(), q).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::NotFound);

        // Naming the repository itself is the plain case.
        let t1 = make(&f, &[r], "t1", &[("g.txt", Some("G\n"))]).await;
        let mut same = req(r, t1, wire::MergeStyle::FastForwardOnly);
        same.head_repo = Some(wire::RepoRef {
            name: "o/r".into(),
            id_mode: 0,
        });
        assert_eq!(result_of(&f.m.merge_inner(&ctx(), same).await.unwrap()), t1);
    }

    #[tokio::test]
    async fn another_users_ref_is_not_a_merge_head() {
        let f = fx().await;
        let r = root(&f).await;
        let hidden = make(&f, &[r], "hidden", &[("g.txt", Some("secret\n"))]).await;
        let main = make(&f, &[r], "main", &[("f.txt", Some(&with_line(1, "ONE")))]).await;
        set_ref(&f, "refs/users/42/heads/x", hidden).await;
        // Spellings that still name a commit, then the ones that name a range or a path.
        for head in [
            "master..users/42/heads/x",
            "users/42/heads/x...master",
            "^users/42/heads/x",
            "refs/users/42/heads/x@{3}",
            "users/42/heads/x:g.txt",
        ] {
            let q = req(main, hidden, wire::MergeStyle::Merge);
            let q = wire::MergeRequest {
                head: head.into(),
                ..q
            };
            let err = f.m.merge_inner(&ctx_as("43:bob"), q).await.unwrap_err();
            assert_eq!(err.code(), tonic::Code::NotFound, "bob merged {head}");
        }
        for head in [
            "refs/users/42/heads/x",
            "users/42/heads/x",
            "users/42/heads/x@{0}",
            "users/42/heads/x~0",
            "refs/users/42/heads/x^{commit}",
        ] {
            let mut q = req(main, hidden, wire::MergeStyle::Merge);
            q.head = head.into();
            let err =
                f.m.merge_inner(&ctx_as("43:bob"), q.clone())
                    .await
                    .unwrap_err();
            assert_eq!(err.code(), tonic::Code::NotFound, "bob merged {head}");
            if head.contains("@{") && !head.starts_with("refs/") {
                continue; // `name@{n}` takes the full ref name, for everyone
            }
            let got = f.m.merge_inner(&ctx_as("42:alice"), q).await.unwrap();
            assert!(!got.result.is_empty(), "the owner merges {head}");
        }
    }
}
