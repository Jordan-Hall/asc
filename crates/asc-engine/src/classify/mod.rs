//! Change classification (SPEC §8.10, PLAN U67).
//!
//! [`Engine::classify_trees`] gives each changed file a [`ChangeClass`] and the maximum over the
//! files as `overall`. The rule is soundness: a false `LOGIC` is acceptable, a false lower class
//! is a bug. So a lexer error, an unknown extension, a binary or oversized file, a symlink, a
//! submodule or a type change is always `LOGIC`.
//!
//! Callers read the [`Rules`] (`.asc/review.toml`) and the `linguist-generated` attribute from the
//! trusted side (the base of the comparison), never from the head being judged, and every change to
//! those files themselves is `LOGIC`.

use async_trait::async_trait;
use asc_core::id::{FileId, TreeId};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

use crate::Engine;
use crate::diff::{self, DiffOptions, RenameOptions, Status, TreeChange};

pub mod lexer;

use lexer::{Family, Lang, Lines};

/// Largest file (per side) whose content is lexed; a larger one is `LOGIC`.
pub const MAX_CONTENT_BYTES: u64 = 512 * 1024;
/// Most changed files one classification handles.
pub const MAX_FILES: usize = 100_000;
/// Most content bytes one classification reads in total; later files fall back to the path rules.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Most globs in one class of `review.toml`, and the longest glob.
pub const MAX_GLOBS: usize = 256;
pub const MAX_GLOB_BYTES: usize = 512;

/// How a file changed, from the most to the least trivial. The order is the order of risk, and
/// the numbers are the `ChangeClass` wire values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ChangeClass {
    RenameOnly = 0,
    ModeOnly = 1,
    WhitespaceOnly = 2,
    CommentOnly = 3,
    Docs = 4,
    Generated = 5,
    Lockfile = 6,
    Test = 7,
    Logic = 8,
}

impl ChangeClass {
    pub fn from_u8(n: u8) -> Option<Self> {
        Some(match n {
            0 => Self::RenameOnly,
            1 => Self::ModeOnly,
            2 => Self::WhitespaceOnly,
            3 => Self::CommentOnly,
            4 => Self::Docs,
            5 => Self::Generated,
            6 => Self::Lockfile,
            7 => Self::Test,
            8 => Self::Logic,
            _ => return None,
        })
    }

    /// The name the CLI and CI print.
    pub fn name(self) -> &'static str {
        match self {
            Self::RenameOnly => "RENAME_ONLY",
            Self::ModeOnly => "MODE_ONLY",
            Self::WhitespaceOnly => "WHITESPACE_ONLY",
            Self::CommentOnly => "COMMENT_ONLY",
            Self::Docs => "DOCS",
            Self::Generated => "GENERATED",
            Self::Lockfile => "LOCKFILE",
            Self::Test => "TEST",
            Self::Logic => "LOGIC",
        }
    }
}

// ------------------------------------------------------------------------------ rules

/// A `review.toml` that cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("too many globs in `{0}` (at most {MAX_GLOBS})")]
    TooMany(&'static str),
    #[error("glob in `{0}` is too long (at most {MAX_GLOB_BYTES} bytes)")]
    TooLong(&'static str),
    #[error("bad glob {1:?} in `{0}`: {2}")]
    Bad(&'static str, String, globset::Error),
}

/// The path classes: which files are documentation, tests or lockfiles.
#[derive(Clone, Debug)]
pub struct Rules {
    docs: GlobSet,
    test: GlobSet,
    lockfile: GlobSet,
}

const DEFAULT_DOCS: &[&str] = &["**/*.md", "docs/**"];
const DEFAULT_TEST: &[&str] = &["**/*test*/**"];
const DEFAULT_LOCKFILE: &[&str] = &["bun.lock", "Cargo.lock", "uv.lock", "**/packages.lock.json"];

fn build(name: &'static str, globs: &[String]) -> Result<GlobSet, RulesError> {
    if globs.len() > MAX_GLOBS {
        return Err(RulesError::TooMany(name));
    }
    let mut b = GlobSetBuilder::new();
    for g in globs {
        if g.len() > MAX_GLOB_BYTES {
            return Err(RulesError::TooLong(name));
        }
        let glob: Glob = GlobBuilder::new(g)
            .literal_separator(true)
            .build()
            .map_err(|e| RulesError::Bad(name, g.clone(), e))?;
        b.add(glob);
    }
    b.build()
        .map_err(|e| RulesError::Bad(name, String::new(), e))
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

impl Rules {
    /// The globs of `.asc/review.toml`; `None` for a class keeps its default.
    pub fn new(
        docs: Option<&[String]>,
        test: Option<&[String]>,
        lockfile: Option<&[String]>,
    ) -> Result<Self, RulesError> {
        let pick = |name, given: Option<&[String]>, default: &[&str]| match given {
            Some(g) => build(name, g),
            None => build(name, &strings(default)),
        };
        Ok(Self {
            docs: pick("docs", docs, DEFAULT_DOCS)?,
            test: pick("test", test, DEFAULT_TEST)?,
            lockfile: pick("lockfile", lockfile, DEFAULT_LOCKFILE)?,
        })
    }

    /// The default globs (SPEC §8.10).
    pub fn defaults() -> Self {
        Self::new(None, None, None).expect("the default globs are valid")
    }

    /// No path classes at all: only content decides, and everything else is `LOGIC`. This is what
    /// an unreadable `review.toml` means.
    pub fn none() -> Self {
        Self {
            docs: GlobSet::empty(),
            test: GlobSet::empty(),
            lockfile: GlobSet::empty(),
        }
    }

    /// The most risky path class `path` matches. A path that is not UTF-8 or holds a backslash
    /// matches nothing.
    pub fn path_class(&self, path: &[u8]) -> Option<ChangeClass> {
        let p = std::str::from_utf8(path).ok()?;
        if p.contains('\\') {
            return None;
        }
        [
            (&self.test, ChangeClass::Test),
            (&self.lockfile, ChangeClass::Lockfile),
            (&self.docs, ChangeClass::Docs),
        ]
        .into_iter()
        .find(|(set, _)| set.is_match(p))
        .map(|(_, c)| c)
    }
}

/// Files that configure classification or attributes: changing one is always `LOGIC`.
fn is_config_path(path: &[u8]) -> bool {
    let name = path.rsplit(|&c| c == b'/').next().unwrap_or(path);
    matches!(name, b".gitattributes" | b".ascattributes") || path.starts_with(b".asc/")
}

// ------------------------------------------------------------------------------ content

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\x0b' | '\x0c')
}

fn token_class(lang: Lang, old: &str, new: &str) -> ChangeClass {
    use lexer::tokens;
    if let (Ok(a), Ok(b)) = (tokens(lang, old, true), tokens(lang, new, true))
        && a == b
    {
        return ChangeClass::WhitespaceOnly;
    }
    if let (Ok(a), Ok(b)) = (tokens(lang, old, false), tokens(lang, new, false))
        && a == b
    {
        return ChangeClass::CommentOnly;
    }
    ChangeClass::Logic
}

fn whitespace_only_lines(lang: Lang, old: &str, new: &str, lo: &Lines, ln: &Lines) -> bool {
    let a: Vec<&str> = old.split('\n').collect();
    let b: Vec<&str> = new.split('\n').collect();
    if a.len() != b.len() || lo.protected.len() != a.len() || ln.protected.len() != b.len() {
        return false;
    }
    for i in 0..a.len() {
        if a[i] == b[i] {
            continue;
        }
        if lo.protected[i] || ln.protected[i] {
            return false;
        }
        let (ta, tb) = (a[i].trim_end_matches(is_ws), b[i].trim_end_matches(is_ws));
        if ta != tb || (i == 0 && ta.starts_with("#!")) {
            return false;
        }
        match lang {
            // Two trailing spaces are a hard break.
            Lang::Md | Lang::Html if a[i].len() - ta.len() >= 2 || b[i].len() - tb.len() >= 2 => {
                return false;
            }
            // Trailing blanks of an assignment are part of its value.
            Lang::Make if ta.contains('=') => return false,
            _ => {}
        }
    }
    true
}

/// The lines of `src` with the droppable comments removed. A line left empty by a removed
/// comment disappears; a line that lost a comment has its trailing blanks trimmed (unless its line
/// break is inside a string); every other line is kept exactly.
fn strip_lines(src: &str, lines: &Lines) -> Option<Vec<String>> {
    let mut starts = vec![0usize];
    starts.extend(
        src.bytes()
            .enumerate()
            .filter(|(_, c)| *c == b'\n')
            .map(|(i, _)| i + 1),
    );
    if lines.protected.len() != starts.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut j = 0;
    for (l, &ls) in starts.iter().enumerate() {
        let le = starts.get(l + 1).map_or(src.len(), |e| e - 1);
        while lines.comments.get(j).is_some_and(|&(_, z)| z <= ls) {
            j += 1;
        }
        let mut text = String::new();
        let mut pos = ls;
        let mut touched = false;
        let mut k = j;
        while let Some(&(a, z)) = lines.comments.get(k) {
            if a >= le {
                break;
            }
            if z > ls {
                touched = true;
                if a > pos {
                    text.push_str(src.get(pos..a)?);
                }
                pos = pos.max(z.min(le));
            }
            k += 1;
        }
        text.push_str(src.get(pos..le)?);
        if touched && text.trim_matches(is_ws).is_empty() {
            continue;
        }
        if touched && !lines.protected[l] {
            text.truncate(text.trim_end_matches(is_ws).len());
        }
        out.push(text);
    }
    Some(out)
}

fn line_class(lang: Lang, old: &str, new: &str) -> ChangeClass {
    let (Ok(lo), Ok(ln)) = (lexer::scan_lines(lang, old), lexer::scan_lines(lang, new)) else {
        return ChangeClass::Logic;
    };
    if whitespace_only_lines(lang, old, new, &lo, &ln) {
        return ChangeClass::WhitespaceOnly;
    }
    if lo.comments_ok
        && ln.comments_ok
        && let (Some(a), Some(b)) = (strip_lines(old, &lo), strip_lines(new, &ln))
        && a == b
    {
        return ChangeClass::CommentOnly;
    }
    ChangeClass::Logic
}

/// The class of a change to one file's content: `WHITESPACE_ONLY`, `COMMENT_ONLY` or `LOGIC`
/// (`RENAME_ONLY` when the bytes are equal).
pub fn classify_content(path: &[u8], old: &[u8], new: &[u8]) -> ChangeClass {
    if old == new {
        return ChangeClass::RenameOnly;
    }
    let Some(lang) = Lang::of_path(path) else {
        return ChangeClass::Logic;
    };
    let (Ok(old), Ok(new)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return ChangeClass::Logic;
    };
    if old.len() as u64 > MAX_CONTENT_BYTES || new.len() as u64 > MAX_CONTENT_BYTES {
        return ChangeClass::Logic;
    }
    match lang.family() {
        Family::Token => token_class(lang, old, new),
        Family::Line => line_class(lang, old, new),
    }
}

// ------------------------------------------------------------------------------ one change

fn regular(k: Kind) -> bool {
    matches!(k, Kind::File | Kind::Exec)
}

/// Whether classifying `c` can use the content of both sides.
pub fn wants_content(c: &TreeChange) -> bool {
    let (Some(o), Some(n)) = (&c.old, &c.new) else {
        return false;
    };
    c.status == Status::Modified
        && regular(o.kind)
        && regular(n.kind)
        && o.id != n.id
        && o.size <= MAX_CONTENT_BYTES
        && n.size <= MAX_CONTENT_BYTES
        && !is_config_path(c.path.as_bytes())
        && Lang::of_path(c.path.as_bytes()).is_some()
}

/// Whether the `linguist-generated` attribute could change the class of `c`.
pub fn wants_generated(c: &TreeChange) -> bool {
    matches!(c.status, Status::Added | Status::Deleted | Status::Modified)
        && c.old.iter().chain(&c.new).all(|e| regular(e.kind))
        && !is_config_path(c.path.as_bytes())
}

/// The class of one tree change. `content` holds the old and new bytes when they were read;
/// `generated` is the `linguist-generated` attribute of the path on the trusted side.
pub fn classify_change(
    rules: &Rules,
    c: &TreeChange,
    generated: bool,
    content: Option<(&[u8], &[u8])>,
) -> ChangeClass {
    use ChangeClass::{Generated, Logic, ModeOnly, RenameOnly};
    let (path, old_path) = (c.path.as_bytes(), c.old_path.as_bytes());
    if is_config_path(path) || is_config_path(old_path) {
        return Logic;
    }
    let path_class = || {
        rules
            .path_class(path)
            .max(generated.then_some(Generated))
            .unwrap_or(Logic)
    };
    match (c.status, &c.old, &c.new) {
        (Status::Renamed(_), Some(o), Some(n)) => {
            if o.kind == Kind::Gitlink || n.kind == Kind::Gitlink || o.id != n.id {
                Logic
            } else if o.kind != n.kind {
                ModeOnly
            } else {
                RenameOnly
            }
        }
        (Status::Added, None, Some(e)) | (Status::Deleted, Some(e), None) if regular(e.kind) => {
            path_class()
        }
        (Status::Modified, Some(o), Some(n)) if regular(o.kind) && regular(n.kind) => {
            let mode = if o.kind == n.kind {
                RenameOnly
            } else {
                ModeOnly
            };
            if o.id == n.id {
                return mode.max(ModeOnly);
            }
            let by_content = content
                .map_or(Logic, |(a, b)| classify_content(path, a, b))
                .max(mode);
            if by_content < ChangeClass::Docs {
                by_content
            } else {
                path_class().max(mode)
            }
        }
        _ => Logic,
    }
}

// ------------------------------------------------------------------------------ engine

/// Why a gate refused.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("text memory budget exhausted")]
    Exhausted,
    #[error("{0}")]
    Failed(String),
}

/// Held while the content of a file pair is buffered.
pub struct Admission(#[allow(dead_code)] Option<Box<dyn Send + Sync>>);

impl Admission {
    pub fn none() -> Self {
        Self(None)
    }

    pub fn holding(guard: impl Send + Sync + 'static) -> Self {
        Self(Some(Box::new(guard)))
    }
}

/// What the caller supplies to a classification: the attribute lookup and the memory budget.
#[async_trait]
pub trait Gate: Send + Sync {
    /// `linguist-generated` for `path`, read on the trusted side of the comparison.
    async fn generated(&self, path: &RepoPath) -> Result<bool, GateError>;
    /// Called before `bytes` of file content are buffered.
    async fn admit(&self, bytes: u64) -> Result<Admission, GateError>;
}

/// A gate with no attributes and no budget (local use).
pub struct NoGate;

#[async_trait]
impl Gate for NoGate {
    async fn generated(&self, _: &RepoPath) -> Result<bool, GateError> {
        Ok(false)
    }

    async fn admit(&self, _: u64) -> Result<Admission, GateError> {
        Ok(Admission::none())
    }
}

/// Classification failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Diff(#[from] diff::Error),
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error("more than {MAX_FILES} changed files")]
    TooManyFiles,
}

/// One file's class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileClass {
    pub path: RepoPath,
    pub class: ChangeClass,
}

/// The classes of every changed file and their maximum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classified {
    pub files: Vec<FileClass>,
    pub overall: ChangeClass,
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Classifies the changes from tree `a` to tree `b` (SPEC §8.10). Exact renames are found
    /// without reading content.
    pub async fn classify_trees(
        &self,
        a: TreeId,
        b: TreeId,
        rules: &Rules,
        gate: &dyn Gate,
    ) -> Result<Classified, Error> {
        let opts = DiffOptions {
            renames: Some(RenameOptions {
                threshold: 100,
                limit: 0,
                max_file_size: 0,
            }),
            ..DiffOptions::default()
        };
        let changes = self.diff_trees(a, b, None, &opts).await?;
        if changes.len() > MAX_FILES {
            return Err(Error::TooManyFiles);
        }
        let mut left = MAX_TOTAL_BYTES;
        let mut files = Vec::with_capacity(changes.len());
        let mut overall = ChangeClass::RenameOnly;
        for c in changes {
            let mut content: Option<(bytes::Bytes, bytes::Bytes, Admission)> = None;
            if wants_content(&c)
                && let (Some(o), Some(n)) = (&c.old, &c.new)
                && o.size + n.size <= left
            {
                left -= o.size + n.size;
                let guard = gate.admit(o.size + n.size).await?;
                let old = self.read_file(FileId::from_bytes(o.id), o.size).await?;
                let new = self.read_file(FileId::from_bytes(n.id), n.size).await?;
                if old.len() as u64 <= MAX_CONTENT_BYTES && new.len() as u64 <= MAX_CONTENT_BYTES {
                    content = Some((old, new, guard));
                }
            }
            let bytes = content.as_ref().map(|(o, n, _)| (&o[..], &n[..]));
            let mut class = classify_change(rules, &c, false, bytes);
            if class == ChangeClass::Logic && wants_generated(&c) && gate.generated(&c.path).await?
            {
                class = classify_change(rules, &c, true, bytes);
            }
            overall = overall.max(class);
            files.push(FileClass {
                path: c.path,
                class,
            });
        }
        Ok(Classified { files, overall })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use asc_core::content::{Body, ingest};
    use asc_core::object::Change;
    use asc_core::path::RefName;
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;
    use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, RepoKey};
    use proptest::prelude::*;

    use super::ChangeClass::{self, *};
    use super::*;

    fn c(path: &str, old: &str, new: &str) -> ChangeClass {
        classify_content(path.as_bytes(), old.as_bytes(), new.as_bytes())
    }

    // ---- tables, one per class per language ----

    #[test]
    fn rust_table() {
        let f = "a.rs";
        assert_eq!(
            c(f, "fn a(){let x=1;}", "fn a() {\n    let x = 1;\n}\n"),
            WhitespaceOnly
        );
        assert_eq!(c(f, "let x = 1;", "let x = 1; // one"), CommentOnly);
        assert_eq!(
            c(
                f,
                "let x = 1;",
                "/// doc\nlet x = /* a /* nested */ b */ 1;"
            ),
            CommentOnly
        );
        assert_eq!(c(f, "let x = 1;", "let x = 2;"), Logic);
        // Whitespace that merges or splits tokens is a change.
        assert_eq!(c(f, "let ab = 1;", "let a b = 1;"), Logic);
        assert_eq!(c(f, "a && b", "a & &b"), Logic);
        assert_eq!(c(f, "a&&b", "a && b"), WhitespaceOnly);
        // Strings and raw strings are not comments.
        assert_eq!(c(f, "let s = \"a//b\";", "let s = \"a//c\";"), Logic);
        assert_eq!(c(f, "let s = r#\"/* x\"#;", "let s = r#\"/* y\"#;"), Logic);
        assert_eq!(c(f, "let s = \"/*\"; f();", "let s = \"/*\"; g();"), Logic);
        assert_eq!(c(f, "let s = \"//\";", "let s = \"//\"; // c"), CommentOnly);
        // Lifetimes and chars.
        assert_eq!(
            c(f, "fn f<'a>(x: &'a str) {}", "fn f<'a>(x: &'a str) {} // c"),
            CommentOnly
        );
        assert_eq!(c(f, "let c = '/'; f();", "let c = '/'; g();"), Logic);
        // Unterminated things are refused.
        assert_eq!(c(f, "let x = 1; /* a", "let x = 2; /* a"), Logic);
        assert_eq!(c(f, "let x = \"a", "let x = \"a "), Logic);
    }

    #[test]
    fn ts_js_table() {
        for f in ["a.ts", "a.js"] {
            assert_eq!(c(f, "let x = 1;", "let   x =\t1;"), WhitespaceOnly);
            assert_eq!(c(f, "let x = 1;", "// c\nlet x = 1; /* d */"), CommentOnly);
            assert_eq!(c(f, "let x = 1;", "let x = 2;"), Logic);
            // A line break is a token (automatic semicolons).
            assert_eq!(c(f, "return\nx", "return x"), Logic);
            assert_eq!(c(f, "let s = 'a//b';", "let s = 'a//c';"), Logic);
            assert_eq!(c(f, "let s = `a//${x}b`;", "let s = `a//${x}c`;"), Logic);
            // Regexes may hide comment markers: refused.
            assert_eq!(c(f, "x = /[//]/.test(y);", "x = /[//]/.test(z);"), Logic);
            assert_eq!(c(f, "let y = a / b;", "let y = a / b; // c"), CommentOnly);
            // Tool directives are not droppable comments.
            assert_eq!(c(f, "f();", "// @ts-ignore\nf();"), Logic);
            assert_eq!(c(f, "f();", "//# sourceMappingURL=x.map\nf();"), Logic);
        }
        for f in ["a.tsx", "a.jsx"] {
            assert_eq!(
                c(
                    f,
                    "<a href=\"x\">http://y</a>",
                    "<a href=\"x\">http://z</a>"
                ),
                Logic
            );
            assert_eq!(c(f, "let x = 1;", "let x = 1; // c"), CommentOnly);
        }
    }

    #[test]
    fn go_c_cs_java_table() {
        assert_eq!(c("a.go", "x := 1\n", "x  :=  1 // c\n"), CommentOnly);
        assert_eq!(c("a.go", "x := 1\ny := 2", "x := 1 y := 2"), Logic);
        assert_eq!(c("a.go", "s := `a//b`", "s := `a//c`"), Logic);
        assert_eq!(c("a.go", "f()", "//go:generate foo\nf()"), Logic);
        assert_eq!(c("a.c", "int a;", "int a; /* c */"), CommentOnly);
        assert_eq!(
            c(
                "a.c",
                "#include <a//b>\nint a;",
                "#include <a//b>\nint a; // c"
            ),
            CommentOnly
        );
        assert_eq!(
            c("a.c", "#include <a//b>\nint a;", "#include <a//c>\nint a;"),
            Logic
        );
        // A comment continues over a backslash-newline; the second line is comment text.
        assert_eq!(
            c(
                "a.c",
                "int a; // c \\\nint b;\nint d;",
                "int a; // c \\\nint z;\nint d;"
            ),
            CommentOnly
        );
        assert_eq!(c("a.c", "#define X 1\nint a;", "#define X 1 int a;"), Logic);
        assert_eq!(
            c("a.cpp", "int a = 1'000;", "int a = 1'000; // c"),
            CommentOnly
        );
        assert_eq!(
            c("a.cpp", "auto s = R\"(//)\";", "auto s = R\"(//x)\";"),
            Logic
        );
        assert_eq!(c("a.cs", "var s = @\"a//b\";", "var s = @\"a//c\";"), Logic);
        assert_eq!(
            c(
                "a.cs",
                "var s = @\"a\"\"//b\";",
                "var s = @\"a\"\"//b\"; // c"
            ),
            CommentOnly
        );
        assert_eq!(
            c("a.cs", "var s = $\"{x}//\";", "var s = $\"{y}//\";"),
            Logic
        );
        assert_eq!(c("a.java", "int a = 1;", "int a = 1; /* c */"), CommentOnly);
        assert_eq!(
            c("a.java", "String s = \"//\";", "String s = \"//x\";"),
            Logic
        );
        assert_eq!(c("a.java", "int a;", "int \\u0061;"), Logic);
    }

    #[test]
    fn python_table() {
        let f = "a.py";
        assert_eq!(c(f, "x = 1\n", "x = 1   \n"), WhitespaceOnly);
        // Only trailing whitespace is ignored.
        assert_eq!(c(f, "x = 1\n", "x  =  1\n"), Logic);
        assert_eq!(c(f, "if a:\n    x = 1\n", "if a:\n  x = 1\n"), Logic);
        assert_eq!(c(f, "x = 1\n", "x = 1  # one\n"), CommentOnly);
        assert_eq!(
            c(f, "x = 1\ny = 2\n", "x = 1\n# note\ny = 2\n"),
            CommentOnly
        );
        assert_eq!(c(f, "x = \"# no\"\n", "x = \"# yes\"\n"), Logic);
        assert_eq!(c(f, "x = '''a\n# b'''\n", "x = '''a\n# c'''\n"), Logic);
        // Trailing blanks inside a triple-quoted string are content.
        assert_eq!(c(f, "x = '''a\nb'''\n", "x = '''a  \nb'''\n"), Logic);
        // Nested quotes in an f-string expression are refused.
        assert_eq!(
            c(f, "x = f\"{d[\"#\"]}\"\n", "x = f\"{d[\"#\"]}\"  # c\n"),
            Logic
        );
        assert_eq!(c(f, "x = f\"{a}#\"\n", "x = f\"{a}#\"  # c\n"), CommentOnly);
        // Shebang and tool comments are directives.
        assert_eq!(
            c(
                f,
                "#!/usr/bin/python\nx = 1\n",
                "#!/usr/bin/python3\nx = 1\n"
            ),
            Logic
        );
        assert_eq!(c(f, "x = 1\n", "x = 1  # type: ignore\n"), Logic);
    }

    #[test]
    fn yaml_sh_toml_table() {
        assert_eq!(c("a.yml", "a: 1\n", "a: 1  \n"), WhitespaceOnly);
        assert_eq!(c("a.yaml", "a: 1\n", "# c\na: 1 # d\n"), CommentOnly);
        assert_eq!(c("a.yaml", "a: \"x # y\"\n", "a: \"x # z\"\n"), Logic);
        assert_eq!(c("a.yaml", "a: |\n  # x\n", "a: |\n  # y\n"), Logic);
        assert_eq!(c("a.yaml", "a: 1\n", "a:  1\n"), Logic);
        assert_eq!(c("a.sh", "echo $# a\n", "echo $# a # c\n"), CommentOnly);
        assert_eq!(c("a.sh", "echo a#b\n", "echo a#c\n"), Logic);
        assert_eq!(c("a.sh", "echo \"#\"\n", "echo \"#x\"\n"), Logic);
        assert_eq!(
            c("a.sh", "cat <<EOF\n# x\nEOF\n", "cat <<EOF\n# y\nEOF\n"),
            Logic
        );
        assert_eq!(
            c(
                "a.sh",
                "echo \"$(echo \"a # b\")\"\n",
                "echo \"$(echo \"a # c\")\"\n"
            ),
            Logic
        );
        assert_eq!(c("a.sh", "#!/bin/sh\nx\n", "#!/bin/bash\nx\n"), Logic);
        assert_eq!(c("a.sh", "a \\\n  b\n", "a \\ \n  b\n"), Logic);
        assert_eq!(c("a.toml", "a = 1\n", "a = 1 # c\n"), CommentOnly);
        assert_eq!(c("a.toml", "a = \"#x\"\n", "a = \"#y\"\n"), Logic);
        assert_eq!(
            c("a.toml", "a = '''\n# x\n'''\n", "a = '''\n# y\n'''\n"),
            Logic
        );
        assert_eq!(c("a.toml", "a = 1\n", "a = 1  \n"), WhitespaceOnly);
    }

    #[test]
    fn markup_and_make_table() {
        assert_eq!(c("a.md", "text\n", "text <!-- c -->\n"), CommentOnly);
        assert_eq!(c("a.md", "text\n", "text \n"), WhitespaceOnly);
        // Two trailing spaces are a hard line break.
        assert_eq!(c("a.md", "text\n", "text  \n"), Logic);
        // A comment marker in a code block is text.
        assert_eq!(
            c("a.md", "```\n<!-- a -->\n```\n", "```\n<!-- b -->\n```\n"),
            Logic
        );
        assert_eq!(
            c("a.html", "<p>x</p>\n", "<p>x</p><!-- c -->\n"),
            CommentOnly
        );
        assert_eq!(
            c(
                "a.html",
                "<a t=\"<!-- x\">y</a>-->",
                "<a t=\"<!-- z\">y</a>-->"
            ),
            Logic
        );
        assert_eq!(
            c("a.html", "<script>a</script>\n", "<script>b</script>\n"),
            Logic
        );
        assert_eq!(
            c("Makefile", "all:\n\tcc a.c\n", "all:\n\tcc a.c  \n"),
            WhitespaceOnly
        );
        assert_eq!(c("Makefile", "A = x\n", "A = x \n"), Logic);
        assert_eq!(
            c("Makefile", "all:\n\tcc a.c\n", "# c\nall:\n\tcc a.c\n"),
            Logic
        );
    }

    #[test]
    fn refusals_are_logic() {
        assert_eq!(c("a.txt", "a", "a "), Logic);
        assert_eq!(c("noext", "a", "a "), Logic);
        assert_eq!(c("a.rs", "a\0", "a\0 "), Logic);
        assert_eq!(classify_content(b"a.rs", b"\xff", b"\xff "), Logic);
        let big = "a ".repeat(MAX_CONTENT_BYTES as usize / 2 + 1);
        assert_eq!(c("a.rs", &big, &format!("{big} ")), Logic);
        assert_eq!(c("a.rs", "x", "x"), RenameOnly);
    }

    // ---- fail-closed regressions: an under-classified change bypasses K17 approvals ----

    #[test]
    fn go_raw_string_has_no_escapes() {
        assert_eq!(
            c(
                "a.go",
                "a := `\\`\nb := `\n// c\n`\nx := 1 // `",
                "a := `\\`\nb := `\n// d\n`\nx := 1 // `"
            ),
            Logic
        );
    }

    #[test]
    fn go_cgo_preamble_comment_is_code() {
        assert_eq!(
            c(
                "a.go",
                "/*\nint x = 1;\n*/\nimport \"C\"\n",
                "/*\nint x = 2;\n*/\nimport \"C\"\n"
            ),
            Logic
        );
    }

    #[test]
    fn java_unicode_escape_ends_comment() {
        assert_eq!(
            c(
                "a.java",
                "f(); // c \\u000a g();\n",
                "f(); // c \\u000a h();\n"
            ),
            Logic
        );
    }

    #[test]
    fn lone_cr_and_unicode_breaks_end_line_comments() {
        for f in ["a.java", "a.cs", "a.js", "a.ts", "a.c", "a.go", "a.rs"] {
            assert_eq!(c(f, "f(); // c\rg();\n", "f(); // c\rh();\n"), Logic, "{f}");
        }
        for f in ["a.js", "a.cs", "a.java"] {
            assert_eq!(
                c(f, "f(); // c\u{2028}g();\n", "f(); // c\u{2028}h();\n"),
                Logic,
                "{f}"
            );
        }
        assert_eq!(c("a.py", "f() # c\rg()\n", "f() # c\rh()\n"), Logic);
        assert_eq!(c("a.yaml", "a: 1 # c\rb: 2\n", "a: 1 # c\rb: 3\n"), Logic);
    }

    #[test]
    fn c_block_comment_splice_and_trigraph_close_comment() {
        for f in ["a.c", "a.cpp"] {
            assert_eq!(
                c(f, "/* c *\\\n/ f(); /* z */", "/* c *\\\n/ g(); /* z */"),
                Logic,
                "{f}"
            );
            assert_eq!(
                c(f, "/* c *\\ \n/ f(); /* z */", "/* c *\\ \n/ g(); /* z */"),
                Logic,
                "{f}"
            );
            assert_eq!(
                c(f, "/* c *??/\n/ f(); /* z */", "/* c *??/\n/ g(); /* z */"),
                Logic,
                "{f}"
            );
        }
    }

    #[test]
    fn js_slash_after_statement_paren_or_default_is_regex() {
        assert_eq!(
            c(
                "a.js",
                "if (x) /[/*]/.test(s); f(); /* z */",
                "if (x) /[/*]/.test(s); g(); /* z */"
            ),
            Logic
        );
        assert_eq!(
            c(
                "a.js",
                "export default /[/*]/; f(); /* z */",
                "export default /[/*]/; g(); /* z */"
            ),
            Logic
        );
        // A parenthesised division stays classifiable.
        assert_eq!(
            c("a.js", "y = (a + b) / 2;", "y = (a + b) / 2; // c"),
            CommentOnly
        );
    }

    #[test]
    fn sh_escaped_char_before_hash_is_not_a_comment() {
        assert_eq!(c("a.sh", "echo a\\ #b\n", "echo a\\ #c\n"), Logic);
        assert_eq!(c("a.sh", "echo \\;#b\n", "echo \\;#c\n"), Logic);
    }

    #[test]
    fn sh_continuation_then_hash_is_not_a_comment() {
        assert_eq!(c("a.sh", "echo a\\\n# b\n", "echo a\\\n# c\n"), Logic);
    }

    #[test]
    fn sh_parameter_expansion_hash_is_not_a_comment() {
        assert_eq!(c("a.sh", "echo ${x/ #/y}\n", "echo ${x/ #/z}\n"), Logic);
        assert_eq!(c("a.sh", "echo ${x}\n", "echo ${x} # c\n"), CommentOnly);
    }

    #[test]
    fn py_template_and_backslash_brace_strings() {
        assert_eq!(
            c(
                "a.py",
                "x = t\"{ \"a # b\" }\"\n",
                "x = t\"{ \"a # c\" }\"\n"
            ),
            Logic
        );
        assert_eq!(
            c(
                "a.py",
                "x = f\"\\{ \"a # b\" }\"\n",
                "x = f\"\\{ \"a # c\" }\"\n"
            ),
            Logic
        );
    }

    #[test]
    fn html_comment_forms_that_end_early() {
        assert_eq!(
            c(
                "a.html",
                "<p>a</p><!-- c --!><b>x</b> -->\n",
                "<p>a</p><!-- c --!><b>y</b> -->\n"
            ),
            Logic
        );
        assert_eq!(
            c(
                "a.html",
                "<p>a</p><!--> <b>x</b> -->\n",
                "<p>a</p><!--> <b>y</b> -->\n"
            ),
            Logic
        );
        assert_eq!(
            c(
                "a.md",
                "<p>a</p><!---> <b>x</b> -->\n",
                "<p>a</p><!---> <b>y</b> -->\n"
            ),
            Logic
        );
    }

    #[test]
    fn html_rcdata_and_raw_text_elements_hold_no_comments() {
        assert_eq!(
            c(
                "a.html",
                "<title><!-- a --></title>\n",
                "<title><!-- b --></title>\n"
            ),
            Logic
        );
        assert_eq!(
            c(
                "a.html",
                "<noscript><!-- a --></noscript>\n",
                "<noscript><!-- b --></noscript>\n"
            ),
            Logic
        );
        // A title elsewhere does not stop comments being dropped.
        assert_eq!(
            c(
                "a.html",
                "<title>t</title><p>x</p>\n",
                "<title>t</title><p>x</p><!-- c -->\n"
            ),
            CommentOnly
        );
    }

    #[test]
    fn md_escaped_or_front_matter_comment_is_text() {
        assert_eq!(c("a.md", "\\<!-- a -->\n", "\\<!-- b -->\n"), Logic);
        assert_eq!(
            c(
                "a.md",
                "---\ntitle: x <!-- a -->\n---\n",
                "---\ntitle: x <!-- b -->\n---\n"
            ),
            Logic
        );
    }

    /// Comments that tools or compilers act on: adding one is `LOGIC`, never `COMMENT_ONLY`.
    #[test]
    fn behaviour_changing_comments_are_logic() {
        let js = [
            "/*#__PURE__*/",
            "/*@__PURE__*/",
            "//#__PURE__",
            "/*@__NO_SIDE_EFFECTS__*/",
            "//# sourceMappingURL=a.map",
            "//# sourceURL=a.js",
            "//@ sourceMappingURL=a.map",
            "// @ts-ignore",
            "// @ts-expect-error",
            "// @ts-nocheck",
            "/// <reference path=\"a\" />",
            "// eslint-disable-next-line x",
            "/* eslint-disable */",
            "// prettier-ignore",
            "// biome-ignore lint/x: y",
            "// deno-lint-ignore x",
            "// tslint:disable",
            "/** @jsx h */",
            "/** @jsxImportSource preact */",
            "/**\n * @jsxImportSource preact\n */",
            "/*! keep */",
            "/* x @license MIT */",
            "/* @preserve */",
            "// v8 ignore next",
        ];
        let go = [
            "//go:generate x",
            "//go:build linux",
            "// +build linux",
            "//nolint:x",
            "//line a.go:1",
            "//export F",
        ];
        let py = [
            "# type: ignore",
            "# noqa",
            "# noqa: E501",
            "# pragma: no cover",
            "# -*- coding: latin-1 -*-",
            "# coding: latin-1",
            "# ruff: noqa",
            "# nosec",
            "# fmt: off",
            "# pyright: basic",
            "# mypy: ignore-errors",
        ];
        let mut bad = Vec::new();
        let mut check = |f: &str, old: &str, new: String| {
            if c(f, old, &new) != Logic {
                bad.push(format!("{f}: {new:?}"));
            }
        };
        for d in js {
            for f in ["a.js", "a.ts"] {
                check(f, "f();\n", format!("{d}\nf();\n"));
            }
        }
        for d in go {
            check("a.go", "f()\n", format!("{d}\nf()\n"));
        }
        check("a.rs", "f();\n", "// rustfmt: skip\nf();\n".to_owned());
        check("a.cs", "int a;\n", "#nullable enable\nint a;\n".to_owned());
        check(
            "a.cs",
            "int a;\n",
            "#pragma warning disable\nint a;\n".to_owned(),
        );
        for d in py {
            check("a.py", "f()\n", format!("{d}\nf()\n"));
        }
        check("a.sh", "f\n", "# shellcheck disable=SC2086\nf\n".to_owned());
        check(
            "a.yaml",
            "a: 1\n",
            "# yaml-language-server: $schema=x\na: 1\n".to_owned(),
        );
        check(
            "a.md",
            "t\n",
            "<!-- markdownlint-disable -->\nt\n".to_owned(),
        );
        check(
            "a.html",
            "<p>x</p>\n",
            "<!-- prettier-ignore -->\n<p>x</p>\n".to_owned(),
        );
        assert!(bad.is_empty(), "classified below Logic: {bad:#?}");
        // Ordinary comments still drop.
        assert_eq!(c("a.js", "f();", "// note\n/* x */ f();"), CommentOnly);
        assert_eq!(c("a.py", "f()\n", "## note\nf()\n"), CommentOnly);
    }

    #[test]
    fn pure_annotation_is_not_droppable() {
        assert_eq!(c("a.js", "x = f();", "x = /*#__PURE__*/ f();"), Logic);
        assert_eq!(c("a.js", "x = /*#__PURE__*/ f();", "x = f();"), Logic);
    }

    #[test]
    fn directives_match_on_any_comment_line() {
        for d in [
            "/*\n eslint-disable */",
            "/*\n#__PURE__*/",
            "/**\n * @license MIT\n */",
            "/*\n\n  @ts-nocheck\n*/",
            "/*\n * x\n * prettier-ignore\n */",
            "/*!\n biome-ignore x */",
        ] {
            for f in ["a.js", "a.ts"] {
                assert_eq!(c(f, "f();\n", &format!("{d}\nf();\n")), Logic, "{f} {d:?}");
            }
        }
        assert_eq!(c("a.go", "f()\n", "/*\n//go:generate x\n*/\nf()\n"), Logic);
        assert_eq!(
            c(
                "a.html",
                "<p>x</p>\n",
                "<!--\n prettier-ignore\n-->\n<p>x</p>\n"
            ),
            Logic
        );
        assert_eq!(
            c(
                "a.html",
                "<p>x</p>\n",
                "<!--\n<!-- eslint-disable -->\n-->\n<p>x</p>\n"
            ),
            Logic
        );
        // An ordinary multi-line comment still drops.
        assert_eq!(
            c("a.js", "f();\n", "/*\n * Calls f.\n * Twice.\n */\nf();\n"),
            CommentOnly
        );
    }

    // Raw and verbatim forms the audit found sound; these pin them.
    #[test]
    fn raw_and_verbatim_forms_stay_sound() {
        // Rust: any number of `#`, byte raw strings, backslash is literal.
        assert_eq!(
            c(
                "a.rs",
                "let s = r##\"a\"# \"##; // c",
                "let s = r##\"a\"# \"##; // d"
            ),
            CommentOnly
        );
        assert_eq!(
            c(
                "a.rs",
                "let s = r##\"a\"# // x\"##;",
                "let s = r##\"a\"# // y\"##;"
            ),
            Logic
        );
        assert_eq!(
            c("a.rs", "let s = br\"\\\"; // c", "let s = br\"\\\"; // d"),
            CommentOnly
        );
        assert_eq!(c("a.rs", "let s = r#\"a", "let s = r#\"a "), Logic);
        // C#: verbatim has only `\"\"`, raw strings refused.
        assert_eq!(
            c("a.cs", "var s = @\"\\\"; // c", "var s = @\"\\\"; // d"),
            CommentOnly
        );
        assert_eq!(
            c(
                "a.cs",
                "var s = \"\"\"a // b\"\"\";",
                "var s = \"\"\"a // c\"\"\";"
            ),
            Logic
        );
        assert_eq!(
            c(
                "a.java",
                "s = \"\"\"\n// a\n\"\"\";",
                "s = \"\"\"\n// b\n\"\"\";"
            ),
            Logic
        );
        // Python: raw strings keep the backslash but still cannot end on it.
        assert_eq!(
            c("a.py", "x = r\"\\\"\" # c\n", "x = r\"\\\"\" # d\n"),
            CommentOnly
        );
        assert_eq!(c("a.py", "x = r'''a # b'''\n", "x = r'''a # c'''\n"), Logic);
        // JS: template text and nested holes.
        assert_eq!(
            c("a.js", "s = `${a} // ${b}`;", "s = `${a} // ${c}`;"),
            Logic
        );
        assert_eq!(
            c("a.js", "s = `${`${x}`}`;", "s = `${`${x}`}`; // c"),
            Logic
        );
        assert_eq!(
            c("a.js", "s = `a ${x} b`;", "s = `a ${x} b`; // c"),
            CommentOnly
        );
        // C++ raw strings with any prefix are refused.
        assert_eq!(
            c(
                "a.cpp",
                "auto s = u8R\"x(//)x\";",
                "auto s = u8R\"x(//)x\"; // c"
            ),
            Logic
        );
    }

    // ---- rules ----

    #[test]
    fn default_rules() {
        let r = Rules::defaults();
        let t = |p: &str| r.path_class(p.as_bytes());
        assert_eq!(t("README.md"), Some(Docs));
        assert_eq!(t("a/b/c.md"), Some(Docs));
        assert_eq!(t("docs/a/b.png"), Some(Docs));
        assert_eq!(t("src/tests/a.rs"), Some(Test));
        assert_eq!(t("src/a_test/b.rs"), Some(Test));
        assert_eq!(t("src/a.rs"), None);
        assert_eq!(t("Cargo.lock"), Some(Lockfile));
        assert_eq!(t("sub/Cargo.lock"), None);
        assert_eq!(t("a/b/packages.lock.json"), Some(Lockfile));
        // The riskier class wins.
        assert_eq!(t("docs/tests/a.md"), Some(Test));
        assert_eq!(t("docs\\a"), None);
        assert_eq!(r.path_class(b"docs/\xff"), None);
    }

    #[test]
    fn custom_and_bad_rules() {
        let docs = vec!["manual/**".to_owned()];
        let r = Rules::new(Some(&docs), Some(&[]), None).unwrap();
        assert_eq!(r.path_class(b"manual/a"), Some(Docs));
        assert_eq!(r.path_class(b"README.md"), None);
        assert_eq!(r.path_class(b"tests/a.rs"), None);
        assert_eq!(r.path_class(b"Cargo.lock"), Some(Lockfile));
        assert!(Rules::new(Some(&["[".to_owned()]), None, None).is_err());
        assert!(Rules::new(Some(&vec!["a".to_owned(); MAX_GLOBS + 1]), None, None).is_err());
        assert!(Rules::new(None, Some(&["a".repeat(MAX_GLOB_BYTES + 1)]), None).is_err());
        assert_eq!(Rules::none().path_class(b"README.md"), None);
    }

    // ---- trees ----

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

    type TestEngine = Engine<BucketObjects, NoRefs>;

    fn engine() -> TestEngine {
        Engine::new(
            Arc::new(BucketObjects::memory()),
            Arc::new(NoRefs),
            RepoKey(1),
        )
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    async fn put_file(e: &TestEngine, kind: Kind, content: &[u8]) -> Change {
        let got = ingest(std::io::Cursor::new(content)).unwrap();
        if let Body::Inline(b) = got.body {
            e.objects
                .put(&ObjKey::File(got.file), encode_stored(&b))
                .await
                .unwrap();
        }
        Change::write(kind, got.file, got.size, got.git, None).unwrap()
    }

    async fn tree_of(e: &TestEngine, files: &[(&str, Kind, &[u8])]) -> TreeId {
        let mut changes = Vec::new();
        for (path, kind, content) in files {
            changes.push((p(path), put_file(e, *kind, content).await));
        }
        e.apply(TestEngine::empty_tree_id(), &changes)
            .await
            .unwrap()
    }

    #[derive(Default)]
    struct TestGate {
        admitted: AtomicU64,
        refuse: bool,
    }

    #[async_trait]
    impl Gate for TestGate {
        async fn generated(&self, path: &RepoPath) -> Result<bool, GateError> {
            Ok(path.as_bytes().ends_with(b".gen.rs"))
        }

        async fn admit(&self, bytes: u64) -> Result<Admission, GateError> {
            if self.refuse {
                return Err(GateError::Exhausted);
            }
            self.admitted.fetch_add(bytes, Ordering::SeqCst);
            Ok(Admission::none())
        }
    }

    async fn run(a: &[(&str, Kind, &[u8])], b: &[(&str, Kind, &[u8])]) -> (Classified, u64) {
        let e = engine();
        let (ta, tb) = (tree_of(&e, a).await, tree_of(&e, b).await);
        let gate = TestGate::default();
        let got = e
            .classify_trees(ta, tb, &Rules::defaults(), &gate)
            .await
            .unwrap();
        (got, gate.admitted.load(Ordering::SeqCst))
    }

    fn classes(got: &Classified) -> Vec<(String, ChangeClass)> {
        got.files
            .iter()
            .map(|f| {
                (
                    String::from_utf8_lossy(f.path.as_bytes()).into_owned(),
                    f.class,
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn every_class_through_the_engine() {
        use Kind::{Exec, File, Symlink};
        let a: &[(&str, Kind, &[u8])] = &[
            ("moved.rs", File, b"fn moved() {}"),
            ("mode.sh", File, b"echo hi\n"),
            ("ws.rs", File, b"fn a(){}"),
            ("cm.rs", File, b"fn a() {}"),
            ("logic.rs", File, b"fn a() {}"),
            ("docs/guide.txt", File, b"one"),
            ("tests/t.rs", File, b"fn t() {}"),
            ("Cargo.lock", File, b"a"),
            ("gone.gen.rs", File, b"x"),
            ("link", Symlink, b"target"),
            ("sub/.gitattributes", File, b"a"),
        ];
        let b: &[(&str, Kind, &[u8])] = &[
            ("renamed/moved.rs", File, b"fn moved() {}"),
            ("mode.sh", Exec, b"echo hi\n"),
            ("ws.rs", File, b"fn a() {\n}"),
            ("cm.rs", File, b"fn a() {} // c"),
            ("logic.rs", File, b"fn b() {}"),
            ("docs/guide.txt", File, b"two"),
            ("tests/t.rs", File, b"fn t() { 1; }"),
            ("Cargo.lock", File, b"b"),
            ("link", Symlink, b"other"),
            ("new.gen.rs", File, b"y"),
            ("sub/.gitattributes", File, b"b"),
        ];
        let (got, admitted) = run(a, b).await;
        let m: std::collections::HashMap<_, _> = classes(&got).into_iter().collect();
        assert_eq!(m["renamed/moved.rs"], RenameOnly);
        assert_eq!(m["mode.sh"], ModeOnly);
        assert_eq!(m["ws.rs"], WhitespaceOnly);
        assert_eq!(m["cm.rs"], CommentOnly);
        assert_eq!(m["logic.rs"], Logic);
        assert_eq!(m["docs/guide.txt"], Docs);
        assert_eq!(m["tests/t.rs"], Test);
        assert_eq!(m["Cargo.lock"], Lockfile);
        assert_eq!(m["new.gen.rs"], Generated);
        assert_eq!(m["gone.gen.rs"], Generated);
        assert_eq!(m["link"], Logic);
        assert_eq!(m["sub/.gitattributes"], Logic);
        assert!(!m.contains_key("moved.rs"));
        assert_eq!(got.overall, Logic);
        assert!(admitted > 0);
    }

    #[tokio::test]
    async fn overall_is_the_maximum_and_empty_is_lowest() {
        use Kind::File;
        let (got, admitted) = run(
            &[("a.rs", File, b"fn a() {}"), ("README.md", File, b"x")],
            &[("a.rs", File, b"fn a() {} // c"), ("README.md", File, b"y")],
        )
        .await;
        assert_eq!(got.overall, Docs);
        assert_eq!(admitted, 9 + 14 + 1 + 1);
        let (none, _) = run(&[("a.rs", File, b"x")], &[("a.rs", File, b"x")]).await;
        assert!(none.files.is_empty());
        assert_eq!(none.overall, RenameOnly);
    }

    #[tokio::test]
    async fn budget_is_asked_before_buffering_and_failure_propagates() {
        use Kind::File;
        let e = engine();
        let ta = tree_of(&e, &[("a.rs", File, b"fn a() {}")]).await;
        let tb = tree_of(&e, &[("a.rs", File, b"fn a() {} // c")]).await;
        let gate = TestGate {
            refuse: true,
            ..TestGate::default()
        };
        let err = e
            .classify_trees(ta, tb, &Rules::defaults(), &gate)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Gate(GateError::Exhausted)));
    }

    #[tokio::test]
    async fn no_rules_means_only_content_decides() {
        use Kind::File;
        let e = engine();
        let ta = tree_of(&e, &[("README.md", File, b"a"), ("x.rs", File, b"a")]).await;
        let tb = tree_of(&e, &[("README.md", File, b"b"), ("x.rs", File, b"a // c")]).await;
        let got = e
            .classify_trees(ta, tb, &Rules::none(), &NoGate)
            .await
            .unwrap();
        let m: std::collections::HashMap<_, _> = classes(&got).into_iter().collect();
        assert_eq!(m["README.md"], Logic);
        assert_eq!(m["x.rs"], CommentOnly);
    }

    // ---- properties ----

    const RUST_POOL: &[&str] = &[
        "let",
        "fn",
        "x",
        "y1",
        "foo",
        "(",
        ")",
        "{",
        "}",
        "=",
        "+",
        "-",
        "*",
        "1",
        "42",
        ";",
        ",",
        "\"a//b\"",
        "\"s /* x */\"",
        "r#\"//\"#",
        "'c'",
        "&",
        "&&",
        "<",
        ">",
        "::",
        "->",
    ];
    const TS_POOL: &[&str] = &[
        "const", "let", "x", "y2", "foo", "(", ")", "=>", "=", "+", "1", "'a//b'", "\"q\"",
        "`t${x}`", "2", ";", ",", "{", "}",
    ];
    const IDENTS: &[&str] = &["x", "y1", "foo", "y2", "const", "let", "fn"];
    const GAPS: &[&str] = &[" ", "  ", "\t ", " /* c */ ", "/**/ ", " /* a // b */ "];

    fn is_ident(t: &str) -> bool {
        IDENTS.contains(&t)
    }

    fn join(toks: &[&str], gaps: &[usize]) -> String {
        let mut s = String::new();
        for (i, t) in toks.iter().enumerate() {
            if i > 0 {
                s.push_str(GAPS[gaps[i % gaps.len()] % GAPS.len()]);
            }
            s.push_str(t);
        }
        s
    }

    proptest! {
        #[test]
        fn rust_noise_never_exceeds_comment_only(
            idx in prop::collection::vec(0..RUST_POOL.len(), 1..40),
            gaps in prop::collection::vec(0usize..6, 1..40),
            lead in prop::sample::select(vec!["", "// c\n", "/// d\n", "/* e */ "]),
        ) {
            let toks: Vec<&str> = idx.iter().map(|&i| RUST_POOL[i]).collect();
            let base = toks.join(" ");
            let noisy = format!("{lead}{}", join(&toks, &gaps));
            prop_assert!(c("a.rs", &base, &noisy) <= CommentOnly);
            // Line comments may follow any token when a line break ends them.
            let with_line = toks.iter().map(|t| format!("{t} // c\n")).collect::<String>();
            prop_assert!(c("a.rs", &base, &with_line) <= CommentOnly);
        }

        #[test]
        fn rust_one_token_change_is_logic(
            idx in prop::collection::vec(0..RUST_POOL.len(), 1..40),
            at in 0usize..40,
        ) {
            let toks: Vec<&str> = idx.iter().map(|&i| RUST_POOL[i]).collect();
            let spots: Vec<usize> = (0..toks.len()).filter(|&i| is_ident(toks[i])).collect();
            prop_assume!(!spots.is_empty());
            let k = spots[at % spots.len()];
            let mut changed = toks.clone();
            changed[k] = "zzz9";
            prop_assert_eq!(c("a.rs", &toks.join(" "), &changed.join(" ")), Logic);
            let noisy = join(&changed, &[0, 3, 1]);
            prop_assert_eq!(c("a.rs", &toks.join(" "), &noisy), Logic);
        }

        #[test]
        fn ts_noise_never_exceeds_comment_only(
            idx in prop::collection::vec(0..TS_POOL.len(), 3..30),
            gaps in prop::collection::vec(0usize..6, 1..30),
            breaks in prop::collection::vec(any::<bool>(), 3..30),
            extra in prop::sample::select(vec!["", "// c\n", "/* x */\n", "\n\n", "  // y\n"]),
        ) {
            let toks: Vec<&str> = idx.iter().map(|&i| TS_POOL[i]).collect();
            let mut base = String::new();
            let mut noisy = String::new();
            for (i, t) in toks.iter().enumerate() {
                if i > 0 {
                    if breaks[i % breaks.len()] {
                        base.push('\n');
                        noisy.push('\n');
                        noisy.push_str(extra);
                    } else {
                        base.push(' ');
                        noisy.push_str(GAPS[gaps[i % gaps.len()] % GAPS.len()]);
                    }
                }
                base.push_str(t);
                noisy.push_str(t);
            }
            prop_assert!(c("a.ts", &base, &noisy) <= CommentOnly);
            prop_assert!(c("a.js", &base, &noisy) <= CommentOnly);
        }

        #[test]
        fn ts_one_token_change_is_logic(
            idx in prop::collection::vec(0..TS_POOL.len(), 1..30),
            at in 0usize..30,
        ) {
            let toks: Vec<&str> = idx.iter().map(|&i| TS_POOL[i]).collect();
            let spots: Vec<usize> = (0..toks.len()).filter(|&i| is_ident(toks[i])).collect();
            prop_assume!(!spots.is_empty());
            let mut changed = toks.clone();
            changed[spots[at % spots.len()]] = "zzz9";
            prop_assert_eq!(c("a.ts", &toks.join(" "), &changed.join(" ")), Logic);
        }
    }
}
