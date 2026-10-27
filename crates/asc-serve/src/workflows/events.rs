//! What starts a workflow run: events of the repository, schedules, and a person.
//!
//! The server does not hook the write paths. It reads the durable records they leave, in order,
//! with a cursor per repository and source (`workflow_cursors`), so an event is neither lost
//! across a restart nor seen by only one of several servers:
//!
//! | trigger | source |
//! |---|---|
//! | `change_opened`, `change_updated` | the ref log: a ref (not trunk) moved to a changeset whose change is new, or has an earlier changeset |
//! | `land` | the ref log: an update made by a land |
//! | `check_failed` | the check runs of the newest version of a change that ended in failure |
//! | `comment_mention` | review comments naming `@agent-<name>`, written by a maintainer or the author of the change |
//! | `schedule` | the clock, one slot per matching minute |
//!
//! A cursor starts at "now" the first time a repository is looked at, so switching a workflow on
//! does not replay history. Every event is claimed per workflow in a slot (`workflow_slots`), so
//! it fires a workflow at most once.
//!
//! Loops are cut at the source. An event caused by an agent that a workflow run started does not
//! start the same workflow, and starts others only up to `max_chain_depth` runs deep.

use std::collections::BTreeMap;
use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::policy::{Kind, Who};
use asc_core::workflow::{self as core, TriggerKind};
use sqlx::Row;

use super::defs::Defs;
use super::engine::{Start, StartError};
use super::store::StepState;
use super::{SYSTEM_ACTOR, WorkflowsHub, clean};
use crate::agents::repo::PolicyState;
use crate::agents::tokens::parse_principal;
use crate::ai::clock;
use crate::registry::RepoEntry;
use crate::review::handle_of;

/// Ref log rows read per look.
const REF_BATCH: i64 = 100;
/// Comments read per look.
const COMMENT_BATCH: i64 = 100;
/// Changes one ref update opens events for.
const MAX_CHANGES_PER_UPDATE: usize = 16;
/// Changesets walked for one ref update.
const MAX_WALK: usize = 256;
/// Paths listed in an event.
const MAX_EVENT_PATHS: usize = 500;
/// Seconds a failed check is looked for again after the cursor, so a report that landed late is
/// not missed; the slot keeps it from firing twice.
const CHECK_SLACK: i64 = 120;
/// Minutes of missed schedule slots caught up after a pause.
const CRON_CATCH_UP: i64 = 10;
/// Longest text of an event field, in bytes.
const MAX_FIELD: usize = 4096;

/// One thing that happened.
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub kind: TriggerKind,
    /// Unique per occurrence, for the slot.
    pub slot: String,
    pub branch: String,
    pub paths: Vec<String>,
    pub mentions: Vec<String>,
    pub fields: BTreeMap<String, String>,
    pub change: Vec<u8>,
    pub change_cs: Vec<u8>,
    /// The principal that caused it.
    pub actor: String,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `@agent-<name>` mentions of a comment, in order, without repeats.
pub fn mentions_of(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let b = body.as_bytes();
    let mut i = 0;
    while let Some(at) = body[i..].find("@agent-") {
        let start = i + at;
        // The mention must start a word: not "mail@agent-x".
        let boundary = start == 0
            || !(b[start - 1].is_ascii_alphanumeric()
                || matches!(b[start - 1], b'_' | b'-' | b'.'));
        let name_start = start + "@agent-".len();
        let mut end = name_start;
        while end < b.len()
            && (b[end].is_ascii_lowercase() || b[end].is_ascii_digit() || b[end] == b'-')
        {
            end += 1;
        }
        let name = body[name_start..end].trim_end_matches('-');
        if boundary
            && !name.is_empty()
            && name.len() <= 40
            && !name.starts_with('-')
            && !out.iter().any(|n| n == name)
        {
            out.push(name.to_owned());
        }
        i = end.max(start + 1);
    }
    out
}

impl WorkflowsHub {
    // -------------------------------------------------------------------------- looking

    /// One look at every repository that has a workflow switched on.
    pub async fn poll_all(self: &Arc<Self>) {
        let repos = match self.store.enabled_repos().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(class = %crate::ai::store::class(&e), "workflow poll");
                return;
            }
        };
        for repo in repos {
            if self.shutdown.is_cancelled() {
                return;
            }
            if let Err(e) = self.poll_repo(&repo).await {
                tracing::warn!(error = %e.message(), "workflow poll of a repository failed");
            }
        }
    }

    /// One look at the event sources of `repo`.
    pub async fn poll_repo(self: &Arc<Self>, repo: &str) -> Result<(), tonic::Status> {
        let name = asc_rpc::server::RepoName::parse(repo)?;
        let entry = match self.registry.open(&name).await {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        let defs = self.defs_of(&entry).await?;
        let enabled = self
            .store
            .enabled_names(repo)
            .await
            .map_err(|e| crate::update::internal("workflows", &e))?;
        let live: Vec<_> = enabled
            .iter()
            .filter(|n| defs.get(n).is_some_and(|d| d.workflow().is_some()))
            .cloned()
            .collect();
        let wants = |k: TriggerKind| {
            live.iter().any(|n| {
                defs.get(n)
                    .and_then(|d| d.workflow())
                    .is_some_and(|w| w.triggers.iter().any(|t| t.kind == k))
            })
        };
        let mut quota = self.cfg.max_events_per_poll;
        if wants(TriggerKind::ChangeOpened)
            || wants(TriggerKind::ChangeUpdated)
            || wants(TriggerKind::Land)
        {
            self.poll_refs(&entry, repo, &defs, &live, &mut quota)
                .await?;
        }
        if wants(TriggerKind::CheckFailed) {
            self.poll_checks(&entry, repo, &defs, &live, &mut quota)
                .await?;
        }
        if wants(TriggerKind::CommentMention) {
            self.poll_comments(&entry, repo, &defs, &live, &mut quota)
                .await?;
        }
        Ok(())
    }

    fn sql(e: &sqlx::Error) -> tonic::Status {
        crate::update::internal("workflows", &crate::ai::store::class(e))
    }

    // -------------------------------------------------------------------------- refs

    async fn poll_refs(
        self: &Arc<Self>,
        entry: &RepoEntry,
        repo: &str,
        defs: &Defs,
        live: &[String],
        quota: &mut u32,
    ) -> Result<(), tonic::Status> {
        let Some(cursor) = self
            .store
            .cursor(repo, "refs")
            .await
            .map_err(|e| Self::sql(&e))?
        else {
            let head: Option<i64> = sqlx::query_scalar(
                "SELECT CAST(MAX(seq) AS BIGINT) FROM ref_log WHERE repo_id = $1",
            )
            .bind(entry.key.0)
            .fetch_one(self.store.pool())
            .await
            .map_err(|e| Self::sql(&e))?;
            self.store
                .set_cursor(repo, "refs", head.unwrap_or(0))
                .await
                .map_err(|e| Self::sql(&e))?;
            return Ok(());
        };
        let rows = sqlx::query(
            "SELECT seq, name, old, new, actor, via FROM ref_log WHERE repo_id = $1 AND seq > $2 ORDER BY seq LIMIT $3",
        )
        .bind(entry.key.0)
        .bind(cursor)
        .bind(REF_BATCH)
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| Self::sql(&e))?;
        let trunk_ref = format!("refs/heads/{}", entry.default_branch);
        let trunk = self.agents.core.view.trunk_tip(entry).await?;
        let mut last = cursor;
        for r in &rows {
            let seq: i64 = r.try_get("seq").map_err(|e| Self::sql(&e))?;
            let name: String = r.try_get("name").map_err(|e| Self::sql(&e))?;
            let old = r
                .try_get::<Option<Vec<u8>>, _>("old")
                .map_err(|e| Self::sql(&e))?
                .and_then(|b| RefTarget::from_bytes(&b).ok());
            let new = r
                .try_get::<Option<Vec<u8>>, _>("new")
                .map_err(|e| Self::sql(&e))?
                .and_then(|b| RefTarget::from_bytes(&b).ok());
            let actor: String = r.try_get("actor").map_err(|e| Self::sql(&e))?;
            let via: String = r.try_get("via").map_err(|e| Self::sql(&e))?;
            last = seq;
            let Some(RefTarget::Changeset(new_cs)) = new else {
                continue;
            };
            let old_cs = match old {
                Some(RefTarget::Changeset(c)) => Some(c),
                _ => None,
            };
            let mut found = Vec::new();
            if via == "land" {
                if let Some(c) = self
                    .land_candidate(entry, seq, &name, old_cs, new_cs, &actor)
                    .await?
                {
                    found.push(c);
                }
            } else if name != trunk_ref
                && (name.starts_with("refs/heads/") || name.starts_with("refs/users/"))
            {
                found = self
                    .change_candidates(entry, &name, old_cs, new_cs, trunk, &actor)
                    .await?;
            }
            for c in found {
                self.fire(repo, entry, defs, live, &c, quota).await;
            }
            if *quota == 0 {
                break;
            }
        }
        self.store
            .set_cursor(repo, "refs", last)
            .await
            .map_err(|e| Self::sql(&e))?;
        Ok(())
    }

    /// The change a changeset belongs to, and how many changesets that change has.
    async fn change_of(
        &self,
        entry: &RepoEntry,
        cs: ChangesetId,
    ) -> Result<Option<(Vec<u8>, i64)>, tonic::Status> {
        let ns = entry.ns.as_bytes().to_vec();
        let change: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT change FROM changes WHERE ns = $1 AND cs = $2")
                .bind(ns.clone())
                .bind(cs.as_bytes().to_vec())
                .fetch_optional(self.store.pool())
                .await
                .map_err(|e| Self::sql(&e))?;
        let Some(change) = change else {
            return Ok(None);
        };
        let n: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM changes WHERE ns = $1 AND change = $2",
        )
        .bind(ns)
        .bind(change.clone())
        .fetch_one(self.store.pool())
        .await
        .map_err(|e| Self::sql(&e))?;
        Ok(Some((change, n)))
    }

    async fn paths_of(&self, entry: &RepoEntry, cs: ChangesetId) -> Vec<String> {
        let mut p = self
            .agents
            .core
            .view
            .changed_paths(entry, cs)
            .await
            .unwrap_or_default();
        p.sort();
        p.dedup();
        p.truncate(MAX_EVENT_PATHS);
        p
    }

    async fn title_of(&self, entry: &RepoEntry, cs: ChangesetId) -> String {
        match entry.backend.engine().get_changeset(cs).await {
            Ok(b) => clean(
                String::from_utf8_lossy(&b.message)
                    .lines()
                    .next()
                    .unwrap_or(""),
                200,
            ),
            Err(_) => String::new(),
        }
    }

    fn base_fields(actor: &str, branch: &str) -> BTreeMap<String, String> {
        let mut f = BTreeMap::new();
        let who = handle_of(actor).unwrap_or_else(|| clean(actor, 120));
        f.insert("actor".to_owned(), who);
        f.insert("branch".to_owned(), clean(branch, 256));
        f
    }

    async fn change_candidates(
        &self,
        entry: &RepoEntry,
        name: &str,
        old: Option<ChangesetId>,
        new: ChangesetId,
        trunk: Option<ChangesetId>,
        actor: &str,
    ) -> Result<Vec<Candidate>, tonic::Status> {
        let mut exclude: Vec<ChangesetId> = old.into_iter().collect();
        exclude.extend(trunk);
        let Some(added) = self
            .agents
            .core
            .view
            .range(entry, new, exclude, MAX_WALK)
            .await?
        else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let mut seen: Vec<Vec<u8>> = Vec::new();
        // Oldest first, so a stack opens its changes bottom to top.
        for cs in added.into_iter().rev() {
            let Some((change, n)) = self.change_of(entry, cs).await? else {
                continue;
            };
            if seen.contains(&change) {
                continue;
            }
            seen.push(change.clone());
            let kind = if n <= 1 {
                TriggerKind::ChangeOpened
            } else {
                TriggerKind::ChangeUpdated
            };
            let paths = self.paths_of(entry, cs).await;
            let mut fields = Self::base_fields(actor, &entry.default_branch);
            fields.insert("change".to_owned(), hex(&change));
            fields.insert("new".to_owned(), hex(cs.as_bytes()));
            if let Some(o) = old {
                fields.insert("old".to_owned(), hex(o.as_bytes()));
            }
            fields.insert("ref".to_owned(), clean(name, 256));
            fields.insert("title".to_owned(), self.title_of(entry, cs).await);
            fields.insert("version".to_owned(), n.max(1).to_string());
            fields.insert("paths".to_owned(), clean(&paths.join("\n"), MAX_FIELD));
            out.push(Candidate {
                kind,
                slot: format!("cs:{}", hex(cs.as_bytes())),
                branch: entry.default_branch.clone(),
                paths,
                mentions: Vec::new(),
                fields,
                change,
                change_cs: cs.as_bytes().to_vec(),
                actor: actor.to_owned(),
            });
            if out.len() >= MAX_CHANGES_PER_UPDATE {
                break;
            }
        }
        Ok(out)
    }

    async fn land_candidate(
        &self,
        entry: &RepoEntry,
        seq: i64,
        name: &str,
        old: Option<ChangesetId>,
        new: ChangesetId,
        actor: &str,
    ) -> Result<Option<Candidate>, tonic::Status> {
        let branch = name.strip_prefix("refs/heads/").unwrap_or(name).to_owned();
        let exclude: Vec<ChangesetId> = old.into_iter().collect();
        let mut paths: Vec<String> = Vec::new();
        let mut top_change = Vec::new();
        if let Some(landed) = self
            .agents
            .core
            .view
            .range(entry, new, exclude, MAX_WALK)
            .await?
        {
            for (i, cs) in landed.iter().enumerate() {
                if i == 0
                    && let Some((c, _)) = self.change_of(entry, *cs).await?
                {
                    top_change = c;
                }
                for p in self.paths_of(entry, *cs).await {
                    if paths.len() < MAX_EVENT_PATHS && !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            }
        }
        paths.sort();
        let mut fields = Self::base_fields(actor, &branch);
        fields.insert("new".to_owned(), hex(new.as_bytes()));
        if let Some(o) = old {
            fields.insert("old".to_owned(), hex(o.as_bytes()));
        }
        if !top_change.is_empty() {
            fields.insert("change".to_owned(), hex(&top_change));
        }
        fields.insert("ref".to_owned(), clean(name, 256));
        fields.insert("title".to_owned(), self.title_of(entry, new).await);
        fields.insert("paths".to_owned(), clean(&paths.join("\n"), MAX_FIELD));
        Ok(Some(Candidate {
            kind: TriggerKind::Land,
            slot: format!("land:{seq}"),
            branch,
            paths,
            mentions: Vec::new(),
            fields,
            change: top_change,
            change_cs: new.as_bytes().to_vec(),
            actor: actor.to_owned(),
        }))
    }

    // -------------------------------------------------------------------------- checks

    /// The newest changeset of a change, which is the version checks are about.
    pub(crate) async fn head_of(
        &self,
        entry: &RepoEntry,
        change: &[u8],
    ) -> Result<Option<ChangesetId>, tonic::Status> {
        let ns = entry.ns.as_bytes().to_vec();
        let versioned: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT cs FROM change_versions WHERE ns = $1 AND change = $2 ORDER BY v DESC LIMIT 1",
        )
        .bind(ns.clone())
        .bind(change.to_vec())
        .fetch_optional(self.store.pool())
        .await
        .map_err(|e| Self::sql(&e))?;
        let cs = match versioned {
            Some(c) => Some(c),
            None => {
                // Without a version log the head is the changeset of the change that no other
                // changeset of it names as a predecessor.
                let rows =
                    sqlx::query("SELECT cs, preds FROM changes WHERE ns = $1 AND change = $2")
                        .bind(ns)
                        .bind(change.to_vec())
                        .fetch_all(self.store.pool())
                        .await
                        .map_err(|e| Self::sql(&e))?;
                let mut named: Vec<Vec<u8>> = Vec::new();
                let mut all: Vec<Vec<u8>> = Vec::new();
                for r in &rows {
                    all.push(r.try_get("cs").map_err(|e| Self::sql(&e))?);
                    let preds: Vec<u8> = r.try_get("preds").map_err(|e| Self::sql(&e))?;
                    named.extend(preds.chunks_exact(32).map(<[u8]>::to_vec));
                }
                all.sort();
                all.into_iter().rev().find(|c| !named.contains(c))
            }
        };
        Ok(cs.and_then(|b| crate::agents::repo::changeset_id(&b)))
    }

    async fn poll_checks(
        self: &Arc<Self>,
        entry: &RepoEntry,
        repo: &str,
        defs: &Defs,
        live: &[String],
        quota: &mut u32,
    ) -> Result<(), tonic::Status> {
        let now = clock::now();
        let Some(cursor) = self
            .store
            .cursor(repo, "checks")
            .await
            .map_err(|e| Self::sql(&e))?
        else {
            self.store
                .set_cursor(repo, "checks", now)
                .await
                .map_err(|e| Self::sql(&e))?;
            return Ok(());
        };
        let rows = sqlx::query(
            "SELECT id, name, change, version, finished FROM check_runs \
             WHERE ns = $1 AND status = 4 AND finished >= $2 AND finished <= $3 ORDER BY finished, id LIMIT $4",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(cursor - CHECK_SLACK)
        .bind(now)
        .bind(REF_BATCH)
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| Self::sql(&e))?;
        for r in &rows {
            let id: i64 = r.try_get("id").map_err(|e| Self::sql(&e))?;
            let name: String = r.try_get("name").map_err(|e| Self::sql(&e))?;
            let change: Vec<u8> = r.try_get("change").map_err(|e| Self::sql(&e))?;
            let version: i64 = r.try_get("version").map_err(|e| Self::sql(&e))?;
            let finished: i64 = r.try_get("finished").map_err(|e| Self::sql(&e))?;
            if change.is_empty() {
                continue;
            }
            // A failure of an old version is history, not an event.
            let latest = self.latest_version(entry, &change).await?;
            if latest > 0 && version < latest {
                continue;
            }
            let slot = format!("ck:{id}:{finished}");
            let head = self.head_of(entry, &change).await?;
            let paths = match head {
                Some(cs) => self.paths_of(entry, cs).await,
                None => Vec::new(),
            };
            let mut fields = Self::base_fields("", &entry.default_branch);
            fields.insert("change".to_owned(), hex(&change));
            fields.insert("check".to_owned(), clean(&name, 255));
            if let Some(cs) = head {
                fields.insert("new".to_owned(), hex(cs.as_bytes()));
                fields.insert("title".to_owned(), self.title_of(entry, cs).await);
            }
            fields.insert("version".to_owned(), version.max(1).to_string());
            fields.insert("paths".to_owned(), clean(&paths.join("\n"), MAX_FIELD));
            let c = Candidate {
                kind: TriggerKind::CheckFailed,
                slot,
                branch: entry.default_branch.clone(),
                paths,
                mentions: Vec::new(),
                fields,
                change,
                change_cs: head.map(|h| h.as_bytes().to_vec()).unwrap_or_default(),
                actor: String::new(),
            };
            self.fire(repo, entry, defs, live, &c, quota).await;
            if *quota == 0 {
                break;
            }
        }
        self.store
            .set_cursor(repo, "checks", now)
            .await
            .map_err(|e| Self::sql(&e))?;
        Ok(())
    }

    /// The newest version number of a change in the version log, 0 when it has none.
    pub(crate) async fn latest_version(
        &self,
        entry: &RepoEntry,
        change: &[u8],
    ) -> Result<i64, tonic::Status> {
        let v: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(MAX(v) AS BIGINT) FROM change_versions WHERE ns = $1 AND change = $2",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(change.to_vec())
        .fetch_one(self.store.pool())
        .await
        .map_err(|e| Self::sql(&e))?;
        Ok(v.unwrap_or(0))
    }

    // -------------------------------------------------------------------------- comments

    async fn poll_comments(
        self: &Arc<Self>,
        entry: &RepoEntry,
        repo: &str,
        defs: &Defs,
        live: &[String],
        quota: &mut u32,
    ) -> Result<(), tonic::Status> {
        let ns = entry.ns.as_bytes().to_vec();
        let Some(cursor) = self
            .store
            .cursor(repo, "comments")
            .await
            .map_err(|e| Self::sql(&e))?
        else {
            let head: Option<i64> = sqlx::query_scalar(
                "SELECT CAST(MAX(id) AS BIGINT) FROM review_comments WHERE ns = $1",
            )
            .bind(ns)
            .fetch_one(self.store.pool())
            .await
            .map_err(|e| Self::sql(&e))?;
            self.store
                .set_cursor(repo, "comments", head.unwrap_or(0))
                .await
                .map_err(|e| Self::sql(&e))?;
            return Ok(());
        };
        let rows = sqlx::query(
            "SELECT c.id AS id, c.body AS body, c.author AS author, t.change AS change \
             FROM review_comments c JOIN review_threads t ON t.ns = c.ns AND t.id = c.thread \
             WHERE c.ns = $1 AND c.id > $2 ORDER BY c.id LIMIT $3",
        )
        .bind(ns.clone())
        .bind(cursor)
        .bind(COMMENT_BATCH)
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| Self::sql(&e))?;
        let policy = match &*self.agents.core.view.policy(entry).await? {
            PolicyState::Valid(p) => Some(p.clone()),
            _ => None,
        };
        let mut last = cursor;
        for r in &rows {
            let id: i64 = r.try_get("id").map_err(|e| Self::sql(&e))?;
            let body: String = r.try_get("body").map_err(|e| Self::sql(&e))?;
            let author: String = r.try_get("author").map_err(|e| Self::sql(&e))?;
            let change: Vec<u8> = r.try_get("change").map_err(|e| Self::sql(&e))?;
            last = id;
            let mentions = mentions_of(&body);
            if mentions.is_empty() {
                continue;
            }
            // Only a person who is trusted with the change may spend an agent on it: a maintainer
            // of the repository, or the author of the change.
            let who = Who::new(Kind::User, author.clone());
            let maintainer = policy.as_ref().is_some_and(|p| p.is_maintainer(&who));
            let first: Option<String> = sqlx::query_scalar(
                "SELECT actor FROM change_versions WHERE ns = $1 AND change = $2 ORDER BY v LIMIT 1",
            )
            .bind(ns.clone())
            .bind(change.clone())
            .fetch_optional(self.store.pool())
            .await
            .map_err(|e| Self::sql(&e))?;
            let is_author = first
                .as_deref()
                .and_then(handle_of)
                .is_some_and(|h| h.eq_ignore_ascii_case(&author));
            if !maintainer && !is_author {
                continue;
            }
            let head = self.head_of(entry, &change).await?;
            let paths = match head {
                Some(cs) => self.paths_of(entry, cs).await,
                None => Vec::new(),
            };
            let mut fields = Self::base_fields("", &entry.default_branch);
            fields.insert("actor".to_owned(), clean(&author, 120));
            fields.insert("change".to_owned(), hex(&change));
            fields.insert("comment".to_owned(), clean(&body, MAX_FIELD));
            if let Some(cs) = head {
                fields.insert("new".to_owned(), hex(cs.as_bytes()));
                fields.insert("title".to_owned(), self.title_of(entry, cs).await);
            }
            fields.insert("paths".to_owned(), clean(&paths.join("\n"), MAX_FIELD));
            let c = Candidate {
                kind: TriggerKind::CommentMention,
                slot: format!("cm:{id}"),
                branch: entry.default_branch.clone(),
                paths,
                mentions,
                fields,
                change,
                change_cs: head.map(|h| h.as_bytes().to_vec()).unwrap_or_default(),
                actor: String::new(),
            };
            self.fire(repo, entry, defs, live, &c, quota).await;
            if *quota == 0 {
                break;
            }
        }
        self.store
            .set_cursor(repo, "comments", last)
            .await
            .map_err(|e| Self::sql(&e))?;
        Ok(())
    }

    // -------------------------------------------------------------------------- firing

    /// The workflow and depth of the run whose agent made `actor`, if `actor` is such an agent.
    async fn origin_of(&self, actor: &str) -> Option<(String, u32)> {
        let p = parse_principal(actor)?;
        let tok = self.agents.core.tokens.get(&p.token_id).await.ok()??;
        if tok.run_id == 0 {
            return None;
        }
        let agent_run = self.agents.core.runs.get(tok.run_id).await.ok()??;
        if agent_run.workflow_run_id == 0 {
            return None;
        }
        let wr = self.store.get_run(agent_run.workflow_run_id).await.ok()??;
        Some((wr.workflow, wr.depth))
    }

    /// Starts a run of every enabled workflow `c` matches.
    async fn fire(
        self: &Arc<Self>,
        repo: &str,
        entry: &RepoEntry,
        defs: &Defs,
        live: &[String],
        c: &Candidate,
        quota: &mut u32,
    ) {
        let origin = self.origin_of(&c.actor).await;
        let ev = core::Event {
            kind: Some(c.kind),
            branch: c.branch.clone(),
            paths: c.paths.clone(),
            mentions: c.mentions.clone(),
        };
        for name in live {
            if *quota == 0 {
                return;
            }
            let Some(def) = defs.get(name) else { continue };
            let Some(wf) = def.workflow() else { continue };
            if !wf.triggers.iter().any(|t| t.matches(&ev)) {
                continue;
            }
            let mut depth = 0;
            let mut origin_name = String::new();
            if let Some((o, d)) = &origin {
                if o == name || d + 1 > self.cfg.max_chain_depth {
                    continue;
                }
                depth = d + 1;
                origin_name = o.clone();
            }
            match self.store.claim_slot(repo, name, &c.slot).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::error!(class = %crate::ai::store::class(&e), "workflow slot");
                    continue;
                }
            }
            let Some(requester) = self.requester_of(repo, name).await else {
                continue;
            };
            let start = Start {
                repo: repo.to_owned(),
                def: def.clone(),
                source: defs
                    .source
                    .map(|s| s.as_bytes().to_vec())
                    .unwrap_or_default(),
                trigger: c.kind,
                started_by: format!("system:{}", c.kind.as_str()),
                requester,
                change: c.change.clone(),
                change_cs: c.change_cs.clone(),
                event: c.fields.clone(),
                inputs: BTreeMap::new(),
                depth,
                origin: origin_name,
            };
            match self.start(start).await {
                Ok(_) => *quota -= 1,
                Err(StartError::Capped(why)) => {
                    tracing::warn!(repo, workflow = %name, why, "workflow event dropped");
                }
                Err(e) => {
                    tracing::warn!(repo, workflow = %name, error = ?e, "workflow event not started")
                }
            }
        }
        let _ = entry;
    }

    /// The principal a workflow's runs act for: whoever switched it on.
    pub(crate) async fn requester_of(&self, repo: &str, name: &str) -> Option<String> {
        match self.store.enabled_by(repo, name).await {
            Ok(Some(p)) if !p.is_empty() => Some(p),
            _ => None,
        }
    }

    // -------------------------------------------------------------------------- schedules

    /// Starts a run for every schedule that matches a minute since the last look.
    pub async fn tick_schedules(self: &Arc<Self>, now: i64) {
        let repos = match self.store.enabled_repos().await {
            Ok(r) => r,
            Err(_) => return,
        };
        let minute = now.div_euclid(60);
        for repo in repos {
            let cursor = match self.store.cursor(&repo, "cron").await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let from = match cursor {
                Some(c) => (c + 1).max(minute - CRON_CATCH_UP),
                None => minute,
            };
            if from > minute {
                continue;
            }
            let Ok(name) = asc_rpc::server::RepoName::parse(&repo) else {
                continue;
            };
            let Ok(entry) = self.registry.open(&name).await else {
                continue;
            };
            let Ok(defs) = self.defs_of(&entry).await else {
                continue;
            };
            let enabled = self.store.enabled_names(&repo).await.unwrap_or_default();
            for wf_name in enabled {
                let Some(def) = defs.get(&wf_name) else {
                    continue;
                };
                let Some(wf) = def.workflow() else { continue };
                for m in from..=minute {
                    let hit = wf.triggers.iter().any(|t| {
                        t.kind == TriggerKind::Schedule
                            && t.cron.as_ref().is_some_and(|c| c.matches(m * 60))
                    });
                    if !hit {
                        continue;
                    }
                    let slot = format!("cron:{m}");
                    if !matches!(
                        self.store.claim_slot(&repo, &wf_name, &slot).await,
                        Ok(true)
                    ) {
                        continue;
                    }
                    let Some(requester) = self.requester_of(&repo, &wf_name).await else {
                        continue;
                    };
                    let mut fields = BTreeMap::new();
                    fields.insert("actor".to_owned(), "schedule".to_owned());
                    fields.insert("branch".to_owned(), entry.default_branch.clone());
                    let start = Start {
                        repo: repo.clone(),
                        def: def.clone(),
                        source: defs
                            .source
                            .map(|s| s.as_bytes().to_vec())
                            .unwrap_or_default(),
                        trigger: TriggerKind::Schedule,
                        started_by: format!("system:{SYSTEM_ACTOR_NAME}"),
                        requester,
                        change: Vec::new(),
                        change_cs: Vec::new(),
                        event: fields,
                        inputs: wf.resolve_inputs(&BTreeMap::new()).unwrap_or_default(),
                        depth: 0,
                        origin: String::new(),
                    };
                    if let Err(e) = self.start(start).await {
                        tracing::warn!(repo = %repo, workflow = %wf_name, error = ?e, "scheduled workflow not started");
                    }
                }
            }
            let _ = self.store.set_cursor(&repo, "cron", minute).await;
        }
    }
}

/// `schedule`, the name of the clock as an actor.
const SYSTEM_ACTOR_NAME: &str = "schedule";

#[allow(dead_code)]
fn _unused(_: StepState, _: &str) {
    let _ = SYSTEM_ACTOR;
}

#[cfg(test)]
mod tests {
    use super::mentions_of;

    #[test]
    fn mentions_are_whole_words_of_the_agent_alphabet() {
        assert_eq!(mentions_of("@agent-fixer please look"), ["fixer"]);
        assert_eq!(
            mentions_of("a @agent-fixer and @agent-doc-bot, then @agent-fixer"),
            ["fixer", "doc-bot"]
        );
        assert_eq!(mentions_of("(@agent-x1)"), ["x1"]);
        assert!(
            mentions_of("mail@agent-fixer.example").is_empty(),
            "inside a word"
        );
        assert!(mentions_of("@agent-").is_empty());
        assert!(mentions_of("@agent-UPPER").is_empty());
        assert!(mentions_of("@agent--x").is_empty());
        assert!(mentions_of("no mention, @someone else").is_empty());
        assert_eq!(
            mentions_of("@agent-fixer-").len(),
            1,
            "a trailing dash is punctuation"
        );
        assert!(
            mentions_of(&format!("@agent-{}", "a".repeat(41))).is_empty(),
            "too long a name"
        );
    }
}
