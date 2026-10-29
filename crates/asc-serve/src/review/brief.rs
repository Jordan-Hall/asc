//! The review brief, the risk summary, provenance, approvals with carry-over, version comparison,
//! suggested changes and reviewer suggestions (`review.proto`, "brief, approvals, suggestions").
//!
//! * **Brief.** What the author writes about a change (what, why, how, risk, where to look). One
//!   row per change; the author or a writer replaces it.
//! * **Risk.** A deterministic path and size classifier with the reasons in words. It never calls
//!   a model.
//! * **Provenance.** The agent run whose change list holds the change, with its task, tool-call
//!   and file-read counts and its write scope.
//! * **Approvals.** The host reports the verdict of a person on a version. A verdict on an older
//!   version still *applies* to the newest one when the patch of every file that version changes
//!   is identical in the newest version (compared as the changesets record them, relative to
//!   their parent), and the answer says so (`carried`), or names the files that differ.
//! * **Suggestions.** A comment may carry replacement text for its lines. `PrepareSuggestion`
//!   returns the file as it would be; the host commits it as the next version and calls
//!   `MarkSuggestionApplied`, which records which version applied it.

use std::collections::{BTreeMap, HashMap, HashSet};

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_rpc::server::{CallCtx, wire};
use bytes::Bytes;
use sqlx::Row;
use tonic::Status;

use super::{Reviews, change_of, handle_of, now, sql, store};
use crate::agents::repo::RepoView;
use crate::changes::versions;
use crate::registry::RepoEntry;

/// Longest text of the brief, and most pointers.
pub const MAX_BRIEF_TEXT: usize = 8 * 1024;
pub const MAX_POINTERS: usize = 20;
/// Files a brief and a comparison list.
pub const MAX_STAT_FILES: usize = 500;
pub const MAX_COMPARE_FILES: usize = 2_000;
/// Versions one change is examined over.
const MAX_VERSIONS: usize = 1_000;
/// Longest file a suggestion is applied to.
const MAX_FILE: u64 = 4 * 1024 * 1024;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn engine_status(what: &str, e: &dyn std::fmt::Display) -> Status {
    super::internal(what, e)
}

/// The change level of a path: where a mistake costs more.
fn path_risk(path: &str) -> Option<&'static str> {
    let p = path.to_ascii_lowercase();
    let has = |s: &str| p.contains(s);
    if has("/auth")
        || p.starts_with("auth")
        || has("security")
        || has("crypto")
        || has("secret")
        || has("password")
        || has("permission")
    {
        return Some("touches authentication, secrets or permissions");
    }
    if has("migrations/") || p.ends_with(".sql") {
        return Some("touches database migrations or SQL");
    }
    if p.starts_with(".github/")
        || p.starts_with(".forgejo/")
        || p.starts_with(".gitlab")
        || has("dockerfile")
        || p.starts_with("deploy/")
        || p.starts_with("infra/")
        || p.ends_with(".tf")
    {
        return Some("touches build, deploy or infrastructure files");
    }
    if p.ends_with("cargo.lock")
        || p.ends_with("package-lock.json")
        || p.ends_with("bun.lock")
        || p.ends_with("yarn.lock")
        || p.ends_with("pnpm-lock.yaml")
        || p.ends_with("go.sum")
    {
        return Some("changes a dependency lock file");
    }
    None
}

/// Level and reasons for a change of `files`.
pub fn classify(files: &[wire::ChangeFileStat], added: u32, removed: u32) -> wire::ChangeRisk {
    let mut reasons: Vec<String> = Vec::new();
    let mut high = false;
    let mut medium = false;
    let mut seen: HashSet<&'static str> = HashSet::new();
    for f in files {
        let path = String::from_utf8_lossy(&f.path);
        if let Some(why) = path_risk(&path)
            && seen.insert(why)
        {
            high = true;
            reasons.push(format!("{why} ({path})"));
        }
    }
    let lines = added.saturating_add(removed);
    if lines > 1_000 {
        high = true;
        reasons.push(format!("changes {lines} lines"));
    } else if lines > 300 {
        medium = true;
        reasons.push(format!("changes {lines} lines"));
    }
    if files.len() > 25 {
        medium = true;
        reasons.push(format!("touches {} files", files.len()));
    }
    if files.iter().any(|f| f.binary) {
        medium = true;
        reasons.push("includes binary files".to_owned());
    }
    let no_tests = !files.is_empty()
        && !files.iter().any(|f| {
            let p = String::from_utf8_lossy(&f.path).to_ascii_lowercase();
            p.contains("test") || p.contains("spec")
        })
        && lines > 100;
    if no_tests {
        medium = true;
        reasons.push("a large change with no test files".to_owned());
    }
    let level = if high {
        wire::RiskLevel::High
    } else if medium {
        wire::RiskLevel::Medium
    } else {
        wire::RiskLevel::Low
    };
    if reasons.is_empty() {
        reasons.push("small, and touches no sensitive paths".to_owned());
    }
    wire::ChangeRisk {
        level: level as i32,
        reasons,
    }
}

/// Whether `new` holds `suggestion` at line `start` (1-based), where `old` had lines `start..=end`
/// there; an empty suggestion is a deletion, which holds when the new file lost at least that many
/// lines. Line endings are not compared.
pub(crate) fn contains_suggestion(
    old: &[u8],
    new: &[u8],
    start: usize,
    end: usize,
    suggestion: &str,
) -> bool {
    let (Ok(old), Ok(new)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return false;
    };
    if start == 0 || end < start {
        return false;
    }
    let (old_lines, new_lines): (Vec<&str>, Vec<&str>) = (
        old.split_inclusive('\n').collect(),
        new.split_inclusive('\n').collect(),
    );
    if suggestion.is_empty() {
        return new_lines.len() + (end - start + 1) <= old_lines.len();
    }
    let want: Vec<&str> = suggestion.split_inclusive('\n').collect();
    let trim = |s: &str| s.trim_end_matches(['\r', '\n']).to_owned();
    new_lines
        .get(start - 1..start - 1 + want.len())
        .is_some_and(|got| got.iter().zip(&want).all(|(g, w)| trim(g) == trim(w)))
}

/// What decides whether a verdict of approval counts.
struct Eligibility {
    policy: asc_core::policy::ReviewPolicy,
    authors: HashSet<String>,
    agents: HashSet<String>,
}

impl Eligibility {
    fn may_approve(&self, reviewer: &str) -> bool {
        (self.policy.author_may_approve || !self.authors.contains(reviewer))
            && (!self.policy.human_approval || !self.agents.contains(reviewer))
    }
}

impl Reviews {
    async fn known(&self, entry: &RepoEntry, change: ChangeId) -> Result<(), Status> {
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        if store::change_known(&mut conn, entry.ns, change).await? {
            Ok(())
        } else {
            Err(Status::not_found("unknown change"))
        }
    }

    async fn all_versions(
        &self,
        entry: &RepoEntry,
        change: ChangeId,
    ) -> Result<Vec<versions::Version>, Status> {
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let mut out = Vec::new();
        let mut after = None;
        while out.len() < MAX_VERSIONS {
            let page = versions::list(&mut conn, entry.ns, change, after, 200)
                .await
                .map_err(|e| engine_status("versions", &e))?;
            let Some(last) = page.last().map(|v| v.v) else {
                break;
            };
            let n = page.len();
            out.extend(page);
            after = Some(last);
            if n < 200 {
                break;
            }
        }
        Ok(out)
    }

    /// The patch of each path the changeset makes (relative to its parent), as a comparable string.
    async fn patch_map(
        &self,
        entry: &RepoEntry,
        cs: ChangesetId,
    ) -> Result<BTreeMap<Vec<u8>, String>, Status> {
        let body = entry
            .backend
            .engine()
            .get_changeset(cs)
            .await
            .map_err(|e| engine_status("changeset", &e))?;
        Ok(body
            .changes
            .as_slice()
            .iter()
            .map(|(p, c)| (p.as_bytes().to_vec(), format!("{c:?}")))
            .collect())
    }

    async fn file_stats(
        &self,
        entry: &RepoEntry,
        cs: ChangesetId,
    ) -> Result<(Vec<wire::ChangeFileStat>, bool), Status> {
        let st = entry
            .backend
            .engine()
            .diffstat(cs)
            .await
            .map_err(|e| engine_status("diffstat", &e))?;
        let truncated = st.truncated || st.files.len() > MAX_STAT_FILES;
        let files = st
            .files
            .iter()
            .take(MAX_STAT_FILES)
            .map(|f| wire::ChangeFileStat {
                path: Bytes::copy_from_slice(f.path.as_bytes()),
                added: f.added,
                removed: f.removed,
                binary: f.binary,
            })
            .collect();
        Ok((files, truncated))
    }

    /// Who may approve this change: not its authors and not an agent, unless the policy of the
    /// repository says otherwise. A policy that cannot be read is the strict default.
    async fn eligibility(
        &self,
        entry: &RepoEntry,
        _change: ChangeId,
        versions: &[versions::Version],
    ) -> Result<Eligibility, Status> {
        let policy = match &*RepoView::new(self.registry.clone()).policy(entry).await? {
            crate::agents::repo::PolicyState::Valid(p) => p.review,
            _ => asc_core::policy::ReviewPolicy::default(),
        };
        let authors: HashSet<String> = versions
            .iter()
            .filter_map(|v| handle_of(&v.actor))
            .collect();
        let agents: HashSet<String> = if policy.human_approval {
            sqlx::query_scalar::<_, String>("SELECT DISTINCT agent FROM agent_runs WHERE repo = $1")
                .bind(entry.name.as_str().to_ascii_lowercase())
                .fetch_all(self.registry.pool())
                .await
                .map_err(sql)?
                .into_iter()
                .collect()
        } else {
            HashSet::new()
        };
        Ok(Eligibility {
            policy,
            authors,
            agents,
        })
    }

    /// The verdicts on a change, each with whether it applies to the newest version.
    async fn approvals_of(
        &self,
        entry: &RepoEntry,
        change: ChangeId,
        versions: &[versions::Version],
    ) -> Result<Vec<wire::ApprovalInfo>, Status> {
        let rows = sqlx::query(
            "SELECT reviewer, reviewer_id, version, state, at FROM review_approvals WHERE ns = $1 AND change = $2 \
             ORDER BY at, reviewer",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        let Some(latest) = versions.last() else {
            return Ok(Vec::new());
        };
        let eligible = self.eligibility(entry, change, versions).await?;
        let mut maps: HashMap<u32, BTreeMap<Vec<u8>, String>> = HashMap::new();
        let mut out = Vec::new();
        for r in &rows {
            let v = u32::try_from(r.try_get::<i64, _>("version").map_err(sql)?).unwrap_or(0);
            let state = r.try_get::<i64, _>("state").map_err(sql)?;
            let mut info = wire::ApprovalInfo {
                reviewer: r.try_get("reviewer").map_err(sql)?,
                version: v,
                state: i32::try_from(state).unwrap_or(0),
                at: r.try_get("at").map_err(sql)?,
                reviewer_id: u64::try_from(r.try_get::<i64, _>("reviewer_id").map_err(sql)?)
                    .unwrap_or(0),
                ..Default::default()
            };
            let live = state != wire::ApprovalState::Dismissed as i64
                && (state != wire::ApprovalState::Approved as i64
                    || eligible.may_approve(&info.reviewer));
            if v == latest.v {
                info.applies = live;
            } else if let Some(from) = versions.iter().find(|x| x.v == v) {
                for (n, cs) in [(v, from.cs), (latest.v, latest.cs)] {
                    if let std::collections::hash_map::Entry::Vacant(e) = maps.entry(n) {
                        e.insert(self.patch_map(entry, cs).await?);
                    }
                }
                let (a, b) = (&maps[&v], &maps[&latest.v]);
                let mut changed: Vec<&Vec<u8>> = Vec::new();
                let mut same = 0usize;
                for (p, patch) in a {
                    if b.get(p) == Some(patch) {
                        same += 1;
                    } else {
                        changed.push(p);
                    }
                }
                changed.extend(b.keys().filter(|p| !a.contains_key(*p)));
                info.unchanged_files = u32_of(same);
                info.changed_files = changed
                    .iter()
                    .take(MAX_STAT_FILES)
                    .map(|p| Bytes::copy_from_slice(p))
                    .collect();
                info.carried =
                    live && changed.is_empty() && eligible.policy.keep_approvals_on_restack;
                info.applies = info.carried;
            }
            out.push(info);
        }
        Ok(out)
    }

    /// The verdicts on a change with whether they apply to its newest version.
    pub(crate) async fn approvals_for(
        &self,
        entry: &RepoEntry,
        change: ChangeId,
    ) -> Result<Vec<wire::ApprovalInfo>, Status> {
        let vs = self.all_versions(entry, change).await?;
        self.approvals_of(entry, change, &vs).await
    }

    async fn provenance(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        change: ChangeId,
        versions: u32,
    ) -> Result<wire::ChangeProvenance, Status> {
        let _ = entry;
        let row = sqlx::query(
            "SELECT id, agent, title, task, created_by, starter, state, started, finished, paths_json FROM agent_runs \
             WHERE repo = $1 AND changes_json LIKE $2 ORDER BY id DESC LIMIT 1",
        )
        .bind(ctx.repo.as_str().to_ascii_lowercase())
        .bind(format!("%{}%", hex(change.as_bytes())))
        .fetch_optional(self.registry.pool())
        .await
        .map_err(sql)?;
        let Some(r) = row else {
            return Ok(wire::ChangeProvenance {
                versions,
                ..Default::default()
            });
        };
        let id: i64 = r.try_get("id").map_err(sql)?;
        let count = |tool: Option<&'static str>| {
            let pool = self.registry.pool().clone();
            async move {
                let q = if tool.is_some() {
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM agent_run_events WHERE run_id = $1 AND kind = 4 AND tool = $2"
                } else {
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM agent_run_events WHERE run_id = $1 AND kind = 4 AND $2 = $2"
                };
                sqlx::query_scalar::<_, i64>(q)
                    .bind(id)
                    .bind(tool.unwrap_or("").to_owned())
                    .fetch_one(&pool)
                    .await
                    .unwrap_or(0)
            }
        };
        let (calls, reads) = (count(None).await, count(Some("read_file")).await);
        let starter: String = r.try_get("starter").map_err(sql)?;
        let created_by: String = r.try_get("created_by").map_err(sql)?;
        let paths: Vec<String> =
            serde_json::from_str(&r.try_get::<String, _>("paths_json").map_err(sql)?)
                .unwrap_or_default();
        Ok(wire::ChangeProvenance {
            run_id: u64::try_from(id).unwrap_or(0),
            agent: r.try_get("agent").map_err(sql)?,
            title: r.try_get("title").map_err(sql)?,
            task: r.try_get("task").map_err(sql)?,
            created_by: if starter.is_empty() {
                created_by
            } else {
                starter
            },
            run_state: crate::agents::runs::RunState::from_i64(r.try_get("state").map_err(sql)?)
                .name()
                .to_owned(),
            tool_calls: u32::try_from(calls).unwrap_or(0),
            files_read: u32::try_from(reads).unwrap_or(0),
            versions,
            paths,
            started: r.try_get("started").map_err(sql)?,
            finished: r.try_get("finished").map_err(sql)?,
        })
    }

    async fn brief_row(
        &self,
        entry: &RepoEntry,
        change: ChangeId,
    ) -> Result<wire::ReviewBrief, Status> {
        let row = sqlx::query(
            "SELECT what, why, how, risk_note, pointers_json, updated_by, updated FROM review_briefs \
             WHERE ns = $1 AND change = $2",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .fetch_optional(self.registry.pool())
        .await
        .map_err(sql)?;
        let Some(r) = row else {
            return Ok(wire::ReviewBrief::default());
        };
        let pointers: Vec<(String, u32, String)> =
            serde_json::from_str(&r.try_get::<String, _>("pointers_json").map_err(sql)?)
                .unwrap_or_default();
        Ok(wire::ReviewBrief {
            what: r.try_get("what").map_err(sql)?,
            why: r.try_get("why").map_err(sql)?,
            how: r.try_get("how").map_err(sql)?,
            risk_note: r.try_get("risk_note").map_err(sql)?,
            where_to_look: pointers
                .into_iter()
                .map(|(p, line, note)| wire::WhereToLook {
                    path: Bytes::from(p.into_bytes()),
                    line,
                    note,
                })
                .collect(),
            updated_by: r.try_get("updated_by").map_err(sql)?,
            updated: r.try_get("updated").map_err(sql)?,
        })
    }

    /// `Review.GetBrief`.
    pub async fn get_brief(
        &self,
        ctx: &CallCtx,
        req: wire::GetBriefRequest,
    ) -> Result<wire::GetBriefResponse, Status> {
        let change = change_of(&req.change)?;
        let entry = self.open(ctx).await?;
        self.known(&entry, change).await?;
        let vs = self.all_versions(&entry, change).await?;
        let latest = vs.last().cloned();
        let (files, truncated) = match &latest {
            Some(v) => self.file_stats(&entry, v.cs).await?,
            None => (Vec::new(), false),
        };
        let (added, removed) = files.iter().fold((0u32, 0u32), |a, f| {
            (a.0.saturating_add(f.added), a.1.saturating_add(f.removed))
        });
        let approvals = self.approvals_of(&entry, change, &vs).await?;
        let (mut conn, ns) = (self.registry.pool().acquire().await.map_err(sql)?, entry.ns);
        let (total, open) = store::thread_counts(&mut conn, ns, change)
            .await
            .map_err(sql)?;
        let requests = store::requests(&mut conn, ns, change).await.map_err(sql)?;
        drop(conn);
        let gate = wire::ReviewGateData {
            open_threads: u32::try_from(open).unwrap_or(u32::MAX),
            total_threads: u32::try_from(total).unwrap_or(u32::MAX),
            pending_reviewers: u32_of(requests.iter().filter(|r| r.done_at == 0).count()),
            finished_reviewers: u32_of(requests.iter().filter(|r| r.done_at != 0).count()),
            approvals_current: u32_of(
                approvals
                    .iter()
                    .filter(|a| a.applies && a.state == wire::ApprovalState::Approved as i32)
                    .count(),
            ),
            approvals_carried: u32_of(
                approvals
                    .iter()
                    .filter(|a| a.carried && a.state == wire::ApprovalState::Approved as i32)
                    .count(),
            ),
            changes_requested: u32_of(
                approvals
                    .iter()
                    .filter(|a| {
                        a.applies && a.state == wire::ApprovalState::ChangesRequested as i32
                    })
                    .count(),
            ),
        };
        Ok(wire::GetBriefResponse {
            brief: Some(self.brief_row(&entry, change).await?),
            latest_version: latest.map_or(0, |v| v.v),
            files: u32_of(files.len()),
            added,
            removed,
            risk: Some(classify(&files, added, removed)),
            file_stats: files,
            stats_truncated: truncated,
            provenance: Some(
                self.provenance(&entry, ctx, change, u32_of(vs.len()))
                    .await?,
            ),
            approvals,
            gate: Some(gate),
        })
    }

    /// `Review.SetBrief`. `by` is the verified writer; `can_write` is whether they hold repo write.
    pub async fn set_brief(
        &self,
        ctx: &CallCtx,
        by: &str,
        can_write: bool,
        req: wire::SetBriefRequest,
    ) -> Result<wire::SetBriefResponse, Status> {
        let change = change_of(&req.change)?;
        let b = req.brief.unwrap_or_default();
        for t in [&b.what, &b.why, &b.how, &b.risk_note] {
            if t.len() > MAX_BRIEF_TEXT || t.contains('\0') {
                return Err(Status::invalid_argument(
                    "each part of the brief is at most 8 KiB of text",
                ));
            }
        }
        if b.where_to_look.len() > MAX_POINTERS {
            return Err(Status::invalid_argument("at most 20 pointers"));
        }
        let mut pointers: Vec<(String, u32, String)> = Vec::new();
        for p in &b.where_to_look {
            if p.path.len() > super::MAX_PATH
                || p.path.contains(&0)
                || p.note.len() > 500
                || p.line > 10_000_000
            {
                return Err(Status::invalid_argument("invalid pointer"));
            }
            let path = std::str::from_utf8(&p.path)
                .map_err(|_| Status::invalid_argument("a pointer path is UTF-8"))?;
            pointers.push((path.to_owned(), p.line, p.note.clone()));
        }
        let entry = self.open(ctx).await?;
        let ns = entry.ns;
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, &entry).await?;
        if !store::change_known(&mut tx, ns, change).await? {
            return Err(Status::not_found("unknown change"));
        }
        if !can_write {
            let author = store::first_actor(&mut tx, ns, change).await?;
            if author.as_deref().and_then(handle_of).as_deref() != Some(by) {
                return Err(Status::permission_denied(
                    "only the author of the change or a writer may write its brief",
                ));
            }
        }
        sqlx::query("DELETE FROM review_briefs WHERE ns = $1 AND change = $2")
            .bind(ns.as_bytes().to_vec())
            .bind(change.as_bytes().to_vec())
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        let at = now();
        sqlx::query(
            "INSERT INTO review_briefs (ns, change, what, why, how, risk_note, pointers_json, updated_by, updated) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .bind(b.what.clone())
        .bind(b.why.clone())
        .bind(b.how.clone())
        .bind(b.risk_note.clone())
        .bind(serde_json::to_string(&pointers).unwrap_or_else(|_| "[]".to_owned()))
        .bind(by.to_owned())
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
        tx.commit().await.map_err(sql)?;
        super::acting::record(
            self.registry.pool(),
            super::acting::event(ctx, "review.brief.set", &hex(&req.change)),
        )
        .await;
        Ok(wire::SetBriefResponse {
            brief: Some(self.brief_row(&entry, change).await?),
        })
    }

    /// `Review.RecordApproval`. `reviewer` is the verified person.
    pub async fn record_approval(
        &self,
        ctx: &CallCtx,
        reviewer: &str,
        req: wire::RecordApprovalRequest,
    ) -> Result<wire::RecordApprovalResponse, Status> {
        let change = change_of(&req.change)?;
        let state = match wire::ApprovalState::try_from(req.state) {
            Ok(
                s @ (wire::ApprovalState::Approved
                | wire::ApprovalState::ChangesRequested
                | wire::ApprovalState::Dismissed),
            ) => s,
            _ => {
                return Err(Status::invalid_argument(
                    "a verdict is approved, changes requested or dismissed",
                ));
            }
        };
        let entry = self.open(ctx).await?;
        self.known(&entry, change).await?;
        let vs = self.all_versions(&entry, change).await?;
        if state != wire::ApprovalState::Dismissed
            && (!req.reviewer.is_empty() || req.reviewer_id != 0)
        {
            return Err(Status::invalid_argument(
                "only a dismissal can name another reviewer",
            ));
        }
        if state == wire::ApprovalState::Dismissed
            && (!req.reviewer.is_empty() || req.reviewer_id != 0)
            && !(req.reviewer == reviewer && req.reviewer_id == 0)
        {
            return self
                .dismiss_verdicts(&entry, ctx, change, &vs, reviewer, &req)
                .await;
        }
        if state == wire::ApprovalState::Approved {
            let e = self.eligibility(&entry, change, &vs).await?;
            if !e.policy.author_may_approve && e.authors.contains(reviewer) {
                return Err(Status::permission_denied(
                    "the author of a change cannot approve it",
                ));
            }
            if e.policy.human_approval && e.agents.contains(reviewer) {
                return Err(Status::permission_denied(
                    "an agent cannot approve a change here; the policy asks for a person",
                ));
            }
        }
        let latest = vs.last().map_or(0, |v| v.v);
        let version = if req.version == 0 {
            latest
        } else {
            req.version
        };
        if !vs.iter().any(|v| v.v == version) {
            return Err(Status::invalid_argument("unknown version"));
        }
        let ns = entry.ns;
        let at = now();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, &entry).await?;
        sqlx::query("DELETE FROM review_approvals WHERE ns = $1 AND change = $2 AND reviewer = $3 AND version = $4")
            .bind(ns.as_bytes().to_vec())
            .bind(change.as_bytes().to_vec())
            .bind(reviewer.to_owned())
            .bind(i64::from(version))
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
        sqlx::query(
            "INSERT INTO review_approvals (ns, change, reviewer, version, state, at, reviewer_id) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .bind(reviewer.to_owned())
        .bind(i64::from(version))
        .bind(state as i64)
        .bind(at)
        .bind(super::user_id_col(&ctx.principal.id))
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
        if state != wire::ApprovalState::Dismissed {
            store::finish_request(&mut tx, ns, change, reviewer, at)
                .await
                .map_err(sql)?;
        }
        tx.commit().await.map_err(sql)?;
        super::acting::record(
            self.registry.pool(),
            super::acting::event(ctx, "review.approval.recorded", &hex(&req.change))
                .detail("version", version.to_string())
                .detail("state", (state as i32).to_string()),
        )
        .await;
        Ok(wire::RecordApprovalResponse {
            approvals: self.approvals_of(&entry, change, &vs).await?,
        })
    }

    /// A reviewer's verdicts stop counting because someone with write access dismissed them: every
    /// approval and request for changes of that reviewer on the change, on every version, becomes a
    /// dismissal. The reviewer is named by login, by the hub's user id, or both.
    async fn dismiss_verdicts(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        change: ChangeId,
        vs: &[versions::Version],
        by: &str,
        req: &wire::RecordApprovalRequest,
    ) -> Result<wire::RecordApprovalResponse, Status> {
        if !req.reviewer.is_empty() && !super::valid_handle(&req.reviewer) {
            return Err(Status::invalid_argument("a reviewer is a login"));
        }
        let id = i64::try_from(req.reviewer_id)
            .map_err(|_| Status::invalid_argument("reviewer_id is out of range"))?;
        let ns = entry.ns.as_bytes().to_vec();
        let at = now();
        let mut tx = self.registry.pool().begin().await.map_err(sql)?;
        self.registry.guard(&mut tx, entry).await?;
        // The rows of that person: by login, or by the user id the verdict was recorded with.
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT DISTINCT reviewer, reviewer_id FROM review_approvals              WHERE ns = $1 AND change = $2 AND state IN ($3, $4)                AND (reviewer = $5 OR ($6 <> 0 AND reviewer_id = $6))",
        )
        .bind(ns.clone())
        .bind(change.as_bytes().to_vec())
        .bind(wire::ApprovalState::Approved as i64)
        .bind(wire::ApprovalState::ChangesRequested as i64)
        .bind(req.reviewer.clone())
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(sql)?;
        if rows.is_empty() {
            return Err(Status::not_found(
                "that reviewer has no verdict on this change",
            ));
        }
        let done = sqlx::query(
            "UPDATE review_approvals SET state = $3, at = $4              WHERE ns = $1 AND change = $2 AND state IN ($5, $6)                AND (reviewer = $7 OR ($8 <> 0 AND reviewer_id = $8))",
        )
        .bind(ns)
        .bind(change.as_bytes().to_vec())
        .bind(wire::ApprovalState::Dismissed as i64)
        .bind(at)
        .bind(wire::ApprovalState::Approved as i64)
        .bind(wire::ApprovalState::ChangesRequested as i64)
        .bind(req.reviewer.clone())
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(sql)?;
        tx.commit().await.map_err(sql)?;
        let (name, user_id) = rows
            .first()
            .cloned()
            .unwrap_or_else(|| (req.reviewer.clone(), id));
        super::acting::record(
            self.registry.pool(),
            super::acting::event(ctx, "review.approval.recorded", &hex(change.as_bytes()))
                .detail("state", (wire::ApprovalState::Dismissed as i32).to_string())
                .detail("version", "all")
                .detail("verdicts", done.rows_affected().to_string())
                .detail("reviewer", name)
                .detail("reviewer_id", user_id.to_string()),
        )
        .await;
        let _ = by;
        Ok(wire::RecordApprovalResponse {
            approvals: self.approvals_of(entry, change, vs).await?,
        })
    }

    /// `Review.CompareVersions`.
    pub async fn compare_versions(
        &self,
        ctx: &CallCtx,
        req: wire::CompareVersionsRequest,
    ) -> Result<wire::CompareVersionsResponse, Status> {
        let change = change_of(&req.change)?;
        let entry = self.open(ctx).await?;
        self.known(&entry, change).await?;
        let vs = self.all_versions(&entry, change).await?;
        let to_v = if req.to_version == 0 {
            vs.last().map_or(0, |v| v.v)
        } else {
            req.to_version
        };
        let find = |n: u32| {
            vs.iter()
                .find(|v| v.v == n)
                .cloned()
                .ok_or_else(|| Status::invalid_argument("unknown version"))
        };
        let (from, to) = (find(req.from_version)?, find(to_v)?);
        let (a, b) = (
            self.patch_map(&entry, from.cs).await?,
            self.patch_map(&entry, to.cs).await?,
        );
        let stats: HashMap<Vec<u8>, (u32, u32)> = self
            .file_stats(&entry, to.cs)
            .await?
            .0
            .into_iter()
            .map(|f| (f.path.to_vec(), (f.added, f.removed)))
            .collect();
        let mut files = Vec::new();
        let (mut changed, mut unchanged, mut added, mut removed) = (0u32, 0u32, 0u32, 0u32);
        let paths: std::collections::BTreeSet<&Vec<u8>> = a.keys().chain(b.keys()).collect();
        let mut truncated = false;
        for p in paths {
            let state = match (a.get(p), b.get(p)) {
                (Some(x), Some(y)) if x == y => "unchanged",
                (Some(_), Some(_)) => "changed",
                (None, Some(_)) => "added",
                _ => "removed",
            };
            let (la, lr) = stats.get(p).copied().unwrap_or((0, 0));
            if state == "unchanged" {
                unchanged += 1;
            } else {
                changed += 1;
                added = added.saturating_add(la);
                removed = removed.saturating_add(lr);
            }
            if files.len() >= MAX_COMPARE_FILES {
                truncated = true;
                continue;
            }
            files.push(wire::VersionFile {
                path: Bytes::copy_from_slice(p),
                state: state.to_owned(),
                added: la,
                removed: lr,
            });
        }
        let approvals = self
            .approvals_of(&entry, change, &vs)
            .await?
            .into_iter()
            .filter(|x| x.version == from.v)
            .collect();
        Ok(wire::CompareVersionsResponse {
            files,
            changed,
            unchanged,
            added,
            removed,
            approvals,
            truncated,
        })
    }

    /// `Review.PrepareSuggestion`.
    pub async fn prepare_suggestion(
        &self,
        ctx: &CallCtx,
        req: wire::PrepareSuggestionRequest,
    ) -> Result<wire::PrepareSuggestionResponse, Status> {
        let change = change_of(&req.change)?;
        let entry = self.open(ctx).await?;
        let (thread, comment) = self
            .suggestion_of(&entry, change, req.thread_id, req.comment_id)
            .await?;
        let vs = self.all_versions(&entry, change).await?;
        let latest = vs.last().map_or(0, |v| v.v);
        let anchor = thread.anchor.clone().unwrap_or_default();
        let version = if anchor.version == 0 {
            latest
        } else {
            anchor.version
        };
        let stale = |reason: &str| wire::PrepareSuggestionResponse {
            path: anchor.path.clone(),
            version,
            latest_version: latest,
            start_line: anchor.start_line,
            end_line: anchor.end_line.max(anchor.start_line),
            stale: true,
            reason: reason.to_owned(),
            ..Default::default()
        };
        if comment.applied_version != 0 {
            return Ok(stale("the suggestion was applied already"));
        }
        if version != latest {
            return Ok(stale(
                "the thread is on an older version; the change has moved on",
            ));
        }
        let Some(v) = vs.iter().find(|v| v.v == version) else {
            return Ok(stale("unknown version"));
        };
        let view = RepoView::new(self.registry.clone());
        let root = view.root_of(&entry, v.cs).await?;
        let path = std::str::from_utf8(&anchor.path)
            .map_err(|_| Status::invalid_argument("a path is UTF-8"))?;
        let Some(old) = view.read(&entry, root, path, MAX_FILE).await? else {
            return Ok(stale("the file is not in this version"));
        };
        let Ok(text) = std::str::from_utf8(&old) else {
            return Ok(stale("the file is not text"));
        };
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let (s, e) = (
            anchor.start_line as usize,
            anchor.end_line.max(anchor.start_line) as usize,
        );
        if s == 0 || e > lines.len() {
            return Ok(stale("the lines are no longer in the file"));
        }
        let mut out = String::with_capacity(text.len() + comment.suggestion.len());
        lines[..s - 1].iter().for_each(|l| out.push_str(l));
        out.push_str(&comment.suggestion);
        let last_has_newline = lines[e - 1].ends_with('\n');
        if last_has_newline && !comment.suggestion.is_empty() && !comment.suggestion.ends_with('\n')
        {
            out.push('\n');
        }
        lines[e..].iter().for_each(|l| out.push_str(l));
        Ok(wire::PrepareSuggestionResponse {
            path: anchor.path,
            version,
            latest_version: latest,
            base_changeset: Bytes::copy_from_slice(v.cs.as_bytes()),
            new_content: Bytes::from(out.into_bytes()),
            start_line: anchor.start_line,
            end_line: anchor.end_line.max(anchor.start_line),
            stale: false,
            reason: String::new(),
        })
    }

    async fn suggestion_of(
        &self,
        entry: &RepoEntry,
        change: ChangeId,
        thread_id: u64,
        comment_id: u64,
    ) -> Result<(wire::ReviewThread, wire::ReviewComment), Status> {
        let tid = i64::try_from(thread_id)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Status::invalid_argument("invalid id"))?;
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let thread = store::thread(&mut conn, entry.ns, tid)
            .await
            .map_err(sql)?
            .filter(|t| t.change.as_ref() == change.as_bytes())
            .ok_or_else(|| Status::not_found("unknown thread"))?;
        let comment = thread
            .comments
            .iter()
            .find(|c| c.id == comment_id)
            .cloned()
            .ok_or_else(|| Status::not_found("unknown comment"))?;
        if comment.suggestion.is_empty() {
            return Err(Status::failed_precondition(
                "the comment holds no suggestion",
            ));
        }
        if thread
            .anchor
            .as_ref()
            .is_none_or(|a| a.path.is_empty() || a.start_line == 0)
        {
            return Err(Status::failed_precondition("the thread is not on lines"));
        }
        Ok((thread, comment))
    }

    /// `Review.MarkSuggestionApplied`. `by` is the verified person who applied it.
    pub async fn mark_suggestion_applied(
        &self,
        ctx: &CallCtx,
        by: &str,
        req: wire::MarkSuggestionAppliedRequest,
    ) -> Result<wire::MarkSuggestionAppliedResponse, Status> {
        let change = change_of(&req.change)?;
        let entry = self.open(ctx).await?;
        let (thread, comment) = self
            .suggestion_of(&entry, change, req.thread_id, req.comment_id)
            .await?;
        let vs = self.all_versions(&entry, change).await?;
        let latest = vs.last().map_or(0, |v| v.v);
        let on = thread.anchor.as_ref().map_or(0, |a| a.version);
        if req.version <= on || req.version > latest {
            return Err(Status::invalid_argument(
                "the version must be later than the thread's and at most the newest",
            ));
        }
        if comment.applied_version != 0 {
            return Err(Status::already_exists("the suggestion was applied already"));
        }
        // The new version must really hold the suggestion: a writer cannot mark a change that was not made.
        {
            let anchor = thread.anchor.clone().unwrap_or_default();
            let path = std::str::from_utf8(&anchor.path)
                .map_err(|_| Status::invalid_argument("a path is UTF-8"))?;
            let bad = || Status::invalid_argument("unknown version");
            let new_v = vs.iter().find(|v| v.v == req.version).ok_or_else(bad)?;
            let old_v = vs.iter().find(|v| v.v == on).ok_or_else(bad)?;
            let view = RepoView::new(self.registry.clone());
            let new_root = view.root_of(&entry, new_v.cs).await?;
            let old_root = view.root_of(&entry, old_v.cs).await?;
            let new_bytes = view
                .read(&entry, new_root, path, MAX_FILE)
                .await?
                .ok_or_else(|| Status::failed_precondition("the version does not have the file"))?;
            let old_bytes = view
                .read(&entry, old_root, path, MAX_FILE)
                .await?
                .unwrap_or_default();
            if !contains_suggestion(
                &old_bytes,
                &new_bytes,
                anchor.start_line as usize,
                anchor.end_line.max(anchor.start_line) as usize,
                &comment.suggestion,
            ) {
                return Err(Status::failed_precondition(
                    "the version does not contain the suggestion",
                ));
            }
        }
        let n = sqlx::query(
            "UPDATE review_comments SET applied_version = $4, applied_by = $5 \
             WHERE ns = $1 AND id = $2 AND thread = $3 AND applied_version = 0",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(i64::try_from(req.comment_id).unwrap_or(0))
        .bind(i64::try_from(req.thread_id).unwrap_or(0))
        .bind(i64::from(req.version))
        .bind(by.to_owned())
        .execute(self.registry.pool())
        .await
        .map_err(sql)?
        .rows_affected();
        if n == 0 {
            return Err(Status::aborted("the suggestion changed; retry"));
        }
        super::acting::record(
            self.registry.pool(),
            super::acting::event(ctx, "review.suggestion.applied", &hex(&req.change))
                .detail("thread", req.thread_id.to_string())
                .detail("comment", req.comment_id.to_string())
                .detail("version", req.version.to_string()),
        )
        .await;
        let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
        let thread = store::thread(
            &mut conn,
            entry.ns,
            i64::try_from(req.thread_id).unwrap_or(0),
        )
        .await
        .map_err(sql)?;
        Ok(wire::MarkSuggestionAppliedResponse { thread })
    }

    /// `Review.SuggestReviewers`: people who finished many reviews here or opened threads on the
    /// files this change touches, without its author.
    pub async fn suggest_reviewers(
        &self,
        ctx: &CallCtx,
        req: wire::SuggestReviewersRequest,
    ) -> Result<wire::SuggestReviewersResponse, Status> {
        let change = change_of(&req.change)?;
        let limit = match req.limit {
            0 => 5,
            n => usize::try_from(n).unwrap_or(20).min(20),
        };
        let entry = self.open(ctx).await?;
        let ns = entry.ns.as_bytes().to_vec();
        self.known(&entry, change).await?;
        let vs = self.all_versions(&entry, change).await?;
        let author = {
            let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
            store::first_actor(&mut conn, entry.ns, change)
                .await?
                .as_deref()
                .and_then(handle_of)
        };
        let mut score: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        let rows = sqlx::query(
            "SELECT reviewer, CAST(COUNT(*) AS BIGINT) AS n FROM review_requests WHERE ns = $1 AND done_at > 0 \
             GROUP BY reviewer ORDER BY n DESC LIMIT 50",
        )
        .bind(ns.clone())
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        for r in &rows {
            let n: i64 = r.try_get("n").map_err(sql)?;
            score
                .entry(r.try_get("reviewer").map_err(sql)?)
                .or_default()
                .0 = u32::try_from(n).unwrap_or(u32::MAX);
        }
        if let Some(v) = vs.last() {
            let (files, _) = self.file_stats(&entry, v.cs).await?;
            for chunk in files.chunks(50) {
                let q = format!(
                    "SELECT opened_by, CAST(COUNT(*) AS BIGINT) AS n FROM review_threads WHERE ns = $1 AND path IN ({}) \
                     GROUP BY opened_by",
                    super::placeholders(2, chunk.len())
                );
                let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.clone());
                for f in chunk {
                    query = query.bind(f.path.to_vec());
                }
                for r in query.fetch_all(self.registry.pool()).await.map_err(sql)? {
                    let n: i64 = r.try_get("n").map_err(sql)?;
                    score
                        .entry(r.try_get("opened_by").map_err(sql)?)
                        .or_default()
                        .1 += u32::try_from(n).unwrap_or(0);
                }
            }
        }
        let agents: HashSet<String> = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT agent FROM agent_runs WHERE repo = $1",
        )
        .bind(ctx.repo.as_str().to_ascii_lowercase())
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?
        .into_iter()
        .collect();
        let mut ranked: Vec<(u32, String, u32, u32)> = score
            .into_iter()
            .filter(|(h, _)| Some(h.as_str()) != author.as_deref())
            .map(|(h, (reviews, same))| {
                (
                    same.saturating_mul(3).saturating_add(reviews),
                    h,
                    reviews,
                    same,
                )
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        Ok(wire::SuggestReviewersResponse {
            reviewers: ranked
                .into_iter()
                .take(limit)
                .map(|(_, handle, reviews, same)| wire::SuggestedReviewer {
                    reason: match (same, reviews) {
                        (0, r) => format!("finished {r} reviews here"),
                        (s, 0) => format!("commented on {s} threads on these files"),
                        (s, r) => format!(
                            "commented on {s} threads on these files and finished {r} reviews"
                        ),
                    },
                    is_agent: agents.contains(&handle),
                    handle,
                    reviews,
                    on_same_files: same,
                })
                .collect(),
        })
    }
}
