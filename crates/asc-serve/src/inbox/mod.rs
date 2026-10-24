//! The `Inbox` service (`inbox.proto`): the changes of one repository that need the caller's
//! attention, derived from asc's own review requests, comment threads and check runs.
//!
//! The inbox is a view, not a store: the only state it owns is the per-user pin and snooze in
//! `inbox_state`. A change appears in a section when:
//!
//! * `NEEDS_REVIEW`: a pending review request names the caller (finished by the caller's first
//!   comment on the change, see `review::rpc`);
//! * `RETURNED`: the caller authored the change (the actor of its first version) and someone else
//!   has an open thread on it;
//! * `CHECKS_FAILED`: the caller authored the change and its newest check run failed;
//! * `WAITING`: the caller has an open thread on a change they did not author.
//!
//! A change that already landed on the repository's default branch is in no section. Every source
//! query is bounded by [`SCAN`] rows, newest first, so a very busy repository shows the newest
//! items rather than slowing every poll.

pub mod rpc;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use asc_core::change::ChangeId;
use asc_rpc::server::wire;
use sqlx::{AnyConnection, Row};
use tonic::Status;

use crate::changes::index;
use crate::registry::RepoEntry;
use crate::review::{Reviews, handle_of, placeholders, sql, store};

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "inbox.v1";
/// Rows one source query reads.
pub const SCAN: i64 = 2_000;
/// Pinned or snoozed changes one user keeps per repository.
pub const MAX_STATE_ROWS: i64 = 1_000;
/// Items in one `List` page when the request names no limit.
pub const DEFAULT_PAGE: usize = 50;
/// Items in one `List` page, at most.
pub const MAX_PAGE: usize = 200;
/// Furthest a snooze may reach, in seconds from now (five years).
pub const MAX_SNOOZE: i64 = 5 * 365 * 24 * 3600;

type Key = Vec<u8>;

/// The per-user state of one change.
#[derive(Clone, Copy, Default)]
struct UserState {
    pinned: bool,
    snoozed_until: i64,
}

fn change_key(b: &[u8]) -> Option<ChangeId> {
    <[u8; 16]>::try_from(b).ok().map(ChangeId::from_bytes)
}

fn section_i(s: wire::InboxSection) -> i32 {
    s as i32
}

fn u32_of(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The sections a request names (all when it names none), in enum order; `UNSPECIFIED` is refused.
pub(crate) fn sections_of(asked: &[i32]) -> Result<Vec<wire::InboxSection>, Status> {
    let mut out = Vec::new();
    for s in asked {
        let s = wire::InboxSection::try_from(*s)
            .ok()
            .filter(|s| *s != wire::InboxSection::Unspecified)
            .ok_or_else(|| Status::invalid_argument("invalid inbox section"))?;
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() {
        out = vec![
            wire::InboxSection::NeedsReview,
            wire::InboxSection::Returned,
            wire::InboxSection::ChecksFailed,
            wire::InboxSection::Waiting,
        ];
    }
    out.sort_by_key(|s| section_i(*s));
    Ok(out)
}

/// A stable tag of `counts` for `If-None-Match`.
pub(crate) fn etag(counts: &[wire::InboxSectionCount]) -> String {
    let mut h = blake3::Hasher::new();
    for c in counts {
        h.update(&c.section.to_be_bytes());
        h.update(&c.count.to_be_bytes());
    }
    format!("\"{}\"", &h.finalize().to_hex()[..16])
}

/// The first-version actor of each of `changes`, as a handle (when it has one).
async fn authors(
    conn: &mut AnyConnection,
    ns: &[u8],
    changes: &HashSet<Key>,
) -> Result<HashMap<Key, String>, Status> {
    let all: Vec<&Key> = changes.iter().collect();
    let mut out = HashMap::new();
    for chunk in all.chunks(100) {
        let q = format!(
            "SELECT change, actor FROM change_versions WHERE ns = $1 AND v = 1 AND change IN ({})",
            placeholders(2, chunk.len())
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.to_vec());
        for c in chunk {
            query = query.bind((*c).clone());
        }
        for r in query.fetch_all(&mut *conn).await.map_err(sql)? {
            let change: Key = r.try_get("change").map_err(sql)?;
            let actor: String = r.try_get("actor").map_err(sql)?;
            if let Some(h) = handle_of(&actor) {
                out.insert(change, h);
            }
        }
    }
    Ok(out)
}

/// `(change, open thread count, newest update)` of the open threads matching `by_me`.
async fn open_threads(
    conn: &mut AnyConnection,
    ns: &[u8],
    handle: &str,
    by_me: bool,
) -> Result<Vec<(Key, i64, i64)>, Status> {
    let cmp = if by_me { "=" } else { "<>" };
    let q = format!(
        "SELECT change, CAST(COUNT(*) AS BIGINT) AS n, MAX(updated) AS updated \
         FROM review_threads WHERE ns = $1 AND status = $2 AND opened_by {cmp} $3 \
         GROUP BY change ORDER BY MAX(updated) DESC LIMIT $4"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(q))
        .bind(ns.to_vec())
        .bind(store::OPEN)
        .bind(handle.to_owned())
        .bind(SCAN)
        .fetch_all(&mut *conn)
        .await
        .map_err(sql)?;
    rows.iter()
        .map(|r| Ok((r.try_get("change")?, r.try_get("n")?, r.try_get("updated")?)))
        .collect::<Result<_, sqlx::Error>>()
        .map_err(sql)
}

impl Reviews {
    /// Every inbox item of `handle` in `entry`, snoozed ones included (they carry their time).
    pub(crate) async fn inbox_items(
        &self,
        entry: &RepoEntry,
        handle: &str,
    ) -> Result<Vec<wire::InboxItem>, Status> {
        let ns = entry.ns.as_bytes().to_vec();
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let conn = &mut *conn;

        // The four sources, before the authorship test.
        let requests = sqlx::query(
            "SELECT change, requested_by, requested_at FROM review_requests \
             WHERE ns = $1 AND reviewer = $2 AND done_at = 0 ORDER BY requested_at DESC LIMIT $3",
        )
        .bind(ns.clone())
        .bind(handle.to_owned())
        .bind(SCAN)
        .fetch_all(&mut *conn)
        .await
        .map_err(sql)?;
        let others = open_threads(conn, &ns, handle, false).await?;
        let mine = open_threads(conn, &ns, handle, true).await?;
        let failed = sqlx::query(
            "SELECT r.change AS change, r.created AS created FROM check_runs r \
             WHERE r.ns = $1 AND r.status = $2 AND r.change <> $3 \
             AND r.id = (SELECT MAX(r2.id) FROM check_runs r2 WHERE r2.ns = r.ns AND r2.change = r.change) \
             ORDER BY r.id DESC LIMIT $4",
        )
        .bind(ns.clone())
        .bind(wire::CiStatus::Failure as i64)
        .bind(Vec::<u8>::new())
        .bind(SCAN)
        .fetch_all(&mut *conn)
        .await
        .map_err(sql)?;

        let mut candidates: HashSet<Key> = HashSet::new();
        for r in &requests {
            candidates.insert(r.try_get("change").map_err(sql)?);
        }
        candidates.extend(others.iter().map(|t| t.0.clone()));
        candidates.extend(mine.iter().map(|t| t.0.clone()));
        for r in &failed {
            candidates.insert(r.try_get("change").map_err(sql)?);
        }
        let author_of = authors(conn, &ns, &candidates).await?;
        let is_mine = |c: &Key| author_of.get(c).map(String::as_str) == Some(handle);

        let mut items: Vec<wire::InboxItem> = Vec::new();
        let mut push = |change: &Key,
                        section: wire::InboxSection,
                        updated: i64,
                        threads: i64,
                        requested_by: String| {
            items.push(wire::InboxItem {
                change: change.clone().into(),
                section: section_i(section),
                author: author_of.get(change).cloned().unwrap_or_default(),
                updated,
                open_threads: u32_of(threads),
                requested_by,
                ..Default::default()
            });
        };
        for r in &requests {
            let change: Key = r.try_get("change").map_err(sql)?;
            push(
                &change,
                wire::InboxSection::NeedsReview,
                r.try_get("requested_at").map_err(sql)?,
                0,
                r.try_get("requested_by").map_err(sql)?,
            );
        }
        for (change, n, updated) in &others {
            if is_mine(change) {
                push(
                    change,
                    wire::InboxSection::Returned,
                    *updated,
                    *n,
                    String::new(),
                );
            }
        }
        for r in &failed {
            let change: Key = r.try_get("change").map_err(sql)?;
            if is_mine(&change) {
                push(
                    &change,
                    wire::InboxSection::ChecksFailed,
                    r.try_get("created").map_err(sql)?,
                    0,
                    String::new(),
                );
            }
        }
        for (change, n, updated) in &mine {
            if !is_mine(change) {
                push(
                    change,
                    wire::InboxSection::Waiting,
                    *updated,
                    *n,
                    String::new(),
                );
            }
        }

        // Landed changes need nobody; the rest learn their newest version and their user state.
        let land_ref = format!("refs/heads/{}", entry.default_branch);
        let mut keep: HashMap<Key, u32> = HashMap::new();
        let distinct: HashSet<Key> = items.iter().map(|i| i.change.to_vec()).collect();
        for c in distinct {
            let Some(id) = change_key(&c) else { continue };
            let landed = index::landed_as(&mut *conn, entry.ns, id, &land_ref)
                .await
                .map_err(|e| crate::review::internal("change index", &e))?;
            if landed.is_none() {
                let latest = store::latest_version(conn, entry.ns, id).await?;
                keep.insert(c, latest);
            }
        }
        items.retain(|i| keep.contains_key(i.change.as_ref()));
        let states = user_states(conn, &ns, handle).await?;
        for i in &mut items {
            i.latest_version = keep[i.change.as_ref()];
            if let Some(s) = states.get(i.change.as_ref()) {
                i.pinned = s.pinned;
                i.snoozed_until = s.snoozed_until;
            }
        }
        items.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.updated.cmp(&a.updated))
                .then_with(|| a.change.cmp(&b.change))
                .then(a.section.cmp(&b.section))
        });
        Ok(items)
    }
}

async fn user_states(
    conn: &mut AnyConnection,
    ns: &[u8],
    handle: &str,
) -> Result<HashMap<Key, UserState>, Status> {
    let rows = sqlx::query(
        "SELECT change, pinned, snoozed_until FROM inbox_state WHERE ns = $1 AND handle = $2",
    )
    .bind(ns.to_vec())
    .bind(handle.to_owned())
    .fetch_all(&mut *conn)
    .await
    .map_err(sql)?;
    let mut out = HashMap::new();
    for r in rows {
        let change: Key = r.try_get("change").map_err(sql)?;
        let pinned: i64 = r.try_get("pinned").map_err(sql)?;
        out.insert(
            change,
            UserState {
                pinned: pinned != 0,
                snoozed_until: r.try_get("snoozed_until").map_err(sql)?,
            },
        );
    }
    Ok(out)
}

/// Whether `item` shows at `now`: not snoozed, pinned, or the caller asked for snoozed items.
pub(crate) fn visible(item: &wire::InboxItem, now: i64, include_snoozed: bool) -> bool {
    include_snoozed || item.pinned || item.snoozed_until <= now
}
