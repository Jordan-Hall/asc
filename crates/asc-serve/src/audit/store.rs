//! Appending, reading and verifying audit events (docs/BUILDOUT.md section 3).
//!
//! Appends serialise per scope on the scope's `audit_heads` row: the append bumps the row's `seq`
//! (taking the row lock on Postgres; SQLite has one writer), reads the previous hash, inserts the
//! event and stores the new hash, all in the caller's transaction. A rolled-back action therefore
//! leaves neither a gap nor an event. Scopes are always locked in the same order (instance, owner,
//! repository) so two appends cannot deadlock.

use std::collections::BTreeMap;

use sqlx::any::{Any, AnyRow};
use sqlx::{AnyConnection, AnyPool, Row, Transaction};

use super::chain::{Anchor, Event, GENESIS, event_hash, verify};
use super::{
    Actor, AuditError, DEFAULT_PAGE, Details, MAX_PAGE, MAX_TARGET_BYTES, NewEvent, Outcome, Scope,
    ScopeKind, catalogue, meta, now_ms,
};

/// Rows read per statement while verifying and exporting.
pub const SCAN_PAGE: usize = 1000;

const COLUMNS: &str = "seq, id, time_ms, actor_kind, actor_name, on_behalf_kind, on_behalf_name, \
     action, repo, target, outcome, request_id, remote_addr, user_agent, details_json, prev_hash, hash";

/// Actor kinds the log knows.
const ACTOR_KINDS: &[&str] = &["user", "agent", "deploy", "ci", "host", "system"];

fn is_sqlite(pool: &AnyPool) -> bool {
    !matches!(
        pool.connect_options().database_url.scheme(),
        "postgres" | "postgresql"
    )
}

/// Starts a transaction that can append: SQLite takes its write lock up front so a read followed by
/// a write cannot fail with a lock upgrade error.
pub async fn begin(pool: &AnyPool) -> Result<Transaction<'static, Any>, sqlx::Error> {
    if is_sqlite(pool) {
        pool.begin_with("BEGIN IMMEDIATE").await
    } else {
        pool.begin().await
    }
}

/// Appends `event` in a transaction of its own.
pub async fn record(pool: &AnyPool, event: &NewEvent) -> Result<(), AuditError> {
    let mut tx = begin(pool).await?;
    append_in_tx(&mut tx, event).await?;
    tx.commit().await?;
    Ok(())
}

/// [`record`] for a path that must not fail because the log did: a failure is logged (the action
/// and the error, never the details).
pub async fn record_best_effort(pool: &AnyPool, event: &NewEvent) {
    if let Err(e) = record(pool, event).await {
        tracing::error!(action = %event.action, error = %e, "audit event not recorded");
    }
}

fn clean_field(s: &str, max: usize) -> String {
    let s = catalogue::plain(s);
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// An event after validation, before it has a place in a scope.
struct Prepared {
    template: Event,
    scopes: Vec<Scope>,
}

fn prepare(ev: &NewEvent) -> Result<Prepared, AuditError> {
    if catalogue::info(&ev.action).is_none() {
        return Err(AuditError::UnknownAction(ev.action.clone()));
    }
    let actor_ok = |a: &Actor| {
        ACTOR_KINDS.contains(&a.kind.as_str()) && !a.name.is_empty() && a.name.len() <= 255
    };
    if !actor_ok(&ev.actor) || ev.on_behalf_of.as_ref().is_some_and(|a| !actor_ok(a)) {
        return Err(AuditError::Invalid("actor"));
    }
    if let Some(r) = &ev.repo
        && asc_rpc::server::RepoName::parse(r).is_err()
    {
        return Err(AuditError::Invalid("repo"));
    }
    if let Some(o) = &ev.owner
        && !crate::review::valid_handle(o)
    {
        return Err(AuditError::Invalid("owner"));
    }
    let meta = ev.meta.clone().unwrap_or_else(meta::current);
    let template = Event {
        seq: 0,
        id: uuid::Uuid::new_v4().to_string(),
        time_ms: ev.time_ms.unwrap_or_else(now_ms),
        actor: ev.actor.clone(),
        on_behalf_of: ev.on_behalf_of.clone(),
        action: ev.action.clone(),
        repo: ev.repo.clone().unwrap_or_default(),
        target: clean_field(&ev.target, MAX_TARGET_BYTES),
        outcome: ev.outcome,
        request_id: clean_field(&meta.request_id, meta::MAX_REQUEST_ID),
        remote_addr: clean_field(&meta.remote_addr, 64),
        user_agent: meta::clean_user_agent(&meta.user_agent),
        details: catalogue::sanitize_details(&ev.action, &ev.details),
        prev_hash: GENESIS,
        hash: GENESIS,
    };
    let mut scopes = ev.scopes();
    scopes.sort();
    scopes.dedup();
    Ok(Prepared { template, scopes })
}

fn json_of(details: &Details) -> String {
    serde_json::to_string(details).unwrap_or_else(|_| "{}".into())
}

/// Appends `event` to every scope it belongs to, inside the caller's transaction. Returns the
/// `(scope, seq)` pairs written.
pub async fn append_in_tx(
    conn: &mut AnyConnection,
    event: &NewEvent,
) -> Result<Vec<(Scope, u64)>, AuditError> {
    let prepared = prepare(event)?;
    let mut written = Vec::with_capacity(prepared.scopes.len());
    for scope in &prepared.scopes {
        let (kind, name) = (scope.kind.as_str(), scope.name.as_str());
        sqlx::query(
            "INSERT INTO audit_heads (scope_kind, scope_name, seq, hash) VALUES ($1, $2, 0, $3) \
             ON CONFLICT (scope_kind, scope_name) DO NOTHING",
        )
        .bind(kind)
        .bind(name)
        .bind(GENESIS.to_vec())
        .execute(&mut *conn)
        .await?;
        // Bumping the row first takes its lock; the read below then sees the committed head.
        sqlx::query(
            "UPDATE audit_heads SET seq = seq + 1 WHERE scope_kind = $1 AND scope_name = $2",
        )
        .bind(kind)
        .bind(name)
        .execute(&mut *conn)
        .await?;
        let row = sqlx::query(
            "SELECT seq, hash FROM audit_heads WHERE scope_kind = $1 AND scope_name = $2",
        )
        .bind(kind)
        .bind(name)
        .fetch_one(&mut *conn)
        .await?;
        let seq: i64 = row.try_get("seq")?;
        let prev = hash_of(&row, "hash")?;
        let mut e = prepared.template.clone();
        e.seq = u64::try_from(seq).map_err(|_| AuditError::Corrupt("sequence number"))?;
        e.prev_hash = prev;
        e.hash = event_hash(&prev, scope, &e);
        let (behalf_kind, behalf_name) = e
            .on_behalf_of
            .as_ref()
            .map_or((String::new(), String::new()), |a| {
                (a.kind.clone(), a.name.clone())
            });
        sqlx::query(
            "INSERT INTO audit_events (scope_kind, scope_name, seq, id, time_ms, actor_kind, \
             actor_name, on_behalf_kind, on_behalf_name, action, repo, target, outcome, request_id, \
             remote_addr, user_agent, details_json, prev_hash, hash) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)",
        )
        .bind(kind)
        .bind(name)
        .bind(seq)
        .bind(e.id.clone())
        .bind(e.time_ms)
        .bind(e.actor.kind.clone())
        .bind(e.actor.name.clone())
        .bind(behalf_kind)
        .bind(behalf_name)
        .bind(e.action.clone())
        .bind(e.repo.clone())
        .bind(e.target.clone())
        .bind(i64::from(e.outcome.code()))
        .bind(e.request_id.clone())
        .bind(e.remote_addr.clone())
        .bind(e.user_agent.clone())
        .bind(json_of(&e.details))
        .bind(e.prev_hash.to_vec())
        .bind(e.hash.to_vec())
        .execute(&mut *conn)
        .await?;
        sqlx::query("UPDATE audit_heads SET hash = $3 WHERE scope_kind = $1 AND scope_name = $2")
            .bind(kind)
            .bind(name)
            .bind(e.hash.to_vec())
            .execute(&mut *conn)
            .await?;
        written.push((scope.clone(), e.seq));
    }
    Ok(written)
}

fn hash_of(row: &AnyRow, col: &str) -> Result<[u8; 32], AuditError> {
    let bytes: Vec<u8> = row.try_get(col)?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| AuditError::Corrupt("hash length"))
}

fn event_of(row: &AnyRow) -> Result<Event, AuditError> {
    let seq: i64 = row.try_get("seq")?;
    let behalf_kind: String = row.try_get("on_behalf_kind")?;
    let behalf_name: String = row.try_get("on_behalf_name")?;
    let outcome: i64 = row.try_get("outcome")?;
    let details_json: String = row.try_get("details_json")?;
    let details: BTreeMap<String, String> =
        serde_json::from_str(&details_json).map_err(|_| AuditError::Corrupt("details"))?;
    Ok(Event {
        seq: u64::try_from(seq).map_err(|_| AuditError::Corrupt("sequence number"))?,
        id: row.try_get("id")?,
        time_ms: row.try_get("time_ms")?,
        actor: Actor {
            kind: row.try_get("actor_kind")?,
            name: row.try_get("actor_name")?,
        },
        on_behalf_of: (!behalf_name.is_empty()).then_some(Actor {
            kind: behalf_kind,
            name: behalf_name,
        }),
        action: row.try_get("action")?,
        repo: row.try_get("repo")?,
        target: row.try_get("target")?,
        outcome: Outcome::from_code(outcome).ok_or(AuditError::Corrupt("outcome"))?,
        request_id: row.try_get("request_id")?,
        remote_addr: row.try_get("remote_addr")?,
        user_agent: row.try_get("user_agent")?,
        details,
        prev_hash: hash_of(row, "prev_hash")?,
        hash: hash_of(row, "hash")?,
    })
}

// ------------------------------------------------------------------------------ reading

/// What `List` and `Export` narrow by.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// `name` or `kind:name`.
    pub actors: Vec<String>,
    /// Exact action names, or a prefix ending in `.*`.
    pub actions: Vec<String>,
    pub outcome: Option<Outcome>,
    /// Matches `target` as a prefix.
    pub target: String,
    pub since_ms: i64,
    pub until_ms: i64,
}

enum Bind {
    Text(String),
    Int(i64),
}

struct Where {
    sql: String,
    binds: Vec<Bind>,
}

impl Where {
    fn new(scope: &Scope) -> Self {
        let mut w = Self {
            sql: String::new(),
            binds: Vec::new(),
        };
        let (k, n) = (
            w.bind(Bind::Text(scope.kind.as_str().to_owned())),
            w.bind(Bind::Text(scope.name.clone())),
        );
        w.sql = format!("scope_kind = {k} AND scope_name = {n}");
        w
    }

    fn bind(&mut self, b: Bind) -> String {
        self.binds.push(b);
        format!("${}", self.binds.len())
    }

    fn and(&mut self, clause: &str) {
        self.sql.push_str(" AND ");
        self.sql.push_str(clause);
    }

    /// `substr(col, 1, n) = prefix`: a prefix test that is case sensitive on both databases.
    fn prefix(&mut self, col: &str, prefix: &str) {
        let len = self.bind(Bind::Int(
            i64::try_from(prefix.chars().count()).unwrap_or(0),
        ));
        let text = self.bind(Bind::Text(prefix.to_owned()));
        self.and(&format!(
            "substr({col}, 1, CAST({len} AS INTEGER)) = {text}"
        ));
    }
}

fn filtered(scope: &Scope, f: &Filter) -> Where {
    let mut w = Where::new(scope);
    if !f.actors.is_empty() {
        let mut ors = Vec::new();
        for a in &f.actors {
            let p = w.bind(Bind::Text(a.clone()));
            ors.push(format!(
                "(actor_name = {p} OR actor_kind || ':' || actor_name = {p})"
            ));
        }
        w.and(&format!("({})", ors.join(" OR ")));
    }
    if !f.actions.is_empty() {
        let mut ors = Vec::new();
        for a in &f.actions {
            if let Some(prefix) = a.strip_suffix('*') {
                let len = w.bind(Bind::Int(
                    i64::try_from(prefix.chars().count()).unwrap_or(0),
                ));
                let text = w.bind(Bind::Text(prefix.to_owned()));
                ors.push(format!(
                    "substr(action, 1, CAST({len} AS INTEGER)) = {text}"
                ));
            } else {
                let p = w.bind(Bind::Text(a.clone()));
                ors.push(format!("action = {p}"));
            }
        }
        w.and(&format!("({})", ors.join(" OR ")));
    }
    if let Some(o) = f.outcome {
        let p = w.bind(Bind::Int(i64::from(o.code())));
        w.and(&format!("outcome = {p}"));
    }
    if !f.target.is_empty() {
        w.prefix("target", &f.target);
    }
    if f.since_ms > 0 {
        let p = w.bind(Bind::Int(f.since_ms));
        w.and(&format!("time_ms >= {p}"));
    }
    if f.until_ms > 0 {
        let p = w.bind(Bind::Int(f.until_ms));
        w.and(&format!("time_ms < {p}"));
    }
    w
}

/// One page of events.
#[derive(Debug, Default)]
pub struct Page {
    pub events: Vec<Event>,
    /// The `after` to pass for the next page; `None` on the last.
    pub next: Option<u64>,
}

/// Events of `scope` that match `filter`, newest first (or oldest first), `limit` at a time
/// (0 means [`DEFAULT_PAGE`], at most [`MAX_PAGE`]). `after` is the `next` of the page before.
pub async fn list(
    pool: &AnyPool,
    scope: &Scope,
    filter: &Filter,
    oldest_first: bool,
    limit: usize,
    after: Option<u64>,
) -> Result<Page, AuditError> {
    let limit = match limit {
        0 => DEFAULT_PAGE,
        n => n.min(MAX_PAGE),
    };
    page(pool, scope, filter, oldest_first, limit, after).await
}

/// [`list`] without the page-size clamp, for the scans of export and verification.
pub(crate) async fn page(
    pool: &AnyPool,
    scope: &Scope,
    filter: &Filter,
    oldest_first: bool,
    limit: usize,
    after: Option<u64>,
) -> Result<Page, AuditError> {
    let mut w = filtered(scope, filter);
    if let Some(a) = after {
        let p = w.bind(Bind::Int(i64::try_from(a).unwrap_or(i64::MAX)));
        w.and(&format!("seq {} {p}", if oldest_first { ">" } else { "<" }));
    }
    let fetch = w.bind(Bind::Int(i64::try_from(limit + 1).unwrap_or(i64::MAX)));
    let sql = format!(
        "SELECT {COLUMNS} FROM audit_events WHERE {} ORDER BY seq {} LIMIT {fetch}",
        w.sql,
        if oldest_first { "ASC" } else { "DESC" }
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for b in w.binds {
        q = match b {
            Bind::Text(s) => q.bind(s),
            Bind::Int(i) => q.bind(i),
        };
    }
    let rows = q.fetch_all(pool).await?;
    let mut events = rows.iter().map(event_of).collect::<Result<Vec<_>, _>>()?;
    let next = if events.len() > limit {
        events.truncate(limit);
        events.last().map(|e| e.seq)
    } else {
        None
    };
    Ok(Page { events, next })
}

/// Every matching event, in order, `SCAN_PAGE` rows per statement, at most `max` events. The
/// callback returns false to stop.
pub async fn scan(
    pool: &AnyPool,
    scope: &Scope,
    filter: &Filter,
    oldest_first: bool,
    max: u64,
    mut each: impl FnMut(&Event) -> bool,
) -> Result<u64, AuditError> {
    let mut after = None;
    let mut seen = 0u64;
    loop {
        let want = usize::try_from(max.saturating_sub(seen))
            .unwrap_or(usize::MAX)
            .min(SCAN_PAGE);
        if want == 0 {
            return Ok(seen);
        }
        let p = page(pool, scope, filter, oldest_first, want, after).await?;
        for e in &p.events {
            seen += 1;
            if !each(e) {
                return Ok(seen);
            }
        }
        match p.next {
            Some(n) => after = Some(n),
            None => return Ok(seen),
        }
    }
}

/// The newest sequence number and hash of a scope, or `None` when it has no event.
pub async fn head(pool: &AnyPool, scope: &Scope) -> Result<Option<(u64, [u8; 32])>, AuditError> {
    let row =
        sqlx::query("SELECT seq, hash FROM audit_heads WHERE scope_kind = $1 AND scope_name = $2")
            .bind(scope.kind.as_str())
            .bind(&scope.name)
            .fetch_optional(pool)
            .await?;
    row.map(|r| {
        let seq: i64 = r.try_get("seq")?;
        Ok((
            u64::try_from(seq).map_err(|_| AuditError::Corrupt("sequence number"))?,
            hash_of(&r, "hash")?,
        ))
    })
    .transpose()
    .map(|h: Option<(u64, [u8; 32])>| h.filter(|(seq, _)| *seq > 0))
}

/// The segment retention left for a scope: `(to_seq, head_hash)`.
pub async fn segment(pool: &AnyPool, scope: &Scope) -> Result<Option<(u64, [u8; 32])>, AuditError> {
    let row = sqlx::query(
        "SELECT to_seq, head_hash FROM audit_segments WHERE scope_kind = $1 AND scope_name = $2",
    )
    .bind(scope.kind.as_str())
    .bind(&scope.name)
    .fetch_optional(pool)
    .await?;
    row.map(|r| {
        let seq: i64 = r.try_get("to_seq")?;
        Ok((
            u64::try_from(seq).map_err(|_| AuditError::Corrupt("sequence number"))?,
            hash_of(&r, "head_hash")?,
        ))
    })
    .transpose()
}

// ------------------------------------------------------------------------------ verification

/// The answer of [`verify_chain`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    pub ok: bool,
    pub checked: u64,
    /// 0 when `ok`.
    pub first_bad_seq: u64,
    pub reason: String,
    pub head_hash: [u8; 32],
}

/// Checks the chain of `scope` from `from_seq` (0 or 1: the start; after retention, the first
/// event left) to `to_seq` (0: the newest). A check to the newest event also compares the last
/// event with the scope's head, so a removed tail is found.
pub async fn verify_chain(
    pool: &AnyPool,
    scope: &Scope,
    from_seq: u64,
    to_seq: u64,
) -> Result<Verification, AuditError> {
    let bad = |seq: u64, reason: &str, head_hash: [u8; 32], checked: u64| Verification {
        ok: false,
        checked,
        first_bad_seq: seq,
        reason: reason.to_owned(),
        head_hash,
    };
    let head = head(pool, scope).await?;
    let (base_seq, base_prev) = match segment(pool, scope).await? {
        Some((to, hash)) => (to + 1, hash),
        None => (1, GENESIS),
    };
    let start = from_seq.max(base_seq);
    let mut anchor = if start == base_seq {
        Anchor {
            seq: base_seq,
            prev: base_prev,
        }
    } else {
        // A slice: it starts from the stored hash of the event before it.
        let row = sqlx::query(
            "SELECT hash FROM audit_events WHERE scope_kind = $1 AND scope_name = $2 AND seq = $3",
        )
        .bind(scope.kind.as_str())
        .bind(&scope.name)
        .bind(i64::try_from(start - 1).unwrap_or(i64::MAX))
        .fetch_optional(pool)
        .await?;
        match row {
            Some(r) => Anchor {
                seq: start,
                prev: hash_of(&r, "hash")?,
            },
            None => {
                return Ok(bad(
                    start - 1,
                    "the event before the range is missing",
                    GENESIS,
                    0,
                ));
            }
        }
    };
    let mut checked = 0u64;
    let mut after = start.checked_sub(1).filter(|s| *s > 0);
    loop {
        let mut w = Where::new(scope);
        if let Some(a) = after {
            let p = w.bind(Bind::Int(i64::try_from(a).unwrap_or(i64::MAX)));
            w.and(&format!("seq > {p}"));
        }
        if to_seq > 0 {
            let p = w.bind(Bind::Int(i64::try_from(to_seq).unwrap_or(i64::MAX)));
            w.and(&format!("seq <= {p}"));
        }
        let lim = w.bind(Bind::Int(i64::try_from(SCAN_PAGE).unwrap_or(1000)));
        let sql = format!(
            "SELECT {COLUMNS} FROM audit_events WHERE {} ORDER BY seq ASC LIMIT {lim}",
            w.sql
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for b in w.binds {
            q = match b {
                Bind::Text(s) => q.bind(s),
                Bind::Int(i) => q.bind(i),
            };
        }
        let rows = q.fetch_all(pool).await?;
        if rows.is_empty() {
            break;
        }
        let events = rows.iter().map(event_of).collect::<Result<Vec<_>, _>>()?;
        match verify(scope, anchor, &events) {
            Ok((last_hash, n)) => {
                checked += n;
                anchor = Anchor {
                    seq: anchor.seq + n,
                    prev: last_hash,
                };
            }
            Err(b) => {
                return Ok(bad(b.seq, &b.reason, anchor.prev, checked + b.checked));
            }
        }
        after = events.last().map(|e| e.seq);
        if events.len() < SCAN_PAGE {
            break;
        }
    }
    let last_seq = anchor.seq - 1;
    if to_seq == 0 {
        match head {
            None if last_seq >= base_seq => {
                return Ok(bad(
                    last_seq,
                    "events exist but the scope has no head",
                    anchor.prev,
                    checked,
                ));
            }
            Some((hseq, hhash)) => {
                if hseq > last_seq {
                    return Ok(bad(
                        last_seq + 1,
                        &format!("events after {last_seq} are missing"),
                        anchor.prev,
                        checked,
                    ));
                }
                if hseq < last_seq {
                    return Ok(bad(
                        hseq + 1,
                        "events were written past the head of the scope",
                        anchor.prev,
                        checked,
                    ));
                }
                if hhash != anchor.prev {
                    return Ok(bad(
                        hseq,
                        "the last event does not match the head of the scope",
                        anchor.prev,
                        checked,
                    ));
                }
            }
            None => {}
        }
    }
    Ok(Verification {
        ok: true,
        checked,
        first_bad_seq: 0,
        reason: String::new(),
        head_hash: anchor.prev,
    })
}

/// Every scope that has events, for the retention worker.
pub async fn scopes(pool: &AnyPool) -> Result<Vec<Scope>, AuditError> {
    let rows = sqlx::query(
        "SELECT scope_kind, scope_name FROM audit_heads ORDER BY scope_kind, scope_name",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let kind: String = r.try_get("scope_kind")?;
        out.push(Scope {
            kind: ScopeKind::parse(&kind).ok_or(AuditError::Corrupt("scope kind"))?,
            name: r.try_get("scope_name")?,
        });
    }
    Ok(out)
}
