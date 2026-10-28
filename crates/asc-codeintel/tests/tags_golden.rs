//! Golden tags per language, the TypeScript grammar against tree-sitter core, outline and bounds.
//! Set `ASC_BLESS=1` to rewrite the golden files after a reviewed query change.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use asc_codeintel::lang::Lang;
use asc_codeintel::skip::MAX_BYTES;
use asc_codeintel::tags::{
    Kind, NO_CONTAINER, Role, Tag, TagsError, extract, extract_within, outline, query_source,
};

const ALL: [Lang; 8] = [
    Lang::Rust,
    Lang::TypeScript,
    Lang::Tsx,
    Lang::Tsrx,
    Lang::JavaScript,
    Lang::Python,
    Lang::CSharp,
    Lang::Go,
];

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tags")
}

fn render(tags: &[Tag]) -> String {
    let mut out = String::new();
    for (i, t) in tags.iter().enumerate() {
        let role = if t.role == Role::Def { "def" } else { "ref" };
        let container = if t.container == NO_CONTAINER {
            "-".to_string()
        } else {
            assert!((t.container as usize) < i, "container precedes its tag");
            tags[t.container as usize].name.clone()
        };
        write!(
            out,
            "{role} {} {} L{}-{} in {container}",
            t.kind.name(),
            t.name,
            t.range.start_row + 1,
            t.range.end_row + 1
        )
        .unwrap();
        if t.role == Role::Def {
            write!(out, " | sig={:?}", String::from_utf8_lossy(&t.sig)).unwrap();
            if !t.doc.is_empty() {
                write!(out, " | doc={:?}", String::from_utf8_lossy(&t.doc)).unwrap();
            }
        }
        out.push('\n');
    }
    out
}

fn golden(file: &str, lang: Lang) {
    let src = std::fs::read(dir().join(file)).unwrap();
    let got = render(&extract(lang, &src).unwrap());
    let path = dir().join(format!("{file}.golden"));
    if std::env::var_os("ASC_BLESS").is_some() {
        std::fs::write(&path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(got, want, "golden mismatch for {file}");
}

#[test]
fn golden_rust() {
    golden("sample.rs", Lang::Rust);
}

#[test]
fn golden_typescript() {
    golden("sample.ts", Lang::TypeScript);
}

#[test]
fn golden_tsx() {
    golden("sample.tsx", Lang::Tsx);
}

#[test]
fn golden_tsrx() {
    golden("sample.tsrx", Lang::Tsrx);
}

#[test]
fn golden_javascript() {
    golden("sample.js", Lang::JavaScript);
}

#[test]
fn golden_python() {
    golden("sample.py", Lang::Python);
}

#[test]
fn golden_csharp() {
    golden("sample.cs", Lang::CSharp);
}

#[test]
fn golden_go() {
    golden("sample.go", Lang::Go);
}

/// typescript 0.23.2 was built against an older core than 0.27.1: both TS and TSX must load,
/// parse, and answer the project query. If this fails the grammar has to be vendored.
#[test]
fn typescript_grammar_works_with_core() {
    let ts = extract(
        Lang::TypeScript,
        b"export function f(a: number): number { return g(a); }",
    )
    .unwrap();
    assert!(
        ts.iter()
            .any(|t| t.role == Role::Def && t.name == "f" && t.kind == Kind::Function)
    );
    assert!(ts.iter().any(|t| t.role == Role::Ref && t.name == "g"));
    let tsx = extract(Lang::Tsx, b"const A = () => <b>{h()}</b>;").unwrap();
    assert!(tsx.iter().any(|t| t.role == Role::Def && t.name == "A"));
    assert!(tsx.iter().any(|t| t.role == Role::Ref && t.name == "h"));
}

#[test]
fn every_query_compiles_and_names_known_kinds() {
    for lang in ALL {
        extract(lang, b"").unwrap();
        for cap in query_source(lang).split('@').skip(1) {
            let cap: String = cap
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_')
                .collect();
            if let Some(k) = cap
                .strip_prefix("def.")
                .or_else(|| cap.strip_prefix("ref."))
            {
                assert!(Kind::from_name(k).is_some(), "{lang:?}: unknown kind {cap}");
            } else {
                assert_eq!(cap, "name", "{lang:?}: unknown capture {cap}");
            }
        }
    }
}

#[test]
fn methods_are_functions_inside_types() {
    let tags = extract(Lang::Rust, b"struct S; impl S { fn m(&self) {} } fn f() {}").unwrap();
    let kind = |n: &str| {
        tags.iter()
            .find(|t| t.name == n && t.role == Role::Def)
            .unwrap()
            .kind
    };
    assert_eq!(kind("m"), Kind::Method);
    assert_eq!(kind("f"), Kind::Function);
}

#[test]
fn locals_are_not_symbols() {
    let tags = extract(Lang::Rust, b"fn f() { const A: u8 = 1; let b = 2; }").unwrap();
    assert!(!tags.iter().any(|t| t.name == "A"));
}

#[test]
fn outline_nests_definitions_in_source_order() {
    let src = std::fs::read(dir().join("sample.rs")).unwrap();
    let tags = extract(Lang::Rust, &src).unwrap();
    let out = outline(&tags);
    assert!(out.iter().all(|e| tags[e.index as usize].role == Role::Def));
    assert!(out.windows(2).all(|w| w[0].index < w[1].index));
    let depth = |n: &str| {
        out.iter()
            .find(|e| tags[e.index as usize].name == n)
            .map(|e| e.depth)
            .unwrap()
    };
    assert_eq!(depth("Point"), 0);
    assert_eq!(depth("x"), 1);
    assert_eq!(depth("deep"), 1);
    assert_eq!(depth("inner"), 0);
}

#[test]
fn outline_survives_hostile_containers() {
    let mut tags = extract(Lang::Rust, b"fn a() {} fn b() {}").unwrap();
    tags[0].container = 1;
    tags[1].container = 99;
    let out = outline(&tags);
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|e| e.depth == 0));
}

#[test]
fn oversized_file_is_refused() {
    let big = vec![b'a'; MAX_BYTES + 1];
    assert_eq!(extract(Lang::Rust, &big), Err(TagsError::TooLarge));
}

#[test]
fn zero_budget_cancels() {
    let mut src = Vec::new();
    while src.len() < 900_000 {
        src.extend_from_slice(b"fn function_name(argument: u32) -> u32 { argument + 1 }\n");
    }
    assert_eq!(
        extract_within(Lang::Rust, &src, Duration::ZERO),
        Err(TagsError::Cancelled)
    );
}

#[test]
fn garbage_and_invalid_utf8_do_not_panic() {
    let junk: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    for lang in ALL {
        let _ = extract(lang, &junk);
        let _ = extract(lang, b"fn \xff\xfe(x) { \xc3 }");
    }
}

#[test]
fn deep_nesting_is_bounded() {
    let src = format!("{}{}", "fn f() { ".repeat(5000), "}".repeat(5000));
    let _ = extract(Lang::Rust, src.as_bytes());
    let src = format!("{}1{}", "(".repeat(20_000), ")".repeat(20_000));
    let _ = extract(Lang::JavaScript, src.as_bytes());
}

#[test]
fn long_names_are_dropped_and_text_is_bounded() {
    let long = "a".repeat(5000);
    let src = format!("fn {long}() {{}} fn ok({}) {{}}", "p: u8, ".repeat(2000));
    let tags = extract(Lang::Rust, src.as_bytes()).unwrap();
    assert!(!tags.iter().any(|t| t.name.len() > 256));
    let ok = tags.iter().find(|t| t.name == "ok").unwrap();
    assert!(ok.sig.len() <= 512);
}

#[test]
fn extraction_is_deterministic() {
    let src = std::fs::read(dir().join("sample.go")).unwrap();
    assert_eq!(extract(Lang::Go, &src), extract(Lang::Go, &src));
}
