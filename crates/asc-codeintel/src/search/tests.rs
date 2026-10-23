use std::collections::{BTreeSet, HashMap};
use std::io::Cursor as IoCursor;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use asc_core::content::{Body, ingest};
use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use roaring::RoaringBitmap;

use super::*;
use crate::doc::DocError;
use crate::grams::distinct_trigrams;
use crate::index::manifest::{ManifestStore, MemManifest, Publisher};
use crate::index::schema::IndexKind;
use crate::index::segments::{FileDoc, Replica, SegmentWriter, WRITER_HEAP};

struct NoRefs;

#[async_trait]
impl Refs for NoRefs {
    async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
        Ok(None)
    }
    async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
        Ok(Vec::new())
    }
    async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
        Err(RefError::Corrupt("unused".into()))
    }
    async fn log(
        &self,
        _: RepoKey,
        _: &RefName,
        _: u32,
    ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
        Ok(Vec::new())
    }
}

/// Ord table with the reverse map and the interactive queue.
#[derive(Default)]
struct Docs {
    m: Mutex<(HashMap<FileId, u32>, Vec<FileId>)>,
    queued: Mutex<RoaringBitmap>,
}

impl OrdSource for Docs {
    fn ord_for(&self, id: FileId) -> Result<u32, DocError> {
        let mut g = self.m.lock().unwrap();
        if let Some(&o) = g.0.get(&id) {
            return Ok(o);
        }
        let o = g.1.len() as u32;
        g.0.insert(id, o);
        g.1.push(id);
        Ok(o)
    }
    fn get(&self, id: &FileId) -> Option<u32> {
        self.m.lock().unwrap().0.get(id).copied()
    }
}

impl DocDirectory for Docs {
    fn file_of(&self, ord: u32) -> Option<FileId> {
        self.m.lock().unwrap().1.get(ord as usize).copied()
    }
    fn enqueue_interactive(&self, ords: &RoaringBitmap) {
        *self.queued.lock().unwrap() |= ords;
    }
}

type Eng = Engine<BucketObjects, NoRefs>;

struct Env {
    e: Arc<Eng>,
    objs: Arc<BucketObjects>,
    docs: Arc<Docs>,
    searcher: Searcher<BucketObjects, NoRefs, Docs, Docs>,
    dir: tempfile::TempDir,
    files: Vec<(String, String)>,
}

impl Env {
    fn new() -> Env {
        let objs = Arc::new(BucketObjects::memory());
        let e = Arc::new(Engine::new(objs.clone(), Arc::new(NoRefs), RepoKey(1)));
        let docs = Arc::new(Docs::default());
        let searcher = Searcher::new(e.clone(), objs.clone(), docs.clone(), docs.clone());
        Env {
            e,
            objs,
            docs,
            searcher,
            dir: tempfile::tempdir().unwrap(),
            files: Vec::new(),
        }
    }

    async fn commit(&mut self, files: &[(&str, &str)]) -> ChangesetId {
        let mut changes = Vec::new();
        for (p, c) in files {
            let got = ingest(IoCursor::new(c.as_bytes())).unwrap();
            let Body::Inline(b) = got.body else {
                unreachable!("test files are small")
            };
            self.objs
                .put(&ObjKey::File(got.file), encode_stored(&b))
                .await
                .unwrap();
            let ch = Change::write(Kind::File, got.file, got.size, got.git, None).unwrap();
            changes.push((RepoPath::new(*p).unwrap(), ch));
            self.files.push((p.to_string(), c.to_string()));
        }
        let root = self.e.apply(Eng::empty_tree_id(), &changes).await.unwrap();
        let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap();
        self.e
            .create_changeset(
                NewChangeset {
                    parents: Vec::new(),
                    author: sig.clone(),
                    committer: sig,
                    message: b"m\n".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    /// Indexes the files whose path satisfies `pred` and returns the pinned snapshot.
    async fn index(&self, pred: impl Fn(&str) -> bool) -> Arc<Snapshot> {
        let m = MemManifest::new();
        let p = Publisher {
            objects: &*self.objs,
            manifests: &m,
            ns: "ns1",
            kind: IndexKind::Files,
            holder: "t",
        };
        assert!(p.lease(1_000_000, 60_000).await.unwrap());
        let mut w = SegmentWriter::new(IndexKind::Files, self.dir.path(), WRITER_HEAP).unwrap();
        let mut seen = BTreeSet::new();
        for (path, content) in &self.files {
            if !pred(path) {
                continue;
            }
            let ord = self.docs.ord_for(FileId::of(content.as_bytes())).unwrap();
            if !seen.insert(ord) {
                continue;
            }
            let grams = distinct_trigrams(content.as_bytes(), 1 << 20).unwrap();
            let langs: Vec<Lang> = lang::detect(path, content.as_bytes()).into_iter().collect();
            w.add_file(&FileDoc {
                ord,
                grams: &grams,
                sym_def: &[],
                sym_ref: &[],
                ident: content,
                doc: "",
                path,
                langs: &langs,
            })
            .unwrap();
        }
        let manifest = match w.finish().unwrap() {
            Some(pack) => p.commit(&[pack], 1_000_000).await.unwrap(),
            None => m.load("ns1", IndexKind::Files).await.unwrap(),
        };
        let rep = Replica::new(self.dir.path().join("ci"));
        Arc::new(rep.snapshot(&*self.objs, "ns1", &manifest).await.unwrap())
    }

    /// Brute force over every file: the paths with a matching line.
    fn oracle(&self, re: &str) -> BTreeSet<String> {
        let re = regex::bytes::Regex::new(re).unwrap();
        self.files
            .iter()
            .filter(|(_, c)| {
                c.as_bytes()
                    .strip_suffix(b"\n")
                    .unwrap_or(c.as_bytes())
                    .split(|&b| b == b'\n')
                    .any(|l| re.is_match(l))
            })
            .map(|(p, _)| p.clone())
            .collect()
    }
}

fn paths(r: &SearchResult) -> BTreeSet<String> {
    r.hits
        .iter()
        .flat_map(|h| h.paths.iter())
        .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
        .collect()
}

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

const FIXTURE: [(&str, &str); 7] = [
    ("src/lib.rs", "pub fn parse_widget() {}\nfn helper() {}\n"),
    ("src/main.rs", "fn main() { parse_widget(); }\n"),
    ("src/deep/er/widget.rs", "// Widget notes\nstruct Widget;\n"),
    ("tests/widget_test.rs", "fn t() { parse_widget(); }\n"),
    ("README.md", "The widget parser.\nNo code here\n"),
    ("copy/a.txt", "same content widget\n"),
    ("copy/b.txt", "same content widget\n"),
];

async fn fixture() -> (Env, ChangesetId) {
    let mut env = Env::new();
    let cs = env.commit(&FIXTURE).await;
    (env, cs)
}

async fn run(env: &Env, snap: &Arc<Snapshot>, req: &SearchRequest<'_>) -> SearchResult {
    env.searcher.search(snap, req).await.unwrap()
}

#[tokio::test]
async fn results_equal_the_brute_force_oracle() {
    let (env, cs) = fixture().await;
    let snap = env.index(|_| true).await;
    let cases: [(&str, Mode, bool, &str); 7] = [
        ("parse_widget", Mode::Literal, true, "parse_widget"),
        ("WIDGET", Mode::Literal, false, "(?i)WIDGET"),
        ("widget", Mode::Literal, true, "widget"),
        ("pa.se_[a-z]+", Mode::Regex, true, "pa.se_[a-z]+"),
        ("fn (main|helper)", Mode::Regex, true, "fn (main|helper)"),
        ("widget", Mode::Word, true, r"\bwidget\b"),
        ("^struct", Mode::Regex, true, "^struct"),
    ];
    for (pat, mode, cs_, re) in cases {
        let mut req = SearchRequest::new(cs, pat, mode);
        req.case_sensitive = cs_;
        let got = run(&env, &snap, &req).await;
        assert_eq!(paths(&got), env.oracle(re), "{pat} {mode:?}");
        assert_eq!(got.coverage, 1.0);
        assert!(got.next.is_none());
    }
}

#[tokio::test]
async fn hit_lines_and_spans_are_exact() {
    let (env, cs) = fixture().await;
    let snap = env.index(|_| true).await;
    let req = SearchRequest::new(cs, "parse_widget", Mode::Literal);
    let got = run(&env, &snap, &req).await;
    let lib = got
        .hits
        .iter()
        .find(|h| h.paths == vec![p("src/lib.rs")])
        .unwrap();
    assert_eq!(
        lib.lines,
        vec![LineMatch {
            line: 1,
            text: b"pub fn parse_widget() {}".to_vec(),
            spans: vec![(7, 19)],
        }]
    );
    assert_eq!(got.generation, snap.generation);
}

#[tokio::test]
async fn a_file_at_two_paths_is_one_hit() {
    let (env, cs) = fixture().await;
    let snap = env.index(|_| true).await;
    let got = run(
        &env,
        &snap,
        &SearchRequest::new(cs, "same content", Mode::Literal),
    )
    .await;
    assert_eq!(got.hits.len(), 1);
    assert_eq!(got.hits[0].paths, vec![p("copy/a.txt"), p("copy/b.txt")]);
    // A filter that keeps one of the paths reports only that path.
    let mut req = SearchRequest::new(cs, "same content", Mode::Literal);
    req.exclude_paths = vec![p("copy/b.txt")];
    let got = run(&env, &snap, &req).await;
    assert_eq!(got.hits[0].paths, vec![p("copy/a.txt")]);
}

#[tokio::test]
async fn path_and_language_filters_apply() {
    let (env, cs) = fixture().await;
    let snap = env.index(|_| true).await;
    let mut req = SearchRequest::new(cs, "widget", Mode::Literal);
    req.paths = vec![p("src")];
    let got = run(&env, &snap, &req).await;
    assert_eq!(
        paths(&got),
        BTreeSet::from(["src/lib.rs", "src/main.rs", "src/deep/er/widget.rs"].map(String::from))
            .into_iter()
            .filter(|x| env.oracle("widget").contains(x))
            .collect()
    );
    let mut req = SearchRequest::new(cs, "widget", Mode::Literal);
    req.langs = vec![Lang::Rust];
    let got = run(&env, &snap, &req).await;
    assert!(paths(&got).iter().all(|x| x.ends_with(".rs")));
    assert!(!got.hits.is_empty());
    req.exclude_paths = vec![p("src"), p("tests")];
    assert!(run(&env, &snap, &req).await.hits.is_empty());
}

#[tokio::test]
async fn ranking_prefers_basename_and_demotes_tests() {
    let (env, cs) = fixture().await;
    let snap = env.index(|_| true).await;
    let mut req = SearchRequest::new(cs, "widget", Mode::Literal);
    req.case_sensitive = false;
    let got = run(&env, &snap, &req).await;
    let order: Vec<_> = got
        .hits
        .iter()
        .map(|h| String::from_utf8_lossy(h.paths[0].as_bytes()).into_owned())
        .collect();
    let at = |x: &str| order.iter().position(|p| p == x).unwrap();
    // The basename match outranks a shallow non-match, and the test file comes last of the
    // `.rs` files.
    assert!(at("src/deep/er/widget.rs") < at("src/lib.rs"));
    assert!(at("src/deep/er/widget.rs") < at("tests/widget_test.rs"));
}

#[test]
fn score_features_order_as_specified() {
    let one = |s: &str| vec![p(s)];
    let base = score(&one("src/a.rs"), false, Some("zzz"));
    assert!(
        score(&one("src/a.rs"), true, Some("zzz")) > base,
        "definition hit"
    );
    assert!(
        score(&one("src/zzz.rs"), false, Some("zzz")) > base,
        "basename"
    );
    assert!(score(&one("a.rs"), false, Some("zzz")) > base, "shallower");
    assert!(
        score(&one("tests/a.rs"), false, Some("zzz")) < score(&one("lib/a.rs"), false, Some("zzz"))
    );
    assert!(score(&one("src/a.min.js"), false, None) < base, "generated");
}

#[tokio::test]
async fn the_unindexed_part_of_the_scope_is_streamed() {
    let (env, cs) = fixture().await;
    // Only the README is indexed; the rest lags behind.
    let snap = env.index(|p| p == "README.md").await;
    let got = run(
        &env,
        &snap,
        &SearchRequest::new(cs, "parse_widget", Mode::Literal),
    )
    .await;
    assert_eq!(paths(&got), env.oracle("parse_widget"));
    assert_eq!(got.coverage, 1.0);
    assert_eq!(got.missing, 0);
    // An empty index streams everything, and the language filter still applies.
    let empty = env.index(|_| false).await;
    let mut req = SearchRequest::new(cs, "widget", Mode::Literal);
    req.langs = vec![Lang::Rust];
    let got = run(&env, &empty, &req).await;
    assert!(paths(&got).iter().all(|x| x.ends_with(".rs")));
    assert_eq!(got.coverage, 1.0);
    let got = run(
        &env,
        &empty,
        &SearchRequest::new(cs, "widget", Mode::Literal),
    )
    .await;
    assert_eq!(paths(&got), env.oracle("widget"));
}

#[tokio::test]
async fn streaming_stops_at_the_bound_and_enqueues_the_rest() {
    let mut env = Env::new();
    let n = STREAM_FILES + 100;
    let owned: Vec<(String, String)> = (0..n)
        .map(|i| (format!("d{}/f{i}.txt", i % 10), format!("needle {i}\n")))
        .collect();
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let cs = env.commit(&refs).await;
    let snap = env.index(|_| false).await;
    let mut req = SearchRequest::new(cs, "needle", Mode::Literal);
    req.paths = vec![
        p("d0"),
        p("d1"),
        p("d2"),
        p("d3"),
        p("d4"),
        p("d5"),
        p("d6"),
        p("d7"),
        p("d8"),
        p("d9"),
    ];
    let got = run(&env, &snap, &req).await;
    assert_eq!(got.missing, 100);
    assert!(got.coverage < 1.0);
    assert_eq!(got.hits.len(), STREAM_FILES);
    assert_eq!(env.docs.queued.lock().unwrap().len(), 100);
}

#[tokio::test]
async fn a_pattern_without_a_trigram_needs_a_filter_or_a_small_scope() {
    let (env, cs) = fixture().await;
    let snap = env.index(|_| true).await;
    // Small scope: a scan is allowed.
    let got = run(&env, &snap, &SearchRequest::new(cs, ".*", Mode::Regex)).await;
    assert_eq!(paths(&got).len(), FIXTURE.len());
    let got = run(&env, &snap, &SearchRequest::new(cs, "fn", Mode::Literal)).await;
    assert_eq!(paths(&got), env.oracle("fn"));

    // Large scope: refused without a path filter, allowed with one.
    let mut big = Env::new();
    let owned: Vec<(String, String)> = (0..SCAN_SCOPE_FILES + 5)
        .map(|i| (format!("g{}/f{i}.txt", i % 2), format!("line {i}\n")))
        .collect();
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let cs = big.commit(&refs).await;
    let snap = big.index(|_| true).await;
    let err = big
        .searcher
        .search(&snap, &SearchRequest::new(cs, ".*", Mode::Regex))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "INVALID_ARGUMENT: pattern too broad");
    let mut req = SearchRequest::new(cs, ".*", Mode::Regex);
    req.paths = vec![p("g0")];
    let got = run(&big, &snap, &req).await;
    assert_eq!(
        got.hits.len(),
        owned.iter().filter(|(p, _)| p.starts_with("g0/")).count()
    );
}

#[tokio::test]
async fn bad_patterns_are_invalid_arguments() {
    let (env, cs) = fixture().await;
    let snap = env.index(|_| true).await;
    for (pat, mode) in [
        ("", Mode::Literal),
        ("(", Mode::Regex),
        ("(a{1000}){1000}", Mode::Regex),
        (&"x".repeat(MAX_PATTERN + 1), Mode::Literal),
    ] {
        let err = env
            .searcher
            .search(&snap, &SearchRequest::new(cs, pat, mode))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SearchError::InvalidArgument(_)),
            "{pat:.20}: {err}"
        );
    }
}

#[tokio::test]
async fn the_cursor_resumes_after_the_last_examined_ord() {
    let (env, cs) = fixture().await;
    let snap = env.index(|_| true).await;
    let all = run(
        &env,
        &snap,
        &SearchRequest::new(cs, "widget", Mode::Literal),
    )
    .await;
    let mut ords: Vec<u32> = all.hits.iter().map(|h| h.ord).collect();
    ords.sort_unstable();
    assert!(ords.len() > 2);
    let mut req = SearchRequest::new(cs, "widget", Mode::Literal);
    req.cursor = Some(Cursor {
        window: 1,
        ord: ords[1],
    });
    let rest = run(&env, &snap, &req).await;
    let mut got: Vec<u32> = rest.hits.iter().map(|h| h.ord).collect();
    got.sort_unstable();
    assert_eq!(got, ords[2..].to_vec());
    let c = Cursor { window: 3, ord: 77 };
    assert_eq!(Cursor::from_bytes(&c.to_bytes()), Some(c));
    assert_eq!(Cursor::from_bytes(&[1, 2, 3]), None);
}

#[tokio::test]
async fn long_lines_and_many_matches_are_bounded() {
    let mut env = Env::new();
    // A line over the skip bound is never searched; one at the bound is kept whole.
    let long = format!(
        "{}needle{}
",
        "a".repeat(3000),
        "b".repeat(3000)
    );
    let edge = format!(
        "{}needle{}
",
        "a".repeat(1000),
        "b".repeat(994)
    );
    let many = "needle
"
    .repeat(MAX_LINES_PER_FILE + 50);
    let cs = env
        .commit(&[
            ("long.txt", &long),
            ("edge.txt", &edge),
            ("many.txt", &many),
        ])
        .await;
    let snap = env.index(|_| false).await;
    let got = run(
        &env,
        &snap,
        &SearchRequest::new(cs, "needle", Mode::Literal),
    )
    .await;
    let by = |n: &str| {
        got.hits
            .iter()
            .find(|h| h.paths[0].as_bytes() == n.as_bytes())
    };
    assert!(by("long.txt").is_none());
    let l = &by("edge.txt").unwrap().lines[0];
    assert_eq!(l.text.len(), MAX_LINE_BYTES);
    assert_eq!(l.spans, vec![(1000, 1006)]);
    let m = by("many.txt").unwrap();
    assert_eq!(m.lines.len(), MAX_LINES_PER_FILE);
    assert!(m.more_lines);
}

#[tokio::test]
async fn binary_files_are_not_searched() {
    let mut env = Env::new();
    let cs = env
        .commit(&[("a.bin", "needle\0binary"), ("a.txt", "needle")])
        .await;
    let snap = env.index(|_| false).await;
    let got = run(
        &env,
        &snap,
        &SearchRequest::new(cs, "needle", Mode::Literal),
    )
    .await;
    assert_eq!(paths(&got), BTreeSet::from(["a.txt".to_string()]));
}
