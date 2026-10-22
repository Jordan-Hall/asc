//! The mainline and path index of the land branches (SPEC §5.1).
//!
//! `mainline` numbers the first-parent changesets of a land branch from 0; `path_changes` records,
//! per position, every changed path and each of its ancestor directories (root included) under a
//! 16-byte key. Ancestry, path-filtered logs, `LastCommits` and blame candidates are then range
//! scans over these tables: no changeset object is read. The write side ([`apply`]) runs inside the
//! `UpdateRefs` step-8 transaction; the read side is the [`MainlineIndex`] trait.
//!
//! The index holds no tree data. The caller (U46's step 8) computes each new first-parent
//! changeset's changed paths from the engine and passes them in as [`MainlineStep`]s.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use asc_core::id::ChangesetId;
use asc_core::path::{Name, RepoPath};
use sqlx::any::Any;
use sqlx::{AnyPool, Row, Transaction};

use crate::registry::Ns;

/// Ids per `IN (...)` lookup; keeps statements under every driver's parameter limit.
const LOOKUP_BATCH: usize = 500;
/// Most paths a path-filtered `Log` or blame lookup may name.
pub const MAX_QUERY_PATHS: usize = 256;
/// Most names one `LastCommits` call may ask about.
pub const MAX_LAST_COMMITS: usize = 10_000;
/// Largest page a read returns.
pub const MAX_PAGE: usize = 1_000;
/// Most changed paths one mainline step may carry.
pub const MAX_STEP_PATHS: usize = 1_000_000;

/// Why an index read or write failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("mainline: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("mainline: base {0} is not on the mainline")]
    BaseMissing(ChangesetId),
    #[error("mainline: too many {what} (max {max})")]
    TooMany { what: &'static str, max: usize },
    #[error("mainline: damaged row: {0}")]
    Corrupt(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A 0-based position on a land branch's first-parent chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos(pub u64);

impl Pos {
    fn to_sql(self) -> i64 {
        i64::try_from(self.0).unwrap_or(i64::MAX)
    }

    fn from_sql(v: i64) -> Result<Self> {
        u64::try_from(v)
            .map(Pos)
            .map_err(|_| Error::Corrupt("negative position"))
    }
}

/// One new first-parent changeset and the paths it changed (files only; directories are derived).
#[derive(Debug, Clone)]
pub struct MainlineStep {
    pub cs: ChangesetId,
    pub paths: Vec<RepoPath>,
}

/// One cursor-paginated read (SPEC §5.1: never scan skipped entries).
#[derive(Debug, Clone, Copy)]
pub struct Page {
    /// Return positions strictly below this one; `None` starts at the tip.
    pub before: Option<Pos>,
    /// Entries wanted; clamped to `1..=MAX_PAGE`.
    pub limit: usize,
}

/// A page of entries, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    pub entries: Vec<(Pos, ChangesetId)>,
    /// The cursor for the next page (`Page::before`), when more entries exist.
    pub next: Option<Pos>,
}

/// The first 16 bytes of `blake3(path)`; the empty path is the root directory.
pub fn path_key(path: &[u8]) -> [u8; 16] {
    let h = blake3::hash(path);
    let mut k = [0u8; 16];
    k.copy_from_slice(&h.as_bytes()[..16]);
    k
}

/// The keys a change to `path` is recorded under: the path, each ancestor directory and the root.
fn keys_for(path: &RepoPath) -> Vec<[u8; 16]> {
    let mut keys = vec![path_key(path.as_bytes())];
    let mut cur = path.clone();
    while let Some(parent) = cur.parent() {
        keys.push(path_key(parent.as_bytes()));
        cur = parent;
    }
    keys.push(path_key(b""));
    keys
}

fn id_of(bytes: &[u8]) -> Result<ChangesetId> {
    <[u8; 32]>::try_from(bytes)
        .map(ChangesetId::from_bytes)
        .map_err(|_| Error::Corrupt("changeset id is not 32 bytes"))
}

fn placeholders(n: usize, from: usize) -> String {
    (0..n)
        .map(|i| format!("${}", from + i))
        .collect::<Vec<_>>()
        .join(",")
}

async fn position_in(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    refname: &str,
    cs: ChangesetId,
) -> Result<Option<Pos>> {
    let row = sqlx::query("SELECT pos FROM mainline WHERE ns = $1 AND ref = $2 AND cs = $3")
        .bind(ns.as_bytes().to_vec())
        .bind(refname)
        .bind(cs.as_bytes().to_vec())
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|r| Pos::from_sql(r.try_get("pos")?)).transpose()
}

/// Deletes every row with a position above `keep` (`None` empties the branch).
async fn truncate(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    refname: &str,
    keep: Option<Pos>,
) -> Result<()> {
    let above = keep.map_or(-1, Pos::to_sql);
    for table in ["path_changes", "mainline"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE ns = $1 AND ref = $2 AND pos > $3"
        )))
        .bind(ns.as_bytes().to_vec())
        .bind(refname)
        .bind(above)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Moves the index of `refname` to a new tip, inside the caller's transaction.
///
/// `base` is the last changeset the new `steps` extend: the old tip for a fast-forward, or the
/// fork point of a forced update (`None` rebuilds from position 0). Rows above `base` are deleted
/// first, then each step is appended at the next position. Returns the new tip position.
pub async fn apply(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    refname: &str,
    base: Option<ChangesetId>,
    steps: &[MainlineStep],
) -> Result<Option<Pos>> {
    let base_pos = match base {
        Some(cs) => Some(
            position_in(tx, ns, refname, cs)
                .await?
                .ok_or(Error::BaseMissing(cs))?,
        ),
        None => None,
    };
    truncate(tx, ns, refname, base_pos).await?;
    let mut next = base_pos.map_or(0, |p| p.0 + 1);
    for step in steps {
        if step.paths.len() > MAX_STEP_PATHS {
            return Err(Error::TooMany {
                what: "paths in one step",
                max: MAX_STEP_PATHS,
            });
        }
        let pos = Pos(next).to_sql();
        sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, $3, $4)")
            .bind(ns.as_bytes().to_vec())
            .bind(refname)
            .bind(pos)
            .bind(step.cs.as_bytes().to_vec())
            .execute(&mut **tx)
            .await?;
        let keys: HashSet<[u8; 16]> = step
            .paths
            .iter()
            .flat_map(keys_for)
            .chain(std::iter::once(path_key(b"")))
            .collect();
        for key in keys {
            sqlx::query(
                "INSERT INTO path_changes (ns, ref, path_key, pos) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (ns, ref, path_key, pos) DO NOTHING",
            )
            .bind(ns.as_bytes().to_vec())
            .bind(refname)
            .bind(key.to_vec())
            .bind(pos)
            .execute(&mut **tx)
            .await?;
        }
        next += 1;
    }
    Ok(next.checked_sub(1).map(Pos))
}

/// Reads of the mainline and path index. U46 (`IsAncestor`, `FindLanding`), U47 (`Log`,
/// `LastCommits`, `Compare`) and U28a's blame-candidate hook call this.
#[async_trait]
pub trait MainlineIndex: Send + Sync {
    /// The position of `cs` on `refname`'s mainline, if it is on it.
    async fn position(&self, ns: Ns, refname: &str, cs: ChangesetId) -> Result<Option<Pos>>;

    /// The newest mainline entry.
    async fn tip(&self, ns: Ns, refname: &str) -> Result<Option<(Pos, ChangesetId)>>;

    /// `Some(answer)` when both ids are on the mainline (compares positions, O(1)); `None` when
    /// either is not, and the caller falls back to the generation-pruned walk (SPEC §4.3).
    async fn is_ancestor(
        &self,
        ns: Ns,
        refname: &str,
        ancestor: ChangesetId,
        tip: ChangesetId,
    ) -> Result<Option<bool>>;

    /// Mainline changesets that changed any of `paths` (or a path under one), newest first.
    /// No paths means the whole mainline.
    async fn log(&self, ns: Ns, refname: &str, paths: &[RepoPath], page: Page) -> Result<LogPage>;

    /// For each name under `dir` (`None` is the root), the newest mainline changeset that changed
    /// it. One batched `MAX(pos)` query per [`LOOKUP_BATCH`] names.
    async fn last_commits(
        &self,
        ns: Ns,
        refname: &str,
        dir: Option<&RepoPath>,
        names: &[Name],
    ) -> Result<Vec<Option<(Pos, ChangesetId)>>>;

    /// Blame candidates for `path`: the mainline changesets that touched it, newest first. The
    /// caller adds `copy_from` hops.
    async fn blame_candidates(
        &self,
        ns: Ns,
        refname: &str,
        path: &RepoPath,
        page: Page,
    ) -> Result<LogPage> {
        self.log(ns, refname, std::slice::from_ref(path), page)
            .await
    }
}

/// [`MainlineIndex`] over the server's SQL tables.
pub struct SqlMainline {
    pool: AnyPool,
    rows: AtomicU64,
}

impl SqlMainline {
    pub fn new(pool: AnyPool) -> Self {
        Self {
            pool,
            rows: AtomicU64::new(0),
        }
    }

    /// Index rows returned by reads so far; tests assert the O(result) bound with it.
    pub fn rows_read(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }

    fn count(&self, n: usize) {
        self.rows.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Resolves `positions` to ids, newest first.
    async fn entries(
        &self,
        ns: Ns,
        refname: &str,
        positions: &[Pos],
    ) -> Result<Vec<(Pos, ChangesetId)>> {
        let mut out = Vec::with_capacity(positions.len());
        for chunk in positions.chunks(LOOKUP_BATCH) {
            let sql = format!(
                "SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 AND pos IN ({})",
                placeholders(chunk.len(), 3)
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(ns.as_bytes().to_vec())
                .bind(refname.to_owned());
            for p in chunk {
                q = q.bind(p.to_sql());
            }
            let rows = q.fetch_all(&self.pool).await?;
            self.count(rows.len());
            for row in rows {
                let cs: Vec<u8> = row.try_get("cs")?;
                out.push((Pos::from_sql(row.try_get("pos")?)?, id_of(&cs)?));
            }
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.0));
        Ok(out)
    }
}

fn clamp(limit: usize) -> usize {
    limit.clamp(1, MAX_PAGE)
}

/// Trims a `limit + 1` fetch to a page and sets the cursor when an extra entry was found.
fn finish(mut entries: Vec<(Pos, ChangesetId)>, limit: usize) -> LogPage {
    let more = entries.len() > limit;
    entries.truncate(limit);
    let next = if more {
        entries.last().map(|e| e.0)
    } else {
        None
    };
    LogPage { entries, next }
}

#[async_trait]
impl MainlineIndex for SqlMainline {
    async fn position(&self, ns: Ns, refname: &str, cs: ChangesetId) -> Result<Option<Pos>> {
        let row = sqlx::query("SELECT pos FROM mainline WHERE ns = $1 AND ref = $2 AND cs = $3")
            .bind(ns.as_bytes().to_vec())
            .bind(refname)
            .bind(cs.as_bytes().to_vec())
            .fetch_optional(&self.pool)
            .await?;
        self.count(usize::from(row.is_some()));
        row.map(|r| Pos::from_sql(r.try_get("pos")?)).transpose()
    }

    async fn tip(&self, ns: Ns, refname: &str) -> Result<Option<(Pos, ChangesetId)>> {
        let row = sqlx::query(
            "SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 ORDER BY pos DESC LIMIT 1",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(refname)
        .fetch_optional(&self.pool)
        .await?;
        self.count(usize::from(row.is_some()));
        row.map(|r| {
            let cs: Vec<u8> = r.try_get("cs")?;
            Ok((Pos::from_sql(r.try_get("pos")?)?, id_of(&cs)?))
        })
        .transpose()
    }

    async fn is_ancestor(
        &self,
        ns: Ns,
        refname: &str,
        ancestor: ChangesetId,
        tip: ChangesetId,
    ) -> Result<Option<bool>> {
        let rows = sqlx::query(
            "SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 AND cs IN ($3, $4)",
        )
        .bind(ns.as_bytes().to_vec())
        .bind(refname)
        .bind(ancestor.as_bytes().to_vec())
        .bind(tip.as_bytes().to_vec())
        .fetch_all(&self.pool)
        .await?;
        self.count(rows.len());
        let (mut a, mut t) = (None, None);
        for row in rows {
            let cs: Vec<u8> = row.try_get("cs")?;
            let pos = Pos::from_sql(row.try_get("pos")?)?;
            let id = id_of(&cs)?;
            if id == ancestor {
                a = Some(pos);
            }
            if id == tip {
                t = Some(pos);
            }
        }
        // Both on the first-parent chain: the earlier one is the ancestor of the later one.
        Ok(a.zip(t).map(|(a, t)| a <= t))
    }

    async fn log(&self, ns: Ns, refname: &str, paths: &[RepoPath], page: Page) -> Result<LogPage> {
        if paths.len() > MAX_QUERY_PATHS {
            return Err(Error::TooMany {
                what: "paths",
                max: MAX_QUERY_PATHS,
            });
        }
        let limit = clamp(page.limit);
        let before = page.before.map_or(i64::MAX, Pos::to_sql);
        let want = i64::try_from(limit + 1).unwrap_or(i64::MAX);
        if paths.is_empty() {
            let rows = sqlx::query(
                "SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 AND pos < $3 \
                 ORDER BY pos DESC LIMIT $4",
            )
            .bind(ns.as_bytes().to_vec())
            .bind(refname)
            .bind(before)
            .bind(want)
            .fetch_all(&self.pool)
            .await?;
            self.count(rows.len());
            let mut entries = Vec::with_capacity(rows.len());
            for row in rows {
                let cs: Vec<u8> = row.try_get("cs")?;
                entries.push((Pos::from_sql(row.try_get("pos")?)?, id_of(&cs)?));
            }
            return Ok(finish(entries, limit));
        }
        let keys: Vec<[u8; 16]> = paths
            .iter()
            .map(|p| path_key(p.as_bytes()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let sql = format!(
            "SELECT DISTINCT pos FROM path_changes WHERE ns = $1 AND ref = $2 \
             AND pos < $3 AND path_key IN ({}) ORDER BY pos DESC LIMIT $4",
            placeholders(keys.len(), 5)
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ns.as_bytes().to_vec())
            .bind(refname.to_owned())
            .bind(before)
            .bind(want);
        for k in &keys {
            q = q.bind(k.to_vec());
        }
        let rows = q.fetch_all(&self.pool).await?;
        self.count(rows.len());
        let positions = rows
            .iter()
            .map(|r| Pos::from_sql(r.try_get("pos")?))
            .collect::<Result<Vec<_>>>()?;
        Ok(finish(self.entries(ns, refname, &positions).await?, limit))
    }

    async fn last_commits(
        &self,
        ns: Ns,
        refname: &str,
        dir: Option<&RepoPath>,
        names: &[Name],
    ) -> Result<Vec<Option<(Pos, ChangesetId)>>> {
        if names.len() > MAX_LAST_COMMITS {
            return Err(Error::TooMany {
                what: "names",
                max: MAX_LAST_COMMITS,
            });
        }
        let keys: Vec<[u8; 16]> = names
            .iter()
            .map(|n| match dir {
                Some(d) => path_key(d.join(n).as_bytes()),
                None => path_key(n.as_bytes()),
            })
            .collect();
        let mut newest: HashMap<[u8; 16], Pos> = HashMap::with_capacity(keys.len());
        for chunk in keys.chunks(LOOKUP_BATCH) {
            let sql = format!(
                "SELECT path_key, MAX(pos) AS pos FROM path_changes WHERE ns = $1 AND ref = $2 \
                 AND path_key IN ({}) GROUP BY path_key",
                placeholders(chunk.len(), 3)
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(ns.as_bytes().to_vec())
                .bind(refname.to_owned());
            for k in chunk {
                q = q.bind(k.to_vec());
            }
            let rows = q.fetch_all(&self.pool).await?;
            self.count(rows.len());
            for row in rows {
                let key: Vec<u8> = row.try_get("path_key")?;
                let key = <[u8; 16]>::try_from(key.as_slice())
                    .map_err(|_| Error::Corrupt("path key is not 16 bytes"))?;
                newest.insert(key, Pos::from_sql(row.try_get("pos")?)?);
            }
        }
        let mut positions: Vec<Pos> = newest.values().copied().collect();
        positions.sort_unstable();
        positions.dedup();
        let ids: HashMap<Pos, ChangesetId> = self
            .entries(ns, refname, &positions)
            .await?
            .into_iter()
            .collect();
        Ok(keys
            .iter()
            .map(|k| {
                let pos = *newest.get(k)?;
                Some((pos, *ids.get(&pos)?))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::migrate;

    const REF: &str = "refs/heads/master";
    const FILES: [&str; 6] = [
        "a/b/c.rs",
        "a/b/f.rs",
        "a/d.rs",
        "e.rs",
        "g/h/i/j.txt",
        "README",
    ];

    fn cs(n: u32) -> ChangesetId {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        ChangesetId::from_bytes(b)
    }

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn first(limit: usize) -> Page {
        Page {
            before: None,
            limit,
        }
    }

    async fn pool() -> AnyPool {
        let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        pool
    }

    async fn run(
        pool: &AnyPool,
        ns: Ns,
        base: Option<ChangesetId>,
        steps: &[MainlineStep],
    ) -> Result<Option<Pos>> {
        let mut tx = pool.begin().await?;
        let tip = apply(&mut tx, ns, REF, base, steps).await?;
        tx.commit().await?;
        Ok(tip)
    }

    /// Deterministic generator; no external randomness.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    /// The from-scratch model: each position's changeset and changed files.
    type Model = Vec<(ChangesetId, BTreeSet<&'static str>)>;

    fn expected_keys(model: &Model) -> BTreeSet<([u8; 16], u64)> {
        let mut out = BTreeSet::new();
        for (pos, (_, files)) in model.iter().enumerate() {
            out.insert((path_key(b""), pos as u64));
            for f in files {
                for k in keys_for(&rp(f)) {
                    out.insert((k, pos as u64));
                }
            }
        }
        out
    }

    type Dump = (Vec<(u64, ChangesetId)>, BTreeSet<([u8; 16], u64)>);

    async fn dump(pool: &AnyPool, ns: Ns) -> Dump {
        let m = sqlx::query("SELECT pos, cs FROM mainline WHERE ns = $1 AND ref = $2 ORDER BY pos")
            .bind(ns.as_bytes().to_vec())
            .bind(REF)
            .fetch_all(pool)
            .await
            .unwrap();
        let mainline = m
            .iter()
            .map(|r| {
                let cs: Vec<u8> = r.get("cs");
                (r.get::<i64, _>("pos") as u64, id_of(&cs).unwrap())
            })
            .collect();
        let p = sqlx::query("SELECT path_key, pos FROM path_changes WHERE ns = $1 AND ref = $2")
            .bind(ns.as_bytes().to_vec())
            .bind(REF)
            .fetch_all(pool)
            .await
            .unwrap();
        let paths = p
            .iter()
            .map(|r| {
                let k: Vec<u8> = r.get("path_key");
                (
                    <[u8; 16]>::try_from(k.as_slice()).unwrap(),
                    r.get::<i64, _>("pos") as u64,
                )
            })
            .collect();
        (mainline, paths)
    }

    fn random_step(rng: &mut Lcg, id: u32) -> (MainlineStep, BTreeSet<&'static str>) {
        let n = 1 + rng.next(3);
        let files: BTreeSet<&'static str> = (0..n).map(|_| FILES[rng.next(FILES.len())]).collect();
        let step = MainlineStep {
            cs: cs(id),
            paths: files.iter().map(|f| rp(f)).collect(),
        };
        (step, files)
    }

    #[tokio::test]
    async fn index_equals_recomputation_after_random_sequences() {
        for seed in 1..=4u64 {
            let pool = pool().await;
            let ns = Ns::random();
            let idx = SqlMainline::new(pool.clone());
            let mut rng = Lcg(seed);
            let mut model: Model = Vec::new();
            let mut next_id = 1u32;
            for _ in 0..60 {
                // Land (extend the tip), force (fork earlier) or merge (a many-path step).
                let kind = rng.next(10);
                let keep = if kind < 2 && !model.is_empty() {
                    rng.next(model.len() + 1)
                } else {
                    model.len()
                };
                let count = if kind < 2 {
                    rng.next(3)
                } else {
                    1 + rng.next(3)
                };
                let base = keep.checked_sub(1).map(|i| model[i].0);
                let mut steps = Vec::new();
                model.truncate(keep);
                for _ in 0..count {
                    let (mut step, mut files) = random_step(&mut rng, next_id);
                    if kind == 9 {
                        for f in FILES {
                            files.insert(f);
                            step.paths.push(rp(f));
                        }
                    }
                    next_id += 1;
                    model.push((step.cs, files));
                    steps.push(step);
                }
                let tip = run(&pool, ns, base, &steps).await.unwrap();
                assert_eq!(
                    tip.map(|p| p.0 + 1),
                    Some(model.len() as u64).filter(|&n| n > 0)
                );

                let (mainline, paths) = dump(&pool, ns).await;
                let want: Vec<_> = model
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (i as u64, m.0))
                    .collect();
                assert_eq!(mainline, want);
                assert_eq!(paths, expected_keys(&model));
            }

            // Reads agree with the model at the end.
            let all = idx.log(ns, REF, &[], first(MAX_PAGE)).await.unwrap();
            assert_eq!(all.entries.len(), model.len());
            for f in FILES {
                let want: Vec<_> = model
                    .iter()
                    .enumerate()
                    .rev()
                    .filter(|(_, m)| m.1.contains(f))
                    .map(|(i, m)| (Pos(i as u64), m.0))
                    .collect();
                let got = idx.log(ns, REF, &[rp(f)], first(MAX_PAGE)).await.unwrap();
                assert_eq!(got.entries, want, "log of {f}");
            }
            let dir_want: Vec<_> = model
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, m)| m.1.iter().any(|f| f.starts_with("a/")))
                .map(|(i, m)| (Pos(i as u64), m.0))
                .collect();
            let got = idx.log(ns, REF, &[rp("a")], first(MAX_PAGE)).await.unwrap();
            assert_eq!(got.entries, dir_want);
            if let Some((tip_pos, tip_cs)) = idx.tip(ns, REF).await.unwrap() {
                assert_eq!(tip_pos.0 + 1, model.len() as u64);
                for (i, m) in model.iter().enumerate() {
                    assert_eq!(
                        idx.position(ns, REF, m.0).await.unwrap(),
                        Some(Pos(i as u64))
                    );
                    assert_eq!(
                        idx.is_ancestor(ns, REF, m.0, tip_cs).await.unwrap(),
                        Some(true)
                    );
                }
                let oldest = model[0].0;
                assert_eq!(
                    idx.is_ancestor(ns, REF, tip_cs, oldest).await.unwrap(),
                    Some(model.len() == 1)
                );
                assert_eq!(
                    idx.is_ancestor(ns, REF, cs(9_999_999), tip_cs)
                        .await
                        .unwrap(),
                    None
                );
            }
        }
    }

    #[tokio::test]
    async fn pagination_uses_cursors() {
        let pool = pool().await;
        let ns = Ns::random();
        let idx = SqlMainline::new(pool.clone());
        let steps: Vec<_> = (0..10)
            .map(|i| MainlineStep {
                cs: cs(i),
                paths: vec![rp("e.rs")],
            })
            .collect();
        run(&pool, ns, None, &steps).await.unwrap();
        let mut seen = Vec::new();
        let mut before = None;
        loop {
            let page = idx
                .log(ns, REF, &[rp("e.rs")], Page { before, limit: 4 })
                .await
                .unwrap();
            seen.extend(page.entries.iter().map(|e| e.0.0));
            match page.next {
                Some(n) => before = Some(n),
                None => break,
            }
        }
        assert_eq!(seen, (0..10).rev().collect::<Vec<u64>>());
    }

    #[tokio::test]
    async fn rejects_unknown_base_and_oversized_queries() {
        let pool = pool().await;
        let ns = Ns::random();
        let err = run(&pool, ns, Some(cs(1)), &[]).await.unwrap_err();
        assert!(matches!(err, Error::BaseMissing(_)));
        let idx = SqlMainline::new(pool);
        let many = vec![rp("e.rs"); MAX_QUERY_PATHS + 1];
        let err = idx.log(ns, REF, &many, first(10)).await.unwrap_err();
        assert!(matches!(err, Error::TooMany { .. }));
    }

    #[tokio::test]
    async fn last_commits_batches_one_query() {
        let pool = pool().await;
        let ns = Ns::random();
        let idx = SqlMainline::new(pool.clone());
        let steps = vec![
            MainlineStep {
                cs: cs(1),
                paths: vec![rp("a/b/c.rs"), rp("e.rs")],
            },
            MainlineStep {
                cs: cs(2),
                paths: vec![rp("a/d.rs")],
            },
            MainlineStep {
                cs: cs(3),
                paths: vec![rp("a/b/f.rs")],
            },
        ];
        run(&pool, ns, None, &steps).await.unwrap();
        let names = [
            Name::new("b").unwrap(),
            Name::new("d.rs").unwrap(),
            Name::new("zzz").unwrap(),
        ];
        let got = idx
            .last_commits(ns, REF, Some(&rp("a")), &names)
            .await
            .unwrap();
        assert_eq!(
            got,
            vec![Some((Pos(2), cs(3))), Some((Pos(1), cs(2))), None]
        );
    }

    /// SPEC §5.1 rule: reads are O(result + log n) on a 100k-commit history.
    #[tokio::test]
    async fn reads_stay_bounded_on_a_large_history() {
        const N: u32 = 100_000;
        let pool = pool().await;
        let ns = Ns::random();
        let idx = SqlMainline::new(pool.clone());
        // Seeded in bulk SQL with the rows `apply` writes; `apply` itself is covered above.
        let series = format!(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {}) ",
            N - 1
        );
        let sql = format!(
            "{series}INSERT INTO mainline (ns, ref, pos, cs) SELECT $1, $2, i, randomblob(32) FROM n"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ns.as_bytes().to_vec())
            .bind(REF)
            .execute(&pool)
            .await
            .unwrap();
        let seeds: [(&[u8], &str); 5] = [
            (b"", "1 = 1"),
            (b"common", "i % 1000 <> 0"),
            (b"common/y.rs", "i % 1000 <> 0"),
            (b"rare", "i % 1000 = 0"),
            (b"rare/x.rs", "i % 1000 = 0"),
        ];
        for (path, when) in seeds {
            let sql = format!(
                "{series}INSERT INTO path_changes (ns, ref, path_key, pos)                  SELECT $1, $2, $3, i FROM n WHERE {when}"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(ns.as_bytes().to_vec())
                .bind(REF)
                .bind(path_key(path).to_vec())
                .execute(&pool)
                .await
                .unwrap();
        }
        let old = idx
            .log(
                ns,
                REF,
                &[],
                Page {
                    before: Some(Pos(6)),
                    limit: 1,
                },
            )
            .await
            .unwrap();
        let old = old.entries[0].1;
        let (_, tip) = idx.tip(ns, REF).await.unwrap().unwrap();

        let before = idx.rows_read();
        let page = idx.log(ns, REF, &[rp("rare")], first(20)).await.unwrap();
        assert_eq!(page.entries.len(), 20);
        assert!(idx.rows_read() - before <= 2 * 21 + 1);

        let before = idx.rows_read();
        assert_eq!(
            idx.is_ancestor(ns, REF, old, tip).await.unwrap(),
            Some(true)
        );
        assert!(idx.rows_read() - before <= 2);

        let before = idx.rows_read();
        let names = [Name::new("rare").unwrap(), Name::new("common").unwrap()];
        let got = idx.last_commits(ns, REF, None, &names).await.unwrap();
        assert_eq!(got[0].unwrap().0, Pos(99_000));
        assert_eq!(got[1].unwrap().0, Pos(u64::from(N) - 1));
        assert!(idx.rows_read() - before <= 4);

        let before = idx.rows_read();
        let blame = idx
            .blame_candidates(ns, REF, &rp("rare/x.rs"), first(5))
            .await
            .unwrap();
        assert_eq!(blame.entries.len(), 5);
        assert!(idx.rows_read() - before <= 2 * 6 + 1);
    }
}
