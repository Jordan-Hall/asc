//! SQL of the review tables: threads, comments and review requests.

use std::collections::HashMap;

use asc_core::change::ChangeId;
use asc_rpc::server::wire;
use bytes::Bytes;
use sqlx::any::AnyRow;
use sqlx::{AnyConnection, Row};
use tonic::Status;

use super::{placeholders, sql};
use crate::changes::{index, versions};
use crate::registry::Ns;

const THREAD_COLS: &str = "id, change, path, version, side, start_line, end_line, status, \
     opened_by, created, updated, resolved_by, resolved_at";

pub(crate) const OPEN: i64 = wire::ThreadStatus::Open as i64;
pub(crate) const RESOLVED: i64 = wire::ThreadStatus::Resolved as i64;

fn u32_of(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

fn thread_of(r: &AnyRow) -> Result<wire::ReviewThread, sqlx::Error> {
    Ok(wire::ReviewThread {
        id: u64::try_from(r.try_get::<i64, _>("id")?).unwrap_or(0),
        change: Bytes::from(r.try_get::<Vec<u8>, _>("change")?),
        anchor: Some(wire::ThreadAnchor {
            path: Bytes::from(r.try_get::<Vec<u8>, _>("path")?),
            version: u32_of(r.try_get("version")?),
            side: i32::try_from(r.try_get::<i64, _>("side")?).unwrap_or(0),
            start_line: u32_of(r.try_get("start_line")?),
            end_line: u32_of(r.try_get("end_line")?),
        }),
        status: i32::try_from(r.try_get::<i64, _>("status")?).unwrap_or(0),
        opened_by: r.try_get("opened_by")?,
        created: r.try_get("created")?,
        updated: r.try_get("updated")?,
        resolved_by: r.try_get("resolved_by")?,
        resolved_at: r.try_get("resolved_at")?,
        comments: Vec::new(),
    })
}

fn ns_of(ns: Ns) -> Vec<u8> {
    ns.as_bytes().to_vec()
}

// ------------------------------------------------------------------------------ the change

/// Whether the change index knows `change`.
pub(crate) async fn change_known(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
) -> Result<bool, Status> {
    let known = index::changesets_of(&mut *conn, ns, change, 1)
        .await
        .map_err(|e| super::internal("change index", &e))?;
    Ok(!known.is_empty())
}

/// The actor that recorded the first version of `change`; its author for review purposes.
pub(crate) async fn first_actor(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
) -> Result<Option<String>, Status> {
    sqlx::query_scalar(
        "SELECT actor FROM change_versions WHERE ns = $1 AND change = $2 ORDER BY v LIMIT 1",
    )
    .bind(ns_of(ns))
    .bind(change.as_bytes().to_vec())
    .fetch_optional(&mut *conn)
    .await
    .map_err(sql)
}

/// The newest recorded version of `change`, 0 when it has none.
pub(crate) async fn latest_version(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
) -> Result<u32, Status> {
    let latest = versions::latest(conn, ns, change)
        .await
        .map_err(|e| super::internal("versions", &e))?;
    Ok(latest.map_or(0, |v| v.v))
}

// ------------------------------------------------------------------------------ threads

/// One thread with its comments.
pub(crate) async fn thread(
    conn: &mut AnyConnection,
    ns: Ns,
    id: i64,
) -> Result<Option<wire::ReviewThread>, sqlx::Error> {
    let q = format!("SELECT {THREAD_COLS} FROM review_threads WHERE ns = $1 AND id = $2");
    let Some(row) = sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(ns_of(ns))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    let mut t = thread_of(&row)?;
    let mut by = comments_of(conn, ns, &[id]).await?;
    t.comments = by.remove(&id).unwrap_or_default();
    Ok(Some(t))
}

/// The comments of `threads`, oldest first, grouped by thread.
async fn comments_of(
    conn: &mut AnyConnection,
    ns: Ns,
    threads: &[i64],
) -> Result<HashMap<i64, Vec<wire::ReviewComment>>, sqlx::Error> {
    let mut out: HashMap<i64, Vec<wire::ReviewComment>> = HashMap::new();
    for chunk in threads.chunks(100) {
        let q = format!(
            "SELECT id, thread, author, body, created, external_id, suggestion, applied_version, applied_by FROM review_comments \
             WHERE ns = $1 AND thread IN ({}) ORDER BY id",
            placeholders(2, chunk.len())
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns_of(ns));
        for t in chunk {
            query = query.bind(*t);
        }
        for r in query.fetch_all(&mut *conn).await? {
            out.entry(r.try_get("thread")?)
                .or_default()
                .push(wire::ReviewComment {
                    id: u64::try_from(r.try_get::<i64, _>("id")?).unwrap_or(0),
                    author: r.try_get("author")?,
                    body: r.try_get("body")?,
                    created: r.try_get("created")?,
                    external_id: r.try_get("external_id")?,
                    suggestion: r.try_get("suggestion")?,
                    applied_version: u32::try_from(r.try_get::<i64, _>("applied_version")?)
                        .unwrap_or(0),
                    applied_by: r.try_get("applied_by")?,
                });
        }
    }
    Ok(out)
}

/// A page of the threads of `change`: ids after `after`, oldest first, `limit + 1` read so the
/// caller can tell whether another page follows.
pub(crate) async fn threads_page(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
    status: i64,
    version: i64,
    after: i64,
    limit: usize,
) -> Result<Vec<wire::ReviewThread>, sqlx::Error> {
    let q = format!(
        "SELECT {THREAD_COLS} FROM review_threads WHERE ns = $1 AND change = $2 AND id > $3 \
         AND (CAST($4 AS BIGINT) = 0 OR status = $4) AND (CAST($5 AS BIGINT) = 0 OR version = $5) ORDER BY id LIMIT $6"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(ns_of(ns))
        .bind(change.as_bytes().to_vec())
        .bind(after)
        .bind(status)
        .bind(version)
        .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
        .fetch_all(&mut *conn)
        .await?;
    let mut threads = rows.iter().map(thread_of).collect::<Result<Vec<_>, _>>()?;
    let ids: Vec<i64> = threads
        .iter()
        .map(|t| i64::try_from(t.id).unwrap_or(0))
        .collect();
    let mut by = comments_of(conn, ns, &ids).await?;
    for t in &mut threads {
        t.comments = by
            .remove(&i64::try_from(t.id).unwrap_or(0))
            .unwrap_or_default();
    }
    Ok(threads)
}

/// Open threads and all threads of `change`.
pub(crate) async fn thread_counts(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
) -> Result<(i64, i64), sqlx::Error> {
    let row = sqlx::query(
        "SELECT CAST(COUNT(*) AS BIGINT) AS total, \
         CAST(COALESCE(SUM(CASE WHEN status = $3 THEN 1 ELSE 0 END), 0) AS BIGINT) AS open \
         FROM review_threads WHERE ns = $1 AND change = $2",
    )
    .bind(ns_of(ns))
    .bind(change.as_bytes().to_vec())
    .bind(OPEN)
    .fetch_one(&mut *conn)
    .await?;
    Ok((row.try_get("total")?, row.try_get("open")?))
}

/// The comment count of one thread.
pub(crate) async fn comment_count(
    conn: &mut AnyConnection,
    ns: Ns,
    thread: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM review_comments WHERE ns = $1 AND thread = $2",
    )
    .bind(ns_of(ns))
    .bind(thread)
    .fetch_one(&mut *conn)
    .await
}

/// `(change, opened_by, status)` of a thread, for authorisation before an update.
pub(crate) async fn thread_head(
    conn: &mut AnyConnection,
    ns: Ns,
    id: i64,
) -> Result<Option<(Vec<u8>, String, i64)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT change, opened_by, status FROM review_threads WHERE ns = $1 AND id = $2",
    )
    .bind(ns_of(ns))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok((
            r.try_get("change")?,
            r.try_get("opened_by")?,
            r.try_get("status")?,
        ))
    })
    .transpose()
}

pub(crate) struct NewThread<'a> {
    pub id: i64,
    pub change: ChangeId,
    pub path: &'a [u8],
    pub version: i64,
    pub side: i64,
    pub start_line: i64,
    pub end_line: i64,
    pub opened_by: &'a str,
    pub at: i64,
}

pub(crate) async fn insert_thread(
    conn: &mut AnyConnection,
    ns: Ns,
    t: &NewThread<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO review_threads (ns, id, change, path, version, side, start_line, end_line, \
         status, opened_by, created, updated) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11)",
    )
    .bind(ns_of(ns))
    .bind(t.id)
    .bind(t.change.as_bytes().to_vec())
    .bind(t.path.to_vec())
    .bind(t.version)
    .bind(t.side)
    .bind(t.start_line)
    .bind(t.end_line)
    .bind(OPEN)
    .bind(t.opened_by.to_owned())
    .bind(t.at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A comment to store.
pub(crate) struct NewComment<'a> {
    pub id: i64,
    pub thread: i64,
    pub author: &'a str,
    pub body: &'a str,
    pub at: i64,
    /// The host's own id of the comment, or empty.
    pub external_id: &'a str,
    pub suggestion: &'a str,
}

/// The `(thread, comment)` already stored under the host's `external_id`, if any.
pub(crate) async fn comment_by_external(
    conn: &mut AnyConnection,
    ns: Ns,
    external_id: &str,
) -> Result<Option<(i64, i64)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT thread, id FROM review_comments WHERE ns = $1 AND external_id = $2 ORDER BY id LIMIT 1",
    )
    .bind(ns_of(ns))
    .bind(external_id.to_owned())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| Ok((r.try_get("thread")?, r.try_get("id")?)))
        .transpose()
}

pub(crate) async fn insert_comment(
    conn: &mut AnyConnection,
    ns: Ns,
    c: &NewComment<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO review_comments (ns, id, thread, author, body, created, external_id, suggestion) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(ns_of(ns))
    .bind(c.id)
    .bind(c.thread)
    .bind(c.author.to_owned())
    .bind(c.body.to_owned())
    .bind(c.at)
    .bind(c.external_id.to_owned())
    .bind(c.suggestion.to_owned())
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE review_threads SET updated = $3 WHERE ns = $1 AND id = $2")
        .bind(ns_of(ns))
        .bind(c.thread)
        .bind(c.at)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Sets the status of a thread; `by` and `at` record who resolved it (empty and 0 when reopened).
pub(crate) async fn set_status(
    conn: &mut AnyConnection,
    ns: Ns,
    id: i64,
    status: i64,
    by: &str,
    at: i64,
) -> Result<(), sqlx::Error> {
    let (by, resolved_at) = if status == RESOLVED {
        (by, at)
    } else {
        ("", 0)
    };
    sqlx::query(
        "UPDATE review_threads SET status = $3, resolved_by = $4, resolved_at = $5, updated = $6 \
         WHERE ns = $1 AND id = $2",
    )
    .bind(ns_of(ns))
    .bind(id)
    .bind(status)
    .bind(by.to_owned())
    .bind(resolved_at)
    .bind(at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------------------ requests

/// The requests on `change`, oldest first.
pub(crate) async fn requests(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
) -> Result<Vec<wire::ReviewRequestInfo>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT reviewer, requested_by, requested_at, done_at FROM review_requests \
         WHERE ns = $1 AND change = $2 ORDER BY requested_at, reviewer",
    )
    .bind(ns_of(ns))
    .bind(change.as_bytes().to_vec())
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(wire::ReviewRequestInfo {
                reviewer: r.try_get("reviewer")?,
                requested_by: r.try_get("requested_by")?,
                requested_at: r.try_get("requested_at")?,
                done_at: r.try_get("done_at")?,
            })
        })
        .collect()
}

/// Asks `reviewer` to review `change`. A pending request stays as it is; a finished one is asked
/// again.
pub(crate) async fn request_review(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
    reviewer: &str,
    by: &str,
    at: i64,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE review_requests SET requested_by = $4, requested_at = $5, done_at = 0 \
         WHERE ns = $1 AND change = $2 AND reviewer = $3 AND done_at <> 0",
    )
    .bind(ns_of(ns))
    .bind(change.as_bytes().to_vec())
    .bind(reviewer.to_owned())
    .bind(by.to_owned())
    .bind(at)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO review_requests (ns, change, reviewer, requested_by, requested_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(ns_of(ns))
        .bind(change.as_bytes().to_vec())
        .bind(reviewer.to_owned())
        .bind(by.to_owned())
        .bind(at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub(crate) async fn remove_request(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
    reviewer: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM review_requests WHERE ns = $1 AND change = $2 AND reviewer = $3")
        .bind(ns_of(ns))
        .bind(change.as_bytes().to_vec())
        .bind(reviewer.to_owned())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Finishes the pending request of `reviewer` on `change`, if any: they have engaged.
pub(crate) async fn finish_request(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
    reviewer: &str,
    at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE review_requests SET done_at = $4 \
         WHERE ns = $1 AND change = $2 AND reviewer = $3 AND done_at = 0",
    )
    .bind(ns_of(ns))
    .bind(change.as_bytes().to_vec())
    .bind(reviewer.to_owned())
    .bind(at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(crate) async fn request_count(
    conn: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM review_requests WHERE ns = $1 AND change = $2",
    )
    .bind(ns_of(ns))
    .bind(change.as_bytes().to_vec())
    .fetch_one(&mut *conn)
    .await
}
