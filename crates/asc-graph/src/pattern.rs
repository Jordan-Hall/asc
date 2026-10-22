//! Input patterns (SPEC §12A.3) and the directory decision `decide` (SPEC §12A.6).

use asc_core::path::RepoPath;
use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

const PROJECT_ROOT: &str = "{projectRoot}";
const WORKSPACE_ROOT: &str = "{workspaceRoot}";

/// Why a pattern was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    #[error("empty input item")]
    Empty,
    #[error("invalid glob {pattern:?}: {reason}")]
    Glob { pattern: String, reason: String },
    #[error("'!' cannot apply to named input or ^ item {0:?}")]
    BadExclude(String),
}

/// One item of an input list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputItem {
    /// A bare identifier: another named input.
    Named(String),
    /// `^name`: the same named input of each dependency.
    Dep(String),
    /// A glob template.
    Include(String),
    /// `!glob`: exclusions apply after all inclusions.
    Exclude(String),
}

impl InputItem {
    /// Parses one item: `name`, `^name`, `!glob` or `glob`.
    pub fn parse(s: &str) -> Result<Self, PatternError> {
        let ident = |t: &str| {
            !t.is_empty()
                && t.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        };
        if s.is_empty() || s == "!" || s == "^" {
            return Err(PatternError::Empty);
        }
        if let Some(rest) = s.strip_prefix('!') {
            if ident(rest) || rest.starts_with('^') {
                return Err(PatternError::BadExclude(s.to_owned()));
            }
            check_glob(rest)?;
            return Ok(Self::Exclude(rest.to_owned()));
        }
        if let Some(rest) = s.strip_prefix('^') {
            if !ident(rest) {
                return Err(PatternError::Glob {
                    pattern: s.to_owned(),
                    reason: "^ needs a named input".into(),
                });
            }
            return Ok(Self::Dep(rest.to_owned()));
        }
        if ident(s) {
            return Ok(Self::Named(s.to_owned()));
        }
        check_glob(s)?;
        Ok(Self::Include(s.to_owned()))
    }
}

fn build(glob: &str) -> Result<globset::Glob, PatternError> {
    GlobBuilder::new(glob)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|e| PatternError::Glob {
            pattern: glob.to_owned(),
            reason: e.kind().to_string(),
        })
}

fn check_glob(template: &str) -> Result<(), PatternError> {
    build(&expand(template, None)).map(|_| ())
}

fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// Expands `{projectRoot}` and `{workspaceRoot}`; `None` is the repository root.
pub fn expand(template: &str, root: Option<&RepoPath>) -> String {
    let root = root.map(|r| escape(&r.to_string())).unwrap_or_default();
    let s = template
        .replace(PROJECT_ROOT, &root)
        .replace(WORKSPACE_ROOT, "");
    s.trim_start_matches('/').to_owned()
}

/// One compiled glob with the facts `decide` needs.
#[derive(Clone, Debug)]
struct Pat {
    matcher: GlobMatcher,
    /// Leading components without glob syntax.
    prefix: Vec<Vec<u8>>,
    /// Components after the prefix are exactly `**`.
    tail_all: bool,
    /// Number of components when every path it matches has the same depth.
    fixed_depth: Option<usize>,
    /// The glob has no wildcard at all.
    literal: bool,
}

impl Pat {
    fn new(expanded: &str) -> Result<Self, PatternError> {
        let matcher = build(expanded)?.compile_matcher();
        let is_meta = |c: &str| c.contains(['*', '?', '[', ']', '{', '}', '\\']);
        let parts: Vec<&str> = expanded.split('/').collect();
        let n = parts.iter().position(|c| is_meta(c)).unwrap_or(parts.len());
        let rest = &parts[n..];
        let complex = expanded.contains(['{', '\\']) || expanded.contains("**");
        Ok(Self {
            matcher,
            prefix: parts[..n].iter().map(|c| c.as_bytes().to_vec()).collect(),
            tail_all: rest == ["**"],
            fixed_depth: (!complex).then_some(parts.len()),
            literal: rest.is_empty(),
        })
    }

    /// True when `dir` is the prefix's ancestor-or-equal and the glob is `prefix/**`.
    fn covers(&self, dir: &[&[u8]]) -> bool {
        self.tail_all && is_prefix(&self.prefix, dir)
    }

    /// May match a path strictly inside `dir` (conservative).
    fn may_match_under(&self, dir: &[&[u8]]) -> bool {
        if self.fixed_depth.is_some_and(|d| d <= dir.len()) {
            return false;
        }
        if self.literal {
            return is_prefix(dir, &self.prefix);
        }
        let k = self.prefix.len().min(dir.len());
        self.prefix[..k]
            .iter()
            .zip(dir)
            .all(|(a, b)| a.as_slice() == *b)
    }
}

fn is_prefix<A: AsRef<[u8]>, B: AsRef<[u8]>>(a: &[A], b: &[B]) -> bool {
    a.len() <= b.len() && a.iter().zip(b).all(|(x, y)| x.as_ref() == y.as_ref())
}

fn components(dir: Option<&RepoPath>) -> Vec<&[u8]> {
    dir.map(|d| d.components().collect()).unwrap_or_default()
}

/// A compiled set of include and exclude globs for one project and target.
#[derive(Clone, Debug, Default)]
pub struct PatternSet {
    includes: Vec<Pat>,
    excludes: Vec<Pat>,
}

/// What a directory contributes to a project's `files_digest` (SPEC §12A.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The whole subtree is an input: contribute its tree id.
    All,
    /// Nothing under it is an input.
    None,
    /// Recurse into its entries.
    Partial,
}

impl PatternSet {
    /// Compiles templates for a project at `root` (`None` = repository root).
    pub fn new<'a>(
        includes: impl IntoIterator<Item = &'a str>,
        excludes: impl IntoIterator<Item = &'a str>,
        root: Option<&RepoPath>,
    ) -> Result<Self, PatternError> {
        let compile = |t: &str| Pat::new(&expand(t, root));
        Ok(Self {
            includes: includes
                .into_iter()
                .map(compile)
                .collect::<Result<_, _>>()?,
            excludes: excludes
                .into_iter()
                .map(compile)
                .collect::<Result<_, _>>()?,
        })
    }

    /// True when a file path is selected: some include matches and no exclude does.
    pub fn matches(&self, path: &RepoPath) -> bool {
        let p = std::path::Path::new(std::str::from_utf8(path.as_bytes()).unwrap_or(""));
        let hit = |ps: &[Pat]| ps.iter().any(|x| x.matcher.is_match(p));
        hit(&self.includes) && !hit(&self.excludes)
    }

    /// SPEC §12A.6 `decide(S, D)`.
    ///
    /// `nested_roots` are the roots of other projects, `ignore` the `ignore`
    /// patterns of the config; either one reaching under `dir` rules out `All`.
    pub fn decide(
        &self,
        dir: Option<&RepoPath>,
        nested_roots: &[RepoPath],
        ignore: &IgnoreSet,
    ) -> Decision {
        let d = components(dir);
        if !self.includes.iter().any(|p| p.may_match_under(&d)) {
            return Decision::None;
        }
        let blocked = self.excludes.iter().any(|p| p.may_match_under(&d))
            || ignore.0.iter().any(|p| p.may_match_under(&d))
            || nested_roots.iter().any(|r| {
                let r: Vec<&[u8]> = r.components().collect();
                is_prefix(&d, &r) || is_prefix(&r, &d)
            });
        if !blocked && self.includes.iter().any(|p| p.covers(&d)) {
            Decision::All
        } else {
            Decision::Partial
        }
    }
}

/// The config `ignore` globs, compiled.
#[derive(Clone, Debug, Default)]
pub struct IgnoreSet(Vec<Pat>);

impl IgnoreSet {
    pub fn new<'a>(globs: impl IntoIterator<Item = &'a str>) -> Result<Self, PatternError> {
        Ok(Self(
            globs
                .into_iter()
                .map(|g| Pat::new(&expand(g, None)))
                .collect::<Result<_, _>>()?,
        ))
    }

    /// True when `path` matches an ignore glob.
    pub fn is_ignored(&self, path: &RepoPath) -> bool {
        let p = std::path::Path::new(std::str::from_utf8(path.as_bytes()).unwrap_or(""));
        self.0.iter().any(|x| x.matcher.is_match(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn set(inc: &[&str], exc: &[&str], root: Option<&str>) -> PatternSet {
        let root = root.map(rp);
        PatternSet::new(inc.iter().copied(), exc.iter().copied(), root.as_ref()).unwrap()
    }

    #[test]
    fn item_grammar() {
        use InputItem::*;
        assert_eq!(
            InputItem::parse("default").unwrap(),
            Named("default".into())
        );
        assert_eq!(
            InputItem::parse("^production").unwrap(),
            Dep("production".into())
        );
        assert_eq!(
            InputItem::parse("{projectRoot}/**").unwrap(),
            Include("{projectRoot}/**".into())
        );
        assert_eq!(
            InputItem::parse("!{projectRoot}/tests/**").unwrap(),
            Exclude("{projectRoot}/tests/**".into())
        );
        assert_eq!(
            InputItem::parse("src/main.rs").unwrap(),
            Include("src/main.rs".into())
        );
        assert!(matches!(
            InputItem::parse("!prod"),
            Err(PatternError::BadExclude(_))
        ));
        assert!(matches!(
            InputItem::parse("a/[b"),
            Err(PatternError::Glob { .. })
        ));
        assert_eq!(InputItem::parse(""), Err(PatternError::Empty));
    }

    #[test]
    fn expansion() {
        let r = rp("apps/web");
        assert_eq!(expand("{projectRoot}/**", Some(&r)), "apps/web/**");
        assert_eq!(expand("{projectRoot}/**", None), "**");
        assert_eq!(expand("{workspaceRoot}/schemas/**", Some(&r)), "schemas/**");
        assert_eq!(
            expand("{projectRoot}/*.md", Some(&rp("a[1]"))),
            "a\\[1\\]/*.md"
        );
    }

    #[test]
    fn file_matching() {
        let s = set(
            &["{projectRoot}/**"],
            &["{projectRoot}/**/*.md", "{projectRoot}/tests/**"],
            Some("pkg"),
        );
        assert!(s.matches(&rp("pkg/src/lib.rs")));
        assert!(!s.matches(&rp("pkg/README.md")));
        assert!(!s.matches(&rp("pkg/tests/a.rs")));
        assert!(!s.matches(&rp("other/src/lib.rs")));
        let flat = set(&["*.md"], &[], None);
        assert!(flat.matches(&rp("a.md")));
        assert!(!flat.matches(&rp("d/a.md")));
    }

    #[test]
    fn decide_all_none_partial() {
        let none = IgnoreSet::default();
        let s = set(&["{projectRoot}/**"], &[], Some("apps/web"));
        assert_eq!(s.decide(Some(&rp("apps/web")), &[], &none), Decision::All);
        assert_eq!(
            s.decide(Some(&rp("apps/web/src")), &[], &none),
            Decision::All
        );
        assert_eq!(s.decide(Some(&rp("apps")), &[], &none), Decision::Partial);
        assert_eq!(s.decide(None, &[], &none), Decision::Partial);
        assert_eq!(s.decide(Some(&rp("apps/api")), &[], &none), Decision::None);
        assert_eq!(s.decide(Some(&rp("libs")), &[], &none), Decision::None);
    }

    #[test]
    fn decide_excludes_block_all() {
        let none = IgnoreSet::default();
        let s = set(
            &["{projectRoot}/**"],
            &["{projectRoot}/**/*.md", "{projectRoot}/tests/**"],
            Some("pkg"),
        );
        assert_eq!(s.decide(Some(&rp("pkg")), &[], &none), Decision::Partial);
        assert_eq!(
            s.decide(Some(&rp("pkg/tests")), &[], &none),
            Decision::Partial
        );
        // `**/*.md` can match anywhere, so even src is partial.
        assert_eq!(
            s.decide(Some(&rp("pkg/src")), &[], &none),
            Decision::Partial
        );
        let tests_only = set(
            &["{projectRoot}/**"],
            &["{projectRoot}/tests/**"],
            Some("pkg"),
        );
        assert_eq!(
            tests_only.decide(Some(&rp("pkg/src")), &[], &none),
            Decision::All
        );
        assert_eq!(
            tests_only.decide(Some(&rp("pkg/tests")), &[], &none),
            Decision::Partial
        );
        // A fixed-depth exclude cannot reach below a shallower directory.
        let top_md = set(&["{projectRoot}/**"], &["{projectRoot}/*.md"], Some("pkg"));
        assert_eq!(
            top_md.decide(Some(&rp("pkg/src")), &[], &none),
            Decision::All
        );
        assert_eq!(
            top_md.decide(Some(&rp("pkg")), &[], &none),
            Decision::Partial
        );
    }

    #[test]
    fn decide_nested_roots_and_ignore() {
        let s = set(&["{projectRoot}/**"], &[], Some("a"));
        let none = IgnoreSet::default();
        let nested = [rp("a/b/inner")];
        assert_eq!(s.decide(Some(&rp("a")), &nested, &none), Decision::Partial);
        assert_eq!(
            s.decide(Some(&rp("a/b")), &nested, &none),
            Decision::Partial
        );
        assert_eq!(
            s.decide(Some(&rp("a/b/inner")), &nested, &none),
            Decision::Partial
        );
        assert_eq!(s.decide(Some(&rp("a/c")), &nested, &none), Decision::All);
        let ig = IgnoreSet::new(["**/fixtures/**", "examples/**"]).unwrap();
        assert_eq!(s.decide(Some(&rp("a/c")), &[], &ig), Decision::Partial);
        let ig2 = IgnoreSet::new(["examples/**"]).unwrap();
        assert_eq!(s.decide(Some(&rp("a/c")), &[], &ig2), Decision::All);
        assert!(ig.is_ignored(&rp("x/fixtures/y.rs")));
        assert!(!ig.is_ignored(&rp("x/src/y.rs")));
    }

    #[test]
    fn decide_literal_and_star_prefixes() {
        let none = IgnoreSet::default();
        let s = set(
            &[
                "{workspaceRoot}/schemas/**",
                "docker-compose*.yml",
                "packages/*/package.json",
            ],
            &[],
            Some("pkg"),
        );
        assert_eq!(s.decide(Some(&rp("schemas")), &[], &none), Decision::All);
        assert_eq!(s.decide(Some(&rp("schemas/v1")), &[], &none), Decision::All);
        assert_eq!(
            s.decide(Some(&rp("packages")), &[], &none),
            Decision::Partial
        );
        assert_eq!(
            s.decide(Some(&rp("packages/a")), &[], &none),
            Decision::Partial
        );
        assert_eq!(
            s.decide(Some(&rp("packages/a/src")), &[], &none),
            Decision::None
        );
        assert_eq!(s.decide(Some(&rp("docs")), &[], &none), Decision::None);
        let single = set(&["src/main.rs"], &[], None);
        assert_eq!(
            single.decide(Some(&rp("src")), &[], &none),
            Decision::Partial
        );
        assert_eq!(
            single.decide(Some(&rp("src/deep")), &[], &none),
            Decision::None
        );
        let all = set(&["**"], &[], None);
        assert_eq!(all.decide(None, &[], &none), Decision::All);
        assert_eq!(all.decide(Some(&rp("x/y")), &[], &none), Decision::All);
    }
}
