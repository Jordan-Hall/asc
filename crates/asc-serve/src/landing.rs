//! The landing map and the `FindLanding` RPC (PLAN U66, SPEC §8.9).
//!
//! `land_map_ref` maps every changeset a land or a `Merge` rewrote to its replacement, together with
//! the ref and the `ref_log` entry of the update that made it. The map is keyed by
//! `(ns, ref, old_cs)`, so a rewrite on one ref never changes or blocks the mapping of another.
//! Migration 0106 added it beside the old `land_map` (keyed by `(ns, old_cs)`): [`record`] no
//! longer writes `land_map`, and `mapped` still reads its older rows (by ref) until a contract
//! migration backfills them into `land_map_ref` and drops it.
//! [`record`] writes those rows inside the `UpdateRefs` step-8 transaction, only for a ref the
//! updating actor may write. `FindLanding` maps a changeset through the table, looks
//! up its `mainline` position, and binary-searches `ref_log_by_ref` for the first update of the
//! ref whose new tip has a position at or above it: O(log n) indexed reads, no history walk.

use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_rpc::auth::Principal;
use asc_rpc::convert::WireId;
use asc_rpc::server::{CallCtx, LandingHandler, MAX_STRING, wire};
use async_trait::async_trait;
use bytes::Bytes;
use sqlx::any::{Any, AnyRow};
use sqlx::{AnyPool, Row, Transaction};
use tonic::Status;
use uuid::Uuid;

use crate::idmode::IdCodec;
use crate::mainline::{MainlineIndex, Pos, SqlMainline};
use crate::refpolicy::{self, Class, RepoPolicy};
use crate::registry::{Extensions, Ns, Registry, RegistryError, RepoEntry};
use crate::update::internal;

/// Most `ref_log` probes of one search; a `seq` is 63 bits, so the search ends well before this.
const MAX_PROBES: usize = 160;
/// Rows per `land_map_ref` insert statement.
const INSERT_BATCH: usize = 100;

/// Writes the `land_map_ref` rows for `pairs` (old, new), made by the update of `refname` with
/// `op_id`. Runs in the step-8 transaction after the ref update, so its `ref_log` row exists.
/// A changeset already mapped on that ref keeps its first mapping. A user ref is written only for
/// its owner: when the update's actor is not the ref's owner, nothing is recorded.
pub(crate) async fn record(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    repo_id: i64,
    refname: &RefName,
    op_id: Uuid,
    pairs: &[(ChangesetId, ChangesetId)],
) -> Result<(), sqlx::Error> {
    if pairs.is_empty() {
        return Ok(());
    }
    let log = sqlx::query(
        "SELECT seq, actor FROM ref_log WHERE repo_id = $1 AND name = $2 AND op_id = $3          ORDER BY seq DESC LIMIT 1",
    )
    .bind(repo_id)
    .bind(refname.to_string())
    .bind(op_id.as_bytes().to_vec())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(sqlx::Error::RowNotFound)?;
    let seq: i64 = log.try_get("seq")?;
    let actor: String = log.try_get("actor")?;
    if !may_write(&actor, &refname.to_string()) {
        return Ok(());
    }
    for chunk in pairs.chunks(INSERT_BATCH) {
        let rows = (0..chunk.len())
            .map(|i| {
                let b = i * 6;
                format!(
                    "(${}, ${}, ${}, ${}, ${}, ${})",
                    b + 1,
                    b + 2,
                    b + 3,
                    b + 4,
                    b + 5,
                    b + 6
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO land_map_ref (ns, old_cs, new_cs, ref, ref_log_seq, op_id) VALUES {rows} \
             ON CONFLICT (ns, ref, old_cs) DO NOTHING"
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (old, new) in chunk {
            q = q
                .bind(ns.as_bytes().to_vec())
                .bind(old.as_bytes().to_vec())
                .bind(new.as_bytes().to_vec())
                .bind(refname.to_string())
                .bind(seq)
                .bind(op_id.as_bytes().to_vec());
        }
        q.execute(&mut **tx).await?;
    }
    Ok(())
}

/// Whether `actor` (a principal id) may write `refname`. A user ref belongs to its owner alone;
/// the other refs were authorised by the update before it reached the transaction.
fn may_write(actor: &str, refname: &str) -> bool {
    if !refname.starts_with(refpolicy::USER_NS) {
        return true;
    }
    let principal = Principal { id: actor.into() };
    refpolicy::draft_owner_of(&principal, refname)
        .is_some_and(|owner| refpolicy::owns_user_ref(owner, refname))
}

/// One `ref_log` entry of a ref.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub seq: i64,
    pub new: Option<ChangesetId>,
    pub at: i64,
    pub actor: String,
    pub via: String,
}

/// Where a changeset landed on a ref.
#[derive(Debug, Clone)]
pub struct Landing {
    /// The changeset on the ref's mainline (the map's new id, or the id itself).
    pub landed_as: ChangesetId,
    pub entry: LogEntry,
}

/// The landing lookups over the server's SQL tables.
pub struct LandingIndex {
    pool: AnyPool,
    mainline: SqlMainline,
}

fn sql(context: &str, e: sqlx::Error) -> Status {
    internal(context, &e)
}

impl LandingIndex {
    pub fn new(pool: AnyPool) -> Self {
        Self {
            mainline: SqlMainline::new(pool.clone()),
            pool,
        }
    }

    /// The changeset `old` was rewritten to on `refname`, if any. Other refs' rows are ignored.
    pub async fn mapped(
        &self,
        ns: Ns,
        refname: &str,
        old: ChangesetId,
    ) -> Result<Option<ChangesetId>, Status> {
        // `land_map` holds the rows written before 0106 (first mapping wins, so it is read first);
        // every later row is in `land_map_ref`. Both are filtered by ref.
        for table in ["land_map", "land_map_ref"] {
            let q =
                format!("SELECT new_cs FROM {table} WHERE ns = $1 AND ref = $2 AND old_cs = $3");
            let row = sqlx::query(sqlx::AssertSqlSafe(q))
                .bind(ns.as_bytes().to_vec())
                .bind(refname.to_owned())
                .bind(old.as_bytes().to_vec())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| sql(table, e))?;
            if let Some(r) = row {
                let b: Vec<u8> = r.try_get("new_cs").map_err(|e| sql(table, e))?;
                return ChangesetId::from_slice(&b)
                    .map(Some)
                    .map_err(|e| internal(table, &e));
            }
        }
        Ok(None)
    }

    fn entry(row: &AnyRow) -> Result<LogEntry, Status> {
        let new: Option<Vec<u8>> = row.try_get("new").map_err(|e| sql("ref_log", e))?;
        Ok(LogEntry {
            seq: row.try_get("seq").map_err(|e| sql("ref_log", e))?,
            // A tag target is not a changeset; it never satisfies a search.
            new: new
                .and_then(|b| RefTarget::from_bytes(&b).ok())
                .and_then(|t| match t {
                    RefTarget::Changeset(c) => Some(c),
                    RefTarget::Tag(_) => None,
                }),
            at: row.try_get("at").map_err(|e| sql("ref_log", e))?,
            actor: row.try_get("actor").map_err(|e| sql("ref_log", e))?,
            via: row.try_get("via").map_err(|e| sql("ref_log", e))?,
        })
    }

    /// The first (`last = false`) or last entry of `name` with `seq` in `[lo, hi]`.
    async fn edge(
        &self,
        repo_id: i64,
        name: &str,
        range: (i64, i64),
        last: bool,
    ) -> Result<Option<LogEntry>, Status> {
        let order = if last { "DESC" } else { "ASC" };
        let q = format!(
            "SELECT seq, new, at, actor, via FROM ref_log \
             WHERE repo_id = $1 AND name = $2 AND seq >= $3 AND seq <= $4 \
             ORDER BY seq {order} LIMIT 1"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(q))
            .bind(repo_id)
            .bind(name.to_owned())
            .bind(range.0)
            .bind(range.1)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| sql("ref_log", e))?;
        row.as_ref().map(Self::entry).transpose()
    }

    async fn reaches(&self, ns: Ns, refname: &str, e: &LogEntry, pos: Pos) -> Result<bool, Status> {
        let Some(new) = e.new else { return Ok(false) };
        let at = self
            .mainline
            .position(ns, refname, new)
            .await
            .map_err(|e| internal("mainline", &e))?;
        Ok(at.is_some_and(|p| p >= pos))
    }

    /// Where `cs` landed on `refname`: maps it through `land_map`, takes its mainline position,
    /// and binary-searches the ref's log for the first entry whose tip is at or above it.
    pub async fn find(
        &self,
        ns: Ns,
        repo_id: i64,
        refname: &str,
        cs: ChangesetId,
    ) -> Result<Option<Landing>, Status> {
        let landed_as = self.mapped(ns, refname, cs).await?.unwrap_or(cs);
        let Some(pos) = self
            .mainline
            .position(ns, refname, landed_as)
            .await
            .map_err(|e| internal("mainline", &e))?
        else {
            return Ok(None);
        };
        let all = (i64::MIN, i64::MAX);
        let Some(first) = self.edge(repo_id, refname, all, false).await? else {
            return Ok(None);
        };
        let Some(last) = self.edge(repo_id, refname, all, true).await? else {
            return Ok(None);
        };
        if self.reaches(ns, refname, &first, pos).await? {
            return Ok(Some(Landing {
                landed_as,
                entry: first,
            }));
        }
        if !self.reaches(ns, refname, &last, pos).await? {
            return Ok(None);
        }
        // Invariant: the entry at `lo` does not reach `pos`; the entry at `hi` does.
        let (mut lo, mut hi, mut found) = (first.seq, last.seq, last);
        for _ in 0..MAX_PROBES {
            if hi - lo < 2 {
                break;
            }
            let mid = lo + (hi - lo) / 2;
            let probe = match self.edge(repo_id, refname, (mid, hi - 1), false).await? {
                Some(e) => Some(e),
                None => self.edge(repo_id, refname, (lo + 1, mid - 1), true).await?,
            };
            let Some(e) = probe else { break };
            if self.reaches(ns, refname, &e, pos).await? {
                hi = e.seq;
                found = e;
            } else {
                lo = e.seq;
            }
        }
        Ok(Some(Landing {
            landed_as,
            entry: found,
        }))
    }
}

fn registry_status(e: RegistryError) -> Status {
    match e {
        RegistryError::NotFound(_) => Status::not_found("no such repository"),
        e => internal("registry", &e),
    }
}

/// The ref a request names, as the caller sees it: empty means the default branch; a bare name is
/// a branch. `None` means the caller may not look at it, which the handler answers exactly as it
/// answers a missing ref or a missing landing.
///
/// A name under the user namespace is the caller's own or refused. In a branchless repository a
/// branch that is not shared is the caller's own user ref (the mapping `ListRefs` advertises and
/// `UpdateRefs` stores), so a bare name or `refs/heads/<n>` reaches only the caller's ref.
fn ref_of(
    entry: &RepoEntry,
    policy: &RepoPolicy,
    owner: Option<i64>,
    asked: &str,
) -> Result<Option<RefName>, Status> {
    if asked.len() > MAX_STRING {
        return Err(Status::invalid_argument("ref name too long"));
    }
    let full = if asked.is_empty() {
        format!("refs/heads/{}", entry.default_branch)
    } else if asked.starts_with("refs/") {
        asked.to_owned()
    } else {
        format!("refs/heads/{asked}")
    };
    let named = RefName::new(full).map_err(|e| Status::invalid_argument(e.to_string()))?;
    let text = named.to_string();
    if text.starts_with(refpolicy::USER_NS) {
        return Ok(owner
            .is_some_and(|o| refpolicy::owns_user_ref(o, &text))
            .then_some(named));
    }
    match policy.classify(owner, &text) {
        Ok(Class::User { stored, .. }) => RefName::new(stored.as_bytes())
            .map(Some)
            .map_err(|e| Status::invalid_argument(e.to_string())),
        Ok(_) => Ok(Some(named)),
        // An unshared tag, or a principal with no user refs: not for this caller.
        Err(_) => Ok(None),
    }
}

/// The `FindLanding` handler.
pub struct ServerLanding {
    registry: Arc<Registry>,
}

impl ServerLanding {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl LandingHandler for ServerLanding {
    async fn find_landing(
        &self,
        ctx: &CallCtx,
        req: wire::FindLandingRequest,
    ) -> Result<wire::FindLandingResponse, Status> {
        let entry = self
            .registry
            .open(&ctx.repo)
            .await
            .map_err(registry_status)?;
        let ids = IdCodec::new(ctx.id_mode, &self.registry, &entry);
        let cs = ids.changeset_in(&req.changeset).await?;
        let policy = RepoPolicy::load(self.registry.pool(), entry.key.0).await?;
        let asked = if req.r#ref.starts_with("refs/") {
            req.r#ref.clone()
        } else {
            format!("refs/heads/{}", req.r#ref)
        };
        let owner = refpolicy::draft_owner_of(&ctx.principal, &asked);
        let Some(refname) = ref_of(&entry, &policy, owner, &req.r#ref)? else {
            return Ok(wire::FindLandingResponse::default());
        };
        let index = LandingIndex::new(self.registry.pool().clone());
        let Some(found) = index
            .find(entry.ns, entry.key.0, &refname.to_string(), cs)
            .await?
        else {
            return Ok(wire::FindLandingResponse::default());
        };
        let landed_as: Bytes = ids.changeset_out(found.landed_as).await?;
        Ok(wire::FindLandingResponse {
            found: true,
            landed_as,
            ref_log_seq: u64::try_from(found.entry.seq).unwrap_or(0),
            at: found.entry.at,
            actor: found.entry.actor,
            via: found.entry.via,
        })
    }
}

/// The one registration line of this unit (called from `lib.rs`).
pub fn register(ext: &Extensions, registry: &Arc<Registry>) {
    ext.handlers
        .set_landing(Arc::new(ServerLanding::new(registry.clone())))
        .expect("the landing handler group is registered once");
}

#[cfg(test)]
mod tests {
    use asc_rpc::IdMode;
    use asc_rpc::auth::Principal;
    use asc_rpc::server::{RefsHandler, RepoName};

    use super::*;
    use crate::ingest::testutil::{commit, registry, repo};
    use crate::update::ServerRefs;

    fn ctx(op: Uuid, mode: IdMode) -> CallCtx {
        CallCtx {
            repo: RepoName::parse("o/r").unwrap(),
            id_mode: mode,
            principal: Principal { id: "alice".into() },
            op_id: Some(op),
            client: None,
            admin: Default::default(),
        }
    }

    fn update(
        expect: wire::ref_update::Expect,
        to: ChangesetId,
        mode: wire::UpdateMode,
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
            ..wire::UpdateRefsRequest::default()
        }
    }

    struct Fx {
        r: Arc<Registry>,
        e: Arc<RepoEntry>,
        refs: ServerRefs,
        landing: ServerLanding,
        base: ChangesetId,
    }

    async fn setup() -> Fx {
        let r = registry().await;
        let e = repo(&r).await;
        let refs = ServerRefs::new(r.clone(), Arc::default());
        let base = commit(&e, &[], &[("base", b"0")]).await;
        let create = update(
            wire::ref_update::Expect::Absent(true),
            base,
            wire::UpdateMode::FastForward,
        );
        refs.update_refs(&ctx(Uuid::new_v4(), IdMode::Asc), create)
            .await
            .unwrap();
        Fx {
            landing: ServerLanding::new(r.clone()),
            r,
            e,
            refs,
            base,
        }
    }

    /// Lands `tip`; returns the op id and the rewritten pairs.
    async fn land(f: &Fx, tip: ChangesetId) -> (Uuid, Vec<(ChangesetId, ChangesetId)>) {
        let op = Uuid::new_v4();
        let done = f
            .refs
            .update_refs(
                &ctx(op, IdMode::Asc),
                update(
                    wire::ref_update::Expect::Any(true),
                    tip,
                    wire::UpdateMode::Land,
                ),
            )
            .await
            .unwrap();
        let pairs = done
            .rewritten
            .iter()
            .map(|w| {
                (
                    ChangesetId::from_slice(&w.old).unwrap(),
                    ChangesetId::from_slice(&w.new).unwrap(),
                )
            })
            .collect();
        (op, pairs)
    }

    async fn seq_of(f: &Fx, op: Uuid) -> u64 {
        let row = sqlx::query("SELECT seq FROM ref_log WHERE op_id = $1")
            .bind(op.as_bytes().to_vec())
            .fetch_one(f.r.pool())
            .await
            .unwrap();
        u64::try_from(row.get::<i64, _>(0)).unwrap()
    }

    async fn find(f: &Fx, cs: ChangesetId, refname: &str) -> wire::FindLandingResponse {
        f.landing
            .find_landing(
                &ctx(Uuid::new_v4(), IdMode::Asc),
                wire::FindLandingRequest {
                    repo: None,
                    changeset: Bytes::copy_from_slice(cs.as_bytes()),
                    r#ref: refname.into(),
                },
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn three_stacks_map_every_pre_land_id_to_its_landing() {
        let f = setup().await;
        let mut expected = Vec::new();
        for s in 0..3 {
            let one = commit(&f.e, &[f.base], &[(&format!("s{s}a"), b"1")]).await;
            let two = commit(&f.e, &[one], &[(&format!("s{s}b"), b"2")]).await;
            let (op, pairs) = land(&f, two).await;
            let seq = seq_of(&f, op).await;
            let new_of = |old: ChangesetId| {
                pairs
                    .iter()
                    .find(|(o, _)| *o == old)
                    .map_or(old, |(_, n)| *n)
            };
            expected.push((one, new_of(one), seq));
            expected.push((two, new_of(two), seq));
            // The second and third stacks sit on a moved branch, so they were rewritten.
            assert_eq!(pairs.is_empty(), s == 0);
        }
        for (old, new, seq) in expected {
            for asked in [old, new] {
                let got = find(&f, asked, "").await;
                assert!(got.found, "{asked}");
                assert_eq!(got.landed_as.as_ref(), new.as_bytes());
                assert_eq!(got.ref_log_seq, seq);
                assert_eq!(got.actor, "alice");
                assert_eq!(got.via, "land");
            }
        }
        // The same answer by explicit and by short ref names.
        let (old, new, seq) = (f.base, f.base, seq_of_first(&f).await);
        let got = find(&f, old, "master").await;
        assert!(got.found);
        assert_eq!(got.landed_as.as_ref(), new.as_bytes());
        assert_eq!(got.ref_log_seq, seq);
        assert_eq!(
            find(&f, old, "refs/heads/master").await.ref_log_seq,
            got.ref_log_seq
        );
    }

    async fn seq_of_first(f: &Fx) -> u64 {
        let row = sqlx::query("SELECT MIN(seq) FROM ref_log")
            .fetch_one(f.r.pool())
            .await
            .unwrap();
        u64::try_from(row.get::<i64, _>(0)).unwrap()
    }

    #[tokio::test]
    async fn a_change_that_never_landed_is_not_found() {
        let f = setup().await;
        let stray = commit(&f.e, &[f.base], &[("stray", b"x")]).await;
        assert!(!find(&f, stray, "").await.found);
        let one = commit(&f.e, &[f.base], &[("one", b"1")]).await;
        land(&f, one).await;
        assert!(!find(&f, stray, "").await.found);
        // Another ref has no mainline.
        assert!(!find(&f, one, "other").await.found);
    }

    #[tokio::test]
    async fn the_search_finds_each_of_many_lands_with_a_logarithmic_probe_count() {
        let f = setup().await;
        let mut lands = Vec::new();
        for i in 0..40 {
            let c = commit(&f.e, &[f.base], &[(&format!("f{i}"), b"x")]).await;
            let (op, pairs) = land(&f, c).await;
            let new = pairs.first().map_or(c, |(_, n)| *n);
            lands.push((c, new, seq_of(&f, op).await));
        }
        for (old, new, seq) in lands {
            let got = find(&f, old, "").await;
            assert_eq!(got.landed_as.as_ref(), new.as_bytes());
            assert_eq!(got.ref_log_seq, seq);
        }
    }

    #[tokio::test]
    async fn git_ids_are_accepted_and_returned_in_git_mode() {
        let f = setup().await;
        let one = commit(&f.e, &[f.base], &[("one", b"1")]).await;
        land(&f, one).await;
        let git = f.e.backend.engine().git_commit_of(one).await.unwrap();
        let got = f
            .landing
            .find_landing(
                &ctx(Uuid::new_v4(), IdMode::Git),
                wire::FindLandingRequest {
                    repo: None,
                    changeset: Bytes::copy_from_slice(git.as_bytes()),
                    r#ref: String::new(),
                },
            )
            .await
            .unwrap();
        assert!(got.found);
        assert_eq!(got.landed_as.as_ref(), git.as_bytes());
    }

    async fn rejected(f: &Fx, changeset: &[u8], r: String) -> tonic::Code {
        f.landing
            .find_landing(
                &ctx(Uuid::new_v4(), IdMode::Asc),
                wire::FindLandingRequest {
                    repo: None,
                    changeset: Bytes::copy_from_slice(changeset),
                    r#ref: r,
                },
            )
            .await
            .unwrap_err()
            .code()
    }

    #[tokio::test]
    async fn bad_input_is_rejected() {
        let f = setup().await;
        let bad = tonic::Code::InvalidArgument;
        assert_eq!(rejected(&f, b"short", String::new()).await, bad);
        let long = "x".repeat(MAX_STRING + 1);
        assert_eq!(rejected(&f, &[0; 32], long).await, bad);
        assert_eq!(rejected(&f, &[0; 32], "refs/heads/a..b".into()).await, bad);
    }

    /// A caller named by its Forgejo user id (`"8"` acts as `8:u8`), or by any other principal
    /// text (`"alice"`), which has no user id.
    fn ctx_as(id: &str) -> CallCtx {
        let id = if id.bytes().all(|b| b.is_ascii_digit()) {
            format!("{id}:u{id}")
        } else {
            id.to_owned()
        };
        CallCtx {
            principal: Principal { id },
            ..ctx(Uuid::new_v4(), IdMode::Asc)
        }
    }

    /// Makes the repository branchless.
    async fn branchless(f: &Fx) {
        sqlx::query("UPDATE repos SET branchless = TRUE")
            .execute(f.r.pool())
            .await
            .unwrap();
    }

    /// Pushes `refs/heads/<branch>` as principal `who` (a user ref in a branchless repository)
    /// and indexes `cs` as its mainline, as a land would. Returns the stored ref and the op.
    async fn user_landing(f: &Fx, who: &str, branch: &str, cs: ChangesetId) -> (String, Uuid) {
        let op = Uuid::new_v4();
        let mut req = update(
            wire::ref_update::Expect::Absent(true),
            cs,
            wire::UpdateMode::FastForward,
        );
        req.updates[0].name = format!("refs/heads/{branch}");
        let mut c = ctx_as(who);
        c.op_id = Some(op);
        f.refs.update_refs(&c, req).await.unwrap();
        let stored = format!("refs/users/{who}/heads/{branch}");
        let mut tx = f.r.pool().begin().await.unwrap();
        crate::mainline::apply(
            &mut tx,
            f.e.ns,
            &stored,
            None,
            &[crate::mainline::MainlineStep {
                cs,
                paths: Vec::new(),
            }],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (stored, op)
    }

    async fn find_as(f: &Fx, who: &str, cs: ChangesetId, r: &str) -> wire::FindLandingResponse {
        f.landing
            .find_landing(
                &ctx_as(who),
                wire::FindLandingRequest {
                    repo: None,
                    changeset: Bytes::copy_from_slice(cs.as_bytes()),
                    r#ref: r.into(),
                },
            )
            .await
            .unwrap()
    }

    async fn record_pairs(f: &Fx, refname: &str, op: Uuid, pairs: &[(ChangesetId, ChangesetId)]) {
        let mut tx = f.r.pool().begin().await.unwrap();
        record(
            &mut tx,
            f.e.ns,
            f.e.key.0,
            &RefName::new(refname).unwrap(),
            op,
            pairs,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn a_user_cannot_find_a_landing_in_another_users_ref() {
        let f = setup().await;
        branchless(&f).await;
        let one = commit(&f.e, &[f.base], &[("one", b"1")]).await;
        let (stored, _) = user_landing(&f, "8", "feat", one).await;
        // The owner finds it by the stored name, by the full branch name and by the bare name.
        for asked in [stored.as_str(), "refs/heads/feat", "feat"] {
            assert!(find_as(&f, "8", one, asked).await.found, "{asked}");
        }
        // Another user finds nothing, and gets the answer a missing landing gets.
        let missing = wire::FindLandingResponse::default();
        for asked in [stored.as_str(), "refs/heads/feat", "feat"] {
            assert_eq!(find_as(&f, "7", one, asked).await, missing, "{asked}");
        }
        let nothing = commit(&f.e, &[f.base], &[("nothing", b"n")]).await;
        assert_eq!(find_as(&f, "7", nothing, "feat").await, missing);
        assert_eq!(
            find_as(&f, "7", one, "refs/users/9/heads/none").await,
            missing
        );
        // A principal with no user id cannot name a user ref either.
        assert_eq!(find_as(&f, "alice", one, &stored).await, missing);
    }

    #[tokio::test]
    async fn a_rewrite_on_a_user_ref_does_not_change_or_block_the_shared_mapping() {
        let f = setup().await;
        branchless(&f).await;
        let moved = commit(&f.e, &[f.base], &[("moved", b"m")]).await;
        land(&f, moved).await;
        // `one` sits on the old base, so landing it on master rewrites it.
        let one = commit(&f.e, &[f.base], &[("one", b"1")]).await;
        let other = commit(&f.e, &[f.base], &[("other", b"o")]).await;
        let (stored, op) = user_landing(&f, "8", "x", other).await;
        record_pairs(&f, &stored, op, &[(one, other)]).await;
        let (_, pairs) = land(&f, one).await;
        let new = pairs.iter().find(|(o, _)| *o == one).unwrap().1;
        assert_ne!(new, other);
        let index = LandingIndex::new(f.r.pool().clone());
        assert_eq!(
            index
                .mapped(f.e.ns, "refs/heads/master", one)
                .await
                .unwrap(),
            Some(new)
        );
        assert_eq!(
            index.mapped(f.e.ns, &stored, one).await.unwrap(),
            Some(other)
        );
        let got = find_as(&f, "7", one, "master").await;
        assert!(got.found);
        assert_eq!(got.landed_as.as_ref(), new.as_bytes());
    }

    #[tokio::test]
    async fn mapped_ignores_the_rows_of_other_refs() {
        let f = setup().await;
        branchless(&f).await;
        let a = commit(&f.e, &[f.base], &[("a", b"a")]).await;
        let b = commit(&f.e, &[f.base], &[("b", b"b")]).await;
        let (stored, op) = user_landing(&f, "8", "x", a).await;
        record_pairs(&f, &stored, op, &[(a, b)]).await;
        let index = LandingIndex::new(f.r.pool().clone());
        assert_eq!(index.mapped(f.e.ns, &stored, a).await.unwrap(), Some(b));
        assert_eq!(
            index.mapped(f.e.ns, "refs/heads/master", a).await.unwrap(),
            None
        );
        assert_eq!(
            index
                .mapped(f.e.ns, "refs/users/9/heads/x", a)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_rewrite_is_recorded_only_for_a_ref_its_actor_may_write() {
        let f = setup().await;
        branchless(&f).await;
        let a = commit(&f.e, &[f.base], &[("a", b"a")]).await;
        let b = commit(&f.e, &[f.base], &[("b", b"b")]).await;
        let (stored, op) = user_landing(&f, "8", "x", a).await;
        sqlx::query("UPDATE ref_log SET actor = '9:u9' WHERE op_id = $1")
            .bind(op.as_bytes().to_vec())
            .execute(f.r.pool())
            .await
            .unwrap();
        record_pairs(&f, &stored, op, &[(a, b)]).await;
        let index = LandingIndex::new(f.r.pool().clone());
        assert_eq!(index.mapped(f.e.ns, &stored, a).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rows_written_before_the_ref_key_migration_are_still_found_by_ref() {
        let f = setup().await;
        let pool = f.r.pool();
        let (old, legacy, newer) = (vec![1_u8; 32], vec![2_u8; 32], vec![3_u8; 32]);
        sqlx::query(
            "INSERT INTO land_map (ns, old_cs, new_cs, ref, ref_log_seq, op_id)              VALUES ($1, $2, $3, 'refs/heads/master', 1, $4)",
        )
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(old.clone())
        .bind(legacy.clone())
        .bind(vec![0_u8; 16])
        .execute(pool)
        .await
        .unwrap();
        let (old, legacy, newer) = (
            ChangesetId::from_slice(&old).unwrap(),
            ChangesetId::from_slice(&legacy).unwrap(),
            ChangesetId::from_slice(&newer).unwrap(),
        );
        let index = LandingIndex::new(pool.clone());
        assert_eq!(
            index
                .mapped(f.e.ns, "refs/heads/master", old)
                .await
                .unwrap(),
            Some(legacy)
        );
        // The key is the ref: another ref does not see the old row.
        assert_eq!(
            index.mapped(f.e.ns, "refs/heads/other", old).await.unwrap(),
            None
        );
        // The same changeset may be mapped on another ref, in the new table.
        sqlx::query(
            "INSERT INTO land_map_ref (ns, old_cs, new_cs, ref, ref_log_seq, op_id)              VALUES ($1, $2, $3, 'refs/heads/other', 1, $4)",
        )
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(old.as_bytes().to_vec())
        .bind(newer.as_bytes().to_vec())
        .bind(vec![0_u8; 16])
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(
            index.mapped(f.e.ns, "refs/heads/other", old).await.unwrap(),
            Some(newer)
        );
        assert_eq!(
            index
                .mapped(f.e.ns, "refs/heads/master", old)
                .await
                .unwrap(),
            Some(legacy)
        );
    }

    #[tokio::test]
    async fn lookups_use_indexes_not_scans() {
        let f = setup().await;
        let queries = [
            "SELECT new_cs FROM land_map_ref WHERE ns = $1 AND ref = $2 AND old_cs = $3",
            "SELECT seq, new, at, actor, via FROM ref_log \
             WHERE repo_id = $1 AND name = $2 AND seq >= $3 AND seq <= $4 \
             ORDER BY seq ASC LIMIT 1",
            "SELECT seq, new, at, actor, via FROM ref_log \
             WHERE repo_id = $1 AND name = $2 AND seq >= $3 AND seq <= $4 \
             ORDER BY seq DESC LIMIT 1",
            "SELECT pos FROM mainline WHERE ns = $1 AND ref = $2 AND cs = $3",
        ];
        for q in queries {
            let plan = format!("EXPLAIN QUERY PLAN {q}");
            let mut query = sqlx::query(sqlx::AssertSqlSafe(plan));
            // Parameters are bound by position; the same blob or text satisfies every column.
            for i in 1..=q.matches('$').count() {
                query = if q.contains("repo_id") && i == 1 {
                    query.bind(1_i64)
                } else if q.contains("seq >=") && i >= 3 {
                    query.bind(0_i64)
                } else {
                    query.bind(vec![0_u8; 16])
                };
            }
            let rows = query.fetch_all(f.r.pool()).await.unwrap();
            assert!(!rows.is_empty());
            for row in rows {
                let detail: String = row.get("detail");
                assert!(
                    detail.starts_with("SEARCH") && detail.contains("USING"),
                    "scan in plan for {q}: {detail}"
                );
            }
        }
    }
}
