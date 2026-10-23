use std::collections::HashMap;

use super::*;
use crate::grams::distinct_trigrams;
use crate::index::schema::IndexKind;
use crate::index::segments::{FileDoc, SegmentWriter, WRITER_HEAP, unpack_and_open};

struct F {
    paths: Vec<&'static str>,
    lang: Option<Lang>,
    text: &'static str,
}

struct Src(HashMap<u32, F>);

impl FileSource for Src {
    fn paths(&self, ord: u32) -> Vec<Vec<u8>> {
        self.0[&ord]
            .paths
            .iter()
            .map(|p| p.as_bytes().to_vec())
            .collect()
    }
    fn lang(&self, ord: u32) -> Option<Lang> {
        self.0.get(&ord)?.lang
    }
    fn bytes(&self, ord: u32) -> Result<Option<Arc<[u8]>>> {
        Ok(self.0.get(&ord).map(|f| Arc::from(f.text.as_bytes())))
    }
    fn read_path(&self, path: &[u8]) -> Result<Option<Arc<[u8]>>> {
        Ok(self
            .0
            .values()
            .find(|f| f.paths.iter().any(|p| p.as_bytes() == path))
            .map(|f| Arc::from(f.text.as_bytes())))
    }
    fn untagged(&self, scope: &RoaringBitmap, limit: usize) -> Vec<u32> {
        let mut v: Vec<u32> = scope
            .iter()
            .filter(|o| self.0.get(o).is_some_and(|f| f.lang.is_none()))
            .collect();
        v.truncate(limit);
        v
    }
}

struct Proj;

impl ProjectRank for Proj {
    fn distance(&self, from: &[u8], to: &[u8]) -> Option<u32> {
        let top = |p: &[u8]| p.split(|&c| c == b'/').next().map(<[u8]>::to_vec);
        (top(from) == top(to)).then_some(0)
    }
}

fn snapshot(dir: &std::path::Path, src: &Src) -> Snapshot {
    let mut w = SegmentWriter::new(IndexKind::Files, dir, WRITER_HEAP).unwrap();
    let mut ords: Vec<u32> = src.0.keys().copied().collect();
    ords.sort_unstable();
    for ord in ords {
        let f = &src.0[&ord];
        let tags = f
            .lang
            .map(|l| tags::extract(l, f.text.as_bytes()).unwrap())
            .unwrap_or_default();
        let defs: Vec<&str> = tags
            .iter()
            .filter(|t| t.role == Role::Def)
            .map(|t| t.name.as_str())
            .collect();
        let refs: Vec<&str> = tags
            .iter()
            .filter(|t| t.role == Role::Ref)
            .map(|t| t.name.as_str())
            .collect();
        let grams = distinct_trigrams(f.text.as_bytes(), 1 << 20).unwrap();
        let langs: Vec<Lang> = f.lang.into_iter().collect();
        w.add_file(&FileDoc {
            ord,
            grams: &grams,
            sym_def: &defs,
            sym_ref: &refs,
            ident: f.text,
            doc: "",
            path: f.paths[0],
            langs: &langs,
        })
        .unwrap();
    }
    let pack = w.finish().unwrap().unwrap();
    let raw = asc_store::codec::decode_stored_capped(&pack.stored, 1 << 28).unwrap();
    let seg = unpack_and_open(IndexKind::Files, pack.hash, &raw, &dir.join("open")).unwrap();
    Snapshot {
        generation: 1,
        kind: IndexKind::Files,
        indexed: pack.ords.clone(),
        segments: vec![seg],
    }
}

const LIB: &str =
    "pub fn parse_widget() {}\npub struct Gadget;\nimpl Gadget {\n    pub fn spin(&self) {}\n}\n";
const USER: &str = "use crate::lib_a;\nfn run() {\n    parse_widget();\n}\n";
const OTHER: &str = "fn other() {\n    parse_widget();\n}\n";
const NOTES: &str = "see parse_widget( here\nparse_widgets are fine\n";

fn rust(paths: Vec<&'static str>, text: &'static str) -> F {
    F {
        paths,
        lang: Some(Lang::Rust),
        text,
    }
}

fn fixture() -> Src {
    let mut m = HashMap::new();
    m.insert(1, rust(vec!["a/src/lib_a.rs"], LIB));
    m.insert(2, rust(vec!["b/src/user.rs"], USER));
    m.insert(3, rust(vec!["c/src/other.rs"], OTHER));
    m.insert(
        4,
        F {
            paths: vec!["docs/notes.txt"],
            lang: None,
            text: NOTES,
        },
    );
    m.insert(
        5,
        rust(
            vec!["a/src/copy_a.rs", "z/copy_b.rs"],
            "fn parse_widget_two() {}\n",
        ),
    );
    Src(m)
}

fn all() -> RoaringBitmap {
    (1..=5).collect()
}

fn names(s: &Symbols) -> Vec<(String, String)> {
    s.symbols
        .iter()
        .map(|s| (s.name.clone(), String::from_utf8(s.path.clone()).unwrap()))
        .collect()
}

#[test]
fn def_exact_prefix_fuzzy_and_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture();
    let snap = snapshot(dir.path(), &src);
    let e = SymbolEngine::new(&snap, &src, &NoProjects);
    let r = e.def(&all(), &SymbolQuery::new("parse_widget")).unwrap();
    assert_eq!(
        names(&r),
        vec![("parse_widget".into(), "a/src/lib_a.rs".into())]
    );
    assert_eq!(r.symbols[0].line, 1);
    assert_eq!(r.symbols[0].kind, "function");
    let r = e.def(&all(), &SymbolQuery::new("parse_*")).unwrap();
    assert_eq!(r.symbols.len(), 3, "{:?}", names(&r));
    assert!(r.symbols.iter().any(|s| s.name == "parse_widget"));
    // Fuzzy: one edit away, only when the exact name finds nothing.
    let r = e.def(&all(), &SymbolQuery::new("parse_widgit")).unwrap();
    assert_eq!(r.symbols[0].name, "parse_widget");
    assert!(
        e.def(&all(), &SymbolQuery::new("pxrse_widgit"))
            .unwrap()
            .symbols
            .is_empty()
    );
    let mut q = SymbolQuery::new("Gadget");
    q.kinds = vec![Kind::Struct];
    assert!(
        e.def(&all(), &q)
            .unwrap()
            .symbols
            .iter()
            .all(|s| s.kind == "struct")
    );
    q.kinds = vec![Kind::Trait];
    assert!(e.def(&all(), &q).unwrap().symbols.is_empty());
    // Out of scope: invisible.
    let only: RoaringBitmap = [2].into_iter().collect();
    assert!(
        e.def(&only, &SymbolQuery::new("parse_widget"))
            .unwrap()
            .symbols
            .is_empty()
    );
}

#[test]
fn def_reports_every_path_of_a_shared_file() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture();
    let snap = snapshot(dir.path(), &src);
    let e = SymbolEngine::new(&snap, &src, &NoProjects);
    let r = e
        .def(&all(), &SymbolQuery::new("parse_widget_two"))
        .unwrap();
    assert_eq!(r.symbols.len(), 2);
}

#[test]
fn def_ranks_exact_then_project_then_importer() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = fixture();
    src.0
        .insert(6, rust(vec!["b/src/near.rs"], "fn parse_widget() {}\n"));
    let snap = snapshot(dir.path(), &src);
    let e = SymbolEngine::new(&snap, &src, &Proj);
    let mut q = SymbolQuery::new("parse_widget");
    q.at = Some((b"b/src/user.rs".to_vec(), 3));
    let scope: RoaringBitmap = (1..=6).collect();
    let r = e.def(&scope, &q).unwrap();
    assert_eq!(names(&r)[0].1, "b/src/near.rs", "same project first");
    assert_eq!(names(&r)[1].1, "a/src/lib_a.rs");
    // Without a project graph the importer wins: `user.rs` imports `lib_a`.
    let e = SymbolEngine::new(&snap, &src, &NoProjects);
    let r = e.def(&scope, &q).unwrap();
    assert_eq!(names(&r)[0].1, "a/src/lib_a.rs");
}

#[test]
fn refs_rank_by_import_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture();
    let snap = snapshot(dir.path(), &src);
    let e = SymbolEngine::new(&snap, &src, &Proj);
    let mut q = SymbolQuery::new("parse_widget");
    let r = e.refs(&all(), &q).unwrap();
    assert!(r.symbols.iter().all(|s| !s.is_def));
    assert!(r.symbols.iter().all(|s| s.confidence == NAME_MATCH));
    q.at = Some((b"a/src/lib_a.rs".to_vec(), 1));
    let r = e.refs(&all(), &q).unwrap();
    let user = r
        .symbols
        .iter()
        .find(|s| s.path == b"b/src/user.rs")
        .unwrap();
    let other = r
        .symbols
        .iter()
        .find(|s| s.path == b"c/src/other.rs")
        .unwrap();
    assert_eq!(user.confidence, IMPORT_MATCH);
    assert_eq!(other.confidence, NAME_MATCH);
    let pos = |p: &[u8]| r.symbols.iter().position(|s| s.path == p).unwrap();
    assert!(pos(b"b/src/user.rs") < pos(b"c/src/other.rs"));
}

#[test]
fn refs_fall_back_to_word_boundary_scan() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture();
    let snap = snapshot(dir.path(), &src);
    let e = SymbolEngine::new(&snap, &src, &NoProjects);
    let r = e.refs(&all(), &SymbolQuery::new("parse_widget")).unwrap();
    let notes: Vec<_> = r
        .symbols
        .iter()
        .filter(|s| s.path == b"docs/notes.txt")
        .collect();
    assert_eq!(notes.len(), 1, "`parse_widgets` is not a word match");
    assert_eq!(notes[0].line, 1);
    assert_eq!(notes[0].signature, b"see parse_widget( here");
}

#[test]
fn callers_group_by_container() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = fixture();
    src.0.insert(
        7,
        rust(
            vec!["b/src/twice.rs"],
            "fn twice() {\n    parse_widget();\n    parse_widget();\n}\n",
        ),
    );
    let snap = snapshot(dir.path(), &src);
    let e = SymbolEngine::new(&snap, &src, &NoProjects);
    let scope: RoaringBitmap = (1..=7).collect();
    let r = e
        .callers(&scope, &SymbolQuery::new("parse_widget"))
        .unwrap();
    let in_twice: Vec<_> = r
        .symbols
        .iter()
        .filter(|s| s.path == b"b/src/twice.rs")
        .collect();
    assert_eq!(
        in_twice.len(),
        1,
        "two calls in one function make one caller"
    );
    assert_eq!(in_twice[0].name, "twice");
    assert!(in_twice[0].is_def);
    assert!(r.symbols.iter().any(|s| s.name == "run"));
    assert!(r.symbols.iter().any(|s| s.name == "other"));
    // The fallback counts only a name followed by `(`.
    assert!(r.symbols.iter().any(|s| s.path == b"docs/notes.txt"));
}

#[test]
fn find_is_a_prefix_search() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture();
    let snap = snapshot(dir.path(), &src);
    let e = SymbolEngine::new(&snap, &src, &NoProjects);
    let r = e.find(&all(), &SymbolQuery::new("Gad")).unwrap();
    // The struct and its `impl` block.
    assert_eq!(r.symbols.len(), 2);
    assert!(r.symbols.iter().all(|s| s.name == "Gadget"));
}

#[test]
fn outline_is_in_source_order_with_containers() {
    let r = outline(b"a/src/lib_a.rs", Lang::Rust, LIB.as_bytes(), &[], 0).unwrap();
    let n: Vec<_> = r.symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(n, vec!["parse_widget", "Gadget", "Gadget", "spin"]);
    let spin = &r.symbols[3];
    assert_eq!(spin.container, "Gadget");
    assert!(spin.line > r.symbols[2].line);
    let r = outline(b"p", Lang::Rust, LIB.as_bytes(), &[Kind::Struct], 0).unwrap();
    assert_eq!(r.symbols.len(), 1);
}

#[test]
fn limits_and_hostile_names() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture();
    let snap = snapshot(dir.path(), &src);
    let e = SymbolEngine::new(&snap, &src, &NoProjects);
    for bad in ["", "*", "a*b", "a\0b", "x-y*"] {
        assert!(
            matches!(
                e.def(&all(), &SymbolQuery::new(bad)),
                Err(SymbolError::InvalidName(_))
            ),
            "{bad:?}"
        );
    }
    let long = "a".repeat(MAX_NAME_BYTES + 1);
    assert!(e.def(&all(), &SymbolQuery::new(long)).is_err());
    assert!(e.refs(&all(), &SymbolQuery::new("p*")).is_err());
    let mut q = SymbolQuery::new("parse_*");
    q.limit = 1;
    let r = e.def(&all(), &q).unwrap();
    assert_eq!(r.symbols.len(), 1);
    assert!(r.truncated);
    assert!(r.est_tokens > 0);
}

#[test]
fn helpers() {
    assert!(within_one("abc", "abd"));
    assert!(within_one("abc", "ab"));
    assert!(within_one("abc", "abcd"));
    assert!(!within_one("abc", "bca"));
    assert!(!within_one("abc", "xyz"));
    assert!(imports_path(b"from pkg import util\n", b"pkg/util.py"));
    assert!(!imports_path(b"let util = 1;\n", b"pkg/util.py"));
    assert!(imports_path(b"use crate::net;\n", b"src/net/mod.rs"));
    assert_eq!(find_word(b"a parse_widgets b", b"parse_widget"), None);
}
