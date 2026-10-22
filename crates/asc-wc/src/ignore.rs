//! Ignore rules (SPEC §2.4.2): one gitignore-syntax parser, `.ascignore` else `.gitignore` per
//! directory, then the repo-local exclude, then the global excludes file.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use ::ignore::Match;
use ::ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::safe_path::is_link;

/// Per-directory file read in preference to [`GITIGNORE`].
pub const ASCIGNORE: &str = ".ascignore";
/// Per-directory fallback.
pub const GITIGNORE: &str = ".gitignore";

/// Where the non-per-directory rules come from.
#[derive(Clone, Debug)]
pub struct IgnoreSources {
    /// `asc.excludesFile`; the caller passes `~/.config/asc/ignore` when unset.
    pub asc_excludes: PathBuf,
    /// git's `core.excludesFile`, used when `asc_excludes` does not exist.
    pub core_excludes: Option<PathBuf>,
    /// `<worktree root>/.asc/info/exclude`.
    pub local_exclude: PathBuf,
    /// `<common>/info/exclude`, used when `local_exclude` does not exist.
    pub common_exclude: PathBuf,
}

/// Largest ignore file `differ` compares; a larger pair is reported as differing.
const MAX_COMPARE: u64 = 1 << 20;

/// Whether `path` is a regular file reached without a link at its end. Ignore files come from
/// the repository, so a symlink or reparse point there (to a secret or a network share) is never
/// followed.
fn real_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| !is_link(&m) && m.is_file())
}

impl IgnoreSources {
    /// The repo-local exclude file in effect.
    pub fn local(&self) -> &Path {
        if real_file(&self.local_exclude) {
            &self.local_exclude
        } else {
            &self.common_exclude
        }
    }

    /// The global excludes file in effect.
    pub fn global(&self) -> Option<&Path> {
        if self.asc_excludes.is_file() {
            Some(&self.asc_excludes)
        } else {
            self.core_excludes.as_deref().filter(|p| p.is_file())
        }
    }
}

/// The pattern that decided a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The ignore file, when known.
    pub file: Option<PathBuf>,
    pub pattern: String,
    /// A `!pattern`: the path is explicitly not ignored.
    pub negated: bool,
}

impl Hit {
    fn from_match(m: Match<&::ignore::gitignore::Glob>) -> Option<Self> {
        let (glob, negated) = match m {
            Match::None => return None,
            Match::Ignore(g) => (g, false),
            Match::Whitelist(g) => (g, true),
        };
        Some(Self {
            file: glob.from().map(Path::to_owned),
            pattern: glob.original().to_owned(),
            negated,
        })
    }
}

/// The rules in effect while walking down a work tree.
pub struct IgnoreStack {
    root: PathBuf,
    /// One matcher per directory entered, root first.
    levels: Vec<Gitignore>,
    local: Gitignore,
    global: Gitignore,
}

fn build(root: &Path, file: Option<&Path>) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    if let Some(f) = file {
        // A file with unparsable lines still yields its valid patterns.
        let _ = b.add(f);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

impl IgnoreStack {
    /// Starts with the repo-local and global rules for the work tree at `root`.
    pub fn new(root: &Path, sources: &IgnoreSources) -> Self {
        Self {
            root: root.to_owned(),
            levels: Vec::new(),
            local: build(root, Some(sources.local())),
            global: build(root, sources.global()),
        }
    }

    /// Enters directory `dir` (absolute), loading its `.ascignore` else `.gitignore`.
    /// Returns whether a differing `.gitignore` is shadowed by the `.ascignore`.
    pub fn push_dir(&mut self, dir: &Path) -> io::Result<bool> {
        let asc = dir.join(ASCIGNORE);
        let git = dir.join(GITIGNORE);
        let (file, shadowed) = if real_file(&asc) {
            (Some(asc.clone()), real_file(&git) && differ(&asc, &git)?)
        } else {
            (real_file(&git).then_some(git), false)
        };
        self.levels.push(build(dir, file.as_deref()));
        Ok(shadowed)
    }

    /// Leaves the directory entered by the last [`push_dir`](Self::push_dir).
    pub fn pop_dir(&mut self) {
        self.levels.pop();
    }

    /// The pattern deciding `abs` (a path below the root), deepest directory first. `None` when
    /// nothing matches. `.git` and `.asc/info/` are implicit hits.
    pub fn explain(&self, abs: &Path, is_dir: bool) -> Option<Hit> {
        if self.implicit(abs) {
            return Some(Hit {
                file: None,
                pattern: ".asc/info/".to_owned(),
                negated: false,
            });
        }
        let levels = self.levels.iter().rev();
        levels
            .chain([&self.local, &self.global])
            .find_map(|m| Hit::from_match(m.matched(abs, is_dir)))
    }

    /// Whether `abs` is ignored.
    pub fn is_ignored(&self, abs: &Path, is_dir: bool) -> bool {
        self.explain(abs, is_dir).is_some_and(|h| !h.negated)
    }

    fn implicit(&self, abs: &Path) -> bool {
        let Ok(rel) = abs.strip_prefix(&self.root) else {
            return false;
        };
        let mut c = rel.components().map(|c| c.as_os_str());
        match c.next() {
            Some(first) if first == ".git" => true,
            Some(first) => c.next().is_some_and(|second| {
                asc_core::path::is_asc_info(
                    first.to_string_lossy().as_bytes(),
                    second.to_string_lossy().as_bytes(),
                )
            }),
            None => false,
        }
    }
}

fn differ(a: &Path, b: &Path) -> io::Result<bool> {
    let (la, lb) = (fs::metadata(a)?.len(), fs::metadata(b)?.len());
    if la != lb || la > MAX_COMPARE {
        return Ok(true);
    }
    let read = |p: &Path| {
        let mut buf = Vec::new();
        fs::File::open(p)?.take(MAX_COMPARE).read_to_end(&mut buf)?;
        io::Result::Ok(buf)
    };
    Ok(read(a)? != read(b)?)
}

/// The SPEC §2.4.2 warning for directory `dir` (repository-relative, empty for the root).
pub fn shadow_warning(dir: &str) -> String {
    let d = if dir.is_empty() {
        String::new()
    } else {
        format!("{}/", dir.trim_end_matches('/'))
    };
    format!(
        "warning: asc: {d}.gitignore is not read because {d}.ascignore exists \
         (rename or merge it: asc migrate ignore)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symlink_file(target: &Path, link: &Path) -> io::Result<()> {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        return std::os::windows::fs::symlink_file(target, link);
        #[cfg(not(any(unix, windows)))]
        Err(io::ErrorKind::Unsupported.into())
    }

    fn sources(root: &Path) -> IgnoreSources {
        IgnoreSources {
            asc_excludes: root.join("nope-asc"),
            core_excludes: None,
            local_exclude: root.join(".asc/info/exclude"),
            common_exclude: root.join("common-exclude"),
        }
    }

    #[test]
    fn ascignore_replaces_gitignore_in_its_directory() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::write(r.join(".gitignore"), "*.log\n").unwrap();
        fs::write(r.join(ASCIGNORE), "*.tmp\n").unwrap();
        let mut s = IgnoreStack::new(r, &sources(r));
        s.push_dir(r).unwrap();
        assert!(s.is_ignored(&r.join("a.tmp"), false));
        assert!(!s.is_ignored(&r.join("a.log"), false));
    }

    #[test]
    fn info_dir_is_implicitly_ignored_and_exclude_is_read() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join(".asc/info")).unwrap();
        fs::write(r.join(".asc/info/exclude"), "scratch/\n").unwrap();
        let mut s = IgnoreStack::new(r, &sources(r));
        s.push_dir(r).unwrap();
        assert!(s.is_ignored(&r.join(".asc/info"), true));
        assert!(s.is_ignored(&r.join(".asc/info/exclude"), false));
        assert!(!s.is_ignored(&r.join(".asc/redirections"), false));
        assert!(s.is_ignored(&r.join("scratch"), true));
    }

    #[test]
    fn symlinked_ignore_files_are_not_read() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let secret = d.path().join("secret");
        fs::write(
            &secret, "*.leak
",
        )
        .unwrap();
        fs::create_dir_all(r.join(".asc/info")).unwrap();
        let linked = [
            (r.join(".gitignore"), "a.leak"),
            (r.join(".asc/info/exclude"), "b.leak"),
        ];
        for (at, _) in &linked {
            if symlink_file(&secret, at).is_err() {
                return;
            }
        }
        let mut s = IgnoreStack::new(r, &sources(r));
        s.push_dir(r).unwrap();
        for (_, name) in linked {
            assert!(!s.is_ignored(&r.join(name), false), "{name}");
            assert!(s.explain(&r.join(name), false).is_none());
        }
        assert_eq!(sources(r).local(), r.join("common-exclude"));
    }

    #[test]
    fn symlinked_gitignore_does_not_shadow_or_load() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let secret = d.path().join("secret");
        fs::write(
            &secret, "x
",
        )
        .unwrap();
        fs::create_dir(r.join("w")).unwrap();
        let w = r.join("w");
        fs::write(
            w.join(ASCIGNORE),
            "y
",
        )
        .unwrap();
        if symlink_file(&secret, &w.join(GITIGNORE)).is_err() {
            return;
        }
        let mut s = IgnoreStack::new(&w, &sources(r));
        assert!(!s.push_dir(&w).unwrap());
    }

    #[test]
    fn oversized_pair_is_reported_without_reading() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        let big = vec![b'x'; (MAX_COMPARE + 1) as usize];
        fs::write(&a, &big).unwrap();
        fs::write(&b, &big).unwrap();
        assert!(differ(&a, &b).unwrap());
    }

    #[test]
    fn asc_info_aliases_are_implicitly_ignored() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let s = IgnoreStack::new(r, &sources(r));
        for p in [".ASC/info/exclude", "ASC~1/Info/x", ".asc./info"] {
            assert!(s.is_ignored(&r.join(p), false), "{p}");
        }
        assert!(!s.is_ignored(&r.join(".asc/other"), false));
    }

    #[test]
    fn nested_files_win_and_negate() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir(r.join("sub")).unwrap();
        fs::write(r.join(".gitignore"), "*.o\n").unwrap();
        fs::write(r.join("sub/.gitignore"), "!keep.o\n").unwrap();
        let mut s = IgnoreStack::new(r, &sources(r));
        s.push_dir(r).unwrap();
        s.push_dir(&r.join("sub")).unwrap();
        assert!(!s.is_ignored(&r.join("sub/keep.o"), false));
        assert!(s.is_ignored(&r.join("sub/x.o"), false));
        s.pop_dir();
        assert!(s.is_ignored(&r.join("keep.o"), false));
    }

    #[test]
    fn shadow_reported_only_when_contents_differ() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("same")).unwrap();
        fs::create_dir_all(r.join("diff")).unwrap();
        for sub in ["same", "diff"] {
            fs::write(r.join(sub).join(GITIGNORE), "a\n").unwrap();
        }
        fs::write(r.join("same").join(ASCIGNORE), "a\n").unwrap();
        fs::write(r.join("diff").join(ASCIGNORE), "b\n").unwrap();
        let mut s = IgnoreStack::new(r, &sources(r));
        assert!(!s.push_dir(&r.join("same")).unwrap());
        assert!(s.push_dir(&r.join("diff")).unwrap());
        assert_eq!(
            shadow_warning("diff"),
            "warning: asc: diff/.gitignore is not read because diff/.ascignore exists \
             (rename or merge it: asc migrate ignore)"
        );
    }
}
