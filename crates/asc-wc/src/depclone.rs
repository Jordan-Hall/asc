//! Installed dependencies for a new worktree without a second copy on disk (SPEC §9.3.2).
//!
//! A plain worktree starts without `node_modules`, so the package manager runs again and its build
//! caches (Rspack's and webpack's `node_modules/.cache`, Vite's `node_modules/.vite`) start cold.
//! When the new worktree pins the same dependencies as the worktree it was made from (every
//! lockfile at the root and the `package.json` beside each `node_modules` are byte for byte the
//! same), this module clones those directories, and turbo's content-addressed `.turbo/cache`, by
//! **reflink**: the copies share their blocks with the originals until one side writes, so the
//! clone costs metadata only. A volume that cannot reflink (NTFS, ext4) gets nothing and the
//! package manager installs as before. asc never falls back to a byte copy (a second
//! `node_modules` on disk) or to hard links (an edit through one would change the other worktree).

use std::fs::{self, File, Metadata};
use std::io;
use std::path::Path;

/// Lockfiles that pin a JavaScript dependency tree. Every one present in either worktree must be
/// the same in both.
pub const LOCKFILES: [&str; 6] = [
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

/// Directory levels below the root searched for `node_modules` (workspace packages).
pub const MAX_DEPTH: usize = 6;

/// Directories never searched: asc's own, git's, and build outputs that hold no `node_modules`.
const SKIP: [&str; 8] = [
    ".git",
    ".asc",
    "dist",
    ".turbo",
    ".next",
    "target",
    ".venv",
    "__pycache__",
];

/// Build caches cloned whatever the lockfiles say: their entries are named by content hash.
pub const CONTENT_CACHES: [&str; 1] = [".turbo/cache"];

/// What a clone did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cloned {
    /// The directories made, relative to the root, `/`-separated.
    pub dirs: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    /// The lockfile that differs, when the dependencies were left to the package manager.
    pub lockfile_differs: Option<String>,
}

/// Why nothing was cloned.
#[derive(Debug, PartialEq, Eq)]
pub enum Skipped {
    /// The source worktree has no lockfile with `node_modules`, and no content cache.
    Nothing,
    /// A lockfile differs, or is in one worktree only: the dependencies must be installed.
    LockfilesDiffer(String),
    /// The volume cannot reflink; cloning would mean a full second copy.
    NoReflink,
}

/// Clones the installed dependencies and content caches of `src` into the new worktree `dst`.
/// A failure part way removes the directory being made; directories finished before it stay.
pub fn clone_deps(src: &Path, dst: &Path) -> io::Result<Result<Cloned, Skipped>> {
    let lock_match = lockfiles_match(src, dst)?;
    let mut todo: Vec<String> = Vec::new();
    if lock_match == Ok(true) {
        find_node_modules(src, dst, "", 0, &mut todo)?;
    }
    for c in CONTENT_CACHES {
        if is_real_dir(&src.join(c)) && !exists(&dst.join(c))? {
            todo.push(c.to_owned());
        }
    }
    if todo.is_empty() {
        return Ok(Err(match lock_match {
            Err(name) => Skipped::LockfilesDiffer(name),
            Ok(_) => Skipped::Nothing,
        }));
    }
    if !can_reflink(src, dst)? {
        return Ok(Err(Skipped::NoReflink));
    }
    let mut out = Cloned {
        lockfile_differs: lock_match.err(),
        ..Cloned::default()
    };
    for rel in todo {
        let from = src.join(&rel);
        let to = dst.join(&rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let name = to
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let tmp = to.with_file_name(format!("{name}.asc-clone-{}", std::process::id()));
        let mut walk = Walk {
            src,
            dst,
            files: 0,
            bytes: 0,
        };
        let made = walk.tree(&from, &tmp).and_then(|()| fs::rename(&tmp, &to));
        if let Err(e) = made {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        out.files += walk.files;
        out.bytes += walk.bytes;
        out.dirs.push(rel);
    }
    Ok(Ok(out))
}

/// `Ok(true)` when some lockfile exists and all are the same in both; `Ok(false)` with none;
/// `Err(name)` naming the first that differs.
fn lockfiles_match(src: &Path, dst: &Path) -> io::Result<Result<bool, String>> {
    let mut any = false;
    for name in LOCKFILES {
        let (a, b) = (read_opt(&src.join(name))?, read_opt(&dst.join(name))?);
        if a != b {
            return Ok(Err(name.to_owned()));
        }
        any |= a.is_some();
    }
    Ok(Ok(any))
}

/// The bytes of a regular file, or `None` when there is none (a link is not followed).
fn read_opt(p: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_file() => fs::read(p).map(Some),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn exists(p: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Collects `rel/node_modules` for every real directory of `src` (to [`MAX_DEPTH`]) whose
/// `node_modules` is a real directory, whose `package.json` is the same in both worktrees and
/// whose place in `dst` exists and is free.
fn find_node_modules(
    src: &Path,
    dst: &Path,
    rel: &str,
    depth: usize,
    out: &mut Vec<String>,
) -> io::Result<()> {
    let here = src.join(rel);
    if is_real_dir(&here.join("node_modules"))
        && is_real_dir(&dst.join(rel))
        && !exists(&dst.join(rel).join("node_modules"))?
    {
        let pkg = |root: &Path| read_opt(&root.join(rel).join("package.json"));
        let theirs = pkg(src)?;
        if theirs.is_some() && theirs == pkg(dst)? {
            out.push(join(rel, "node_modules"));
        }
    }
    if depth >= MAX_DEPTH {
        return Ok(());
    }
    let mut names = Vec::new();
    for e in fs::read_dir(&here)? {
        let e = e?;
        if !e.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == "node_modules" || SKIP.contains(&name.as_str()) {
            continue;
        }
        names.push(name);
    }
    names.sort();
    for name in names {
        find_node_modules(src, dst, &join(rel, &name), depth + 1, out)?;
    }
    Ok(())
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_owned()
    } else {
        format!("{rel}/{name}")
    }
}

/// Whether the volume of `dir` reflinks: a small probe file in `dir` is cloned to a second name
/// in `dir`, and both are removed afterwards. `dir` must exist and be writable.
pub fn volume_reflinks(dir: &Path) -> io::Result<bool> {
    let pid = std::process::id();
    let origin = dir.join(format!(".asc-reflink-probe-{pid}-a"));
    let copy = dir.join(format!(".asc-reflink-probe-{pid}-b"));
    let _ = fs::remove_file(&copy);
    fs::write(&origin, b"asc reflink probe")?;
    let ok = reflink_copy::reflink(&origin, &copy).is_ok();
    let _ = fs::remove_file(&origin);
    let _ = fs::remove_file(&copy);
    Ok(ok)
}

/// Whether a file of `src` reflinks into `dst`: a small probe in each, removed afterwards.
fn can_reflink(src: &Path, dst: &Path) -> io::Result<bool> {
    let pid = std::process::id();
    let origin = src.join(format!(".asc-reflink-probe-{pid}"));
    let copy = dst.join(format!(".asc-reflink-probe-{pid}"));
    fs::write(&origin, b"asc reflink probe")?;
    let ok = reflink_copy::reflink(&origin, &copy).is_ok();
    let _ = fs::remove_file(&origin);
    let _ = fs::remove_file(&copy);
    Ok(ok)
}

struct Walk<'a> {
    src: &'a Path,
    dst: &'a Path,
    files: u64,
    bytes: u64,
}

impl Walk<'_> {
    fn tree(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        fs::create_dir(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            let (a, b) = (e.path(), to.join(e.file_name()));
            let m = fs::symlink_metadata(&a)?;
            if m.file_type().is_symlink() {
                self.link(&a, &b)?;
            } else if m.is_dir() {
                self.tree(&a, &b)?;
            } else if m.is_file() {
                reflink_copy::reflink(&a, &b)?;
                keep_meta(&b, &m)?;
                self.files += 1;
                self.bytes += m.len();
            }
        }
        if let Ok(m) = fs::metadata(from) {
            dir_time(to, &m);
        }
        Ok(())
    }

    /// A link is made again with the same target; an absolute target inside the source worktree
    /// points at the same place in the new one.
    fn link(&self, a: &Path, b: &Path) -> io::Result<()> {
        let target = fs::read_link(a)?;
        let target = match target.strip_prefix(self.src) {
            Ok(rest) if target.is_absolute() => self.dst.join(rest),
            _ => target,
        };
        if fs::metadata(a).is_ok_and(|m| m.is_dir()) {
            crate::worktree::make_dir_link(b, &target)
        } else {
            file_link(&target, b)
        }
    }
}

fn file_link(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// The mode and modification time of the original: package managers compare them (npm trusts its
/// hidden lockfile only when it is newer than the package folders).
fn keep_meta(path: &Path, m: &Metadata) -> io::Result<()> {
    if let Ok(t) = m.modified() {
        File::options().write(true).open(path)?.set_modified(t)?;
    }
    fs::set_permissions(path, m.permissions())
}

/// A directory's modification time, where the platform lets a directory be opened for it.
fn dir_time(path: &Path, m: &Metadata) {
    #[cfg(unix)]
    if let (Ok(t), Ok(f)) = (m.modified(), File::open(path)) {
        let _ = f.set_modified(t);
    }
    #[cfg(not(unix))]
    let _ = (path, m);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// Two worktrees with the same pins: a root `node_modules` with a cache inside, a workspace
    /// package's own `node_modules`, and turbo's cache.
    fn pair() -> (tempfile::TempDir, tempfile::TempDir) {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for root in [a.path(), b.path()] {
            write(root, "bun.lock", "lock v1");
            write(root, "package.json", "{\"name\":\"root\"}");
            write(root, "packages/ui/package.json", "{\"name\":\"ui\"}");
            write(root, "packages/ui/src/index.ts", "export {}");
        }
        write(
            a.path(),
            "node_modules/left-pad/index.js",
            "module.exports = 1",
        );
        write(a.path(), "node_modules/.cache/rspack/pack", "cache bytes");
        write(a.path(), "node_modules/.vite/deps/_metadata.json", "{}");
        write(a.path(), "packages/ui/node_modules/react/index.js", "react");
        write(a.path(), ".turbo/cache/0abc.tar.zst", "turbo");
        (a, b)
    }

    #[test]
    fn the_search_finds_every_pinned_node_modules() {
        let (a, b) = pair();
        assert_eq!(lockfiles_match(a.path(), b.path()).unwrap(), Ok(true));
        let mut found = Vec::new();
        find_node_modules(a.path(), b.path(), "", 0, &mut found).unwrap();
        assert_eq!(found, ["node_modules", "packages/ui/node_modules"]);

        // A package.json that differs leaves that package to the package manager.
        write(
            b.path(),
            "packages/ui/package.json",
            "{\"name\":\"ui\",\"v\":2}",
        );
        let mut found = Vec::new();
        find_node_modules(a.path(), b.path(), "", 0, &mut found).unwrap();
        assert_eq!(found, ["node_modules"]);
    }

    #[test]
    fn a_lockfile_that_differs_or_is_missing_stops_the_dependencies() {
        let (a, b) = pair();
        write(b.path(), "bun.lock", "lock v2");
        assert_eq!(
            lockfiles_match(a.path(), b.path()).unwrap(),
            Err("bun.lock".to_owned())
        );
        write(b.path(), "bun.lock", "lock v1");
        write(b.path(), "yarn.lock", "extra");
        assert_eq!(
            lockfiles_match(a.path(), b.path()).unwrap(),
            Err("yarn.lock".to_owned())
        );
        let none = tempfile::tempdir().unwrap();
        assert_eq!(
            lockfiles_match(none.path(), none.path()).unwrap(),
            Ok(false)
        );
    }

    #[test]
    fn the_volume_probe_leaves_nothing_behind() {
        let d = tempfile::tempdir().unwrap();
        volume_reflinks(d.path()).unwrap();
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0);
        assert!(volume_reflinks(&d.path().join("missing")).is_err());
    }

    #[test]
    fn a_clone_is_by_reflink_or_not_at_all() {
        let (a, b) = pair();
        match clone_deps(a.path(), b.path()).unwrap() {
            Ok(c) => {
                assert_eq!(
                    c.dirs,
                    ["node_modules", "packages/ui/node_modules", ".turbo/cache"]
                );
                assert_eq!(c.files, 5);
                for rel in [
                    "node_modules/left-pad/index.js",
                    "node_modules/.cache/rspack/pack",
                    "node_modules/.vite/deps/_metadata.json",
                    "packages/ui/node_modules/react/index.js",
                    ".turbo/cache/0abc.tar.zst",
                ] {
                    assert_eq!(
                        fs::read(b.path().join(rel)).unwrap(),
                        fs::read(a.path().join(rel)).unwrap(),
                        "{rel}"
                    );
                    let (ma, mb) = (
                        fs::metadata(a.path().join(rel)).unwrap(),
                        fs::metadata(b.path().join(rel)).unwrap(),
                    );
                    assert_eq!(ma.modified().unwrap(), mb.modified().unwrap(), "{rel}");
                }
                // A write to the clone leaves the original alone.
                fs::write(b.path().join("node_modules/left-pad/index.js"), "changed").unwrap();
                assert_eq!(
                    fs::read(a.path().join("node_modules/left-pad/index.js")).unwrap(),
                    b"module.exports = 1"
                );
                // Once cloned, a second run finds nothing left to do.
                assert_eq!(
                    clone_deps(a.path(), b.path()).unwrap(),
                    Err(Skipped::Nothing)
                );
            }
            Err(Skipped::NoReflink) => {
                // This volume cannot reflink: nothing was written, not even a probe.
                for rel in ["node_modules", "packages/ui/node_modules", ".turbo"] {
                    assert!(!b.path().join(rel).exists(), "{rel}");
                }
                let stray: Vec<_> = fs::read_dir(b.path())
                    .unwrap()
                    .chain(fs::read_dir(a.path()).unwrap())
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().contains(".asc-"))
                    .collect();
                assert!(stray.is_empty(), "{stray:?}");
            }
            Err(other) => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn without_pins_only_the_content_cache_is_a_candidate() {
        let (a, b) = pair();
        write(b.path(), "bun.lock", "lock v2");
        match clone_deps(a.path(), b.path()).unwrap() {
            Ok(c) => {
                assert_eq!(c.dirs, [".turbo/cache"]);
                assert_eq!(c.lockfile_differs.as_deref(), Some("bun.lock"));
            }
            Err(s) => assert_eq!(s, Skipped::NoReflink),
        }
        fs::remove_dir_all(a.path().join(".turbo")).unwrap();
        let _ = fs::remove_dir_all(b.path().join(".turbo"));
        assert_eq!(
            clone_deps(a.path(), b.path()).unwrap(),
            Err(Skipped::LockfilesDiffer("bun.lock".to_owned()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_keep_their_targets_and_absolute_ones_move_with_the_worktree() {
        let (a, b) = pair();
        std::os::unix::fs::symlink("../../packages/ui", a.path().join("node_modules/ui")).unwrap();
        std::os::unix::fs::symlink(
            a.path().join("packages/ui/src/index.ts"),
            a.path().join("node_modules/abs.ts"),
        )
        .unwrap();
        let walk = Walk {
            src: a.path(),
            dst: b.path(),
            files: 0,
            bytes: 0,
        };
        fs::create_dir_all(b.path().join("node_modules")).unwrap();
        walk.link(
            &a.path().join("node_modules/ui"),
            &b.path().join("node_modules/ui"),
        )
        .unwrap();
        walk.link(
            &a.path().join("node_modules/abs.ts"),
            &b.path().join("node_modules/abs.ts"),
        )
        .unwrap();
        assert_eq!(
            fs::read_link(b.path().join("node_modules/ui")).unwrap(),
            Path::new("../../packages/ui")
        );
        assert_eq!(
            fs::read_link(b.path().join("node_modules/abs.ts")).unwrap(),
            b.path().join("packages/ui/src/index.ts")
        );
    }
}
