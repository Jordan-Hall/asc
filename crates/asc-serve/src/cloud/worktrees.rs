//! Worktree reports (SPEC §21.3.9): which worktrees of a repository one device holds, so the owner
//! sees them on the hub. `Cloud.ReportWorktrees` replaces every `cloud_worktrees` row of one
//! `(repository, owner, device)`; `Cloud.ListWorktrees` reads them back.
//!
//! Choices where SPEC is silent:
//!
//! * Only metadata is stored. `ws_hash` is an opaque hash the device derives; a path, a file name or
//!   content has no field. `ws_hash` is exactly 32 bytes. `head_ref` is empty or a valid ref name by the
//!   rules of `git check-ref-format`, so a path (`C:/x`, `~/x`, `/x`, a backslash) is refused.
//! * A report holds at most [`MAX_WORKTREES`] worktrees; a repeated `ws_hash` keeps its first entry.
//! * A row lives [`WORKTREE_TTL_S`] after its last report: the write that follows drops older rows of the
//!   repository, and a read does not return them even before that write.
//! * An owner keeps at most [`MAX_DEVICES`] devices per repository, the ones reported last. A device name
//!   is the owner's own to choose, so one owner may fill the table up to that bound (the same as
//!   `ReportDevice`; accepted).
//! * [`prune_expired`] is the hourly worker's sweep of rows nobody reads or rewrites any more.
//! * A page is [`PAGE`] rows ordered by `(owner, device, ws_hash)`; the page token names the last row.

use asc_rpc::server::wire;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sqlx::{AnyPool, Row as _};
use tonic::Status;

use super::rpc::{MAX_DEVICES, MAX_NAME};
use crate::commit_graph_rpc::internal;

/// Worktrees in one `ReportWorktrees`, at most.
pub const MAX_WORKTREES: usize = 256;
/// Age after which a worktree row is dropped: 7 days.
pub const WORKTREE_TTL_S: i64 = 7 * 86_400;
/// Rows in one `ListWorktrees` page.
pub const PAGE: usize = 200;
/// Length of `ws_hash`, in bytes.
pub const WS_HASH_LEN: usize = 32;
/// Longest `head_ref`, in bytes.
pub const MAX_HEAD_REF: usize = 256;
/// Longest `mode`, in bytes.
pub const MAX_MODE: usize = 32;
/// Longest `in_progress`, in bytes.
pub const MAX_IN_PROGRESS: usize = 64;

/// Tag of a `ListWorktrees` page token: the tag, the owner (8 big-endian bytes), the device length
/// (1 byte), the device and the `ws_hash` of the last row of the page.
const PAGE_TAG: &[u8; 2] = b"w1";
/// The longest page token text.
const MAX_TOKEN: usize = 256;

fn sql(e: sqlx::Error) -> Status {
    internal("database", &e)
}

/// One validated worktree of a report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    ws_hash: Vec<u8>,
    head_ref: String,
    head: Vec<u8>,
    mode: String,
    dirty: bool,
    ahead: i64,
    behind: i64,
    ephemeral_until: i64,
    in_progress: String,
}

fn token_text(raw: &str, max: usize, what: &str) -> Result<(), Status> {
    let ok = raw.len() <= max
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(Status::invalid_argument(format!("invalid {what}")))
    }
}

/// Whether `raw` is empty or a valid ref name by the rules of `git check-ref-format`. This also
/// refuses every drive path (`C:/x`), home path (`~/x`), rooted path and backslash.
fn ref_text(raw: &str) -> Result<(), Status> {
    let bad = || Status::invalid_argument("invalid head_ref");
    if raw.is_empty() {
        return Ok(());
    }
    if raw.len() > MAX_HEAD_REF
        || raw == "@"
        || raw.starts_with(['/', '-'])
        || raw.ends_with(['/', '.'])
        || raw.contains("..")
        || raw.contains("//")
        || raw.contains("@{")
        || raw
            .chars()
            .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(bad());
    }
    if raw
        .split('/')
        .any(|part| part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(bad());
    }
    Ok(())
}

/// Validates the worktrees of one report and drops repeated `ws_hash` entries.
pub(super) fn validate(reports: &[wire::WorktreeReport]) -> Result<Vec<Entry>, Status> {
    if reports.len() > MAX_WORKTREES {
        return Err(Status::invalid_argument("too many worktrees"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(reports.len());
    for w in reports {
        if w.ws_hash.len() != WS_HASH_LEN {
            return Err(Status::invalid_argument("invalid ws_hash"));
        }
        if !w.head.is_empty() && w.head.len() != 32 {
            return Err(Status::invalid_argument(
                "head is empty or a native changeset id",
            ));
        }
        ref_text(&w.head_ref)?;
        token_text(&w.mode, MAX_MODE, "mode")?;
        token_text(&w.in_progress, MAX_IN_PROGRESS, "in_progress")?;
        if w.ephemeral_until < 0 {
            return Err(Status::invalid_argument("ephemeral_until is negative"));
        }
        if seen.insert(w.ws_hash.clone()) {
            out.push(Entry {
                ws_hash: w.ws_hash.to_vec(),
                head_ref: w.head_ref.clone(),
                head: w.head.to_vec(),
                mode: w.mode.clone(),
                dirty: w.dirty,
                ahead: i64::from(w.ahead),
                behind: i64::from(w.behind),
                ephemeral_until: w.ephemeral_until,
                in_progress: w.in_progress.clone(),
            });
        }
    }
    Ok(out)
}

/// Replaces the rows of `(ns, owner, device)` with `rows`, all reported at `at`.
pub(super) async fn replace(
    pool: &AnyPool,
    ns: &[u8],
    owner: i64,
    device: &str,
    rows: &[Entry],
    at: i64,
) -> Result<(), Status> {
    let mut tx = pool.begin().await.map_err(sql)?;
    sqlx::query("DELETE FROM cloud_worktrees WHERE ns = $1 AND reported_at < $2")
        .bind(ns.to_vec())
        .bind(at.saturating_sub(WORKTREE_TTL_S))
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
    sqlx::query("DELETE FROM cloud_worktrees WHERE ns = $1 AND owner = $2 AND device = $3")
        .bind(ns.to_vec())
        .bind(owner)
        .bind(device.to_owned())
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
    for r in rows {
        sqlx::query(
            "INSERT INTO cloud_worktrees (ns, owner, device, ws_hash, head_ref, head, mode, dirty, \
             ahead, behind, ephemeral_until, in_progress, reported_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(ns.to_vec())
        .bind(owner)
        .bind(device.to_owned())
        .bind(r.ws_hash.clone())
        .bind(r.head_ref.clone())
        .bind(r.head.clone())
        .bind(r.mode.clone())
        .bind(i64::from(r.dirty))
        .bind(r.ahead)
        .bind(r.behind)
        .bind(r.ephemeral_until)
        .bind(r.in_progress.clone())
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
    }
    // Keep the devices reported most recently; this one counts as the newest.
    let others: Vec<String> = sqlx::query_scalar(
        "SELECT device FROM cloud_worktrees WHERE ns = $1 AND owner = $2 AND device <> $3 \
         GROUP BY device ORDER BY MAX(reported_at) DESC, device",
    )
    .bind(ns.to_vec())
    .bind(owner)
    .bind(device.to_owned())
    .fetch_all(&mut *tx)
    .await
    .map_err(sql)?;
    let keep = usize::try_from(MAX_DEVICES - 1).unwrap_or(0);
    for old in others.into_iter().skip(keep) {
        sqlx::query("DELETE FROM cloud_worktrees WHERE ns = $1 AND owner = $2 AND device = $3")
            .bind(ns.to_vec())
            .bind(owner)
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
    }
    tx.commit().await.map_err(sql)
}

/// Deletes the rows of `ns` last reported more than [`WORKTREE_TTL_S`] before `at`; returns how many.
pub async fn prune_expired(pool: &AnyPool, ns: &[u8], at: i64) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM cloud_worktrees WHERE ns = $1 AND reported_at < $2")
        .bind(ns.to_vec())
        .bind(at.saturating_sub(WORKTREE_TTL_S))
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

struct Cursor {
    owner: i64,
    device: String,
    ws_hash: Vec<u8>,
}

fn page_token(c: &Cursor) -> String {
    let mut b = PAGE_TAG.to_vec();
    b.extend_from_slice(&c.owner.to_be_bytes());
    b.push(u8::try_from(c.device.len()).unwrap_or(u8::MAX));
    b.extend_from_slice(c.device.as_bytes());
    b.extend_from_slice(&c.ws_hash);
    URL_SAFE_NO_PAD.encode(b)
}

fn parse_page_token(t: &str) -> Result<Option<Cursor>, Status> {
    if t.is_empty() {
        return Ok(None);
    }
    let bad = || Status::invalid_argument("bad page token");
    if t.len() > MAX_TOKEN {
        return Err(bad());
    }
    let b = URL_SAFE_NO_PAD.decode(t).map_err(|_| bad())?;
    let rest = b.strip_prefix(PAGE_TAG.as_slice()).ok_or_else(bad)?;
    let (owner, rest) = rest.split_first_chunk::<8>().ok_or_else(bad)?;
    let (len, rest) = rest.split_first().ok_or_else(bad)?;
    let len = usize::from(*len);
    if rest.len() <= len || len > MAX_NAME {
        return Err(bad());
    }
    let (device, ws_hash) = rest.split_at(len);
    let device = std::str::from_utf8(device).map_err(|_| bad())?;
    if ws_hash.len() != WS_HASH_LEN {
        return Err(bad());
    }
    Ok(Some(Cursor {
        owner: i64::from_be_bytes(*owner),
        device: device.to_owned(),
        ws_hash: ws_hash.to_vec(),
    }))
}

/// One page of the rows of `ns` reported within [`WORKTREE_TTL_S`] of `at`; `owner` limits them to one
/// user, `None` returns every user's.
pub(super) async fn list(
    pool: &AnyPool,
    ns: &[u8],
    owner: Option<i64>,
    token: &str,
    at: i64,
) -> Result<wire::ListWorktreesResponse, Status> {
    let after = parse_page_token(token)?;
    let mut text = String::from(
        "SELECT owner, device, ws_hash, head_ref, head, mode, dirty, ahead, behind, \
         ephemeral_until, in_progress, reported_at FROM cloud_worktrees \
         WHERE ns = $1 AND reported_at >= $2",
    );
    let mut n = 2;
    if owner.is_some() {
        n += 1;
        text.push_str(&format!(" AND owner = ${n}"));
    }
    if after.is_some() {
        let (o, d, w) = (n + 1, n + 2, n + 3);
        text.push_str(&format!(
            " AND (owner > ${o} OR (owner = ${o} AND (device > ${d} \
             OR (device = ${d} AND ws_hash > ${w}))))"
        ));
    }
    text.push_str(&format!(
        " ORDER BY owner, device, ws_hash LIMIT {}",
        PAGE + 1
    ));
    let mut q = sqlx::query(sqlx::AssertSqlSafe(text))
        .bind(ns.to_vec())
        .bind(at.saturating_sub(WORKTREE_TTL_S));
    if let Some(o) = owner {
        q = q.bind(o);
    }
    if let Some(c) = &after {
        q = q
            .bind(c.owner)
            .bind(c.device.clone())
            .bind(c.ws_hash.clone());
    }
    let found = q.fetch_all(pool).await.map_err(sql)?;
    let more = found.len() > PAGE;
    let mut rows = Vec::with_capacity(found.len().min(PAGE));
    let mut last = None;
    for r in found.iter().take(PAGE) {
        let owner: i64 = r.try_get("owner").map_err(sql)?;
        let device: String = r.try_get("device").map_err(sql)?;
        let ws_hash: Vec<u8> = r.try_get("ws_hash").map_err(sql)?;
        let count = |col: &str| -> Result<u32, Status> {
            let v: i64 = r.try_get(col).map_err(sql)?;
            Ok(u32::try_from(v).unwrap_or(u32::MAX))
        };
        let dirty: i64 = r.try_get("dirty").map_err(sql)?;
        let head: Vec<u8> = r.try_get("head").map_err(sql)?;
        rows.push(wire::CloudWorktreeRow {
            owner: owner.to_string(),
            device: device.clone(),
            worktree: Some(wire::WorktreeReport {
                ws_hash: ws_hash.clone().into(),
                head_ref: r.try_get("head_ref").map_err(sql)?,
                head: head.into(),
                mode: r.try_get("mode").map_err(sql)?,
                dirty: dirty != 0,
                ahead: count("ahead")?,
                behind: count("behind")?,
                ephemeral_until: r.try_get("ephemeral_until").map_err(sql)?,
                in_progress: r.try_get("in_progress").map_err(sql)?,
            }),
            reported_at: r.try_get("reported_at").map_err(sql)?,
        });
        last = Some(Cursor {
            owner,
            device,
            ws_hash,
        });
    }
    let next_page_token = match (more, last) {
        (true, Some(c)) => page_token(&c),
        _ => String::new(),
    };
    Ok(wire::ListWorktreesResponse {
        rows,
        next_page_token,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use asc_rpc::IdMode;
    use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
    use asc_rpc::server::ServerInfo;
    use asc_rpc::services::cloud_server::Cloud;
    use bytes::Bytes;
    use tonic::Request;

    use super::*;
    use crate::cloud::Limits;
    use crate::cloud::rpc::{CloudRpc, CloudService, register};
    use crate::ingest::testutil::{registry, repo};
    use crate::registry::{Extensions, Registry, RepoEntry};

    const ALICE: &str = "alice-token";
    const BOB: &str = "bob-token";
    const WRITER: &str = "writer-token";

    struct Fx {
        reg: Arc<Registry>,
        e: Arc<RepoEntry>,
        rpc: Arc<CloudRpc>,
        svc: CloudService,
    }

    async fn fx() -> Fx {
        let reg = registry().await;
        let e = repo(&reg).await;
        sqlx::query("UPDATE repos SET cloud = TRUE WHERE id = $1")
            .bind(e.key.0)
            .execute(reg.pool())
            .await
            .unwrap();
        let ext = Extensions::new();
        register(&ext, &reg, Limits::default());
        let auth = StaticTokens::new()
            .with_token(ALICE, "42:alice", Access::Read)
            .with_token(BOB, "43:bob", Access::Read)
            .with_token(WRITER, "44:carol", Access::Write);
        let rpc = ext.cloud.get().unwrap().clone();
        let svc = CloudService::new(rpc.clone(), Arc::new(auth), ServerInfo::default());
        Fx { reg, e, rpc, svc }
    }

    fn req<T>(m: T, token: &str) -> Request<T> {
        let mut r = Request::new(m);
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(token).unwrap()));
        r
    }

    fn repo_ref() -> Option<wire::RepoRef> {
        Some(wire::RepoRef {
            name: "o/r".into(),
            id_mode: IdMode::Asc.to_wire(),
        })
    }

    fn wt(n: u8) -> wire::WorktreeReport {
        wire::WorktreeReport {
            ws_hash: Bytes::copy_from_slice(&[n; 32]),
            head_ref: format!("main-{n}"),
            head: Bytes::copy_from_slice(&[n; 32]),
            mode: "plain".into(),
            dirty: n.is_multiple_of(2),
            ahead: u32::from(n),
            behind: 1,
            ephemeral_until: 0,
            in_progress: String::new(),
        }
    }

    async fn report(
        f: &Fx,
        token: &str,
        device: &str,
        worktrees: Vec<wire::WorktreeReport>,
    ) -> Result<u32, Status> {
        f.svc
            .report_worktrees(req(
                wire::ReportWorktreesRequest {
                    repo: repo_ref(),
                    device: device.into(),
                    worktrees,
                },
                token,
            ))
            .await
            .map(|r| r.into_inner().kept)
    }

    async fn list_page(
        f: &Fx,
        token: &str,
        owner: &str,
        page: &str,
    ) -> Result<wire::ListWorktreesResponse, Status> {
        f.svc
            .list_worktrees(req(
                wire::ListWorktreesRequest {
                    repo: repo_ref(),
                    owner: owner.into(),
                    page_token: page.into(),
                },
                token,
            ))
            .await
            .map(|r| r.into_inner())
    }

    async fn all(f: &Fx, token: &str, owner: &str) -> Vec<wire::CloudWorktreeRow> {
        let mut rows = Vec::new();
        let mut page = String::new();
        loop {
            let r = list_page(f, token, owner, &page).await.unwrap();
            rows.extend(r.rows);
            if r.next_page_token.is_empty() {
                return rows;
            }
            page = r.next_page_token;
        }
    }

    fn keys(rows: &[wire::CloudWorktreeRow]) -> Vec<(String, String, u8)> {
        rows.iter()
            .map(|r| {
                (
                    r.owner.clone(),
                    r.device.clone(),
                    r.worktree.as_ref().unwrap().ws_hash[0],
                )
            })
            .collect()
    }

    fn now() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_report_replaces_the_set_of_its_device_only() {
        let f = fx().await;
        assert_eq!(
            report(&f, ALICE, "d1", vec![wt(1), wt(2)]).await.unwrap(),
            2
        );
        assert_eq!(report(&f, ALICE, "d2", vec![wt(3)]).await.unwrap(), 1);
        let rows = all(&f, ALICE, "").await;
        assert_eq!(
            keys(&rows),
            vec![
                ("42".into(), "d1".into(), 1),
                ("42".into(), "d1".into(), 2),
                ("42".into(), "d2".into(), 3)
            ]
        );
        let first = rows[0].worktree.as_ref().unwrap();
        assert_eq!(first.head_ref, "main-1");
        assert_eq!(first.head.len(), 32);
        assert_eq!((first.ahead, first.behind, first.dirty), (1, 1, false));
        assert!(rows[0].reported_at > 0);

        // The next report of d1 replaces its set; a repeated ws_hash keeps the first entry.
        let mut changed = wt(2);
        changed.head_ref = "topic".into();
        changed.dirty = true;
        let mut dup = wt(2);
        dup.head_ref = "ignored".into();
        assert_eq!(
            report(&f, ALICE, "d1", vec![changed, dup]).await.unwrap(),
            1
        );
        let rows = all(&f, ALICE, "").await;
        assert_eq!(
            keys(&rows),
            vec![("42".into(), "d1".into(), 2), ("42".into(), "d2".into(), 3)]
        );
        let w = rows[0].worktree.as_ref().unwrap();
        assert_eq!((w.head_ref.as_str(), w.dirty), ("topic", true));

        // An empty report clears the device.
        assert_eq!(report(&f, ALICE, "d1", vec![]).await.unwrap(), 0);
        assert_eq!(
            keys(&all(&f, ALICE, "").await),
            vec![("42".into(), "d2".into(), 3)]
        );
    }

    #[tokio::test]
    async fn rows_expire_after_the_ttl_on_read_and_on_the_next_write() {
        let f = fx().await;
        report(&f, ALICE, "d1", vec![wt(1)]).await.unwrap();
        report(&f, BOB, "d9", vec![wt(5)]).await.unwrap();
        let age = |secs: i64, device: &'static str| {
            let pool = f.reg.pool().clone();
            async move {
                sqlx::query("UPDATE cloud_worktrees SET reported_at = $1 WHERE device = $2")
                    .bind(now() - secs)
                    .bind(device)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        age(WORKTREE_TTL_S - 60, "d1").await;
        assert_eq!(all(&f, ALICE, "").await.len(), 1);
        age(WORKTREE_TTL_S + 60, "d1").await;
        // Filtered on read although the row is still stored.
        assert!(all(&f, ALICE, "").await.is_empty());
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_worktrees")
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(stored, 2);
        // Any report of the repository prunes it, whoever owns the stale row.
        report(&f, BOB, "d9", vec![wt(5)]).await.unwrap();
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_worktrees")
            .fetch_one(f.reg.pool())
            .await
            .unwrap();
        assert_eq!(stored, 1);
    }

    #[tokio::test]
    async fn a_reader_sees_only_their_own_rows_and_a_writer_sees_all() {
        let f = fx().await;
        report(&f, ALICE, "d1", vec![wt(1)]).await.unwrap();
        report(&f, BOB, "d1", vec![wt(2)]).await.unwrap();
        report(&f, WRITER, "d1", vec![wt(3)]).await.unwrap();
        assert_eq!(
            keys(&all(&f, ALICE, "").await),
            vec![("42".into(), "d1".into(), 1)]
        );
        assert_eq!(
            keys(&all(&f, ALICE, "alice").await),
            vec![("42".into(), "d1".into(), 1)]
        );
        assert_eq!(
            keys(&all(&f, ALICE, "42").await),
            vec![("42".into(), "d1".into(), 1)]
        );
        for other in ["43", "*", "44"] {
            let e = list_page(&f, ALICE, other, "").await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::PermissionDenied, "{other}");
        }
        assert_eq!(
            keys(&all(&f, WRITER, "*").await),
            vec![
                ("42".into(), "d1".into(), 1),
                ("43".into(), "d1".into(), 2),
                ("44".into(), "d1".into(), 3)
            ]
        );
        assert_eq!(
            keys(&all(&f, WRITER, "43").await),
            vec![("43".into(), "d1".into(), 2)]
        );
        assert_eq!(
            keys(&all(&f, WRITER, "").await),
            vec![("44".into(), "d1".into(), 3)]
        );
    }

    #[tokio::test]
    async fn a_report_lands_under_the_callers_identity_only() {
        let f = fx().await;
        // There is no owner field: bob's report never touches alice's rows.
        report(&f, ALICE, "d1", vec![wt(1)]).await.unwrap();
        report(&f, BOB, "d1", vec![wt(2), wt(3)]).await.unwrap();
        assert_eq!(
            keys(&all(&f, ALICE, "").await),
            vec![("42".into(), "d1".into(), 1)]
        );
        // A caller with no user identity cannot report.
        let none = StaticTokens::new().with_token("svc", "svc-account", Access::Write);
        let svc = CloudService::new(f.rpc.clone(), Arc::new(none), ServerInfo::default());
        let e = svc
            .report_worktrees(req(
                wire::ReportWorktreesRequest {
                    repo: repo_ref(),
                    device: "d1".into(),
                    worktrees: vec![wt(4)],
                },
                "svc",
            ))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        // The cloud flag gates reports.
        sqlx::query("UPDATE repos SET cloud = FALSE WHERE id = $1")
            .bind(f.e.key.0)
            .execute(f.reg.pool())
            .await
            .unwrap();
        let e = report(&f, ALICE, "d1", vec![wt(1)]).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn the_cap_and_the_lengths_are_validated_and_nothing_is_written() {
        let f = fx().await;
        report(&f, ALICE, "d1", vec![wt(1)]).await.unwrap();
        let mut many = Vec::new();
        for i in 0..=MAX_WORKTREES {
            let mut w = wt(1);
            let mut hash = [0u8; WS_HASH_LEN];
            hash[..4].copy_from_slice(&u32::try_from(i).unwrap().to_be_bytes());
            w.ws_hash = Bytes::copy_from_slice(&hash);
            many.push(w);
        }
        let exact: Vec<_> = many[..MAX_WORKTREES].to_vec();
        let mut bad: Vec<(&str, Vec<wire::WorktreeReport>)> = vec![("cap", many)];
        let mut case = |name: &'static str, f: &dyn Fn(&mut wire::WorktreeReport)| {
            let mut w = wt(2);
            f(&mut w);
            bad.push((name, vec![wt(3), w]));
        };
        case("empty hash", &|w| w.ws_hash = Bytes::new());
        case("short hash", &|w| {
            w.ws_hash = Bytes::from(vec![1; WS_HASH_LEN - 1])
        });
        case("long hash", &|w| {
            w.ws_hash = Bytes::from(vec![1; WS_HASH_LEN + 1])
        });
        case("short head", &|w| w.head = Bytes::from(vec![1; 31]));
        case("long ref", &|w| w.head_ref = "a".repeat(MAX_HEAD_REF + 1));
        for (name, r) in [
            ("backslash ref", "work\\tree"),
            ("drive path", "C:\\work\\tree"),
            ("drive slash path", "C:/Users/x/src"),
            ("home path", "~/x"),
            ("rooted ref", "/home/me/tree"),
            ("space", "a b"),
            ("colon", "a:b"),
            ("dotdot", "a..b"),
            ("caret", "a^b"),
            ("question", "a?b"),
            ("star", "a*b"),
            ("bracket", "a[b"),
            ("leading dash", "-x"),
            ("double slash", "a//b"),
            ("reflog syntax", "a@{1}"),
            ("lone at", "@"),
            ("lock suffix", "refs/heads/x.lock"),
            ("lock component", "a.lock/b"),
            ("trailing slash", "a/"),
            ("trailing dot", "a."),
            ("dot component", "a/.b"),
            ("relative", "../x"),
        ] {
            case(name, &|w| w.head_ref = r.into());
        }
        case("control ref", &|w| w.head_ref = "a\nb".into());
        case("long mode", &|w| w.mode = "m".repeat(MAX_MODE + 1));
        case("odd mode", &|w| w.mode = "a b".into());
        case("long state", &|w| {
            w.in_progress = "r".repeat(MAX_IN_PROGRESS + 1)
        });
        case("odd state", &|w| w.in_progress = "a/b".into());
        case("negative ephemeral", &|w| w.ephemeral_until = -1);
        for (name, worktrees) in bad {
            let e = report(&f, ALICE, "d1", worktrees).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{name}");
        }
        for device in ["", "a b", "d/1", &"d".repeat(65)] {
            let e = report(&f, ALICE, device, vec![wt(1)]).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{device:?}");
        }
        // The refused reports left the first one alone.
        assert_eq!(
            keys(&all(&f, ALICE, "").await),
            vec![("42".into(), "d1".into(), 1)]
        );
        // Ref names git accepts pass, whatever their shape.
        for ok in ["main", "refs/heads/feat/x-1", "v1.2.3", "a@b", "x.lockfile"] {
            let mut w = wt(7);
            w.head_ref = ok.into();
            assert_eq!(report(&f, ALICE, "d1", vec![w]).await.unwrap(), 1, "{ok}");
        }
        // The cap itself is allowed, and an empty head and head_ref are too.
        let mut bare = wt(9);
        bare.head = Bytes::new();
        bare.head_ref = String::new();
        bare.mode = String::new();
        assert_eq!(report(&f, ALICE, "d1", vec![bare]).await.unwrap(), 1);
        assert_eq!(
            report(&f, ALICE, "d1", exact).await.unwrap(),
            u32::try_from(MAX_WORKTREES).unwrap()
        );
    }

    #[tokio::test]
    async fn an_owner_keeps_the_devices_reported_last() {
        let f = fx().await;
        let devices = usize::try_from(MAX_DEVICES).unwrap();
        for i in 0..=devices {
            report(&f, ALICE, &format!("d{i}"), vec![wt(1)])
                .await
                .unwrap();
            // Distinct report times, so the oldest is the first one.
            sqlx::query(
                "UPDATE cloud_worktrees SET reported_at = reported_at - $1 WHERE device = $2",
            )
            .bind(i64::try_from(devices - i).unwrap() * 10)
            .bind(format!("d{i}"))
            .execute(f.reg.pool())
            .await
            .unwrap();
        }
        let left: Vec<String> = all(&f, ALICE, "")
            .await
            .into_iter()
            .map(|r| r.device)
            .collect();
        assert_eq!(left.len(), devices);
        assert!(!left.contains(&"d0".to_owned()), "{left:?}");
    }

    #[tokio::test]
    async fn pages_resume_after_the_last_row_and_bad_tokens_are_refused() {
        let f = fx().await;
        let per_device = 150u8;
        for device in ["d1", "d2"] {
            let ws: Vec<_> = (1..=per_device).map(wt).collect();
            report(&f, ALICE, device, ws).await.unwrap();
        }
        report(&f, BOB, "d1", (1..=3).map(wt).collect())
            .await
            .unwrap();
        let first = list_page(&f, WRITER, "*", "").await.unwrap();
        assert_eq!(first.rows.len(), PAGE);
        assert!(!first.next_page_token.is_empty());
        let second = list_page(&f, WRITER, "*", &first.next_page_token)
            .await
            .unwrap();
        assert_eq!(second.rows.len(), 2 * 150 + 3 - PAGE);
        assert!(second.next_page_token.is_empty());
        let mut got = keys(&first.rows);
        got.extend(keys(&second.rows));
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(got, sorted, "ordered and no row twice");
        assert_eq!(got.len(), 303);
        // A filtered read pages the same way.
        assert_eq!(all(&f, WRITER, "42").await.len(), 300);
        for t in ["!!", "AAAA"] {
            let e = list_page(&f, WRITER, "*", t).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{t}");
        }
        let long = "A".repeat(MAX_TOKEN + 1);
        assert!(list_page(&f, WRITER, "*", &long).await.is_err());
    }
}
