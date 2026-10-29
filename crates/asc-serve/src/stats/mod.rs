//! `Insights`: what landed, who wrote it and who reviewed it (docs/BUILDOUT.md section 24).
//!
//! A *landed change* is a changeset a land rewrote onto the default branch (`land_map_ref`), at the
//! time of that update of the branch (`ref_log`). Its change id and first publisher come from
//! `change_versions`; its author is an *agent* when an agent run's change list holds the change or
//! the first version was published under an `agent:` principal, a *person* otherwise. A verdict is a
//! row of `review_approvals` (approved or changes requested); the review time of a change is the time
//! from its first version to its first verdict. Sizes come from stored diff stats only: nothing is
//! derived for a statistic, so a change without a stored stat counts under `unknown`.
//!
//! Bounds: at most [`MAX_LANDED`] landed rows and [`MAX_VERDICTS`] verdicts are read; a cut sets
//! `truncated`. Everything is scoped to the repository of the call; the caller needs repo read.

pub mod rpc;

use std::collections::{BTreeMap, HashMap, HashSet};

use asc_core::id::ChangesetId;
use asc_rpc::server::{CallCtx, wire};
use sqlx::Row;
use tonic::Status;

use crate::registry::RepoEntry;
use crate::review::{Reviews, handle_of, now, sql, user_id_of};

/// Seconds in a day.
const DAY: i64 = 86_400;
/// Weeks of the weekly series and of a contributor's sparkline.
pub const WEEKS: usize = 12;
/// Landed rows read at most.
pub const MAX_LANDED: i64 = 50_000;
/// Verdict rows read at most.
pub const MAX_VERDICTS: i64 = 100_000;
/// Contributors returned.
pub const MAX_CONTRIBUTORS: usize = 200;
/// Ids per `IN` list.
const CHUNK: usize = 200;
/// Largest `days`.
pub const MAX_DAYS: u32 = 366;
/// Verdict states counted as a review.
const APPROVED: i64 = 1;
const CHANGES_REQUESTED: i64 = 2;

/// Who wrote a landed change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Author {
    pub agent: bool,
    pub name: String,
    pub run_by: String,
    pub user_id: u64,
}

/// One landed change with what the statistics need.
#[derive(Clone, Debug)]
pub struct Landed {
    pub change: Vec<u8>,
    pub at: i64,
    pub new_cs: Vec<u8>,
    pub author: Author,
    pub opened: i64,
    /// Time of the first verdict, if any.
    pub first_verdict: Option<i64>,
    pub approved_by_person: bool,
}

impl Landed {
    fn review_seconds(&self) -> Option<i64> {
        self.first_verdict.map(|v| (v - self.opened).max(0))
    }
}

/// The window `[from, until)`; the one of the same length before it is the "previous" window.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub from: i64,
    pub until: i64,
    pub days: i64,
}

impl Window {
    pub fn new(days: u32, until: i64) -> Result<Self, Status> {
        let days = match days {
            0 => 30,
            1..=MAX_DAYS => days,
            _ => return Err(Status::invalid_argument("days is 1 to 366")),
        };
        let until = if until == 0 { now() } else { until };
        if until < 0 {
            return Err(Status::invalid_argument("until is a Unix time"));
        }
        Ok(Self {
            from: until - i64::from(days) * DAY,
            until,
            days: i64::from(days),
        })
    }

    fn previous_from(&self) -> i64 {
        self.from - self.days * DAY
    }

    /// The oldest time the data must reach: the previous window and the weekly series.
    fn reach(&self) -> i64 {
        self.previous_from()
            .min(self.until - WEEKS as i64 * 7 * DAY)
    }

    /// Start of week `i` of the weekly series (oldest first).
    fn week(&self, i: usize) -> i64 {
        self.until - (WEEKS - i) as i64 * 7 * DAY
    }
}

pub fn median(mut v: Vec<i64>) -> i64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2
    }
}

fn kind_matches(kind: i32, agent: bool) -> bool {
    match wire::AuthorKind::try_from(kind) {
        Ok(wire::AuthorKind::Person) => !agent,
        Ok(wire::AuthorKind::Agent) => agent,
        _ => true,
    }
}

fn in_list(n: usize, first: usize) -> String {
    (0..n)
        .map(|i| format!("${}", first + i))
        .collect::<Vec<_>>()
        .join(", ")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Everything a statistic is computed from.
pub struct Data {
    pub landed: Vec<Landed>,
    pub roots: Vec<i64>,
    pub truncated: bool,
}

type Runs = HashMap<String, (String, String)>;

impl Reviews {
    /// Reads the landed changes of `[reach, until)` with their authors and review times, and the
    /// first-publication times of root changes.
    pub async fn stats_data(
        &self,
        entry: &RepoEntry,
        ctx: &CallCtx,
        w: &Window,
    ) -> Result<Data, Status> {
        let pool = self.registry.pool();
        let ns = entry.ns.as_bytes().to_vec();
        let refname = format!("refs/heads/{}", entry.default_branch);
        let rows = sqlx::query(
            "SELECT m.old_cs AS old_cs, m.new_cs AS new_cs, l.at AS at FROM land_map_ref m \
             JOIN ref_log l ON l.repo_id = $1 AND l.seq = m.ref_log_seq \
             WHERE m.ns = $2 AND m.ref = $3 AND l.at >= $4 AND l.at < $5 \
             ORDER BY l.at, m.old_cs LIMIT $6",
        )
        .bind(entry.key.0)
        .bind(ns.clone())
        .bind(refname)
        .bind(w.reach())
        .bind(w.until)
        .bind(MAX_LANDED + 1)
        .fetch_all(pool)
        .await
        .map_err(sql)?;
        let cap = usize::try_from(MAX_LANDED).unwrap_or(usize::MAX);
        let mut truncated = rows.len() > cap;
        let mut by_cs: Vec<(Vec<u8>, Vec<u8>, i64)> = Vec::new();
        for r in rows.iter().take(cap) {
            by_cs.push((
                r.try_get("old_cs").map_err(sql)?,
                r.try_get("new_cs").map_err(sql)?,
                r.try_get("at").map_err(sql)?,
            ));
        }
        let mut change_of: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for part in by_cs.chunks(CHUNK) {
            let marks = in_list(part.len(), 2);
            let q = format!(
                "SELECT DISTINCT cs, change FROM change_versions WHERE ns = $1 AND cs IN ({marks})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.clone());
            for (old, _, _) in part {
                query = query.bind(old.clone());
            }
            for r in query.fetch_all(pool).await.map_err(sql)? {
                change_of.insert(
                    r.try_get("cs").map_err(sql)?,
                    r.try_get("change").map_err(sql)?,
                );
            }
        }
        // A change counts once, at its first landing.
        let mut seen = HashSet::new();
        let mut landed: Vec<(Vec<u8>, Vec<u8>, i64)> = Vec::new();
        for (old, new, at) in &by_cs {
            if let Some(ch) = change_of.get(old)
                && seen.insert(ch.clone())
            {
                landed.push((ch.clone(), new.clone(), *at));
            }
        }
        let changes: Vec<Vec<u8>> = landed.iter().map(|(c, _, _)| c.clone()).collect();
        let mut first: HashMap<Vec<u8>, (i64, String)> = HashMap::new();
        let mut verdicts: HashMap<Vec<u8>, (Option<i64>, bool)> = HashMap::new();
        for part in changes.chunks(CHUNK) {
            let marks = in_list(part.len(), 2);
            let q = format!(
                "SELECT change, at, actor FROM change_versions WHERE ns = $1 AND v = 1 AND change IN ({marks})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.clone());
            for c in part {
                query = query.bind(c.clone());
            }
            for r in query.fetch_all(pool).await.map_err(sql)? {
                first.insert(
                    r.try_get("change").map_err(sql)?,
                    (
                        r.try_get("at").map_err(sql)?,
                        r.try_get("actor").map_err(sql)?,
                    ),
                );
            }
            let q = format!(
                "SELECT change, state, at FROM review_approvals WHERE ns = $1 \
                 AND state IN ({APPROVED}, {CHANGES_REQUESTED}) AND change IN ({marks})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(q)).bind(ns.clone());
            for c in part {
                query = query.bind(c.clone());
            }
            for r in query.fetch_all(pool).await.map_err(sql)? {
                let change: Vec<u8> = r.try_get("change").map_err(sql)?;
                let state: i64 = r.try_get("state").map_err(sql)?;
                let at: i64 = r.try_get("at").map_err(sql)?;
                let e = verdicts.entry(change).or_insert((None, false));
                e.0 = Some(e.0.map_or(at, |x| x.min(at)));
                e.1 |= state == APPROVED;
            }
        }
        let runs = self.agent_runs_of(ctx).await?;
        let landed = landed
            .into_iter()
            .map(|(change, new_cs, at)| {
                let (opened, actor) = first.get(&change).cloned().unwrap_or((at, String::new()));
                let (first_verdict, approved_by_person) =
                    verdicts.get(&change).copied().unwrap_or((None, false));
                let author = author_of(&runs, &change, &actor);
                Landed {
                    change,
                    at,
                    new_cs,
                    author,
                    opened,
                    first_verdict,
                    approved_by_person,
                }
            })
            .collect();
        let roots = sqlx::query(
            "SELECT at FROM change_versions WHERE ns = $1 AND v = 1 AND parent_change IS NULL \
             AND at >= $2 AND at < $3 ORDER BY at LIMIT $4",
        )
        .bind(ns)
        .bind(w.reach())
        .bind(w.until)
        .bind(MAX_LANDED + 1)
        .fetch_all(pool)
        .await
        .map_err(sql)?;
        truncated |= roots.len() > cap;
        let roots = roots
            .iter()
            .take(cap)
            .map(|r| r.try_get("at").map_err(sql))
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(Data {
            landed,
            roots,
            truncated,
        })
    }

    /// The agent runs of the repository that hold changes: change (hex) -> (agent, run starter).
    async fn agent_runs_of(&self, ctx: &CallCtx) -> Result<Runs, Status> {
        let rows = sqlx::query(
            "SELECT agent, starter, created_by, changes_json FROM agent_runs \
             WHERE repo = $1 AND changes_json <> '[]' ORDER BY id DESC LIMIT 5000",
        )
        .bind(ctx.repo.as_str().to_ascii_lowercase())
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        let mut out = HashMap::new();
        for r in rows {
            let agent: String = r.try_get("agent").map_err(sql)?;
            let starter: String = r.try_get("starter").map_err(sql)?;
            let created_by: String = r.try_get("created_by").map_err(sql)?;
            let by = if starter.is_empty() {
                handle_of(&created_by).unwrap_or(created_by)
            } else {
                starter
            };
            let changes: Vec<String> =
                serde_json::from_str(&r.try_get::<String, _>("changes_json").map_err(sql)?)
                    .unwrap_or_default();
            for c in changes {
                // The newest run that holds the change keeps it.
                out.entry(c).or_insert_with(|| (agent.clone(), by.clone()));
            }
        }
        Ok(out)
    }
}

fn author_of(runs: &Runs, change: &[u8], actor: &str) -> Author {
    if let Some((agent, by)) = runs.get(&hex(change)) {
        return Author {
            agent: true,
            name: agent.clone(),
            run_by: by.clone(),
            user_id: 0,
        };
    }
    if let Some(name) = actor.strip_prefix("agent:") {
        return Author {
            agent: true,
            name: name.to_owned(),
            run_by: String::new(),
            user_id: 0,
        };
    }
    Author {
        agent: false,
        name: handle_of(actor).unwrap_or_else(|| actor.to_owned()),
        run_by: String::new(),
        user_id: user_id_of(actor).unwrap_or(0),
    }
}

fn size_label(lines: Option<u32>) -> &'static str {
    match lines {
        None => "unknown",
        Some(n) if n < 50 => "under_50",
        Some(n) if n <= 300 => "50_to_300",
        Some(_) => "over_300",
    }
}

impl Reviews {
    /// `Insights.GetStats`.
    pub async fn stats_get(
        &self,
        ctx: &CallCtx,
        req: wire::InsightsStatsRequest,
    ) -> Result<wire::InsightsStatsResponse, Status> {
        let w = Window::new(req.days, req.until)?;
        let entry = self.open(ctx).await?;
        let data = self.stats_data(&entry, ctx, &w).await?;
        let wanted = |l: &Landed| kind_matches(req.kind, l.author.agent);
        let in_window: Vec<&Landed> = data
            .landed
            .iter()
            .filter(|l| l.at >= w.from && wanted(l))
            .collect();
        let ids: Vec<ChangesetId> = in_window
            .iter()
            .filter_map(|l| <[u8; 32]>::try_from(l.new_cs.as_slice()).ok())
            .map(ChangesetId::from_bytes)
            .collect();
        let memos = entry
            .backend
            .engine()
            .diffstat_memos(&ids)
            .await
            .map_err(|e| crate::update::internal("diffstat", &e))?;
        let mut sizes: HashMap<Vec<u8>, u32> = HashMap::new();
        for (id, memo) in ids.iter().zip(memos) {
            if let Some(m) = memo {
                let lines = m.files.iter().fold(0u32, |a, f| {
                    a.saturating_add(f.added).saturating_add(f.removed)
                });
                sizes.insert(id.as_bytes().to_vec(), lines);
            }
        }
        let previous: Vec<&Landed> = data
            .landed
            .iter()
            .filter(|l| l.at >= w.previous_from() && l.at < w.from && wanted(l))
            .collect();
        let everyone: Vec<&Landed> = data.landed.iter().filter(|l| l.at >= w.from).collect();
        let agents = everyone.iter().filter(|l| l.author.agent).count();
        let agents_reviewed = everyone
            .iter()
            .filter(|l| l.author.agent && l.approved_by_person)
            .count();
        let roots = |lo: i64, hi: i64| data.roots.iter().filter(|a| **a >= lo && **a < hi).count();
        let mut per_day: BTreeMap<i64, (u32, u32)> = BTreeMap::new();
        let mut d = w.from - w.from.rem_euclid(DAY);
        while d < w.until {
            per_day.insert(d, (0, 0));
            d += DAY;
        }
        for l in &in_window {
            let e = per_day.entry(l.at - l.at.rem_euclid(DAY)).or_default();
            if l.author.agent {
                e.1 += 1;
            } else {
                e.0 += 1;
            }
        }
        let weekly = (0..WEEKS)
            .map(|i| {
                let (lo, hi) = (w.week(i), w.week(i) + 7 * DAY);
                let of: Vec<&Landed> = data
                    .landed
                    .iter()
                    .filter(|l| l.at >= lo && l.at < hi && wanted(l))
                    .collect();
                wire::InsightsWeek {
                    week_start: lo,
                    landed: count(of.len()),
                    median_review_seconds: median(
                        of.iter().filter_map(|l| l.review_seconds()).collect(),
                    ),
                }
            })
            .collect();
        let mut times: BTreeMap<&'static str, Vec<i64>> = BTreeMap::new();
        let mut counts: BTreeMap<&'static str, u32> = BTreeMap::new();
        for label in ["under_50", "50_to_300", "over_300", "unknown"] {
            times.insert(label, Vec::new());
            counts.insert(label, 0);
        }
        for l in &in_window {
            let label = size_label(sizes.get(&l.new_cs).copied());
            *counts.entry(label).or_default() += 1;
            if let Some(s) = l.review_seconds() {
                times.entry(label).or_default().push(s);
            }
        }
        let by_size = counts
            .into_iter()
            .map(|(label, changes)| wire::InsightsSizeBucket {
                label: label.to_owned(),
                changes,
                median_review_seconds: median(times.remove(label).unwrap_or_default()),
            })
            .collect();
        Ok(wire::InsightsStatsResponse {
            from: w.from,
            until: w.until,
            landed: count(in_window.len()),
            landed_previous: count(previous.len()),
            stacks_opened: count(roots(w.from, w.until)),
            stacks_opened_previous: count(roots(w.previous_from(), w.from)),
            median_review_seconds: median(
                in_window
                    .iter()
                    .filter_map(|l| l.review_seconds())
                    .collect(),
            ),
            median_review_previous: median(
                previous.iter().filter_map(|l| l.review_seconds()).collect(),
            ),
            landed_by_people: count(everyone.len() - agents),
            landed_by_agents: count(agents),
            agent_changes_reviewed: count(agents_reviewed),
            per_day: per_day
                .into_iter()
                .map(|(day, (people, agents))| wire::InsightsDay {
                    day,
                    people,
                    agents,
                })
                .collect(),
            weekly,
            by_size,
            truncated: data.truncated,
        })
    }

    /// `Insights.ListContributors`.
    pub async fn stats_contributors(
        &self,
        ctx: &CallCtx,
        req: wire::ListContributorsRequest,
    ) -> Result<wire::ListContributorsResponse, Status> {
        let w = Window::new(req.days, req.until)?;
        let entry = self.open(ctx).await?;
        let data = self.stats_data(&entry, ctx, &w).await?;
        let rows = sqlx::query(
            "SELECT a.reviewer AS reviewer, a.reviewer_id AS reviewer_id, a.at AS at, v.at AS opened \
             FROM review_approvals a JOIN change_versions v \
               ON v.ns = a.ns AND v.change = a.change AND v.v = 1 \
             WHERE a.ns = $1 AND a.state IN (1, 2) AND a.at >= $2 AND a.at < $3 \
             ORDER BY a.at LIMIT $4",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(w.from)
        .bind(w.until)
        .bind(MAX_VERDICTS + 1)
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        let cap = usize::try_from(MAX_VERDICTS).unwrap_or(usize::MAX);
        let truncated = data.truncated || rows.len() > cap;
        #[derive(Default)]
        struct Row {
            run_by: String,
            user_id: u64,
            landed: u32,
            weekly: [u32; WEEKS],
            reviews: Vec<i64>,
        }
        let mut people: BTreeMap<(bool, String), Row> = BTreeMap::new();
        for l in data
            .landed
            .iter()
            .filter(|l| kind_matches(req.kind, l.author.agent))
        {
            let in_win = l.at >= w.from;
            let week = (0..WEEKS).find(|i| l.at >= w.week(*i) && l.at < w.week(*i) + 7 * DAY);
            if !in_win && week.is_none() {
                continue;
            }
            let e = people
                .entry((l.author.agent, l.author.name.clone()))
                .or_default();
            if !l.author.run_by.is_empty() {
                e.run_by = l.author.run_by.clone();
            }
            if l.author.user_id != 0 {
                e.user_id = l.author.user_id;
            }
            if in_win {
                e.landed += 1;
            }
            if let Some(i) = week {
                e.weekly[i] += 1;
            }
        }
        // Agents never review, so reviews belong to people.
        if kind_matches(req.kind, false) {
            for r in rows.iter().take(cap) {
                let reviewer: String = r.try_get("reviewer").map_err(sql)?;
                let at: i64 = r.try_get("at").map_err(sql)?;
                let opened: i64 = r.try_get("opened").map_err(sql)?;
                let id: i64 = r.try_get("reviewer_id").map_err(sql)?;
                let e = people.entry((false, reviewer)).or_default();
                e.reviews.push((at - opened).max(0));
                if id > 0 {
                    e.user_id = u64::try_from(id).unwrap_or(0);
                }
            }
        }
        let total: u32 = people.values().map(|r| r.landed).sum();
        let mut list: Vec<wire::Contributor> = people
            .into_iter()
            .map(|((agent, name), r)| wire::Contributor {
                name,
                kind: if agent {
                    wire::AuthorKind::Agent
                } else {
                    wire::AuthorKind::Person
                } as i32,
                run_by: r.run_by,
                landed: r.landed,
                reviews_given: count(r.reviews.len()),
                median_review_seconds: median(r.reviews),
                weekly_landed: r.weekly.to_vec(),
                share: if total == 0 {
                    0.0
                } else {
                    f64::from(r.landed) / f64::from(total)
                },
                user_id: r.user_id,
            })
            .filter(|c| c.landed > 0 || c.reviews_given > 0)
            .collect();
        list.sort_by(|a, b| {
            b.landed
                .cmp(&a.landed)
                .then(b.reviews_given.cmp(&a.reviews_given))
                .then(a.name.cmp(&b.name))
        });
        list.truncate(MAX_CONTRIBUTORS);
        Ok(wire::ListContributorsResponse {
            contributors: list,
            landed: total,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests;
