//! Pathspecs for `add`, `rm` and `mv`: literal paths (a file, or a directory with everything
//! below it) and git's default wildcard pattern, in which `*` also crosses `/`. Magic: `top`,
//! `literal` and `exclude` (also `:/`, `:!`, `:^`); every other word is refused.

use std::ffi::OsStr;
use std::path::Path;

use crate::cli::common::{CliError, Result};

#[derive(Debug)]
enum Kind {
    /// A path; everything below it when it is a directory. Empty is the repository root.
    Literal(Vec<u8>),
    /// A wildcard pattern over the whole repository path, with its leading wildcard-free
    /// directory.
    Wild { pat: Vec<u8>, prefix: Vec<u8> },
}

/// One pathspec argument.
#[derive(Debug)]
pub struct Item {
    raw: String,
    kind: Kind,
}

impl Item {
    /// The argument as typed.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The repository path a plain item names.
    pub fn literal(&self) -> Option<&[u8]> {
        match &self.kind {
            Kind::Literal(p) => Some(p),
            Kind::Wild { .. } => None,
        }
    }

    pub fn is_wild(&self) -> bool {
        matches!(self.kind, Kind::Wild { .. })
    }

    /// Whether this item alone selects `path`.
    pub fn matches(&self, path: &[u8]) -> bool {
        match &self.kind {
            Kind::Literal(p) => p.is_empty() || path == p || under(path, p),
            Kind::Wild { pat, .. } => wild(pat, path),
        }
    }

    fn exact(&self, path: &[u8]) -> bool {
        match &self.kind {
            Kind::Literal(p) => path == p,
            Kind::Wild { pat, .. } => wild(pat, path),
        }
    }

    fn base(&self) -> &[u8] {
        match &self.kind {
            Kind::Literal(p) => p,
            Kind::Wild { prefix, .. } => prefix,
        }
    }
}

/// `path` is strictly inside directory `dir` (empty is the root).
fn under(path: &[u8], dir: &[u8]) -> bool {
    dir.is_empty() || (path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/')
}

/// A set of includes and excludes. With no includes every path is included.
#[derive(Debug, Default)]
pub struct Spec {
    includes: Vec<Item>,
    excludes: Vec<Item>,
}

/// Resolves `arg` (relative to the directory `prefix` below the work tree `root`) to a
/// normalised repository path, `.` and `..` resolved; the root is empty.
pub fn repo_path(arg: &OsStr, prefix: &str, root: &Path) -> Result<Vec<u8>> {
    let text = arg.to_string_lossy().replace('\\', "/");
    let outside = || {
        CliError::Fatal(format!(
            "{text}: '{text}' is outside repository at '{}'",
            root.display().to_string().replace('\\', "/")
        ))
    };
    let mut parts: Vec<&str> = Vec::new();
    let relative;
    let rest = if Path::new(arg).is_absolute() {
        let canon = root.canonicalize().unwrap_or_else(|_| root.to_owned());
        let stripped = Path::new(arg)
            .strip_prefix(root)
            .or_else(|_| Path::new(arg).strip_prefix(&canon))
            .map_err(|_| outside())?;
        relative = stripped.to_string_lossy().replace('\\', "/");
        relative.as_str()
    } else {
        parts.extend(prefix.split('/').filter(|c| !c.is_empty()));
        text.as_str()
    };
    for comp in rest.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop().ok_or_else(outside)?;
            }
            c => parts.push(c),
        }
    }
    Ok(parts.join("/").into_bytes())
}

fn has_wildcard(p: &[u8]) -> bool {
    p.iter().any(|c| matches!(c, b'*' | b'?' | b'[' | b'\\'))
}

impl Spec {
    /// Parses `args` typed in the directory `prefix` (repository-relative, empty at the root).
    pub fn parse(args: &[impl AsRef<OsStr>], prefix: &str, root: &Path) -> Result<Self> {
        let mut spec = Self::default();
        for arg in args {
            let raw = arg.as_ref().to_string_lossy().into_owned();
            if raw.is_empty() {
                return Err(CliError::Fatal(
                    "empty string is not a valid pathspec. please use . instead if you meant to match all paths".into(),
                ));
            }
            let (mut top, mut literal, mut exclude) = (false, false, false);
            let mut body = raw.as_str();
            if let Some(after) = body.strip_prefix(":(") {
                let close = after.find(')').ok_or_else(|| {
                    CliError::Fatal(format!("Missing ')' at the end of pathspec magic in '{raw}'"))
                })?;
                for word in after[..close].split(',') {
                    match word {
                        "top" => top = true,
                        "literal" => literal = true,
                        "exclude" => exclude = true,
                        "" => {}
                        w => {
                            return Err(CliError::Fatal(format!(
                                "Unimplemented pathspec magic '{w}' in '{raw}'"
                            )));
                        }
                    }
                }
                body = &after[close + 1..];
            } else if let Some(after) = body.strip_prefix(":!").or_else(|| body.strip_prefix(":^")) {
                exclude = true;
                body = after;
            } else if let Some(after) = body.strip_prefix(":/") {
                top = true;
                body = after;
            }
            let at = if top { "" } else { prefix };
            let item = Item {
                raw: raw.clone(),
                kind: Self::kind(body, at, literal, root)?,
            };
            if exclude {
                spec.excludes.push(item);
            } else {
                spec.includes.push(item);
            }
        }
        Ok(spec)
    }

    fn kind(body: &str, prefix: &str, literal: bool, root: &Path) -> Result<Kind> {
        if literal || !has_wildcard(body.as_bytes()) {
            return Ok(Kind::Literal(repo_path(OsStr::new(body), prefix, root)?));
        }
        // The wildcard-free leading directory is resolved; the rest is the pattern as typed.
        let bytes = body.as_bytes();
        let first = bytes.iter().position(|c| matches!(c, b'*' | b'?' | b'[' | b'\\')).unwrap_or(0);
        let cut = bytes[..first].iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
        let dir = repo_path(OsStr::new(&body[..cut]), prefix, root)?;
        let mut pat = dir.clone();
        if !pat.is_empty() {
            pat.push(b'/');
        }
        pat.extend_from_slice(&bytes[cut..]);
        Ok(Kind::Wild { pat, prefix: dir })
    }

    /// Whether no include was given: every path is selected.
    pub fn is_all(&self) -> bool {
        self.includes.is_empty()
    }

    pub fn includes(&self) -> &[Item] {
        &self.includes
    }

    /// Whether `path` is selected.
    pub fn matches(&self, path: &[u8]) -> bool {
        (self.includes.is_empty() || self.includes.iter().any(|i| i.matches(path)))
            && !self.excludes.iter().any(|e| e.matches(path))
    }

    /// Marks in `seen` (one slot per include) the includes that select `path`.
    pub fn mark(&self, path: &[u8], seen: &mut [bool]) {
        if self.excludes.iter().any(|e| e.matches(path)) {
            return;
        }
        for (slot, item) in seen.iter_mut().zip(&self.includes) {
            *slot |= item.matches(path);
        }
    }

    /// Whether an include names `path` itself (a literal equal to it, or a wildcard it matches).
    pub fn exact(&self, path: &[u8]) -> bool {
        self.includes.iter().any(|i| i.exact(path)) && !self.excludes.iter().any(|e| e.matches(path))
    }

    /// Whether a literal include names a path strictly below directory `dir`.
    pub fn names_below(&self, dir: &[u8]) -> bool {
        self.includes
            .iter()
            .any(|i| i.literal().is_some_and(|p| !p.is_empty() && under(p, dir)))
            && !self.excludes.iter().any(|e| e.matches(dir))
    }

    /// False only when nothing below directory `dir` can be selected.
    pub fn may_contain_dir(&self, dir: &[u8]) -> bool {
        let excluded = self
            .excludes
            .iter()
            .any(|e| matches!(&e.kind, Kind::Literal(p) if dir == p.as_slice() || under(dir, p)));
        !excluded
            && (self.includes.is_empty()
                || self.includes.iter().any(|i| {
                    let b = i.base();
                    b.is_empty() || dir == b || under(dir, b) || under(b, dir)
                }))
    }

    /// The directories or files a tree walk must cover; empty means the whole tree.
    pub fn prefixes(&self) -> Vec<Vec<u8>> {
        if self.includes.iter().any(|i| i.base().is_empty()) {
            return Vec::new();
        }
        self.includes.iter().map(|i| i.base().to_vec()).collect()
    }
}

/// git's `wildmatch` without `WM_PATHNAME`: `*` and `?` also match `/`.
pub fn wild(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if pat.get(p) == Some(&b'*') {
            star = Some((p + 1, t));
            p += 1;
            continue;
        }
        if let Some(next) = single(pat, p, text[t]) {
            p = next;
            t += 1;
        } else if let Some((sp, st)) = star {
            p = sp;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pat.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pat.len()
}

/// Matches one pattern element at `p` against byte `c`; the position after it.
fn single(pat: &[u8], p: usize, c: u8) -> Option<usize> {
    match *pat.get(p)? {
        b'?' => Some(p + 1),
        b'\\' => (pat.get(p + 1) == Some(&c)).then_some(p + 2),
        b'[' => match class(&pat[p..], c) {
            Some((hit, len)) => hit.then_some(p + len),
            None => (c == b'[').then_some(p + 1),
        },
        lit => (lit == c).then_some(p + 1),
    }
}

/// `[...]` at the start of `pat`: whether `c` is in it, and its length; `None` if unterminated.
fn class(pat: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(pat.get(i), Some(b'!' | b'^'));
    if negate {
        i += 1;
    }
    let start = i;
    let mut hit = false;
    while let Some(&b) = pat.get(i) {
        if b == b']' && i > start {
            return Some((hit != negate, i + 1));
        }
        if pat.get(i + 1) == Some(&b'-') && pat.get(i + 2).is_some_and(|&e| e != b']') {
            hit |= (b..=pat[i + 2]).contains(&c);
            i += 3;
        } else {
            hit |= b == c;
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(args: &[&str], prefix: &str) -> Spec {
        Spec::parse(args, prefix, Path::new("/repo")).unwrap()
    }

    #[test]
    fn literals_cover_directories() {
        let s = spec(&["src", "a.txt"], "");
        assert!(s.matches(b"src/x/y.rs") && s.matches(b"a.txt") && !s.matches(b"srcx/y"));
        assert_eq!(s.prefixes(), [b"src".to_vec(), b"a.txt".to_vec()]);
        assert!(s.may_contain_dir(b"src/x") && s.may_contain_dir(b"") && !s.may_contain_dir(b"other"));
    }

    #[test]
    fn relative_to_the_current_directory() {
        let s = spec(&[".", "../x", "y/../z"], "a/b");
        let got: Vec<_> = s.includes().iter().map(|i| i.literal().unwrap().to_vec()).collect();
        assert_eq!(got, [b"a/b".to_vec(), b"a/x".to_vec(), b"a/b/z".to_vec()]);
        let err = Spec::parse(&["../../.."], "a/b", Path::new("/repo")).unwrap_err();
        assert!(err.to_string().contains("is outside repository at '/repo'"), "{err}");
    }

    #[test]
    fn wildcards_cross_slashes_like_git() {
        let s = spec(&["*.txt"], "");
        assert!(s.matches(b"a.txt") && s.matches(b"d/e/a.txt") && !s.matches(b"a.rs"));
        assert!(s.includes()[0].is_wild() && s.prefixes().is_empty());
        let s = spec(&["lib/*.rs"], "pkg");
        assert!(s.matches(b"pkg/lib/a.rs") && s.matches(b"pkg/lib/x/a.rs") && !s.matches(b"lib/a.rs"));
        assert_eq!(s.prefixes(), [b"pkg/lib".to_vec()]);
        assert!(wild(b"f[a-c]?", b"fbz") && !wild(b"f[!a-c]?", b"fbz") && wild(b"a\\*b", b"a*b"));
    }

    #[test]
    fn excludes_and_magic() {
        let s = spec(&[".", ":!vendor", ":(exclude)*.log"], "");
        assert!(s.matches(b"a/b") && !s.matches(b"vendor/x") && !s.matches(b"a.log"));
        assert!(!s.may_contain_dir(b"vendor"));
        assert!(Spec::parse(&[":(glob)*"], "", Path::new("/r")).is_err());
        assert!(Spec::parse(&[""], "", Path::new("/r")).is_err());
        let top = spec(&[":/x"], "deep/dir");
        assert_eq!(top.includes()[0].literal(), Some(&b"x"[..]));
    }

    #[test]
    fn exact_and_marks() {
        let s = spec(&["a", "d/*.c"], "");
        let mut seen = [false; 2];
        s.mark(b"a/b", &mut seen);
        assert_eq!(seen, [true, false]);
        assert!(s.exact(b"a") && !s.exact(b"a/b") && s.exact(b"d/x.c"));
    }
}
