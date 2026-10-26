//! `open_paths` and the `Overlaps` call (PLAN C21, SPEC §20.4 and §20.5).
//!
//! `open_paths(ns, path_key, change)` holds, for each open change, the key of every path its head
//! changeset changes and of every ancestor directory of that path (the root excluded), so one
//! indexed `IN (...)` lookup per request path finds the open changes that touch the path or
//! something below it. A key is the first 16 bytes of `blake3` over the path with every component
//! case-folded, so the lookup is also case-insensitive (§4.5). The rows come from the head
//! changeset's own `changes` keys, so no diff is computed.
//!
//! `Overlaps` never reports a change of the requested change's own stack: the whole connected
//! stack (the `parent_change` chain up, and every child down) is excluded.
//!
//! Known gap, by design: a change that replaces a file `a` by a directory is recorded under `a`;
//! a request for `a/b` does not find it. That is a file/directory type flip, which the merge
//! check in [`super::mergeable`] still sees.
//!
//! Everything read from the database is bounded: request paths, rows scanned, rows returned.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use asc_core::change::ChangeId;
use asc_core::path::{RepoPath, casefold};
use asc_rpc::server::wire;
use bytes::Bytes;
use sqlx::AnyConnection;
use tonic::Status;

use super::rpc::Reader;
use super::{fixed, placeholders};
use crate::commit_graph_rpc::internal;
use crate::mainline::path_key;
use crate::registry::Ns;

/// Most paths one request may name.
pub const MAX_QUERY_PATHS: usize = 1_000;
/// Longest accepted request path.
pub const MAX_PATH_BYTES: usize = 4_096;
/// Most paths recorded for one change (a head that changes more is cut off and logged).
pub const MAX_OPEN_PATHS: usize = 100_000;
/// Most `open_paths` rows one request reads.
pub const MAX_SCANNED_ROWS: usize = 20_000;
/// Most overlapping changes one answer lists.
pub const MAX_OVERLAPS: usize = 200;
/// Most changes in one stack the exclusion walk visits.
pub const MAX_STACK_WALK: usize = 512;
/// Keys per `IN (...)` lookup and rows per insert.
const BATCH: usize = 200;

/// The lookup key of `path`: case-folded component by component.
pub fn key_of(path: &RepoPath) -> [u8; 16] {
    let mut folded = Vec::with_capacity(path.as_bytes().len());
    for (i, comp) in path.components().enumerate() {
        if i > 0 {
            folded.push(b'/');
        }
        folded.extend_from_slice(&casefold(comp));
    }
    path_key(&folded)
}

/// The keys one changed path is recorded under: the path and each ancestor directory.
fn keys_for(path: &RepoPath) -> Vec<[u8; 16]> {
    let mut keys = vec![key_of(path)];
    let mut cur = path.clone();
    while let Some(parent) = cur.parent() {
        if parent.as_bytes().is_empty() {
            break;
        }
        keys.push(key_of(&parent));
        cur = parent;
    }
    keys
}

/// Replaces the rows of `change` by the keys of `paths` (the paths its head changes).
pub async fn record(
    db: &mut AnyConnection,
    ns: Ns,
    change: ChangeId,
    paths: &[RepoPath],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM open_paths WHERE ns = $1 AND change = $2")
        .bind(ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .execute(&mut *db)
        .await?;
    if paths.len() > MAX_OPEN_PATHS {
        tracing::warn!(
            paths = paths.len(),
            "open_paths: a head changes too many paths; cut off"
        );
    }
    let keys: BTreeSet<[u8; 16]> = paths
        .iter()
        .take(MAX_OPEN_PATHS)
        .flat_map(keys_for)
        .collect();
    let keys: Vec<[u8; 16]> = keys.into_iter().collect();
    for chunk in keys.chunks(BATCH) {
        let mut values = String::new();
        for i in 0..chunk.len() {
            if i > 0 {
                values.push_str(", ");
            }
            values.push_str(&format!("($1, ${}, $2)", i + 3));
        }
        let sql = format!("INSERT INTO open_paths (ns, path_key, change) VALUES {values}");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ns.as_bytes().to_vec())
            .bind(change.as_bytes().to_vec());
        for k in chunk {
            q = q.bind(k.to_vec());
        }
        q.execute(&mut *db).await?;
    }
    Ok(())
}

/// Forgets `change`: its open paths and the memoised mergeability of its changesets.
pub async fn close(db: &mut AnyConnection, ns: Ns, change: ChangeId) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM open_paths WHERE ns = $1 AND change = $2")
        .bind(ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .execute(&mut *db)
        .await?;
    sqlx::query(
        "DELETE FROM mergeable WHERE ns = $1 AND head IN \
         (SELECT cs FROM changes WHERE ns = $1 AND change = $2)",
    )
    .bind(ns.as_bytes().to_vec())
    .bind(change.as_bytes().to_vec())
    .execute(&mut *db)
    .await?;
    Ok(())
}

/// The connected stack of `change`: its ancestors by `parent_change`, then every descendant.
async fn stack_members(
    reader: &mut Reader<'_>,
    change: ChangeId,
) -> Result<HashSet<ChangeId>, Status> {
    let mut members = HashSet::from([change]);
    let mut root = change;
    while let Some(p) = reader.head(root).await?.and_then(|h| h.parent) {
        if !members.insert(p) {
            break;
        }
        if members.len() > MAX_STACK_WALK {
            return Err(Status::failed_precondition("the stack is too large"));
        }
        root = p;
    }
    // The ancestors are members already, yet their other children must still be walked.
    let mut walked = HashSet::from([root]);
    let mut queue = vec![root];
    while let Some(at) = queue.pop() {
        for child in reader.children(at).await? {
            if walked.insert(child) {
                members.insert(child);
                if members.len() > MAX_STACK_WALK {
                    return Err(Status::failed_precondition("the stack is too large"));
                }
                queue.push(child);
            }
        }
    }
    Ok(members)
}

/// Open changes that touch the same paths as `change` (or as `paths` when given).
pub(crate) async fn overlaps(
    reader: &mut Reader<'_>,
    req: &wire::OverlapsRequest,
) -> Result<Vec<wire::Overlap>, Status> {
    if req.paths.len() > MAX_QUERY_PATHS {
        return Err(Status::invalid_argument(format!(
            "at most {MAX_QUERY_PATHS} paths per call"
        )));
    }
    let change = <[u8; 16]>::try_from(req.change.as_ref())
        .map(ChangeId::from_bytes)
        .map_err(|_| Status::invalid_argument("a change id is 16 bytes"))?;
    let head = reader
        .head(change)
        .await?
        .ok_or_else(|| Status::not_found("unknown change"))?;
    let mut asked: BTreeSet<RepoPath> = BTreeSet::new();
    for p in &req.paths {
        if p.len() > MAX_PATH_BYTES {
            return Err(Status::invalid_argument("a path is too long"));
        }
        asked.insert(
            RepoPath::new(p.to_vec()).map_err(|e| Status::invalid_argument(e.to_string()))?,
        );
    }
    if asked.is_empty() {
        let cs = reader
            .engine
            .get_changeset(head.cs)
            .await
            .map_err(crate::commit_graph_rpc::commit_status)?;
        asked.extend(
            cs.changes
                .as_slice()
                .iter()
                .take(MAX_QUERY_PATHS)
                .map(|(p, _)| p.clone()),
        );
    }
    let mut by_key: BTreeMap<[u8; 16], Vec<RepoPath>> = BTreeMap::new();
    for p in asked {
        by_key.entry(key_of(&p)).or_default().push(p);
    }
    let own = stack_members(reader, change).await?;

    let keys: Vec<[u8; 16]> = by_key.keys().copied().collect();
    let mut hits: BTreeMap<ChangeId, BTreeSet<RepoPath>> = BTreeMap::new();
    let mut scanned = 0usize;
    for chunk in keys.chunks(BATCH) {
        let sql = format!(
            "SELECT path_key, change FROM open_paths WHERE ns = $1 AND path_key IN ({}) LIMIT ${}",
            placeholders(2, chunk.len()),
            chunk.len() + 2
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(reader.ns.as_bytes().to_vec());
        for k in chunk {
            q = q.bind(k.to_vec());
        }
        let left = MAX_SCANNED_ROWS - scanned;
        q = q.bind(i64::try_from(left + 1).unwrap_or(i64::MAX));
        let rows = q
            .fetch_all(&mut *reader.conn)
            .await
            .map_err(|e| internal("open_paths", &e))?;
        scanned += rows.len();
        if scanned > MAX_SCANNED_ROWS {
            return Err(Status::resource_exhausted(
                "too many open changes touch these paths; name fewer paths",
            ));
        }
        for r in &rows {
            let key: [u8; 16] =
                fixed(r, "path_key").ok_or_else(|| Status::data_loss("damaged open_paths row"))?;
            let other = fixed::<16>(r, "change")
                .map(ChangeId::from_bytes)
                .ok_or_else(|| Status::data_loss("damaged open_paths row"))?;
            if own.contains(&other) {
                continue;
            }
            if let Some(paths) = by_key.get(&key) {
                hits.entry(other).or_default().extend(paths.iter().cloned());
            }
        }
    }
    let mut out = Vec::new();
    for (other, paths) in hits.into_iter().take(MAX_OVERLAPS) {
        out.push(wire::Overlap {
            change: Bytes::copy_from_slice(other.as_bytes()),
            review: reader.review(other).await?,
            paths: paths
                .iter()
                .map(|p| Bytes::copy_from_slice(p.as_bytes()))
                .collect(),
        });
    }
    Ok(out)
}

/// What a forge's review state says about a change's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Open,
    Closed,
    Unknown,
}

/// `open` (drafts included) keeps a change in the index; `closed` and `merged` remove it. Any
/// other word changes nothing: the forge's next event says more.
pub fn lifecycle(state: &str) -> Lifecycle {
    if state.eq_ignore_ascii_case("open") {
        Lifecycle::Open
    } else if state.eq_ignore_ascii_case("closed") || state.eq_ignore_ascii_case("merged") {
        Lifecycle::Closed
    } else {
        Lifecycle::Unknown
    }
}

/// What a review update does to `open_paths`.
pub enum Plan {
    Record(Vec<RepoPath>),
    Close,
    Keep,
}

/// The paths the newest version of `change` changes; `None` when it has no changeset.
pub async fn head_paths(
    db: &mut AnyConnection,
    engine: &crate::idmode::Eng,
    ns: Ns,
    change: ChangeId,
) -> Result<Option<Vec<RepoPath>>, Status> {
    let cs = match super::versions::latest(&mut *db, ns, change)
        .await
        .map_err(|e| internal("versions", &e))?
    {
        Some(v) => v.cs,
        None => {
            let mut all =
                super::index::changesets_of(&mut *db, ns, change, super::index::MAX_PREDS)
                    .await
                    .map_err(|e| internal("change index", &e))?;
            all.sort();
            match all.first() {
                Some(&c) => c,
                None => return Ok(None),
            }
        }
    };
    let cs = engine
        .get_changeset(cs)
        .await
        .map_err(crate::commit_graph_rpc::commit_status)?;
    Ok(Some(paths_of(&cs)))
}

/// The paths a head changes, for [`record`]: at most `MAX_OPEN_PATHS + 1`.
pub fn paths_of(cs: &asc_core::object::Changeset) -> Vec<RepoPath> {
    cs.changes
        .as_slice()
        .iter()
        .take(MAX_OPEN_PATHS + 1)
        .map(|(p, _)| p.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::testkit::{Fx, cid, fx_with};

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    async fn open(f: &Fx, change: u8, paths: &[&str]) {
        let mut conn = f.reg.pool().acquire().await.unwrap();
        let paths: Vec<RepoPath> = paths.iter().map(|s| p(s)).collect();
        record(&mut conn, f.e.ns, cid(change), &paths)
            .await
            .unwrap();
    }

    fn ask(change: u8, paths: &[&str]) -> wire::OverlapsRequest {
        wire::OverlapsRequest {
            repo: None,
            target: String::new(),
            change: Bytes::copy_from_slice(cid(change).as_bytes()),
            paths: paths
                .iter()
                .map(|s| Bytes::copy_from_slice(s.as_bytes()))
                .collect(),
        }
    }

    async fn run(f: &Fx, req: wire::OverlapsRequest) -> Result<Vec<u8>, Status> {
        let mut conn = f.reg.pool().acquire().await.unwrap();
        let mut r = f.reader(&mut conn);
        let got = overlaps(&mut r, &req).await?;
        Ok(got.iter().map(|o| o.change[0]).collect())
    }

    #[tokio::test]
    async fn overlaps_omit_the_requested_changes_own_stack() {
        // Changes 1 <- 2 <- 3 and a fork 2 <- 4; 5 is unrelated (a root of its own).
        let f = fx_with(5).await;
        for c in 1..=5 {
            open(&f, c, &["shared/file.rs", &format!("only{c}")]).await;
        }
        // An empty request uses the head's own paths (change 2's commit writes "b"); the
        // unrelated change also touches that file.
        open(&f, 5, &["shared/file.rs", "only5", "b"]).await;
        // From the middle of the stack: parents, children and forks are all hidden.
        assert_eq!(run(&f, ask(2, &[])).await.unwrap(), vec![5]);
        assert_eq!(run(&f, ask(3, &["shared/file.rs"])).await.unwrap(), vec![5]);
        assert_eq!(run(&f, ask(1, &["shared/file.rs"])).await.unwrap(), vec![5]);
        // The unrelated change sees the whole other stack and never itself.
        assert_eq!(
            run(&f, ask(5, &["shared/file.rs"])).await.unwrap(),
            vec![1, 2, 3, 4]
        );
    }

    #[tokio::test]
    async fn overlaps_match_directories_and_case() {
        let f = fx_with(5).await;
        open(&f, 5, &["src/Deep/Mod.rs"]).await;
        // The file itself, any case, and the directories above it.
        for q in ["src/Deep/Mod.rs", "SRC/deep/mod.rs", "src/deep", "src"] {
            assert_eq!(run(&f, ask(1, &[q])).await.unwrap(), vec![5], "{q}");
        }
        // Siblings and a name that only shares a prefix do not match.
        for q in ["src/Deep/Other.rs", "src/Dee", "other"] {
            assert!(run(&f, ask(1, &[q])).await.unwrap().is_empty(), "{q}");
        }
    }

    #[tokio::test]
    async fn overlaps_default_to_the_heads_own_paths_and_report_the_matching_ones() {
        let f = fx_with(5).await;
        // Change 1's head (commit "a") changes the file "a".
        open(&f, 5, &["a", "b"]).await;
        let mut conn = f.reg.pool().acquire().await.unwrap();
        let mut r = f.reader(&mut conn);
        let got = overlaps(&mut r, &ask(1, &[])).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].paths, vec![Bytes::from_static(b"a")]);
    }

    #[tokio::test]
    async fn closing_a_change_drops_its_paths_and_requests_are_bounded() {
        let f = fx_with(5).await;
        open(&f, 5, &["x"]).await;
        assert_eq!(run(&f, ask(1, &["x"])).await.unwrap(), vec![5]);
        let mut conn = f.reg.pool().acquire().await.unwrap();
        close(&mut conn, f.e.ns, cid(5)).await.unwrap();
        drop(conn);
        assert!(run(&f, ask(1, &["x"])).await.unwrap().is_empty());

        let many: Vec<String> = (0..=MAX_QUERY_PATHS).map(|i| format!("p{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let e = run(&f, ask(1, &refs)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let e = run(&f, ask(1, &["../x"])).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let mut bad = ask(1, &["x"]);
        bad.change = Bytes::from_static(b"short");
        assert_eq!(
            run(&f, bad).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let mut unknown = ask(1, &["x"]);
        unknown.change = Bytes::copy_from_slice(&[0xee; 16]);
        assert_eq!(
            run(&f, unknown).await.unwrap_err().code(),
            tonic::Code::NotFound
        );
    }

    #[tokio::test]
    async fn recording_again_replaces_the_rows() {
        let f = fx_with(5).await;
        open(&f, 5, &["old"]).await;
        open(&f, 5, &["new"]).await;
        assert!(run(&f, ask(1, &["old"])).await.unwrap().is_empty());
        assert_eq!(run(&f, ask(1, &["new"])).await.unwrap(), vec![5]);
    }
}
