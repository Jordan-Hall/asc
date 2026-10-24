//! `asc serve admin branchless enable <repo> [--dry-run]` (SPEC §21.6.2, PLAN B18): moves a
//! branch-mode repository to branchless mode.
//!
//! Every `refs/heads/*` branch is classified, in this order:
//!
//! | branch | action |
//! |---|---|
//! | matches `shared_refs` (default branch, `release/*`, `green/*`, land branches) | shared |
//! | head branch of an open PR | kept shared and listed in `repos.legacy_shared` (B26 converts it) |
//! | fully merged into the default branch | deleted (the old value stays in `ref_log`) |
//! | `agent/*`, `worktree-agent-*` | user ref of the pusher, 7-day TTL |
//! | anything else | user ref of the pusher, same name |
//!
//! The pusher comes from [`ForgeFacts::pusher`] (Forgejo `branch.pusher_id`), else the owner the
//! caller passes. A branch whose user ref cannot be created (the name is taken by a different
//! value, or the owner would pass [`MAX_USER_REFS`]) stays shared and is listed in
//! `legacy_shared`; nothing is ever dropped.
//!
//! Commits of converted branches that no merged history holds and that have no `changes` row get
//! a `ChangeMeta` (`op = Import`) from the B18a rule: jj header, Gerrit trailer, then the derived
//! id. The writes are idempotent, so a run that stopped half way is finished by running again.
//!
//! Ref moves run in batches of one SQL transaction each, with `ref_log.via = branchless-migrate`
//! and one `post_receive` outbox row per batch that carries the deleted branches, so Forgejo's
//! branch table follows. User refs queue no effects. `branchless` turns on in a last transaction,
//! after every batch. Applying a second time finds no non-shared branch left and changes nothing.
//!
//! The walk that decides "merged" is one generation-ordered paint-down over `cs_graph` from the
//! default branch tip and all candidate tips, bounded by [`MAX_VISITS`]; a run that would pass the
//! bound stops with `resource_exhausted` before it changes anything.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use asc_core::change::{ChangeMeta, RewriteOp};
use asc_core::change_migrate::migration_id;
use asc_core::codec::decode_envelope;
use asc_core::git::EXTRA_GIT_COMMIT;
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::object::Changeset;
use asc_core::path::RefName;
use asc_rpc::errors::RpcError;
use asc_rpc::server::{RepoName, wire};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefError, RefTxn, RefUpdate, Refs};
use prost::Message;
use serde::Serialize;
use sqlx::Row;
use tonic::Status;
use uuid::Uuid;

use crate::changes::index::{ChangeIndex, rows_of};
use crate::refpolicy::{self, MAX_USER_REFS, RepoPolicy};
use crate::registry::{Registry, RepoMode};
use crate::update::{EffectRef, OutboxEffects, info, internal, rpc, target_hex};

/// `ref_log.via` of every write of this command.
pub const VIA: &str = "branchless-migrate";
/// The actor recorded for the writes.
pub const ACTOR: &str = "branchless-migrate";
/// TTL of an `agent/*` or `worktree-agent-*` user ref.
pub const AGENT_TTL_SECS: i64 = 7 * 86_400;
/// Most branches one run handles.
pub const MAX_BRANCHES: usize = 100_000;
/// Most `cs_graph` nodes the merged-ness walk visits.
pub const MAX_VISITS: usize = 500_000;
/// Most unmerged commits that get a `ChangeMeta` in one run.
pub const MAX_UNMERGED: usize = 100_000;
/// Default and largest number of ref moves per SQL transaction.
pub const DEFAULT_BATCH: usize = 100;
pub const MAX_BATCH: usize = 1_000;
/// `ChangeMeta` writes per SQL transaction.
const META_BATCH: usize = 200;
const GRAPH_BATCH: usize = 100;
/// Longest `legacy_shared` the column holds.
const MAX_LEGACY: usize = MAX_BRANCHES;

const HEADS: &str = "refs/heads/";

/// What only the forge knows. Unreachable or unknown answers fall back to the repo owner and to
/// "no open PR" is never assumed: an error aborts the run before any change.
#[async_trait]
pub trait ForgeFacts: Send + Sync {
    /// Short names of the head branches of the open PRs.
    async fn open_pr_heads(&self) -> Result<HashSet<String>, Status>;
    /// The numeric id of the user who last pushed `branch` (Forgejo `branch.pusher_id`).
    async fn pusher(&self, branch: &str) -> Result<Option<i64>, Status>;
}

/// A forge that knows nothing: no open PRs, every branch falls back to the owner.
pub struct NoForge;

#[async_trait]
impl ForgeFacts for NoForge {
    async fn open_pr_heads(&self) -> Result<HashSet<String>, Status> {
        Ok(HashSet::new())
    }

    async fn pusher(&self, _branch: &str) -> Result<Option<i64>, Status> {
        Ok(None)
    }
}

/// How one run behaves.
#[derive(Clone, Debug)]
pub struct Options {
    pub dry_run: bool,
    /// Ref moves per transaction (`1..=MAX_BATCH`).
    pub batch: usize,
    /// Unix seconds, for the agent TTL.
    pub now: i64,
    /// The user a branch falls back to when the forge has no pusher (the repo owner). Positive.
    pub fallback_owner: i64,
}

impl Options {
    pub fn new(fallback_owner: i64, now: i64) -> Self {
        Self {
            dry_run: false,
            batch: DEFAULT_BATCH,
            now,
            fallback_owner,
        }
    }
}

/// What happens to one branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Stays a shared ref.
    Shared,
    /// Head of an open PR: stays shared, listed in `legacy_shared`.
    LegacyShared,
    /// Fully merged: deleted.
    DeleteMerged,
    /// Becomes the user ref of `owner`; `ttl` marks the 7-day agent TTL.
    UserRef { owner: i64, ttl: bool },
    /// Left shared and listed in `legacy_shared` because it could not be converted.
    Kept { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BranchRow {
    pub branch: String,
    #[serde(flatten)]
    pub action: Action,
}

/// The report of one run (`--json` prints it as is).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub repo: String,
    pub dry_run: bool,
    pub already_branchless: bool,
    pub branches: Vec<BranchRow>,
    pub shared: usize,
    pub legacy_shared: Vec<String>,
    pub deleted: usize,
    pub converted: usize,
    /// `ChangeMeta` objects written (or, in a dry run, that would be).
    pub change_metas: usize,
    /// Ref transactions committed.
    pub batches: usize,
}

impl Report {
    /// Whether applying changed (or would change) anything.
    pub fn is_noop(&self) -> bool {
        self.deleted == 0
            && self.converted == 0
            && self.change_metas == 0
            && self.already_branchless
            && self.batches == 0
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

fn invalid(m: impl Into<String>) -> Status {
    Status::invalid_argument(m.into())
}

/// Whether a branch is an agent branch (7-day TTL).
pub fn is_agent_branch(branch: &str) -> bool {
    branch.starts_with("agent/") || branch.starts_with("worktree-agent-")
}

/// Runs the migration of `repo`.
pub async fn enable(
    registry: &Arc<Registry>,
    repo: &RepoName,
    facts: &dyn ForgeFacts,
    opts: &Options,
) -> Result<Report, Status> {
    if opts.fallback_owner <= 0 {
        return Err(invalid("the fallback owner id must be positive"));
    }
    if opts.batch == 0 || opts.batch > MAX_BATCH {
        return Err(invalid("batch size out of range"));
    }
    let entry = registry.open(repo).await.map_err(|e| match e {
        crate::registry::RegistryError::NotFound(n) => {
            Status::not_found(format!("repository {n} does not exist"))
        }
        e => internal("registry", &e),
    })?;
    if entry.mode == RepoMode::Mirror {
        return Err(rpc(RpcError::MirrorMode(info(vec![repo.to_string()]))));
    }
    let pool = registry.pool();
    let state = sqlx::query(
        "SELECT CAST(CASE WHEN branchless THEN 1 ELSE 0 END AS BIGINT) AS bl, legacy_shared \
         FROM repos WHERE id = $1",
    )
    .bind(entry.key.0)
    .fetch_one(pool)
    .await
    .map_err(|e| internal("repos", &e))?;
    let already: i64 = state.try_get("bl").map_err(|e| internal("repos", &e))?;
    let old_legacy: Option<String> = state
        .try_get("legacy_shared")
        .map_err(|e| internal("repos", &e))?;
    let mut legacy: BTreeSet<String> = match old_legacy.as_deref() {
        Some(j) => serde_json::from_str(j).map_err(|e| internal("repos.legacy_shared", &e))?,
        None => BTreeSet::new(),
    };
    let policy = RepoPolicy::load(pool, entry.key.0).await?;

    // ---- the branches
    let refs = registry.refs();
    let listed = refs
        .list(entry.key, HEADS)
        .await
        .map_err(|e| internal("refs", &e))?;
    if listed.len() > MAX_BRANCHES {
        return Err(Status::resource_exhausted("too many branches"));
    }
    let open_prs = facts.open_pr_heads().await?;
    let default_ref = format!("{HEADS}{}", entry.default_branch);
    let master_tip = listed
        .iter()
        .find(|e| e.name.as_bytes() == default_ref.as_bytes())
        .and_then(|e| match e.target {
            RefTarget::Changeset(c) => Some(c),
            RefTarget::Tag(_) => None,
        });

    let mut report = Report {
        repo: repo.to_string(),
        dry_run: opts.dry_run,
        already_branchless: already != 0,
        ..Report::default()
    };
    // (branch, tip) of the candidates for deletion or conversion.
    let mut candidates: BTreeMap<String, ChangesetId> = BTreeMap::new();
    let mut rows: BTreeMap<String, Action> = BTreeMap::new();
    for e in &listed {
        let Ok(name) = std::str::from_utf8(e.name.as_bytes()) else {
            continue;
        };
        let Some(branch) = name.strip_prefix(HEADS) else {
            continue;
        };
        if policy.is_shared(name) {
            rows.insert(branch.to_owned(), Action::Shared);
        } else if open_prs.contains(branch) {
            rows.insert(branch.to_owned(), Action::LegacyShared);
        } else if let RefTarget::Changeset(c) = e.target {
            candidates.insert(branch.to_owned(), c);
        } else {
            rows.insert(
                branch.to_owned(),
                Action::Kept {
                    reason: "annotated tag target".into(),
                },
            );
        }
    }

    // ---- merged or not
    let tips: Vec<ChangesetId> = candidates
        .values()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let paint = paint_down(pool, entry.ns, master_tip, &tips).await?;

    // Existing user refs per owner, for the cap and for collisions.
    let mut held: HashMap<i64, usize> = HashMap::new();
    let mut plan: Vec<(String, ChangesetId, i64, bool)> = Vec::new(); // branch, tip, owner, ttl
    let mut deletes: Vec<(String, ChangesetId)> = Vec::new();
    for (branch, tip) in &candidates {
        if paint.merged.contains(tip) {
            rows.insert(branch.clone(), Action::DeleteMerged);
            deletes.push((branch.clone(), *tip));
            continue;
        }
        let owner = facts
            .pusher(branch)
            .await?
            .filter(|o| *o > 0)
            .unwrap_or(opts.fallback_owner);
        let stored = format!("{}heads/{branch}", refpolicy::user_prefix(owner));
        let name = RefName::new(stored.as_bytes()).map_err(|e| invalid(e.to_string()))?;
        if let Some(existing) = refs
            .get(entry.key, &name)
            .await
            .map_err(|e| internal("refs", &e))?
        {
            if existing.target == RefTarget::Changeset(*tip) {
                // A half-done earlier run: only the branch is left to delete.
                rows.insert(branch.clone(), Action::DeleteMerged);
                deletes.push((branch.clone(), *tip));
            } else {
                rows.insert(
                    branch.clone(),
                    Action::Kept {
                        reason: "the owner's user ref of this name holds another value".into(),
                    },
                );
            }
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = held.entry(owner) {
            let n = refs
                .list(entry.key, &refpolicy::user_prefix(owner))
                .await
                .map_err(|e| internal("refs", &e))?
                .len();
            slot.insert(n);
        }
        let n = held
            .get_mut(&owner)
            .ok_or_else(|| internal("plan", &"owner"))?;
        if i64::try_from(*n).unwrap_or(i64::MAX) >= MAX_USER_REFS {
            rows.insert(
                branch.clone(),
                Action::Kept {
                    reason: "the owner is at the user ref limit".into(),
                },
            );
            continue;
        }
        *n += 1;
        let ttl = is_agent_branch(branch);
        rows.insert(branch.clone(), Action::UserRef { owner, ttl });
        plan.push((branch.clone(), *tip, owner, ttl));
    }

    // ---- change ids for the commits the converted branches own
    let metas = unmerged_commits(&plan, &paint);
    let mut todo_metas: Vec<(ChangesetId, String)> = Vec::new();
    {
        let mut conn = pool.acquire().await.map_err(|e| internal("pool", &e))?;
        let ids: Vec<ChangesetId> = metas.iter().map(|(c, _)| *c).collect();
        let have: HashSet<ChangesetId> = rows_of(&mut conn, entry.ns, &ids)
            .await
            .map_err(|e| internal("changes", &e))?
            .into_iter()
            .map(|r| r.cs)
            .collect();
        for (c, scope) in metas {
            if !have.contains(&c) {
                todo_metas.push((c, scope));
            }
        }
    }

    // ---- the report
    for (branch, action) in &rows {
        match action {
            Action::Shared => report.shared += 1,
            Action::LegacyShared => {
                legacy.insert(branch.clone());
            }
            Action::Kept { .. } => {
                legacy.insert(branch.clone());
            }
            Action::DeleteMerged => report.deleted += 1,
            Action::UserRef { .. } => report.converted += 1,
        }
    }
    if legacy.len() > MAX_LEGACY {
        return Err(Status::resource_exhausted(
            "too many legacy_shared branches",
        ));
    }
    report.branches = rows
        .into_iter()
        .map(|(branch, action)| BranchRow { branch, action })
        .collect();
    report.legacy_shared = legacy.iter().cloned().collect();
    report.change_metas = todo_metas.len();
    if opts.dry_run {
        return Ok(report);
    }

    // ---- apply: metas first, then the ref moves, then the mode
    let index = ChangeIndex::new(registry.clone());
    for chunk in todo_metas.chunks(META_BATCH) {
        let mut built = Vec::with_capacity(chunk.len());
        for (cs, scope) in chunk {
            built.push(build_meta(&*entry.objects, *cs, scope.as_bytes()).await?);
        }
        let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
        registry.guard(&mut tx, &entry).await?;
        for m in &built {
            index
                .write_meta(&mut tx, entry.key, m)
                .await
                .map_err(|e| internal("changes", &e))?;
        }
        tx.commit().await.map_err(|e| internal("commit", &e))?;
    }

    enum Move<'a> {
        Delete(&'a str, ChangesetId),
        Convert(&'a str, ChangesetId, i64, bool),
    }
    let mut moves: Vec<Move<'_>> = deletes.iter().map(|(b, t)| Move::Delete(b, *t)).collect();
    moves.extend(
        plan.iter()
            .map(|(b, t, o, ttl)| Move::Convert(b, *t, *o, *ttl)),
    );
    for chunk in moves.chunks(opts.batch) {
        let mut updates = Vec::new();
        let mut results = Vec::new();
        let mut effects = Vec::new();
        for m in chunk {
            let (branch, tip) = match m {
                Move::Delete(b, t) | Move::Convert(b, t, _, _) => (*b, *t),
            };
            let old = RefTarget::Changeset(tip);
            let full = format!("{HEADS}{branch}");
            updates.push(RefUpdate {
                name: RefName::new(full.as_bytes()).map_err(|e| invalid(e.to_string()))?,
                expect: Expect::Value(old),
                new: None,
                expires_at: None,
            });
            results.push(wire::RefResult {
                name: full.clone(),
                old: Some(wire::RefTarget::from(&old)),
                new: None,
                user: false,
            });
            effects.push(EffectRef {
                name: full,
                old: Some(target_hex(&old)),
                new: None,
            });
            if let Move::Convert(_, _, owner, ttl) = m {
                let stored = format!("{}heads/{branch}", refpolicy::user_prefix(*owner));
                updates.push(RefUpdate {
                    name: RefName::new(stored.as_bytes()).map_err(|e| invalid(e.to_string()))?,
                    expect: Expect::Absent,
                    new: Some(old),
                    expires_at: ttl.then_some(opts.now.saturating_add(AGENT_TTL_SECS)),
                });
                results.push(wire::RefResult {
                    name: stored,
                    old: None,
                    new: Some(wire::RefTarget::from(&old)),
                    user: true,
                });
            }
        }
        let txn = RefTxn {
            updates,
            actor: Actor {
                id: ACTOR.into(),
                via: VIA.into(),
            },
            reason: VIA.into(),
            op_id: Uuid::new_v4(),
        };
        let response = wire::UpdateRefsResponse {
            results,
            rewritten: Vec::new(),
            message: String::new(),
        };
        let outbox = serde_json::to_vec(&OutboxEffects {
            effects: &["post_receive"],
            pusher: ACTOR,
            refs: effects,
        })
        .map_err(|e| internal("outbox", &e))?;
        let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
        registry.guard(&mut tx, &entry).await?;
        match refs.update_in_tx(&mut tx, entry.key, &txn).await {
            Ok(()) => {}
            Err(RefError::Conflict(c)) => {
                return Err(Status::aborted(format!(
                    "{} moved during the migration; run it again",
                    c.name
                )));
            }
            Err(e) => return Err(internal("refs", &e)),
        }
        let at = crate::outbox::now();
        sqlx::query("INSERT INTO op_results (op_id, repo_id, result, at) VALUES ($1, $2, $3, $4)")
            .bind(txn.op_id.as_bytes().to_vec())
            .bind(entry.key.0)
            .bind(response.encode_to_vec())
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(|e| internal("op_results", &e))?;
        sqlx::query(
            "INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(entry.key.0)
        .bind(txn.op_id.as_bytes().to_vec())
        .bind(outbox)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("outbox", &e))?;
        tx.commit().await.map_err(|e| internal("commit", &e))?;
        report.batches += 1;
    }

    // ---- the mode
    let new_legacy = (!legacy.is_empty())
        .then(|| serde_json::to_string(&legacy).map_err(|e| internal("legacy_shared", &e)))
        .transpose()?;
    if already == 0 || new_legacy != old_legacy {
        let mut tx = refs.begin().await.map_err(|e| internal("refs", &e))?;
        registry.guard(&mut tx, &entry).await?;
        sqlx::query("UPDATE repos SET branchless = TRUE, legacy_shared = $1 WHERE id = $2")
            .bind(new_legacy)
            .bind(entry.key.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| internal("repos", &e))?;
        tx.commit().await.map_err(|e| internal("commit", &e))?;
    }
    Ok(report)
}

/// The `ChangeMeta` of `cs` by the B18a rule.
async fn build_meta(
    objects: &dyn Objects,
    cs: ChangesetId,
    scope: &[u8],
) -> Result<ChangeMeta, Status> {
    let data = objects
        .get(&ObjKey::Changeset(cs))
        .await
        .map_err(|e| internal("objects", &e))?
        .ok_or_else(|| Status::data_loss("a branch commit is missing from the store"))?;
    let c: Changeset = decode_envelope(&data).map_err(|e| internal("changeset", &e))?;
    let raw = c.extra.get(&EXTRA_GIT_COMMIT.to_vec());
    let jj = raw.and_then(|r| jj_header(r));
    let gerrit = gerrit_trailer(&c.message);
    let id = migration_id(&cs, jj, gerrit, scope).ok_or_else(|| internal("change id", &"no id"))?;
    Ok(ChangeMeta::new(cs, id, Vec::new(), RewriteOp::Import))
}

/// The value of the first `change-id` header of a raw git commit.
fn jj_header(raw: &[u8]) -> Option<&[u8]> {
    let end = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .unwrap_or(raw.len());
    raw[..end]
        .split(|&c| c == b'\n')
        .find_map(|l| l.strip_prefix(b"change-id "))
}

/// The value of the last `Change-Id:` line of the last paragraph of a message.
fn gerrit_trailer(message: &[u8]) -> Option<&[u8]> {
    let end = message
        .iter()
        .rposition(|c| !matches!(c, b'\n' | b'\r'))
        .map_or(0, |i| i + 1);
    let message = &message[..end];
    let start = message.windows(2).rposition(|w| w == b"\n\n")? + 2;
    message[start..]
        .split(|&c| c == b'\n')
        .rev()
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let colon = line.iter().position(|&c| c == b':')?;
            let (key, value) = line.split_at(colon);
            if !key.eq_ignore_ascii_case(b"Change-Id") {
                return None;
            }
            let value = value[1..].trim_ascii();
            (!value.is_empty() && value.len() <= 128 && value.iter().all(|c| c.is_ascii_graphic()))
                .then_some(value)
        })
}

// ------------------------------------------------------------------------------ the walk

const MASTER: u8 = 1;
const BRANCH: u8 = 2;

struct Painted {
    /// Candidate tips the default branch reaches.
    merged: HashSet<ChangesetId>,
    /// Commits only branches reach, with their parents.
    only: HashMap<ChangesetId, Vec<ChangesetId>>,
}

struct Node {
    generation: i64,
    parents: Vec<ChangesetId>,
}

async fn fetch_nodes(
    conn: &mut sqlx::AnyConnection,
    ns: crate::registry::Ns,
    ids: &[ChangesetId],
    into: &mut HashMap<ChangesetId, Node>,
) -> Result<(), Status> {
    let todo: Vec<ChangesetId> = ids
        .iter()
        .copied()
        .filter(|c| !into.contains_key(c))
        .collect();
    for chunk in todo.chunks(GRAPH_BATCH) {
        let marks = (0..chunk.len())
            .map(|i| format!("${}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql =
            format!("SELECT cs, gen, parents FROM cs_graph WHERE ns = $1 AND cs IN ({marks})");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(ns.as_bytes().to_vec());
        for c in chunk {
            q = q.bind(c.as_bytes().to_vec());
        }
        for row in q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| internal("cs_graph", &e))?
        {
            let cs: Vec<u8> = row.try_get("cs").map_err(|e| internal("cs_graph", &e))?;
            let blob: Vec<u8> = row
                .try_get("parents")
                .map_err(|e| internal("cs_graph", &e))?;
            let id = |b: &[u8]| {
                <[u8; 32]>::try_from(b)
                    .map(ChangesetId::from_bytes)
                    .map_err(|_| Status::data_loss("damaged cs_graph row"))
            };
            if !blob.len().is_multiple_of(32) {
                return Err(Status::data_loss("damaged cs_graph parents"));
            }
            let parents = blob.chunks_exact(32).map(id).collect::<Result<_, _>>()?;
            into.insert(
                id(&cs)?,
                Node {
                    generation: row.try_get("gen").map_err(|e| internal("cs_graph", &e))?,
                    parents,
                },
            );
        }
    }
    Ok(())
}

/// Paints the default branch tip and the candidate tips down the graph in generation order. A
/// node popped has all its children done, so its colours are final: both colours mean merged,
/// the branch colour alone means unmerged. Stops when no branch-only node is left in the queue.
async fn paint_down(
    pool: &sqlx::AnyPool,
    ns: crate::registry::Ns,
    master: Option<ChangesetId>,
    tips: &[ChangesetId],
) -> Result<Painted, Status> {
    let mut out = Painted {
        merged: HashSet::new(),
        only: HashMap::new(),
    };
    if tips.is_empty() {
        return Ok(out);
    }
    let mut conn = pool.acquire().await.map_err(|e| internal("pool", &e))?;
    let mut nodes: HashMap<ChangesetId, Node> = HashMap::new();
    let mut start: Vec<ChangesetId> = tips.to_vec();
    start.extend(master);
    fetch_nodes(&mut conn, ns, &start, &mut nodes).await?;
    let mut flags: HashMap<ChangesetId, u8> = HashMap::new();
    let mut heap: BinaryHeap<(i64, [u8; 32])> = BinaryHeap::new();
    let mut queued: HashSet<ChangesetId> = HashSet::new();
    let seeds = master
        .map(|m| (m, MASTER))
        .into_iter()
        .chain(tips.iter().map(|t| (*t, BRANCH)));
    for (c, f) in seeds {
        let n = nodes
            .get(&c)
            .ok_or_else(|| Status::data_loss("a branch tip is missing from cs_graph"))?;
        *flags.entry(c).or_insert(0) |= f;
        if queued.insert(c) {
            heap.push((n.generation, *c.as_bytes()));
        }
    }
    // Queued nodes carrying the branch colour alone: the walk ends when none is left.
    let mut pending = queued
        .iter()
        .filter(|c| flags.get(c) == Some(&BRANCH))
        .count();
    let mut visits = 0usize;
    while pending > 0 {
        let Some((_, bytes)) = heap.pop() else { break };
        let c = ChangesetId::from_bytes(bytes);
        let f = flags.get(&c).copied().unwrap_or(0);
        if f == BRANCH {
            pending -= 1;
        }
        visits += 1;
        if visits > MAX_VISITS {
            return Err(Status::resource_exhausted(
                "the history walk passed its bound; nothing was changed",
            ));
        }
        let parents = nodes
            .get(&c)
            .map(|n| n.parents.clone())
            .ok_or_else(|| Status::data_loss("graph node missing"))?;
        if f == BRANCH {
            if out.only.len() >= MAX_UNMERGED {
                return Err(Status::resource_exhausted(
                    "too many unmerged commits; nothing was changed",
                ));
            }
            out.only.insert(c, parents.clone());
        }
        fetch_nodes(&mut conn, ns, &parents, &mut nodes).await?;
        for p in parents {
            let n = nodes
                .get(&p)
                .ok_or_else(|| Status::data_loss("a parent is missing from cs_graph"))?;
            let before = flags.get(&p).copied().unwrap_or(0);
            let after = before | f;
            flags.insert(p, after);
            if queued.insert(p) {
                heap.push((n.generation, *p.as_bytes()));
                if after == BRANCH {
                    pending += 1;
                }
            } else if before == BRANCH && after != BRANCH {
                pending -= 1;
            }
        }
    }
    for t in tips {
        if flags.get(t).copied().unwrap_or(0) & MASTER != 0 {
            out.merged.insert(*t);
        }
    }
    Ok(out)
}

/// The unmerged commits of the converted branches with the scope (the first branch in sorted
/// order that reaches the commit) for the Gerrit tier.
fn unmerged_commits(
    plan: &[(String, ChangesetId, i64, bool)],
    paint: &Painted,
) -> Vec<(ChangesetId, String)> {
    let mut scoped: HashMap<ChangesetId, String> = HashMap::new();
    let mut order: Vec<ChangesetId> = Vec::new();
    for (branch, tip, _, _) in plan {
        let scope = format!("{HEADS}{branch}");
        let mut stack = vec![*tip];
        while let Some(c) = stack.pop() {
            let Some(parents) = paint.only.get(&c) else {
                continue;
            };
            if scoped.contains_key(&c) {
                continue;
            }
            scoped.insert(c, scope.clone());
            order.push(c);
            stack.extend(parents.iter().copied());
        }
    }
    order
        .into_iter()
        .filter_map(|c| scoped.remove(&c).map(|s| (c, s)))
        .collect()
}
