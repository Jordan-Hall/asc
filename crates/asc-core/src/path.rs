//! Names, repository paths and ref names (SPEC §2.3, §2.7, §2.8).
//!
//! All types hold raw bytes: history may contain anything git allowed.

use std::fmt;

/// Why a name, path or ref name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("empty name")]
    Empty,
    #[error("name contains '/' or NUL")]
    BadByte,
    #[error("name is '.' or '..'")]
    Dot,
    #[error("name is '.git'")]
    DotGit,
    #[error("path has a leading, trailing or doubled '/'")]
    BadSlash,
    #[error("invalid ref name: {0}")]
    BadRef(&'static str),
}

/// One path component.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(Vec<u8>);

impl Name {
    /// Validates per SPEC §2.3.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, PathError> {
        let b = bytes.into();
        if b.is_empty() {
            return Err(PathError::Empty);
        }
        if b.iter().any(|&c| c == b'/' || c == 0) {
            return Err(PathError::BadByte);
        }
        if b == b"." || b == b".." {
            return Err(PathError::Dot);
        }
        if b.eq_ignore_ascii_case(b".git") {
            return Err(PathError::DotGit);
        }
        Ok(Self(b))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Deepest tree nesting asc walks, in directory levels below the root.
///
/// Matches git's `core.maxTreeDepth` default on Windows. [`RepoPath`] has no length limit of its
/// own, so this bound is what keeps every recursive tree walker (import, export, apply, diff)
/// within the stack. A tree deeper than this is rejected, not truncated.
pub const MAX_TREE_DEPTH: usize = 2048;

/// One or more [`Name`]s joined by `/`; ordered by raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(Vec<u8>);

impl RepoPath {
    /// Validates every component per SPEC §2.3.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, PathError> {
        let b = bytes.into();
        if b.is_empty() {
            return Err(PathError::Empty);
        }
        for part in b.split(|&c| c == b'/') {
            if part.is_empty() {
                return Err(PathError::BadSlash);
            }
            Name::new(part)?;
        }
        Ok(Self(b))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Components in order; never empty.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        self.0.split(|&c| c == b'/')
    }

    /// The containing directory, or `None` for a single-component path.
    pub fn parent(&self) -> Option<RepoPath> {
        let i = self.0.iter().rposition(|&c| c == b'/')?;
        Some(Self(self.0[..i].to_vec()))
    }

    /// The last component.
    pub fn file_name(&self) -> Name {
        let last = self.components().next_back().unwrap_or_default();
        Name(last.to_vec())
    }

    /// Appends one component.
    pub fn join(&self, name: &Name) -> RepoPath {
        let mut v = Vec::with_capacity(self.0.len() + 1 + name.0.len());
        v.extend_from_slice(&self.0);
        v.push(b'/');
        v.extend_from_slice(&name.0);
        Self(v)
    }

    /// True when `self` is strictly inside directory `dir`.
    pub fn starts_with_dir(&self, dir: &RepoPath) -> bool {
        self.0.len() > dir.0.len() && self.0.starts_with(&dir.0) && self.0[dir.0.len()] == b'/'
    }
}

impl From<Name> for RepoPath {
    fn from(n: Name) -> Self {
        Self(n.0)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.0))
    }
}

/// Unicode simple case folding of the UTF-8 decode (multi-char lowercase
/// mappings are left unchanged), or ASCII lowercase when not valid UTF-8.
pub fn casefold(name: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(name) {
        Ok(s) => {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                let mut lower = c.to_lowercase();
                match (lower.next(), lower.next()) {
                    (Some(l), None) => out.push(l),
                    _ => out.push(c),
                }
            }
            out.into_bytes()
        }
        Err(_) => name.to_ascii_lowercase(),
    }
}

/// Whether a name can exist on Windows (SPEC §2.3).
pub fn windows_valid(name: &Name) -> bool {
    let b = name.as_bytes();
    if matches!(b.last(), Some(b'.' | b' ')) {
        return false;
    }
    if b.iter().any(|&c| c < 0x20 || b"<>:\"\\|?*".contains(&c)) {
        return false;
    }
    // Windows ignores trailing spaces before the extension: "CON .txt" opens CON (git `is_valid_win32_path`).
    let stem = b.split(|&c| c == b'.').next().unwrap_or_default();
    let stem = &stem[..stem.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1)];
    !is_reserved_device(stem)
}

/// DOS device names, including `CONIN$`/`CONOUT$` and COM/LPT with superscript ¹²³.
fn is_reserved_device(stem: &[u8]) -> bool {
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
        .iter()
        .any(|r| stem.eq_ignore_ascii_case(r.as_bytes()))
    {
        return true;
    }
    if stem.len() < 4
        || !(stem[..3].eq_ignore_ascii_case(b"COM") || stem[..3].eq_ignore_ascii_case(b"LPT"))
    {
        return false;
    }
    matches!(&stem[3..], [b'1'..=b'9'] | [0xC2, 0xB9 | 0xB2 | 0xB3])
}

/// HFS+ ignorable code points (git `next_hfs_char`).
fn hfs_ignorable(c: char) -> bool {
    matches!(c as u32, 0x200C..=0x200F | 0x202A..=0x202E | 0x206A..=0x206F | 0xFEFF)
}

/// True when a single component names `name` (or its 8.3 short name `short`) under NTFS or HFS
/// rules: case, trailing dots/spaces, an alternate-stream suffix and ignorable code points.
fn is_alias(comp: &[u8], name: &str, short: Option<&str>) -> bool {
    // NTFS: ignore an alternate-stream suffix and trailing dots/spaces.
    let base = comp.split(|&c| c == b':').next().unwrap_or_default();
    let end = base
        .iter()
        .rposition(|&c| c != b'.' && c != b' ')
        .map_or(0, |i| i + 1);
    let base = &base[..end];
    if base.eq_ignore_ascii_case(name.as_bytes())
        || short.is_some_and(|s| base.eq_ignore_ascii_case(s.as_bytes()))
    {
        return true;
    }
    // HFS: drop ignorable code points, then compare.
    if let Ok(s) = std::str::from_utf8(comp) {
        let stripped: String = s.chars().filter(|&c| !hfs_ignorable(c)).collect();
        return stripped.eq_ignore_ascii_case(name);
    }
    false
}

/// True when a single component names `.git` under NTFS or HFS rules.
fn is_dotgit_alias(comp: &[u8]) -> bool {
    is_alias(comp, ".git", Some("git~1"))
}

/// True when `first/second` name `.asc/info` (the never-tracked local state) under the same
/// case and alias folding as `.git`, so no alias of it can be materialised or go unignored.
pub fn is_asc_info(first: &[u8], second: &[u8]) -> bool {
    is_alias(first, ".asc", Some("asc~1")) && is_alias(second, "info", None)
}

/// True when a single component names `.asc` (or an alias of it) under the same folding as
/// [`is_asc_info`].
fn is_asc_dir_alias(comp: &[u8]) -> bool {
    is_alias(comp, ".asc", Some("asc~1"))
}

/// Port of git's `verify_path` with `core.protectNTFS` and `core.protectHFS`.
/// Backslash is treated as a separator, as NTFS does.
pub fn checkout_safe(path: &RepoPath) -> bool {
    // git with core.protectNTFS rejects any backslash outright (CVE-2019-1353/1354); ':' and drive
    // letters are rejected on Windows by `windows_valid`, which materialisation also applies.
    let mut comps = path.components();
    match (comps.next(), comps.next()) {
        (Some(first), Some(second)) if is_asc_info(first, second) => return false,
        // A tracked entry that is itself `.asc` is a file or symlink, never the directory of
        // local state; as a link it would redirect `.asc/info/exclude` and `.asc/redirections`.
        (Some(only), None) if is_asc_dir_alias(only) => return false,
        _ => {}
    }
    path.components()
        .all(|c| !c.contains(&b'\\') && !is_dotgit_alias(c) && c != b".." && c != b".")
}

/// A ref name validated per `git check-ref-format`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RefName(Vec<u8>);

impl RefName {
    /// Requires at least two levels (`refs/heads/x`), like `git check-ref-format`.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, PathError> {
        Self::validate(bytes.into(), false)
    }

    /// Also accepts one-level names such as `HEAD` (`--allow-onelevel`).
    pub fn new_onelevel(bytes: impl Into<Vec<u8>>) -> Result<Self, PathError> {
        Self::validate(bytes.into(), true)
    }

    fn validate(b: Vec<u8>, onelevel: bool) -> Result<Self, PathError> {
        let bad = PathError::BadRef;
        if b.is_empty() || b == b"@" {
            return Err(bad("empty or '@'"));
        }
        if b.iter()
            .any(|&c| c < 0x20 || c == 0x7f || b" ~^:?*[\\".contains(&c))
        {
            return Err(bad("forbidden character"));
        }
        if b.windows(2).any(|w| w == b".." || w == b"@{") {
            return Err(bad("contains '..' or '@{'"));
        }
        if matches!(b.last(), Some(b'.' | b'/')) {
            return Err(bad("ends with '.' or '/'"));
        }
        for part in b.split(|&c| c == b'/') {
            if part.is_empty() {
                return Err(bad("empty component"));
            }
            if part[0] == b'.' || part.ends_with(b".lock") {
                return Err(bad("component starts with '.' or ends with '.lock'"));
            }
        }
        if !onelevel && !b.contains(&b'/') {
            return Err(bad("needs at least two levels"));
        }
        Ok(Self(b))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for RefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn name_rules() {
        for ok in ["a", "..a", "a.", "git~1", ".gitignore", ".gitx", "\u{e9}"] {
            assert!(Name::new(ok).is_ok(), "{ok}");
        }
        assert_eq!(Name::new(""), Err(PathError::Empty));
        assert_eq!(Name::new("a/b"), Err(PathError::BadByte));
        assert_eq!(Name::new("a\0"), Err(PathError::BadByte));
        assert_eq!(Name::new("."), Err(PathError::Dot));
        assert_eq!(Name::new(".."), Err(PathError::Dot));
        assert_eq!(Name::new(".GiT"), Err(PathError::DotGit));
    }

    #[test]
    fn path_rules() {
        for bad in [
            "", "/a", "a/", "a//b", "a/./b", "a/../b", "a/.git", ".git/x", "a/.GIT/b",
        ] {
            assert!(RepoPath::new(bad).is_err(), "{bad}");
        }
        let x = p("a/b/c");
        assert_eq!(x.components().collect::<Vec<_>>(), [b"a", b"b", b"c"]);
        assert_eq!(x.parent(), Some(p("a/b")));
        assert_eq!(p("a").parent(), None);
        assert_eq!(x.file_name(), Name::new("c").unwrap());
        assert_eq!(p("a").join(&Name::new("b").unwrap()), p("a/b"));
        assert!(x.starts_with_dir(&p("a")));
        assert!(x.starts_with_dir(&p("a/b")));
        assert!(!x.starts_with_dir(&x));
        assert!(!p("ab/c").starts_with_dir(&p("a")));
        assert_eq!(p("a/b") < p("a.b"), b"a/b" < b"a.b");
    }

    #[test]
    fn casefold_cases() {
        assert_eq!(casefold(b"FooBAR"), b"foobar");
        assert_eq!(casefold("\u{c9}".as_bytes()), "\u{e9}".as_bytes());
        assert_eq!(casefold("\u{130}".as_bytes()), "\u{130}".as_bytes()); // multi-char lower: unchanged
        assert_eq!(casefold(b"A\xffB"), b"a\xffb");
    }

    #[test]
    fn windows_table() {
        let ok = ["a", "con1", "COM10", "LPT0", "COM", "a.b", ".x", "\u{e9}"];
        let bad = [
            "a.",
            "a ",
            "a<",
            "a>",
            "a:b",
            "a\"",
            "a\\",
            "a|",
            "a?",
            "a*",
            "a\x01",
            "a\x1f",
            "CON",
            "con",
            "Prn.txt",
            "aux.tar.gz",
            "NUL",
            "COM1",
            "lpt9.x",
        ];
        for s in ok {
            assert!(windows_valid(&Name::new(s).unwrap()), "{s}");
        }
        for s in bad {
            assert!(!windows_valid(&Name::new(s).unwrap()), "{s:?}");
        }
    }

    // Cases from git t0060-path-utils.sh (is_ntfs_dotgit / is_hfs_dotgit).
    // fidelity: Q-7
    #[test]
    fn checkout_safe_table() {
        let bad = [
            "git~1",
            "GIT~1",
            "a/git~1/b",
            ".git . .",
            "a/.git.",
            "a/.git ",
            "a/.git::$INDEX_ALLOCATION",
            "a/.git::$DATA",
            ".git:x",
            "a/git~1.",
            "a\\.git",
            "a\\..\\b",
            ".g\u{200c}it",
            ".\u{200d}git",
            ".git\u{feff}",
            "\u{202e}.git",
            ".\u{206b}GIT",
            ".asc/info/exclude",
            ".ASC/Info/exclude",
            ".asc./info /exclude",
            "ASC~1/info/exclude",
            ".asc::$DATA/info",
            ".c\u{200c}g/info/x",
            ".asc",
            ".ASC",
            ".asc.",
            "ASC~1",
            ".asc::$DATA",
        ];
        for s in bad {
            let rp = RepoPath::new(s).unwrap();
            assert!(!checkout_safe(&rp), "{s:?}");
        }
        for s in [
            "a/b",
            ".gitignore",
            ".github/x",
            "git~2",
            "gitx",
            "a/.gitmodules",
            "\u{e9}/x",
            ".gi/t",
            ".asc/redirections",
            "a/.asc/info/x",
            "a/.asc",
        ] {
            assert!(checkout_safe(&p(s)), "{s}");
        }
    }

    // Cases from git t1402-check-ref-format.sh.
    #[test]
    fn ref_format_table() {
        let ok = [
            "foo/bar/baz",
            "heads/foo@bar",
            "heads/fu\u{df}",
            "foo/bar",
            "refs/heads/master",
            "heads/foo-bar",
            "a/b@c",
            "a/b{",
            "foo.bar/baz",
            "refs/heads/@x",
            "refs/heads/-x",
        ];
        let bad = [
            "",
            "/",
            "/foo",
            "foo/",
            "foo//bar",
            "refs///heads/foo",
            "heads/foo/",
            "heads/foo.",
            "heads/.foo",
            "heads/foo..bar",
            "heads/foo?bar",
            "heads/foo\x7f",
            "heads/foo\tbar",
            "heads/foo bar",
            "heads/foo:bar",
            "heads/foo^bar",
            "heads/foo~bar",
            "heads/foo*bar",
            "heads/foo[bar",
            "heads/foo\\bar",
            "heads/foo.lock",
            "heads/foo.lock/bar",
            "heads/foo@{bar",
            "@",
            "foo",
            "refs/heads/@{",
            "foo/.bar",
            "a/.b/c",
            "a/b.lock",
        ];
        for s in ok {
            assert!(RefName::new(s).is_ok(), "{s:?}");
        }
        for s in bad {
            assert!(RefName::new(s).is_err(), "{s:?}");
        }
        assert!(RefName::new_onelevel("HEAD").is_ok());
        assert!(RefName::new_onelevel("@").is_err());
        assert!(RefName::new("HEAD").is_err());
    }

    proptest! {
        #[test]
        fn valid_paths_round_trip(parts in proptest::collection::vec(
            proptest::collection::vec(1u8..=255, 1..8).prop_filter("slash", |v| !v.contains(&b'/')), 1..6)) {
            let mut bytes = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                if i > 0 { bytes.push(b'/'); }
                bytes.extend_from_slice(part);
            }
            if let Ok(path) = RepoPath::new(bytes.clone()) {
                prop_assert_eq!(path.as_bytes(), &bytes[..]);
                let comps: Vec<_> = path.components().collect();
                prop_assert_eq!(comps.len(), parts.len());
                let mut rebuilt = RepoPath::from(Name::new(comps[0]).unwrap());
                for c in &comps[1..] { rebuilt = rebuilt.join(&Name::new(*c).unwrap()); }
                prop_assert_eq!(&rebuilt, &path);
                let mut up = path.clone();
                while let Some(par) = up.parent() { prop_assert!(path.starts_with_dir(&par)); up = par; }
            }
        }
    }
}

#[cfg(test)]
mod windows_hardening_tests {
    use super::*;

    #[test]
    fn device_names_with_padding_and_variants_are_invalid() {
        for s in [
            "CON .txt",
            "nul  .tar.gz",
            "CONIN$",
            "conout$.log",
            "COM\u{b9}",
            "lpt\u{b3}.x",
            "AUX ",
        ] {
            assert!(
                !windows_valid(&Name::new(s.as_bytes().to_vec()).unwrap()),
                "{s:?}"
            );
        }
        for s in ["CONSOLE", "COM10", "LPT0", "nully.txt"] {
            assert!(
                windows_valid(&Name::new(s.as_bytes().to_vec()).unwrap()),
                "{s:?}"
            );
        }
    }

    #[test]
    fn backslash_components_are_not_checkout_safe() {
        for s in [r"a\..\b", r"c:\x", r"dir\.git", r"x\\y"] {
            assert!(
                !checkout_safe(&RepoPath::new(s.as_bytes().to_vec()).unwrap()),
                "{s:?}"
            );
        }
    }
}
