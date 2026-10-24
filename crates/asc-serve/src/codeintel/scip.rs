//! The SCIP upload tier (SPEC §23.6 CI-D3, PLAN I31): compiler-grade definitions and
//! references that a client built with a language indexer and uploads, stored per `FileId`.
//!
//! The upload is untrusted input. [`parse`] bounds its size and counts, rejects a path that is
//! not a valid repository path, and keeps a document only when its path exists in the revision
//! the upload names ([`parse`] takes that revision's path-to-`FileId` map from the caller, which
//! has already authorised the write). A document that carries its source text must carry the
//! bytes of the revision's file: a mismatch means the index is stale and the document is dropped,
//! so precise answers are never attached to a file whose content differs. Rows are keyed by
//! `FileId`, so they follow the content to any revision or path that holds it.
//!
//! Hits carry `confidence = "precise"` ([`PRECISE`]); the search-based tags keep `import_match`
//! and `name_match`. The `Symbols` handler merges [`Store::defs`] and [`Store::refs`] ahead of
//! the tag results (TODO(I16): one call site in `rpc.rs`, which the upload tier does not own).

use std::collections::{BTreeMap, HashSet};

use asc_core::id::FileId;
use asc_core::path::RepoPath;
use prost::Message;
use sqlx::{AnyPool, Row};

/// The `confidence` of a hit that comes from an uploaded SCIP index.
pub const PRECISE: &str = "precise";
/// Largest upload accepted, in bytes.
pub const MAX_INDEX_BYTES: usize = 64 << 20;
/// Documents one upload may carry.
pub const MAX_DOCS: usize = 200_000;
/// Occurrences kept from one document.
pub const MAX_DOC_OCCS: usize = 500_000;
/// Occurrences kept from one upload.
pub const MAX_TOTAL_OCCS: usize = 5_000_000;
/// Longest SCIP symbol string kept, in bytes.
pub const MAX_SYMBOL_BYTES: usize = 1024;
/// Longest relative path accepted, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// Hits one lookup returns.
pub const MAX_HITS: usize = 1000;
/// Documents per transaction of an ingest.
const DOCS_PER_TX: usize = 200;
macro_rules! select_hits {
    ($cond:literal) => {
        concat!(
            "SELECT file, name, symbol, CAST(is_def AS BIGINT) AS is_def, CAST(line AS BIGINT) AS line,              CAST(col AS BIGINT) AS col, CAST(end_line AS BIGINT) AS end_line, CAST(end_col AS BIGINT) AS end_col              FROM ci_scip WHERE ns = $1 AND ",
            $cond,
            " ORDER BY file, line, col"
        )
    };
}
/// `symbol_roles` bit of a definition.
const ROLE_DEFINITION: i32 = 1;

/// Minimal SCIP wire messages (`scip.proto`; the fields this tier reads).
mod wire {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Index {
        #[prost(message, repeated, tag = "2")]
        pub documents: Vec<Document>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Document {
        #[prost(string, tag = "1")]
        pub relative_path: String,
        #[prost(message, repeated, tag = "2")]
        pub occurrences: Vec<Occurrence>,
        #[prost(string, tag = "5")]
        pub text: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Occurrence {
        #[prost(int32, repeated, tag = "1")]
        pub range: Vec<i32>,
        #[prost(string, tag = "2")]
        pub symbol: String,
        #[prost(int32, tag = "3")]
        pub symbol_roles: i32,
    }
}

pub use wire::{Document as WireDocument, Index as WireIndex, Occurrence as WireOccurrence};

#[derive(Debug, thiserror::Error)]
pub enum ScipError {
    #[error("scip index is {0} bytes, over the {MAX_INDEX_BYTES} byte limit")]
    TooLarge(usize),
    #[error("scip index is not valid protobuf: {0}")]
    Decode(String),
    #[error("scip index has {0} documents, over the {MAX_DOCS} limit")]
    TooManyDocs(usize),
    #[error("scip document path is not a valid repository path: {0:?}")]
    BadPath(String),
    #[error("scip index has more than {MAX_TOTAL_OCCS} occurrences")]
    TooManyOccs,
    #[error("scip store: {0}")]
    Store(String),
}

/// One kept occurrence: a zero-based range and the symbol it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occ {
    pub symbol: String,
    pub name: String,
    pub is_def: bool,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// One document kept for storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    pub path: RepoPath,
    pub file: FileId,
    pub occs: Vec<Occ>,
}

/// What [`parse`] kept and what it dropped, so the uploader can see a partial import.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub docs: Vec<Doc>,
    /// Documents whose path is not in the revision.
    pub unknown_path: usize,
    /// Documents whose embedded text differs from the revision's file.
    pub stale: usize,
    /// Occurrences with a malformed range, an empty or oversize symbol, or a local symbol.
    pub skipped_occs: usize,
}

impl Parsed {
    pub fn occurrences(&self) -> usize {
        self.docs.iter().map(|d| d.occs.len()).sum()
    }
}

/// The last descriptor name of a SCIP symbol string (`scheme manager package version
/// descriptors`), the key `Symbols` looks names up by. `None` for a local or malformed symbol.
pub fn symbol_name(symbol: &str) -> Option<String> {
    if symbol.is_empty() || symbol.starts_with("local ") {
        return None;
    }
    // Four space-separated header fields; a space inside one is written as two spaces.
    let mut rest = symbol;
    for _ in 0..4 {
        let b = rest.as_bytes();
        let mut i = 0;
        loop {
            match b.get(i) {
                None => return None,
                Some(b' ') if b.get(i + 1) == Some(&b' ') => i += 2,
                Some(b' ') => break,
                Some(_) => i += 1,
            }
        }
        rest = &rest[i + 1..];
    }
    let b = rest.as_bytes();
    let mut end = b.len();
    // Strip the descriptor suffix: `/ # . : !` or a method `(disambiguator).`.
    if end > 0 && b[end - 1] == b'.' {
        end -= 1;
    }
    if end > 0 && b[end - 1] == b')' {
        while end > 0 && b[end - 1] != b'(' {
            end -= 1;
        }
        end = end.saturating_sub(1);
    } else if end > 0 && matches!(b[end - 1], b'/' | b'#' | b':' | b'!') {
        end -= 1;
    } else if end > 0 && b[end - 1] == b']' {
        return None;
    }
    if end == 0 {
        return None;
    }
    let name = if b[end - 1] == b'`' {
        // Backtick-escaped name; a doubled backtick is a literal one.
        let mut out = Vec::new();
        let mut s = end - 1;
        while s > 0 {
            s -= 1;
            if b[s] == b'`' {
                if s > 0 && b[s - 1] == b'`' {
                    out.push(b'`');
                    s -= 1;
                    continue;
                }
                break;
            }
            out.push(b[s]);
        }
        out.reverse();
        String::from_utf8(out).ok()?
    } else {
        let mut s = end;
        while s > 0 && !matches!(b[s - 1], b'/' | b'#' | b'.' | b':' | b'!') {
            s -= 1;
        }
        rest[s..end].to_owned()
    };
    (!name.is_empty()).then_some(name)
}

/// Validates and filters an uploaded index against one revision.
///
/// `root` is the repository directory the indexer ran in (the SCIP `project_root`), or `None`
/// for the repository root. `resolve` maps a repository path to the `FileId` it has at the
/// revision; a path it does not know is dropped.
pub fn parse(
    bytes: &[u8],
    root: Option<&RepoPath>,
    resolve: &dyn Fn(&RepoPath) -> Option<FileId>,
) -> Result<Parsed, ScipError> {
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(ScipError::TooLarge(bytes.len()));
    }
    let index = wire::Index::decode(bytes).map_err(|e| ScipError::Decode(e.to_string()))?;
    if index.documents.len() > MAX_DOCS {
        return Err(ScipError::TooManyDocs(index.documents.len()));
    }
    let mut out = Parsed::default();
    let mut seen: HashSet<RepoPath> = HashSet::new();
    let mut total = 0usize;
    for d in index.documents {
        if d.relative_path.len() > MAX_PATH_BYTES {
            return Err(ScipError::BadPath(
                d.relative_path.chars().take(64).collect(),
            ));
        }
        let rel = RepoPath::new(d.relative_path.as_bytes())
            .map_err(|_| ScipError::BadPath(d.relative_path.clone()))?;
        let path = match root {
            Some(r) => {
                let mut full = r.as_bytes().to_vec();
                full.push(b'/');
                full.extend_from_slice(rel.as_bytes());
                RepoPath::new(full).map_err(|_| ScipError::BadPath(d.relative_path.clone()))?
            }
            None => rel,
        };
        let Some(file) = resolve(&path) else {
            out.unknown_path += 1;
            continue;
        };
        if !d.text.is_empty() && FileId::of(d.text.as_bytes()) != file {
            out.stale += 1;
            continue;
        }
        if !seen.insert(path.clone()) {
            // A path listed twice is ambiguous; keep the first.
            continue;
        }
        let mut occs = Vec::new();
        for o in d.occurrences {
            if occs.len() >= MAX_DOC_OCCS {
                out.skipped_occs += 1;
                continue;
            }
            match occ(o) {
                Some(o) => occs.push(o),
                None => out.skipped_occs += 1,
            }
        }
        total += occs.len();
        if total > MAX_TOTAL_OCCS {
            return Err(ScipError::TooManyOccs);
        }
        out.docs.push(Doc { path, file, occs });
    }
    Ok(out)
}

fn occ(o: wire::Occurrence) -> Option<Occ> {
    if o.symbol.len() > MAX_SYMBOL_BYTES {
        return None;
    }
    let name = symbol_name(&o.symbol)?;
    let r: Vec<u32> = o
        .range
        .iter()
        .map(|&v| u32::try_from(v).ok())
        .collect::<Option<_>>()?;
    let (line, col, end_line, end_col) = match r[..] {
        [l, c, e] => (l, c, l, e),
        [l, c, el, ec] => (l, c, el, ec),
        _ => return None,
    };
    if (end_line, end_col) < (line, col) {
        return None;
    }
    Some(Occ {
        symbol: o.symbol,
        name,
        is_def: o.symbol_roles & ROLE_DEFINITION != 0,
        line,
        col,
        end_line,
        end_col,
    })
}

/// A stored hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub file: FileId,
    pub name: String,
    pub symbol: String,
    pub is_def: bool,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// What an ingest stored.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Ingested {
    pub docs: usize,
    pub occurrences: usize,
}

/// The `ci_scip` rows of one namespace.
pub struct Store {
    pool: AnyPool,
    ns: Vec<u8>,
}

fn store_err(e: sqlx::Error) -> ScipError {
    ScipError::Store(e.to_string())
}

impl Store {
    pub fn new(pool: AnyPool, ns: &[u8]) -> Self {
        Self {
            pool,
            ns: ns.to_vec(),
        }
    }

    /// Replaces the rows of every document in `parsed`; a file the upload does not mention keeps
    /// its rows. A re-upload is idempotent.
    pub async fn ingest(&self, parsed: &Parsed) -> Result<Ingested, ScipError> {
        let mut done = Ingested::default();
        for chunk in parsed.docs.chunks(DOCS_PER_TX) {
            let mut tx = self.pool.begin().await.map_err(store_err)?;
            for d in chunk {
                sqlx::query("DELETE FROM ci_scip WHERE ns = $1 AND file = $2")
                    .bind(self.ns.clone())
                    .bind(d.file.as_bytes().to_vec())
                    .execute(&mut *tx)
                    .await
                    .map_err(store_err)?;
                for o in &d.occs {
                    sqlx::query(
                        "INSERT INTO ci_scip (ns, file, name, symbol, is_def, line, col, end_line, end_col) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                    )
                    .bind(self.ns.clone())
                    .bind(d.file.as_bytes().to_vec())
                    .bind(o.name.clone())
                    .bind(o.symbol.clone())
                    .bind(i32::from(o.is_def))
                    .bind(i64::from(o.line))
                    .bind(i64::from(o.col))
                    .bind(i64::from(o.end_line))
                    .bind(i64::from(o.end_col))
                    .execute(&mut *tx)
                    .await
                    .map_err(store_err)?;
                }
                done.docs += 1;
                done.occurrences += d.occs.len();
            }
            tx.commit().await.map_err(store_err)?;
        }
        Ok(done)
    }

    /// Definitions named `name`, restricted to `scope` (the files of the query's scope).
    pub async fn defs(&self, name: &str, scope: &HashSet<FileId>) -> Result<Vec<Hit>, ScipError> {
        self.by(select_hits!("name = $2 AND is_def = 1"), name, scope).await
    }

    /// References to every symbol named `name`.
    pub async fn refs(&self, name: &str, scope: &HashSet<FileId>) -> Result<Vec<Hit>, ScipError> {
        self.by(select_hits!("name = $2 AND is_def = 0"), name, scope).await
    }

    /// References to exactly `symbol` (the precise join once a definition is chosen).
    pub async fn refs_of_symbol(
        &self,
        symbol: &str,
        scope: &HashSet<FileId>,
    ) -> Result<Vec<Hit>, ScipError> {
        self.by(select_hits!("symbol = $2 AND is_def = 0"), symbol, scope).await
    }

    async fn by(
        &self,
        sql: &'static str,
        key: &str,
        scope: &HashSet<FileId>,
    ) -> Result<Vec<Hit>, ScipError> {
        let rows = sqlx::query(sql)
        .bind(self.ns.clone())
        .bind(key.to_owned())
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        let num = |r: &sqlx::any::AnyRow, c: &str| -> Result<u32, ScipError> {
            let v: i64 = r.try_get(c).map_err(store_err)?;
            u32::try_from(v).map_err(|_| ScipError::Store(format!("bad {c}")))
        };
        let mut out = Vec::new();
        for r in rows {
            let f: Vec<u8> = r.try_get("file").map_err(store_err)?;
            let Ok(arr) = <[u8; 32]>::try_from(f.as_slice()) else {
                continue;
            };
            let file = FileId::from_bytes(arr);
            if !scope.contains(&file) {
                continue;
            }
            out.push(Hit {
                file,
                name: r.try_get("name").map_err(store_err)?,
                symbol: r.try_get("symbol").map_err(store_err)?,
                is_def: num(&r, "is_def")? != 0,
                line: num(&r, "line")?,
                col: num(&r, "col")?,
                end_line: num(&r, "end_line")?,
                end_col: num(&r, "end_col")?,
            });
            if out.len() >= MAX_HITS {
                break;
            }
        }
        Ok(out)
    }

    /// Files that hold precise data, for coverage.
    pub async fn files(&self) -> Result<usize, ScipError> {
        let row = sqlx::query(
            "SELECT CAST(COUNT(DISTINCT file) AS BIGINT) AS n FROM ci_scip WHERE ns = $1",
        )
        .bind(self.ns.clone())
        .fetch_one(&self.pool)
        .await
        .map_err(store_err)?;
        let n: i64 = row.try_get("n").map_err(store_err)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

/// The upload handler's body: parse against a revision map, then store.
/// TODO(I16): the `UploadScip` RPC resolves `revision` from the request scope after the write
/// authorisation check and calls this.
pub async fn upload(
    store: &Store,
    bytes: &[u8],
    root: Option<&RepoPath>,
    revision: &BTreeMap<RepoPath, FileId>,
) -> Result<(Parsed, Ingested), ScipError> {
    let parsed = parse(bytes, root, &|p| revision.get(p).copied())?;
    let done = store.ingest(&parsed).await?;
    Ok((parsed, done))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate;

    const SYM: &str = "scip-rust cargo demo 0.1.0 util/helper().";

    fn occ_w(range: Vec<i32>, symbol: &str, roles: i32) -> WireOccurrence {
        WireOccurrence {
            range,
            symbol: symbol.into(),
            symbol_roles: roles,
        }
    }

    fn doc(path: &str, text: &str, occs: Vec<WireOccurrence>) -> WireDocument {
        WireDocument {
            relative_path: path.into(),
            occurrences: occs,
            text: text.into(),
        }
    }

    fn enc(docs: Vec<WireDocument>) -> Vec<u8> {
        WireIndex { documents: docs }.encode_to_vec()
    }

    fn rev(entries: &[(&str, &[u8])]) -> BTreeMap<RepoPath, FileId> {
        entries
            .iter()
            .map(|(p, b)| (RepoPath::new(*p).unwrap(), FileId::of(b)))
            .collect()
    }

    async fn store() -> Store {
        let p = migrate::connect("sqlite::memory:", 1).await.unwrap();
        migrate::migrate(&p).await.unwrap();
        Store::new(p, b"ns")
    }

    #[test]
    fn names_come_from_the_last_descriptor() {
        assert_eq!(symbol_name(SYM).as_deref(), Some("helper"));
        assert_eq!(
            symbol_name("scip-java maven g 1 com/acme/Foo#bar(+1).").as_deref(),
            Some("bar")
        );
        assert_eq!(
            symbol_name("scip-go gomod m 1 pkg/`odd name`#").as_deref(),
            Some("odd name")
        );
        assert_eq!(
            symbol_name("scip-rust cargo demo 0.1.0 util/Thing#").as_deref(),
            Some("Thing")
        );
        assert_eq!(symbol_name("local 7"), None);
        assert_eq!(symbol_name(""), None);
        assert_eq!(symbol_name("garbage"), None);
    }

    #[test]
    fn unknown_paths_are_dropped_and_known_kept() {
        let r = rev(&[("a.rs", b"fn helper() {}")]);
        let bytes = enc(vec![
            doc("a.rs", "", vec![occ_w(vec![0, 3, 9], SYM, 1)]),
            doc("b.rs", "", vec![occ_w(vec![0, 3, 9], SYM, 0)]),
        ]);
        let p = parse(&bytes, None, &|x| r.get(x).copied()).unwrap();
        assert_eq!(p.docs.len(), 1);
        assert_eq!(p.unknown_path, 1);
        assert!(p.docs[0].occs[0].is_def);
        assert_eq!(p.docs[0].file, FileId::of(b"fn helper() {}"));
    }

    #[test]
    fn traversal_and_dot_git_paths_reject_the_upload() {
        let r = rev(&[("a.rs", b"x")]);
        for bad in [
            "../a.rs",
            "/etc/passwd",
            "a//b.rs",
            ".git/config",
            "x/./a.rs",
            "",
        ] {
            let bytes = enc(vec![doc(bad, "", vec![])]);
            assert!(
                matches!(
                    parse(&bytes, None, &|x| r.get(x).copied()),
                    Err(ScipError::BadPath(_))
                ),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn text_that_differs_from_the_revision_is_stale() {
        let r = rev(&[("a.rs", b"fn helper() {}")]);
        let stale = enc(vec![doc(
            "a.rs",
            "fn other() {}",
            vec![occ_w(vec![0, 3, 9], SYM, 1)],
        )]);
        let p = parse(&stale, None, &|x| r.get(x).copied()).unwrap();
        assert!(p.docs.is_empty());
        assert_eq!(p.stale, 1);
        let fresh = enc(vec![doc(
            "a.rs",
            "fn helper() {}",
            vec![occ_w(vec![0, 3, 9], SYM, 1)],
        )]);
        let p = parse(&fresh, None, &|x| r.get(x).copied()).unwrap();
        assert_eq!(p.docs.len(), 1);
    }

    #[test]
    fn oversize_and_garbage_uploads_are_refused() {
        let big = vec![0u8; MAX_INDEX_BYTES + 1];
        assert!(matches!(
            parse(&big, None, &|_| None),
            Err(ScipError::TooLarge(_))
        ));
        assert!(matches!(
            parse(&[0xff, 0xff, 0xff], None, &|_| None),
            Err(ScipError::Decode(_))
        ));
    }

    #[test]
    fn malformed_ranges_and_local_symbols_are_skipped() {
        let r = rev(&[("a.rs", b"x")]);
        let bytes = enc(vec![doc(
            "a.rs",
            "",
            vec![
                occ_w(vec![-1, 0, 3], SYM, 0),
                occ_w(vec![0, 5, 2], SYM, 0),
                occ_w(vec![2, 0, 1, 0], SYM, 0),
                occ_w(vec![0, 0], SYM, 0),
                occ_w(vec![0, 0, 3], "local 1", 0),
                occ_w(vec![0, 0, 3], SYM, 0),
                occ_w(vec![1, 2, 3, 4], SYM, 1),
            ],
        )]);
        let p = parse(&bytes, None, &|x| r.get(x).copied()).unwrap();
        assert_eq!(p.docs[0].occs.len(), 2);
        assert_eq!(p.skipped_occs, 5);
    }

    #[test]
    fn root_prefix_is_joined_to_document_paths() {
        let r = rev(&[("svc/a.rs", b"x")]);
        let bytes = enc(vec![doc("a.rs", "", vec![occ_w(vec![0, 0, 3], SYM, 1)])]);
        let root = RepoPath::new("svc").unwrap();
        let p = parse(&bytes, Some(&root), &|x| r.get(x).copied()).unwrap();
        assert_eq!(p.docs.len(), 1);
        assert_eq!(p.docs[0].path, RepoPath::new("svc/a.rs").unwrap());
    }

    #[tokio::test]
    async fn precise_defs_and_refs_are_keyed_by_file_and_scoped() {
        let s = store().await;
        let r = rev(&[("a.rs", b"def"), ("b.rs", b"use")]);
        let bytes = enc(vec![
            doc("a.rs", "", vec![occ_w(vec![0, 3, 9], SYM, 1)]),
            doc("b.rs", "", vec![occ_w(vec![4, 0, 6], SYM, 0)]),
        ]);
        let (_, done) = upload(&s, &bytes, None, &r).await.unwrap();
        assert_eq!(
            done,
            Ingested {
                docs: 2,
                occurrences: 2
            }
        );
        let all: HashSet<FileId> = r.values().copied().collect();
        let defs = s.defs("helper", &all).await.unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].file, FileId::of(b"def"));
        assert_eq!((defs[0].line, defs[0].col, defs[0].end_col), (0, 3, 9));
        let refs = s.refs_of_symbol(SYM, &all).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].file, FileId::of(b"use"));
        // A scope without the file hides its rows.
        let only_a: HashSet<FileId> = [FileId::of(b"def")].into();
        assert!(s.refs("helper", &only_a).await.unwrap().is_empty());
        assert_eq!(s.files().await.unwrap(), 2);
        // Re-upload replaces, never duplicates.
        upload(&s, &bytes, None, &r).await.unwrap();
        assert_eq!(s.defs("helper", &all).await.unwrap().len(), 1);
        // Another namespace sees nothing.
        let other = Store::new(s.pool.clone(), b"other");
        assert!(other.defs("helper", &all).await.unwrap().is_empty());
    }
}
