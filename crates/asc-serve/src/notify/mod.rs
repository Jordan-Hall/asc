//! Notification events for the hub (docs/BUILDOUT.md section 21).
//!
//! asc serve knows when an agent pauses on a question, a run ends, checks fail, a restack hits a
//! conflict, the merge queue lands or drops a change, a policy changes, a review is asked for or a
//! person is mentioned. The hub knows who may be told and how (inbox, email, quiet hours). This
//! module is the seam: [`emit`] writes one row into `notify_events` and returns; the
//! [`worker::Dispatcher`] posts the rows to the hub's `POST /api/internal/asc/event` with the
//! internal token and retries a failed row with a backoff until the hub answers or the row is a
//! day old.
//!
//! * **Best effort for the caller.** A call never fails because a notification could not be
//!   written; the error is logged.
//! * **The hub's limits are enforced here first.** At most [`MAX_USERS`] logins per row (a longer
//!   list is split into several rows), a title of at most [`MAX_TITLE`] bytes, a detail of at most
//!   [`MAX_DETAIL`] bytes, no line breaks or control characters, and a path of this hub that starts
//!   with one `/`. A row that would still be refused (no title, no valid login, a name that is not
//!   `owner/name`) is not written at all.
//! * **Nothing is told twice for one cause.** A `dedup` key is unique per repository; the second
//!   [`emit`] with the same key does nothing.
//! * **Bounded.** A repository has at most [`MAX_PENDING`] undelivered rows; later events are
//!   dropped (and logged) until the hub takes some. Rows leave with their repository.

pub mod policy;
pub mod worker;

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod tests_hooks;

use sqlx::{AnyPool, Row};

use crate::outbox::now;
use crate::review::valid_handle;

/// Most logins one hub call may name.
pub const MAX_USERS: usize = 50;
/// Longest title, in bytes.
pub const MAX_TITLE: usize = 200;
/// Longest detail, in bytes.
pub const MAX_DETAIL: usize = 500;
/// Longest path, in bytes.
pub const MAX_PATH: usize = 300;
/// Undelivered rows one repository may hold.
pub const MAX_PENDING: i64 = 2000;
/// Rows written for one [`emit`] at most (a very long list of people is cut after this many rows).
const MAX_CHUNKS: usize = 8;

/// The events people can be told about; the names are the hub's (`services/ascnotify`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    AgentQuestion,
    AgentFinished,
    ChecksFailed,
    RestackConflict,
    MergeQueue,
    PolicyChanged,
    ReviewRequested,
    Mentioned,
}

impl Kind {
    pub const ALL: [Kind; 8] = [
        Kind::AgentQuestion,
        Kind::AgentFinished,
        Kind::ChecksFailed,
        Kind::RestackConflict,
        Kind::MergeQueue,
        Kind::PolicyChanged,
        Kind::ReviewRequested,
        Kind::Mentioned,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::AgentQuestion => "agent_question",
            Kind::AgentFinished => "agent_finished",
            Kind::ChecksFailed => "checks_failed",
            Kind::RestackConflict => "restack_conflict",
            Kind::MergeQueue => "merge_queue",
            Kind::PolicyChanged => "policy_changed",
            Kind::ReviewRequested => "review_requested",
            Kind::Mentioned => "mentioned",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }
}

/// Something to tell people about. Build it with [`Event::new`] and the chained setters; [`emit`]
/// cleans every field.
#[derive(Clone, Debug)]
pub struct Event {
    pub kind: Kind,
    pub users: Vec<String>,
    pub title: String,
    pub detail: String,
    pub path: String,
    /// A key that is unique per repository: an event with a key that was emitted before is dropped.
    /// Empty: always written.
    pub dedup: String,
}

impl Event {
    pub fn new(kind: Kind, title: impl Into<String>) -> Self {
        Self {
            kind,
            users: Vec::new(),
            title: title.into(),
            detail: String::new(),
            path: String::new(),
            dedup: String::new(),
        }
    }

    /// The people the event is for; the hub drops those who cannot read the repository.
    #[must_use]
    pub fn users<I, S>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.users
            .extend(users.into_iter().map(|u| u.as_ref().to_owned()));
        self
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    #[must_use]
    pub fn dedup(mut self, key: impl Into<String>) -> Self {
        self.dedup = key.into();
        self
    }

    /// Leaves out `who` (the person who did it needs no notice).
    #[must_use]
    pub fn except(mut self, who: &str) -> Self {
        self.users.retain(|u| u != who);
        self
    }
}

/// `[notify]`: whether events are queued, and where they link to on the hub. The hub publishes the
/// real route table; until a deployment sets these, the defaults are asc's guesses.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotifyConfig {
    /// `false` queues nothing. Events are queued only when `[forgejo]` with its internal token
    /// names a hub to post them to, whatever this says.
    pub enabled: bool,
    /// Put before every path: empty, or `/` and a path without a closing `/` (a hub served under
    /// a sub-path).
    pub path_prefix: String,
    /// Templates of the links. `{repo}` is `owner/name`, `{change}` the change id in hex and
    /// `{run}` the agent run number.
    pub change_path: String,
    pub run_path: String,
    pub queue_path: String,
    pub policy_path: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path_prefix: String::new(),
            change_path: "/{repo}/changes/{change}".into(),
            run_path: "/{repo}/agents/runs/{run}".into(),
            queue_path: "/{repo}/merge-queue".into(),
            policy_path: "/{repo}/settings/policy".into(),
        }
    }
}

impl NotifyConfig {
    /// Checks the templates. The error names the key.
    pub fn validate(&self) -> Result<(), String> {
        let p = &self.path_prefix;
        if !p.is_empty() && (!p.starts_with('/') || p.ends_with('/') || p.contains("//")) {
            return Err("path_prefix is empty or starts with `/` and does not end with one".into());
        }
        if p.len() > 100
            || p.chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
        {
            return Err("path_prefix is at most 100 characters without spaces".into());
        }
        for (key, t) in [
            ("change_path", &self.change_path),
            ("run_path", &self.run_path),
            ("queue_path", &self.queue_path),
            ("policy_path", &self.policy_path),
        ] {
            let bare = t
                .replace("{repo}", "")
                .replace("{change}", "")
                .replace("{run}", "");
            let ok = t.starts_with('/')
                && !t.starts_with("//")
                && t.len() <= 200
                && !bare.contains(['{', '}'])
                && !t
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() || c == '\\');
            if !ok {
                return Err(format!(
                    "{key} starts with one `/`, has no spaces and uses only {{repo}}, {{change}} and {{run}}"
                ));
            }
            if !t.contains("{repo}") {
                return Err(format!("{key} must contain {{repo}}"));
            }
        }
        Ok(())
    }

    fn render(&self, template: &str, repo: &str, change: &str, run: &str) -> String {
        format!(
            "{}{}",
            self.path_prefix,
            template
                .replace("{repo}", repo)
                .replace("{change}", change)
                .replace("{run}", run)
        )
    }
}

static SETTINGS: std::sync::OnceLock<(NotifyConfig, bool)> = std::sync::OnceLock::new();

/// Sets the process's notification settings at startup: the config, and whether a hub exists to
/// post to. Without a hub nothing is queued. The first call wins.
pub fn configure(cfg: &NotifyConfig, hub: bool) {
    let _ = SETTINGS.set((cfg.clone(), hub));
}

fn settings() -> NotifyConfig {
    SETTINGS
        .get()
        .map_or_else(NotifyConfig::default, |(c, _)| c.clone())
}

fn queueing() -> bool {
    SETTINGS.get().is_none_or(|(c, hub)| c.enabled && *hub)
}

/// The places a notification links to, on the hub (see [`NotifyConfig`]).
pub mod paths {
    use super::settings;

    pub fn change(repo: &str, change_hex: &str) -> String {
        let c = settings();
        c.render(&c.change_path, repo, change_hex, "")
    }

    pub fn run(repo: &str, run: u64) -> String {
        let c = settings();
        c.render(&c.run_path, repo, "", &run.to_string())
    }

    pub fn queue(repo: &str) -> String {
        let c = settings();
        c.render(&c.queue_path, repo, "", "")
    }

    pub fn policy(repo: &str) -> String {
        let c = settings();
        c.render(&c.policy_path, repo, "", "")
    }
}

/// One line of at most `max` bytes (cut on a character boundary): line breaks, tabs and other
/// control characters become single spaces, and runs of spaces collapse.
pub fn one_line(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    let mut space = true; // drops leading spaces
    for c in s.chars() {
        let c = if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
            ' '
        } else {
            c
        };
        if c == ' ' {
            if space {
                continue;
            }
            space = true;
        } else {
            space = false;
        }
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// The path the hub accepts: starts with exactly one `/`, at most [`MAX_PATH`] bytes, no spaces,
/// backslashes or control characters. Anything else gives no path (the hub then links the repository).
fn clean_path(p: &str) -> String {
    let ok = p.starts_with('/')
        && !p.starts_with("//")
        && p.len() <= MAX_PATH
        && !p
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\');
    if ok { p.to_owned() } else { String::new() }
}

/// A repository name the hub understands: `owner/name`, nothing else.
fn hub_repo(repo: &str) -> bool {
    match repo.split_once('/') {
        Some((o, n)) => !o.is_empty() && !n.is_empty() && !n.contains('/') && repo.len() <= 200,
        None => false,
    }
}

/// An event after cleaning: the fields as they are stored and sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cleaned {
    pub kind: Kind,
    pub users: Vec<String>,
    pub title: String,
    pub detail: String,
    pub path: String,
    pub dedup: String,
}

/// Cleans `ev`; `None` when nothing could be sent (no title, no valid login).
pub fn clean(ev: Event) -> Option<Cleaned> {
    let title = one_line(&ev.title, MAX_TITLE);
    if title.is_empty() {
        return None;
    }
    let mut users: Vec<String> = Vec::new();
    for u in ev.users {
        if valid_handle(&u) && !users.contains(&u) {
            users.push(u);
        }
    }
    if users.is_empty() {
        return None;
    }
    Some(Cleaned {
        kind: ev.kind,
        users,
        title,
        detail: one_line(&ev.detail, MAX_DETAIL),
        path: clean_path(&ev.path),
        dedup: one_line(&ev.dedup, 200),
    })
}

/// Writes `ev` for the repository `repo` (`owner/name`) and returns. Failures are logged; the
/// operation that caused the event goes on.
pub async fn emit(pool: &AnyPool, repo: &str, ev: Event) {
    let kind = ev.kind;
    if let Err(e) = record(pool, repo, ev).await {
        tracing::warn!(event = "notify.emit", kind = kind.name(), error = %e, "notification not queued");
    }
}

/// [`emit`] with the error returned: how many rows were written.
pub async fn record(pool: &AnyPool, repo: &str, ev: Event) -> Result<usize, sqlx::Error> {
    if !queueing() || !hub_repo(repo) {
        return Ok(0);
    }
    let Some(c) = clean(ev) else {
        return Ok(0);
    };
    let Some(ns) = ns_of(pool, repo).await? else {
        return Ok(0);
    };
    let pending: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM notify_events WHERE ns = $1 AND state = 0",
    )
    .bind(ns.clone())
    .fetch_one(pool)
    .await?;
    if pending >= MAX_PENDING {
        tracing::warn!(
            event = "notify.full",
            repo,
            pending,
            "notification dropped: the hub has not taken the earlier ones"
        );
        return Ok(0);
    }
    let mut written = 0;
    for (i, chunk) in c.users.chunks(MAX_USERS).take(MAX_CHUNKS).enumerate() {
        let dedup = match (c.dedup.is_empty(), i) {
            (true, _) => String::new(),
            (false, 0) => c.dedup.clone(),
            (false, n) => format!("{}#{n}", c.dedup),
        };
        if insert(pool, &ns, &c, chunk, &dedup).await? {
            written += 1;
        }
    }
    Ok(written)
}

/// The namespace of a repository by name. Agent rows keep the name in lower case, so a name that
/// does not match exactly matches by case only when that names one repository.
pub async fn ns_of(pool: &AnyPool, repo: &str) -> Result<Option<Vec<u8>>, sqlx::Error> {
    let rows = sqlx::query("SELECT name, ns FROM repos WHERE LOWER(name) = LOWER($1) LIMIT 2")
        .bind(repo)
        .fetch_all(pool)
        .await?;
    let mut found: Vec<(String, Vec<u8>)> = Vec::new();
    for r in rows {
        found.push((r.try_get("name")?, r.try_get("ns")?));
    }
    Ok(match found.len() {
        0 => None,
        1 => found.pop().map(|(_, ns)| ns),
        _ => found.into_iter().find(|(n, _)| n == repo).map(|(_, ns)| ns),
    })
}

/// The people who made versions of a change (the logins of the principals that published them).
pub async fn authors_of(pool: &AnyPool, ns: &[u8], change: &[u8]) -> Vec<String> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT actor FROM change_versions WHERE ns = $1 AND change = $2",
    )
    .bind(ns.to_vec())
    .bind(change.to_vec())
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut out: Vec<String> = rows
        .iter()
        .filter_map(|a| crate::review::handle_of(a))
        .collect();
    out.sort();
    out
}

/// Inserts one row; `false` when the dedup key exists already.
async fn insert(
    pool: &AnyPool,
    ns: &[u8],
    c: &Cleaned,
    users: &[String],
    dedup: &str,
) -> Result<bool, sqlx::Error> {
    let users_json = serde_json::to_string(users).unwrap_or_else(|_| "[]".into());
    let at = now();
    for _ in 0..8 {
        if !dedup.is_empty() {
            let seen: i64 = sqlx::query_scalar(
                "SELECT CAST(COUNT(*) AS BIGINT) FROM notify_events WHERE ns = $1 AND dedup = $2",
            )
            .bind(ns.to_vec())
            .bind(dedup)
            .fetch_one(pool)
            .await?;
            if seen > 0 {
                return Ok(false);
            }
        }
        let max: i64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(MAX(id), 0) AS BIGINT) FROM notify_events WHERE ns = $1",
        )
        .bind(ns.to_vec())
        .fetch_one(pool)
        .await?;
        let done = sqlx::query(
            "INSERT INTO notify_events \
             (ns, id, event, users_json, title, detail, path, dedup, created_at, next_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9)",
        )
        .bind(ns.to_vec())
        .bind(max + 1)
        .bind(c.kind.name())
        .bind(users_json.clone())
        .bind(c.title.clone())
        .bind(c.detail.clone())
        .bind(c.path.clone())
        .bind(dedup)
        .bind(at)
        .execute(pool)
        .await;
        match done {
            Ok(_) => return Ok(true),
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {}
            Err(e) => return Err(e),
        }
    }
    Err(sqlx::Error::Protocol(
        "concurrent writers; notification not queued".into(),
    ))
}

/// The `@login` mentions in `text`, in order and without repeats: a login is the
/// [`valid_handle`] alphabet (without `@` and `+`) after an `@` that starts a word.
pub fn mentions(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let starts = b[i] == b'@'
            && (i == 0
                || !(b[i - 1].is_ascii_alphanumeric()
                    || matches!(b[i - 1], b'.' | b'_' | b'-' | b'@')));
        if !starts {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'.' | b'_' | b'-')) {
            j += 1;
        }
        // A trailing dot ends a sentence, not the login.
        let mut end = j;
        while end > i + 1 && b[end - 1] == b'.' {
            end -= 1;
        }
        if end > i + 1 && b[i + 1].is_ascii_alphanumeric() {
            let login = &text[i + 1..end];
            if valid_handle(login) && !out.iter().any(|m| m == login) {
                out.push(login.to_owned());
            }
        }
        i = j.max(i + 1);
    }
    out
}

/// Counts of rows by state, for tests and diagnostics: `(pending, delivered, rejected)`.
pub async fn counts(pool: &AnyPool) -> Result<(i64, i64, i64), sqlx::Error> {
    let rows =
        sqlx::query("SELECT CAST(state AS BIGINT) AS s, CAST(COUNT(*) AS BIGINT) AS n FROM notify_events GROUP BY state")
            .fetch_all(pool)
            .await?;
    let mut out = (0, 0, 0);
    for r in rows {
        let (s, n): (i64, i64) = (r.try_get("s")?, r.try_get("n")?);
        match s {
            0 => out.0 = n,
            1 => out.1 = n,
            _ => out.2 = n,
        }
    }
    Ok(out)
}
