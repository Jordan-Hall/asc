//! Import backfill, `asc serve admin codeintel backfill` and on-demand indexing (SPEC §23.5,
//! §3.3, PLAN I15).
//!
//! * Import ends by indexing the heads only: the default branch, the release branches and the
//!   open PR heads ([`IMPORT_REFS`]), never every historical commit. A head's `FileId`s that
//!   `ci_docs` does not know become `file` jobs at [`Priority::Backfill`].
//! * `backfill --refs <glob> [--since <date>]` widens that to any refs the glob selects. The
//!   glob is matched against the full ref name; `*` stops at `/`, `**` does not.
//! * A query that meets an unindexed revision calls [`Backfill::on_demand`], which enqueues
//!   its files at [`Priority::Interactive`].
//! * Derived index data is never copied between namespaces: [`copy_closure`] is the copy
//!   `Fork` and `CopyClosure` use. It moves content objects only; a fork has no `ci_*` rows
//!   and backfills its own heads.
//!
//! Every pass is bounded: [`MAX_TIPS`] refs, [`MAX_FILES`] distinct files.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::{ChangesetId, RefTarget};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use sqlx::{AnyPool, Row};

use super::modelgen::{CiError, Result};
use super::queue::{ChangeSource, EnqueueEffect, Priority, Written};
use crate::outbox::EffectError;

/// Refs one pass examines at most.
pub const MAX_TIPS: usize = 4096;
/// Distinct files one pass enqueues at most.
pub const MAX_FILES: usize = 2_000_000;
/// Longest accepted glob.
pub const MAX_GLOB: usize = 256;
/// Files handed to the queue per call.
const BATCH: usize = 5000;
/// Repositories one `run_all` visits at most.
const MAX_REPOS: i64 = 100_000;

/// Release branches and open PR heads; the default branch is added per repository.
pub const IMPORT_REFS: [&str; 3] = [
    "refs/heads/release/**",
    "refs/heads/release-*",
    "refs/pull/*/head",
];

// ------------------------------------------------------------------------------------ globs

/// True when `pat` matches all of `text`. `*` matches any run without `/`, `**` any run,
/// `?` one character other than `/`. Runs in O(len(pat) x len(text)).
pub fn glob_match(pat: &str, text: &str) -> bool {
    if pat.len() > MAX_GLOB {
        return false;
    }
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // prev[j]: the pattern prefix so far matches the text prefix of length j.
    let mut prev = vec![false; t.len() + 1];
    prev[0] = true;
    let mut i = 0;
    while i < p.len() {
        let mut cur = vec![false; t.len() + 1];
        if p[i] == '*' {
            let double = p.get(i + 1) == Some(&'*');
            cur[0] = prev[0];
            for j in 1..=t.len() {
                let free = double || t[j - 1] != '/';
                cur[j] = prev[j] || (free && cur[j - 1]);
            }
            if double {
                i += 1;
            }
        } else {
            for j in 1..=t.len() {
                let ok = if p[i] == '?' {
                    t[j - 1] != '/'
                } else {
                    t[j - 1] == p[i]
                };
                cur[j] = ok && prev[j - 1];
            }
        }
        prev = cur;
        i += 1;
    }
    prev[t.len()]
}

/// Seconds since the epoch of `YYYY-MM-DD` (UTC midnight).
pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    if ![y, m, d].iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&m) || d < 1 || d > dim[usize::try_from(m - 1).ok()?] {
        return None;
    }
    // Days from civil date.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400)
}

/// Arguments of `asc serve admin codeintel backfill`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackfillArgs {
    pub refs: String,
    pub since: Option<i64>,
}

impl BackfillArgs {
    /// Parses `--refs <glob> [--since <YYYY-MM-DD>]`. Anything else is an error.
    pub fn parse(args: &[String]) -> std::result::Result<Self, String> {
        let (mut refs, mut since) = (None, None);
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let (name, inline) = match a.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (a.as_str(), None),
            };
            let value = match inline {
                Some(v) => v,
                None => it
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))?,
            };
            match name {
                "--refs" if refs.is_none() => {
                    if value.is_empty() || value.len() > MAX_GLOB {
                        return Err("--refs: glob is empty or too long".into());
                    }
                    refs = Some(value);
                }
                "--since" if since.is_none() => {
                    since = Some(parse_date(&value).ok_or("--since: expected YYYY-MM-DD")?);
                }
                _ => return Err(format!("unknown or repeated argument {name}")),
            }
        }
        Ok(Self {
            refs: refs.ok_or("--refs <glob> is required")?,
            since,
        })
    }
}

// ----------------------------------------------------------------------------------- sources

/// A ref and the revision it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefTip {
    pub name: String,
    pub rev: ChangesetId,
    /// When the ref last moved (seconds), 0 when unknown.
    pub at: i64,
}

/// Lists a repository's refs.
#[async_trait]
pub trait TipSource: Send + Sync {
    /// At most `MAX_TIPS + 1` tips, so the caller can tell the list was cut.
    async fn tips(&self, repo_id: i64) -> Result<Vec<RefTip>>;
}

/// Resolves a revision to the `FileId` bytes of every file its tree holds.
#[async_trait]
pub trait HeadFiles: Send + Sync {
    async fn files(&self, repo_id: i64, rev: ChangesetId) -> Result<Vec<Vec<u8>>>;
}

/// [`TipSource`] over the `refs` and `ref_log` tables.
pub struct SqlTips {
    pool: AnyPool,
}

impl SqlTips {
    pub fn new(pool: AnyPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TipSource for SqlTips {
    async fn tips(&self, repo_id: i64) -> Result<Vec<RefTip>> {
        let rows = sqlx::query(
            "SELECT r.name AS name, r.target AS target, \
             CAST(COALESCE((SELECT MAX(l.at) FROM ref_log l \
                            WHERE l.repo_id = r.repo_id AND l.name = r.name), 0) AS BIGINT) AS at \
             FROM refs r WHERE r.repo_id = $1 ORDER BY r.name LIMIT $2",
        )
        .bind(repo_id)
        .bind(i64::try_from(MAX_TIPS + 1).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let target: Vec<u8> = r.try_get("target")?;
            // Tags and malformed targets carry no tree to index.
            if let Ok(RefTarget::Changeset(rev)) = RefTarget::from_bytes(&target) {
                out.push(RefTip {
                    name: r.try_get("name")?,
                    rev,
                    at: r.try_get("at")?,
                });
            }
        }
        Ok(out)
    }
}

/// The `ChangeSource` slot of [`EnqueueEffect`], which backfill never uses.
struct NoChanges;

#[async_trait]
impl ChangeSource for NoChanges {
    async fn written(&self, _: i64, _: &[Vec<u8>]) -> std::result::Result<Written, EffectError> {
        Err(EffectError("backfill does not resolve changes".into()))
    }
}

// ----------------------------------------------------------------------------------- backfill

/// What a pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Revisions indexed.
    pub tips: usize,
    /// Distinct files met.
    pub files: usize,
    /// Jobs newly enqueued (files `ci_docs` did not know and no job existed for).
    pub enqueued: u64,
    /// The ref list or the file budget was cut.
    pub truncated: bool,
}

/// Enqueues the files of chosen heads.
pub struct Backfill {
    pool: AnyPool,
    tips: Arc<dyn TipSource>,
    heads: Arc<dyn HeadFiles>,
    enqueue: EnqueueEffect,
}

impl Backfill {
    pub fn new(pool: AnyPool, tips: Arc<dyn TipSource>, heads: Arc<dyn HeadFiles>) -> Self {
        Self {
            enqueue: EnqueueEffect::new(pool.clone(), Arc::new(NoChanges)),
            pool,
            tips,
            heads,
        }
    }

    async fn index(
        &self,
        repo_id: i64,
        ns: &[u8],
        revs: &[ChangesetId],
        priority: Priority,
        report: &mut Report,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        let mut revs_seen = HashSet::new();
        for rev in revs {
            if !revs_seen.insert(*rev) {
                continue;
            }
            let files = self.heads.files(repo_id, *rev).await?;
            let mut fresh = Vec::new();
            for f in files {
                if seen.len() >= MAX_FILES {
                    report.truncated = true;
                    break;
                }
                if seen.insert(f.clone()) {
                    fresh.push(f);
                }
            }
            for part in fresh.chunks(BATCH) {
                report.enqueued += self.enqueue.enqueue_unknown(ns, part, priority).await?;
            }
            report.tips += 1;
            if report.truncated {
                break;
            }
        }
        report.files = seen.len();
        Ok(())
    }

    /// Indexes the heads of `repo_id` whose ref name matches any of `globs` and that moved at
    /// or after `since`, at backfill priority.
    pub async fn run(
        &self,
        repo_id: i64,
        ns: &[u8],
        globs: &[String],
        since: Option<i64>,
    ) -> Result<Report> {
        let mut report = Report::default();
        let mut tips = self.tips.tips(repo_id).await?;
        if tips.len() > MAX_TIPS {
            tips.truncate(MAX_TIPS);
            report.truncated = true;
        }
        let revs: Vec<ChangesetId> = tips
            .iter()
            .filter(|t| globs.iter().any(|g| glob_match(g, &t.name)))
            .filter(|t| since.is_none_or(|s| t.at >= s))
            .map(|t| t.rev)
            .collect();
        self.index(repo_id, ns, &revs, Priority::Backfill, &mut report)
            .await?;
        Ok(report)
    }

    /// The end of an import: the default branch, release branches and PR heads only.
    pub async fn after_import(&self, repo_id: i64, ns: &[u8]) -> Result<Report> {
        let row = sqlx::query("SELECT default_branch FROM repos WHERE id = $1")
            .bind(repo_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Err(CiError::Invalid("unknown repository"));
        };
        let default: String = row.try_get("default_branch")?;
        let default = if default.starts_with("refs/") {
            default
        } else {
            format!("refs/heads/{default}")
        };
        let mut globs = vec![literal(&default)];
        globs.extend(IMPORT_REFS.map(String::from));
        self.run(repo_id, ns, &globs, None).await
    }

    /// `rev` met a query before its files were indexed: enqueue the unknown ones ahead of all
    /// other work.
    pub async fn on_demand(&self, repo_id: i64, ns: &[u8], rev: ChangesetId) -> Result<Report> {
        let mut report = Report::default();
        self.index(repo_id, ns, &[rev], Priority::Interactive, &mut report)
            .await?;
        Ok(report)
    }

    /// The admin command: runs `args` over every repository.
    pub async fn run_all(&self, args: &BackfillArgs) -> Result<Vec<(String, Report)>> {
        let rows = sqlx::query("SELECT id, name, ns FROM repos ORDER BY id LIMIT $1")
            .bind(MAX_REPOS)
            .fetch_all(&self.pool)
            .await?;
        let globs = [args.refs.clone()];
        let mut out = Vec::new();
        for r in rows {
            let (id, name, ns): (i64, String, Vec<u8>) =
                (r.try_get("id")?, r.try_get("name")?, r.try_get("ns")?);
            out.push((name, self.run(id, &ns, &globs, args.since).await?));
        }
        Ok(out)
    }
}

/// A pattern that matches only `name`. Git forbids `*` and `?` in ref names, so a name that
/// holds them cannot be a real ref; it gets a pattern that matches nothing.
fn literal(name: &str) -> String {
    if name.contains(['*', '?']) {
        "\0".into()
    } else {
        name.into()
    }
}

// ------------------------------------------------------------------------------------- forks

/// True for objects `Fork` and `CopyClosure` copy: the content closure. Derived values
/// (`d/tdocs.v1`, `d/tags.v1`, `d/chunks.v1`, `d/ciseg.v*`, `d/civec.v*`, `d/ciidx.v*`, ...)
/// are per-namespace and recomputable, so none is copied.
pub fn is_copied(key: &ObjKey) -> bool {
    !matches!(key, ObjKey::Derived { .. })
}

/// Copies the content objects among `keys` from `src` to `dst` as stored bytes (the
/// destination verifies them against the key) and returns how many it copied. Derived keys are
/// skipped; a missing source object is an error.
pub async fn copy_closure<S: Objects + ?Sized, D: Objects + ?Sized>(
    src: &S,
    dst: &D,
    keys: &[ObjKey],
) -> Result<u64> {
    let mut copied = 0;
    for key in keys.iter().filter(|k| is_copied(k)) {
        let stored: Bytes = src
            .get_stored(key)
            .await
            .map_err(|e| CiError::Store(e.to_string()))?
            .ok_or(CiError::Invalid("object missing from the source namespace"))?;
        dst.put(key, stored)
            .await
            .map_err(|e| CiError::Store(e.to_string()))?;
        copied += 1;
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::codeintel::modelgen::tests::pool;
    use crate::codeintel::queue::Queue;

    const NS: &[u8] = b"ns-one";

    fn rev(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    fn file(n: u8) -> Vec<u8> {
        vec![n; 32]
    }

    struct Tips(Vec<RefTip>);

    #[async_trait]
    impl TipSource for Tips {
        async fn tips(&self, _: i64) -> Result<Vec<RefTip>> {
            Ok(self.0.clone())
        }
    }

    struct Trees(HashMap<ChangesetId, Vec<Vec<u8>>>);

    #[async_trait]
    impl HeadFiles for Trees {
        async fn files(&self, _: i64, r: ChangesetId) -> Result<Vec<Vec<u8>>> {
            Ok(self.0.get(&r).cloned().unwrap_or_default())
        }
    }

    fn tip(name: &str, n: u8, at: i64) -> RefTip {
        RefTip {
            name: name.into(),
            rev: rev(n),
            at,
        }
    }

    async fn rig() -> (Backfill, AnyPool) {
        let pool = pool().await;
        sqlx::query(
            "INSERT INTO repos (id, name, ns, default_branch, created) \
             VALUES (1, 'r', $1, 'master', 0)",
        )
        .bind(NS.to_vec())
        .execute(&pool)
        .await
        .unwrap();
        // rev 1 master {1,2}; rev 2 release {2,3}; rev 3 PR {3,4}; rev 4 old branch {5};
        // rev 9 is history only {6,7,8}.
        let trees = Trees(HashMap::from([
            (rev(1), vec![file(1), file(2)]),
            (rev(2), vec![file(2), file(3)]),
            (rev(3), vec![file(3), file(4)]),
            (rev(4), vec![file(5)]),
            (rev(9), vec![file(6), file(7), file(8)]),
        ]));
        let tips = Tips(vec![
            tip("refs/heads/master", 1, 1_000),
            tip("refs/heads/release/1.0", 2, 500),
            tip("refs/pull/7/head", 3, 2_000),
            tip("refs/heads/old-topic", 4, 10),
        ]);
        (
            Backfill::new(pool.clone(), Arc::new(tips), Arc::new(trees)),
            pool,
        )
    }

    async fn queued(pool: &AnyPool) -> Vec<(Vec<u8>, i64)> {
        sqlx::query(
            "SELECT key, CAST(priority AS BIGINT) AS p FROM ci_jobs WHERE ns = $1 ORDER BY key",
        )
        .bind(NS.to_vec())
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get("key"), r.get("p")))
        .collect()
    }

    fn args(v: &[&str]) -> std::result::Result<BackfillArgs, String> {
        BackfillArgs::parse(&v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn globs_follow_path_rules() {
        assert!(glob_match("refs/heads/*", "refs/heads/master"));
        assert!(!glob_match("refs/heads/*", "refs/heads/a/b"));
        assert!(glob_match("refs/heads/**", "refs/heads/a/b"));
        assert!(glob_match("refs/pull/*/head", "refs/pull/12/head"));
        assert!(!glob_match("refs/pull/*/head", "refs/pull/12/merge"));
        assert!(glob_match("refs/heads/release-?", "refs/heads/release-1"));
        assert!(!glob_match("a", "ab"));
        assert!(!glob_match(&"a".repeat(MAX_GLOB + 1), "a"));
    }

    #[test]
    fn dates_and_arguments_parse() {
        assert_eq!(parse_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_date("2024-02-29"), Some(1_709_164_800));
        assert_eq!(parse_date("2023-02-29"), None);
        assert_eq!(parse_date("2024-1-01"), None);
        assert_eq!(parse_date("+024-01-01"), None);
        assert_eq!(
            args(&["--refs", "refs/heads/**", "--since=1970-01-02"]).unwrap(),
            BackfillArgs {
                refs: "refs/heads/**".into(),
                since: Some(86_400)
            }
        );
        assert!(args(&[]).is_err());
        assert!(args(&["--refs"]).is_err());
        assert!(args(&["--refs", "x", "--refs", "y"]).is_err());
        assert!(args(&["--refs", "x", "--since", "soon"]).is_err());
        assert!(args(&["--bogus", "x"]).is_err());
    }

    #[tokio::test]
    async fn import_enqueues_only_head_file_ids() {
        let (bf, pool) = rig().await;
        let r = bf.after_import(1, NS).await.unwrap();
        assert_eq!(r.tips, 3);
        let jobs = queued(&pool).await;
        let keys: Vec<_> = jobs.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![file(1), file(2), file(3), file(4)]);
        assert_eq!(r.enqueued, 4);
        // Neither the old topic branch nor history is indexed.
        assert!(!keys.contains(&file(5)) && !keys.contains(&file(6)));
        assert!(jobs.iter().all(|(_, p)| *p == Priority::Backfill as i64));
        // Idempotent.
        assert_eq!(bf.after_import(1, NS).await.unwrap().enqueued, 0);
    }

    #[tokio::test]
    async fn indexed_files_are_not_enqueued_again() {
        let (bf, pool) = rig().await;
        sqlx::query(
            "INSERT INTO ci_docs (ns, file, ord, size, flags, versions) \
             VALUES ($1, $2, 0, 1, 0, $3)",
        )
        .bind(NS.to_vec())
        .bind(file(1))
        .bind(vec![0u8; 12])
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(bf.after_import(1, NS).await.unwrap().enqueued, 3);
    }

    #[tokio::test]
    async fn backfill_honours_the_glob_and_since() {
        let (bf, pool) = rig().await;
        let globs = ["refs/heads/**".to_owned()];
        let r = bf.run(1, NS, &globs, Some(100)).await.unwrap();
        assert_eq!((r.tips, r.enqueued), (2, 3)); // master and release; old-topic is too old
        let r = bf.run(1, NS, &globs, None).await.unwrap();
        assert_eq!((r.tips, r.enqueued), (3, 1)); // old-topic adds file 5
        assert_eq!(queued(&pool).await.len(), 4);
        let none = bf
            .run(1, NS, &["refs/nothing".to_owned()], None)
            .await
            .unwrap();
        assert_eq!((none.tips, none.enqueued), (0, 0));
    }

    #[tokio::test]
    async fn an_old_revision_is_indexed_on_demand_ahead_of_backfill() {
        let (bf, pool) = rig().await;
        bf.after_import(1, NS).await.unwrap();
        let r = bf.on_demand(1, NS, rev(9)).await.unwrap();
        assert_eq!(r.enqueued, 3);
        let q = Queue::new(pool.clone());
        let leased = q
            .lease(NS, 4, 60, crate::outbox::now() + 10)
            .await
            .unwrap();
        assert_eq!(leased.len(), 4);
        assert!(
            leased[..3]
                .iter()
                .all(|j| j.priority == Priority::Interactive)
        );
        assert_eq!(leased[3].priority, Priority::Backfill);
    }

    #[tokio::test]
    async fn run_all_visits_each_repository() {
        let (bf, _) = rig().await;
        let out = bf
            .run_all(&BackfillArgs {
                refs: "refs/pull/**".into(),
                since: None,
            })
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "r");
        assert_eq!(out[0].1.tips, 1);
    }
}
