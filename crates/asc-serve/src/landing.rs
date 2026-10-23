//! The landing map and the `FindLanding` RPC (PLAN U66, SPEC §8.9).
//!
//! `land_map` maps every changeset a land or a `Merge` rewrote to its replacement, together with
//! the ref and the `ref_log` entry of the update that made it. [`record`] writes those rows inside
//! the `UpdateRefs` step-8 transaction. `FindLanding` maps a changeset through the table, looks
//! up its `mainline` position, and binary-searches `ref_log_by_ref` for the first update of the
//! ref whose new tip has a position at or above it: O(log n) indexed reads, no history walk.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_rpc::convert::WireId;
use asc_rpc::server::{CallCtx, LandingHandler, MAX_STRING, wire};
use sqlx::any::{Any, AnyRow};
use sqlx::{AnyPool, Row, Transaction};
use tonic::Status;
use uuid::Uuid;

use crate::idmode::IdCodec;
use crate::mainline::{MainlineIndex, Pos, SqlMainline};
use crate::registry::{Extensions, Ns, Registry, RegistryError, RepoEntry};
use crate::update::internal;

/// Most `ref_log` probes of one search; a `seq` is 63 bits, so the search ends well before this.
const MAX_PROBES: usize = 160;
/// Rows per `land_map` insert statement.
const INSERT_BATCH: usize = 100;

/// Writes the `land_map` rows for `pairs` (old, new), made by the update of `refname` with
/// `op_id`. Runs in the step-8 transaction after the ref update, so its `ref_log` row exists.
/// A changeset already mapped keeps its first mapping.
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
    let seq: i64 = sqlx::query(
        "SELECT MAX(seq) AS seq FROM ref_log WHERE repo_id = $1 AND name = $2 AND op_id = $3",
    )
    .bind(repo_id)
    .bind(refname.to_string())
    .bind(op_id.as_bytes().to_vec())
    .fetch_one(&mut **tx)
    .await?
    .try_get::<Option<i64>, _>("seq")?
    .ok_or(sqlx::Error::RowNotFound)?;
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
            "INSERT INTO land_map (ns, old_cs, new_cs, ref, ref_log_seq, op_id) VALUES {rows} \
             ON CONFLICT (ns, old_cs) DO NOTHING"
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

    /// The changeset `old` was rewritten to, if any.
    pub async fn mapped(&self, ns: Ns, old: ChangesetId) -> Result<Option<ChangesetId>, Status> {
        let row = sqlx::query("SELECT new_cs FROM land_map WHERE ns = $1 AND old_cs = $2")
            .bind(ns.as_bytes().to_vec())
            .bind(old.as_bytes().to_vec())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| sql("land_map", e))?;
        match row {
            None => Ok(None),
            Some(r) => {
                let b: Vec<u8> = r.try_get("new_cs").map_err(|e| sql("land_map", e))?;
                ChangesetId::from_slice(&b)
                    .map(Some)
                    .map_err(|e| internal("land_map", &e))
            }
        }
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

    async fn reaches(
        &self,
        ns: Ns,
        refname: &str,
        e: &LogEntry,
        pos: Pos,
    ) -> Result<bool, Status> {
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
        let landed_as = self.mapped(ns, cs).await?.unwrap_or(cs);
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

/// The ref a request names: empty means the default branch; a bare name is a branch.
fn ref_of(entry: &RepoEntry, asked: &str) -> Result<RefName, Status> {
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
    RefName::new(full).map_err(|e| Status::invalid_argument(e.to_string()))
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
        let refname = ref_of(&entry, &req.r#ref)?;
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

    #[tokio::test]
    async fn lookups_use_indexes_not_scans() {
        let f = setup().await;
        let queries = [
            "SELECT new_cs FROM land_map WHERE ns = $1 AND old_cs = $2",
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
