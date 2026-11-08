//! The path gate of `asc mcp` (SPEC 22.5, "Path escape (stdio)").
//!
//! Every path argument of a tool call passes [`Gate::check`] before it reaches an engine call or a
//! child verb:
//!
//! - lexical rules: a relative repository path written with `/`, no NUL or control character, no
//!   `..`, no drive or UNC prefix, no backslash, no empty component, and on Windows no `:`
//!   (alternate data streams), trailing dot or space, or reserved device name;
//! - each component is examined **without following links**: a symlink or a junction (any reparse
//!   point) on the way is refused and named, never read through;
//! - the same ignore rules the work tree uses (`.ascignore`/`.gitignore`, the excludes files and the
//!   implicit `.git` and `.asc/info`) refuse an ignored path, and a path below an ignored directory.
//!
//! [`Gate::open`] opens the final file with the platform's no-follow flag and re-checks what was
//! opened, so a link swapped in after [`Gate::check`] is still not read through.
//!
//! The one link the gate reads through is a link-mode file (`asc.worktree.link = symlink`) into
//! this repository's own shared area ([`Gate::with_area`]): a link into another repository's area,
//! or into any area when the gate has none, is refused like any other link.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use asc_wc::ignore::{IgnoreSources, IgnoreStack};
use asc_wc::safe_path::is_link;

/// Longest path accepted.
const MAX_PATH: usize = 4096;
/// Largest number of components.
const MAX_DEPTH: usize = 256;

/// Why a path was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Not a relative repository path (empty component, backslash, control character, device name).
    #[error("'{0}' is not a relative repository path")]
    Invalid(String),
    /// Absolute, drive or UNC.
    #[error("'{0}' is absolute; give a path relative to the worktree")]
    Absolute(String),
    /// Has a `..` component.
    #[error("'{0}' has a '..' component")]
    Parent(String),
    /// A symlink or junction on the way.
    #[error("'{0}' is a link; links are never followed")]
    Link(String),
    /// Ignored by the work tree's ignore rules.
    #[error("'{0}' is ignored")]
    Ignored(String),
}

impl Refusal {
    /// The path text the refusal names, for `ToolError::paths`.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Invalid(p)
            | Self::Absolute(p)
            | Self::Parent(p)
            | Self::Link(p)
            | Self::Ignored(p) => p,
        }
    }
}

/// How a path is examined against the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disk {
    /// The path names a working-copy file or directory: each existing component must be a real
    /// file or directory.
    Worktree,
    /// The path names something in a revision: only the lexical and ignore rules apply (the disk
    /// is not read as content, so a link there is irrelevant).
    Revision,
}

/// The gate of one worktree.
pub struct Gate {
    root: PathBuf,
    sources: IgnoreSources,
    /// The repository's shared blob area: links into it are its link-mode files.
    area: Option<PathBuf>,
}

fn invalid(p: &str) -> Refusal {
    Refusal::Invalid(shown(p))
}

/// The path as an error shows it: bounded and free of control characters.
fn shown(p: &str) -> String {
    p.chars().filter(|c| !c.is_control()).take(200).collect()
}

#[cfg(windows)]
fn windows_component_ok(c: &str) -> bool {
    if c.contains(':') || c.ends_with('.') || c.ends_with(' ') {
        return false;
    }
    let stem = c.split('.').next().unwrap_or(c).to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit());
    !reserved
}

#[cfg(not(windows))]
fn windows_component_ok(_c: &str) -> bool {
    true
}

/// The components of a repository path, or why it is refused. The empty path is the root (no
/// components); one trailing `/` is allowed, a `.` component is dropped.
///
/// # Errors
/// A [`Refusal`] naming the path.
pub fn components(path: &str) -> Result<Vec<&str>, Refusal> {
    if path.len() > MAX_PATH || path.chars().any(char::is_control) {
        return Err(invalid(path));
    }
    let b = path.as_bytes();
    if path.starts_with('/')
        || path.starts_with('\\')
        || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
    {
        return Err(Refusal::Absolute(shown(path)));
    }
    if path.contains('\\') {
        return Err(invalid(path));
    }
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return if path.is_empty() {
            Ok(Vec::new())
        } else {
            Err(invalid(path))
        };
    }
    let mut out = Vec::new();
    for c in trimmed.split('/') {
        match c {
            "" => return Err(invalid(path)),
            "." => {}
            ".." => return Err(Refusal::Parent(shown(path))),
            c if !windows_component_ok(c) => return Err(invalid(path)),
            c => out.push(c),
        }
    }
    if out.len() > MAX_DEPTH {
        return Err(invalid(path));
    }
    Ok(out)
}

impl Gate {
    /// A gate for the worktree at `root` (an absolute path, not canonicalised: ignore matching is
    /// relative to it). `sources` are the ignore sources of the workspace.
    #[must_use]
    pub fn new(root: PathBuf, mut sources: IgnoreSources) -> Self {
        sources.local_exclude = root.join(".asc/info/exclude");
        Self {
            root,
            sources,
            area: None,
        }
    }

    /// The gate with `area` (`<home>/shared-blobs/<repo-id>`) as the repository's shared blob
    /// area: a link-mode file linking into it reads as the file it points to.
    #[must_use]
    pub fn with_area(mut self, area: Option<PathBuf>) -> Self {
        self.area = area;
        self
    }

    /// The target of the link-mode file at `abs`, if it links into this repository's area.
    fn shared_target(&self, abs: &Path) -> Option<PathBuf> {
        let area = self.area.as_deref()?;
        let target = asc_wc::shared::link_target(abs)?;
        let (_, _, of) = asc_wc::shared::area_of_target(&target)?;
        (of == area).then_some(target)
    }

    /// The worktree root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Checks `path` and returns the absolute path it names. `dir_hint` says the last component
    /// is a directory when the disk cannot tell.
    ///
    /// # Errors
    /// A [`Refusal`]: lexical, a link on the way (for [`Disk::Worktree`]) or an ignored path.
    pub fn check(&self, path: &str, disk: Disk, dir_hint: bool) -> Result<PathBuf, Refusal> {
        let parts = components(path)?;
        let mut cur = self.root.clone();
        let mut stack = IgnoreStack::new(&self.root, &self.sources);
        // A directory that cannot be entered (missing, or a file) ends the walk; what follows is
        // still checked by the ignore rules, read from the (possibly absent) directories.
        let mut on_disk = disk == Disk::Worktree;
        let mut rel = String::new();
        stack.push_dir(&self.root).map_err(|_| invalid(path))?;
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            cur.push(part);
            if !rel.is_empty() {
                rel.push('/');
            }
            rel.push_str(part);
            let mut is_dir = !last || dir_hint;
            if on_disk {
                match fs::symlink_metadata(&cur) {
                    // A link-mode file (`asc.worktree.link`) is the shared file it points to.
                    Ok(m) if is_link(&m) && !(last && self.shared_target(&cur).is_some()) => {
                        return Err(Refusal::Link(shown(&rel)));
                    }
                    Ok(m) => is_dir = m.is_dir(),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => on_disk = false,
                    // Anything else (a permission error) is not a reason to read through it.
                    Err(_) => return Err(invalid(path)),
                }
            }
            if stack.is_ignored(&cur, is_dir) {
                return Err(Refusal::Ignored(shown(&rel)));
            }
            if !last {
                if on_disk && !is_dir {
                    on_disk = false;
                }
                stack.push_dir(&cur).map_err(|_| invalid(path))?;
            }
        }
        Ok(cur)
    }

    /// Whether `path` (a path found in a revision or by a search) may be shown: the lexical and
    /// ignore rules, without reading the disk for links.
    #[must_use]
    pub fn shown_ok(&self, path: &str) -> bool {
        self.check(path, Disk::Revision, false).is_ok()
    }

    /// Opens a path [`check`](Self::check) returned, for reading, without following a link at its
    /// end, and verifies what was opened is a regular file.
    ///
    /// # Errors
    /// An I/O error, or `InvalidInput` when the opened object is a link or not a regular file.
    pub fn open(&self, abs: &Path) -> io::Result<File> {
        // A link-mode file of this repository opens as the shared file it points to, itself
        // never a link.
        let target = self.shared_target(abs);
        let abs = target.as_deref().unwrap_or(abs);
        let mut o = OpenOptions::new();
        o.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            o.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        }
        let f = o.open(abs)?;
        let meta = f.metadata()?;
        if is_link(&meta) || !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(dir: &Path) -> Gate {
        Gate::new(
            dir.to_owned(),
            IgnoreSources {
                asc_excludes: dir.join("no-such-asc-ignore"),
                core_excludes: None,
                local_exclude: dir.join(".asc/info/exclude"),
                common_exclude: dir.join("no-such-exclude"),
            },
        )
    }

    #[test]
    fn lexical_rules() {
        assert_eq!(components("").unwrap(), Vec::<&str>::new());
        assert_eq!(components("a/b.txt").unwrap(), ["a", "b.txt"]);
        assert_eq!(components("./a/").unwrap(), ["a"]);
        assert!(matches!(components("../x"), Err(Refusal::Parent(_))));
        assert!(matches!(components("a/../x"), Err(Refusal::Parent(_))));
        assert!(matches!(
            components("/etc/passwd"),
            Err(Refusal::Absolute(_))
        ));
        assert!(matches!(components("C:/x"), Err(Refusal::Absolute(_))));
        assert!(matches!(components("C:x"), Err(Refusal::Absolute(_))));
        assert!(matches!(
            components("\\\\srv\\share"),
            Err(Refusal::Absolute(_))
        ));
        assert!(matches!(components("a\\b"), Err(Refusal::Invalid(_))));
        assert!(matches!(components("a//b"), Err(Refusal::Invalid(_))));
        assert!(matches!(components("a\0b"), Err(Refusal::Invalid(_))));
        assert!(matches!(components("a\nb"), Err(Refusal::Invalid(_))));
        assert!(matches!(components("/"), Err(Refusal::Absolute(_))));
    }

    #[cfg(windows)]
    #[test]
    fn windows_names() {
        for bad in ["a:stream", "nul", "dir/COM1.txt", "x.", "x "] {
            assert!(components(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ignored_paths_and_children_are_refused() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join(".ascignore"), ".env\nbuild/\n").unwrap();
        fs::create_dir(t.path().join("build")).unwrap();
        fs::write(t.path().join(".env"), "secret").unwrap();
        fs::write(t.path().join("build/out.txt"), "x").unwrap();
        fs::write(t.path().join("ok.txt"), "x").unwrap();
        let g = gate(t.path());
        assert!(g.check("ok.txt", Disk::Worktree, false).is_ok());
        assert!(matches!(
            g.check(".env", Disk::Worktree, false),
            Err(Refusal::Ignored(_))
        ));
        assert!(matches!(
            g.check("build/out.txt", Disk::Worktree, false),
            Err(Refusal::Ignored(_))
        ));
        assert!(matches!(
            g.check("build/missing.txt", Disk::Revision, false),
            Err(Refusal::Ignored(_))
        ));
        assert!(matches!(
            g.check(".git/config", Disk::Revision, false),
            Err(Refusal::Ignored(_))
        ));
        assert!(!g.shown_ok(".env"));
        assert!(g.shown_ok("missing/file.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_on_the_way_and_at_the_end() {
        let t = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        fs::write(out.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(out.path(), t.path().join("dirlink")).unwrap();
        std::os::unix::fs::symlink(out.path().join("secret"), t.path().join("filelink")).unwrap();
        let g = gate(t.path());
        assert!(matches!(
            g.check("dirlink/secret", Disk::Worktree, false),
            Err(Refusal::Link(_))
        ));
        assert!(matches!(
            g.check("filelink", Disk::Worktree, false),
            Err(Refusal::Link(_))
        ));
        assert!(g.open(&t.path().join("filelink")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn only_this_repositorys_link_mode_files_are_read_through() {
        use std::io::Read as _;
        let t = tempfile::tempdir().unwrap();
        let h = tempfile::tempdir().unwrap();
        let home = asc_state::AscHome::open(h.path().join("home")).unwrap();
        let (mine, theirs) = (asc_state::RepoId::new(), asc_state::RepoId::new());
        let oid = asc_core::id::GitOid::from_bytes([1; 20]);
        for (repo, name, data) in [(mine, "mine.txt", "m"), (theirs, "theirs.txt", "t")] {
            let file = asc_wc::shared::path_in(&home.shared_blobs(repo), oid, false);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, data).unwrap();
            std::os::unix::fs::symlink(&file, t.path().join(name)).unwrap();
        }
        let g = gate(t.path()).with_area(Some(home.shared_blobs(mine)));
        let abs = g.check("mine.txt", Disk::Worktree, false).unwrap();
        let mut got = String::new();
        g.open(&abs).unwrap().read_to_string(&mut got).unwrap();
        assert_eq!(got, "m");
        assert!(matches!(
            g.check("theirs.txt", Disk::Worktree, false),
            Err(Refusal::Link(_))
        ));
        assert!(g.open(&t.path().join("theirs.txt")).is_err());
        // A gate without an area reads through no link.
        assert!(matches!(
            gate(t.path()).check("mine.txt", Disk::Worktree, false),
            Err(Refusal::Link(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn junctions_and_symlinks_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        fs::write(out.path().join("secret"), "s").unwrap();
        let junction = t.path().join("junc");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(out.path())
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J failed");
        let g = gate(t.path());
        assert!(matches!(
            g.check("junc/secret", Disk::Worktree, false),
            Err(Refusal::Link(_))
        ));
        assert!(matches!(
            g.check("junc", Disk::Worktree, true),
            Err(Refusal::Link(_))
        ));
    }
}
