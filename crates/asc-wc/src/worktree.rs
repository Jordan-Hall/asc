//! Plain worktrees on disk (SPEC §9.3): where a requested path lies relative to the other
//! workspaces, the mount link of a nested path, and removal that never follows a link.
//!
//! Every path here comes from the command line or a hook's JSON, so it is untrusted. Nothing
//! follows a link at an ancestor below the enclosing workspace, and removal deletes a link
//! itself, never what it points at.

use std::io;
use std::path::{Component, Path, PathBuf};

use crate::safe_path::{is_link, remove_link};

/// Longest accepted worktree name (`agent/<name>` is a branch name, the name a directory).
pub const MAX_NAME: usize = 100;
/// Longest `--ttl`: one year, in seconds.
pub const MAX_TTL_SECS: u64 = 365 * 24 * 60 * 60;
/// The `--ttl` of an ephemeral worktree that names none (SPEC §9.3).
pub const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// `base` joined with `path` when relative, with `.` and `..` resolved lexically. The path may
/// not exist yet, so nothing touches the file system.
pub fn absolutize(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir | Component::Prefix(_))
                ) {
                    out.pop();
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `<root>/.claude/worktrees/<name>`: where Claude Code's agents get their worktrees.
pub fn agent_path(root: &Path, name: &str) -> PathBuf {
    root.join(".claude").join("worktrees").join(name)
}

/// A comparison key for a path: `/` separators, no trailing `/`, folded where the file system
/// ignores case.
pub fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.trim_end_matches('/').to_owned();
    if cfg!(any(windows, target_os = "macos")) {
        text.to_lowercase()
    } else {
        text
    }
}

/// Whether `path` lies strictly inside `root` (equal paths are not inside).
pub fn is_inside(path: &Path, root: &Path) -> bool {
    let (p, r) = (path_key(path), path_key(root));
    p.len() > r.len() && p.starts_with(&r) && p.as_bytes()[r.len()] == b'/'
}

/// Whether `name` can be a worktree name: one plain path component of letters, digits, `.`,
/// `_` and `-`, not starting with `.` or `-`, at most [`MAX_NAME`] bytes.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with(['.', '-'])
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.ends_with(".lock")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A `--ttl` such as `90s`, `30m`, `12h`, `7d` or `2w` in seconds; at most [`MAX_TTL_SECS`].
pub fn parse_ttl(text: &str) -> Option<u64> {
    let (digits, unit) = text.split_at(text.len().checked_sub(1)?);
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    let secs = n.checked_mul(match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    })?;
    (1..=MAX_TTL_SECS).contains(&secs).then_some(secs)
}

/// What is at a path a worktree is to be created at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occupancy {
    /// Nothing there.
    Missing,
    /// An empty directory (git reuses it).
    EmptyDir,
    /// A file, a link or a directory with content.
    Occupied,
}

/// What is at `path`, without following a link at `path` itself.
pub fn occupancy(path: &Path) -> io::Result<Occupancy> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Occupancy::Missing),
        Err(e) => Err(e),
        Ok(m) if is_link(&m) || !m.is_dir() => Ok(Occupancy::Occupied),
        Ok(_) => Ok(if std::fs::read_dir(path)?.next().is_none() {
            Occupancy::EmptyDir
        } else {
            Occupancy::Occupied
        }),
    }
}

/// Creates the directories of `dir` below `boundary` one by one and refuses a link at any of
/// them, existing or just made: a worktree is never created beyond a link inside a work tree.
/// `dir` must lie inside `boundary`.
pub fn make_dirs_below(boundary: &Path, dir: &Path) -> io::Result<()> {
    let refuse = |p: &Path| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a link or not a directory", p.display()),
        )
    };
    let rel = dir
        .strip_prefix(boundary)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path is outside its boundary"))?;
    let mut cur = boundary.to_owned();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not normal",
            ));
        };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if is_link(&m) || !m.is_dir() => return Err(refuse(&cur)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match std::fs::create_dir(&cur) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
                if !std::fs::symlink_metadata(&cur).is_ok_and(|m| m.is_dir() && !is_link(&m)) {
                    return Err(refuse(&cur));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Creates the directory link `link` to `target`: a symlink, or on Windows without the
/// symlink privilege a junction. The parent of `link` must exist and `link` must not.
pub fn make_dir_link(link: &Path, target: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).or_else(|_| junction(link, target))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (link, target);
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// `mklink /J`. The arguments pass through `cmd`, so a path with a shell metacharacter is
/// refused.
#[cfg(windows)]
fn junction(link: &Path, target: &Path) -> io::Result<()> {
    let bad = |p: &Path| {
        p.to_string_lossy()
            .chars()
            .any(|c| matches!(c, '&' | '|' | '<' | '>' | '^' | '%' | '"' | '!'))
    };
    if bad(link) || bad(target) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has a character cmd would interpret",
        ));
    }
    let out = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        ))
    }
}

/// Whether `path` is a link or junction.
pub fn is_link_at(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| is_link(&m))
}

/// Removes `path`: a link or junction is removed itself, a directory with everything in it
/// (links inside are removed, never followed). A missing path is not an error.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if is_link(&m) => remove_link(path),
        Ok(m) if m.is_dir() => remove_dir_all_unlinked(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

fn remove_dir_all_unlinked(dir: &Path) -> io::Result<()> {
    // `remove_dir_all` removes a link inside the tree without following it; the permission
    // retry covers read-only files, which Windows will not delete.
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(first) => {
            clear_readonly(dir, 0);
            std::fs::remove_dir_all(dir).map_err(|_| first)
        }
    }
}

fn clear_readonly(dir: &Path, depth: usize) {
    if depth > 128 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if is_link(&meta) {
            continue;
        }
        if meta.is_dir() {
            clear_readonly(&entry.path(), depth + 1);
        } else if meta.permissions().readonly() {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(entry.path(), perms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_resolves_dots_without_touching_the_disk() {
        let base = Path::new(if cfg!(windows) { "C:\\a\\b" } else { "/a/b" });
        let got = absolutize(base, Path::new("../c/./d"));
        assert_eq!(
            path_key(&got),
            path_key(&absolutize(base, Path::new("../c/d")))
        );
        assert!(path_key(&got).ends_with("a/c/d"), "{got:?}");
        // `..` never climbs above the root.
        let top = absolutize(base, Path::new("../../../../x"));
        assert!(path_key(&top).ends_with("/x"), "{top:?}");
    }

    #[tokio::test]
    async fn lock_and_location_are_stored() {
        use asc_state::state::workspaces::{self, Mode, NewWorkspace};
        use asc_state::{AscHome, StateDb, WorkspaceId};
        let t = tempfile::tempdir().unwrap();
        let home = AscHome::open(t.path().join("home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let mk = |id: WorkspaceId, path: &str| NewWorkspace {
            id,
            repo: asc_store::refs::RepoKey(1),
            path: path.into(),
            mount: path.into(),
            mode: Mode::Plain,
            head: "refs/heads/master".into(),
            base_cs: None,
            sparse: None,
            ephemeral_until: None,
        };
        let (a, b) = (WorkspaceId::new(), WorkspaceId::new());
        workspaces::insert(&db, &mk(a, "/a")).await.unwrap();
        workspaces::insert(&db, &mk(b, "/b")).await.unwrap();
        // Registered in the same second, listed in registration order.
        let ids: Vec<_> = workspaces::list(&db, Some(asc_store::refs::RepoKey(1)))
            .await
            .unwrap()
            .iter()
            .map(|w| w.ws.id)
            .collect();
        assert_eq!(ids, [a, b]);
        workspaces::set_locked(&db, b, Some("why")).await.unwrap();
        assert_eq!(
            workspaces::get(&db, b)
                .await
                .unwrap()
                .unwrap()
                .locked
                .as_deref(),
            Some("why")
        );
        workspaces::set_locked(&db, b, Some("")).await.unwrap();
        assert_eq!(
            workspaces::get(&db, b)
                .await
                .unwrap()
                .unwrap()
                .locked
                .as_deref(),
            Some("")
        );
        workspaces::set_locked(&db, b, None).await.unwrap();
        assert_eq!(workspaces::get(&db, b).await.unwrap().unwrap().locked, None);
        workspaces::set_location(&db, b, "/c", "/m").await.unwrap();
        let row = workspaces::get(&db, b).await.unwrap().unwrap();
        assert_eq!((row.ws.path.as_str(), row.ws.mount.as_str()), ("/c", "/m"));
        assert!(
            workspaces::set_locked(&db, WorkspaceId::new(), None)
                .await
                .is_err()
        );
    }

    #[test]
    fn inside_means_strictly_below_on_a_component_boundary() {
        let root = Path::new("/r/repo");
        assert!(is_inside(Path::new("/r/repo/.claude/worktrees/x"), root));
        assert!(!is_inside(Path::new("/r/repo"), root));
        assert!(!is_inside(Path::new("/r/repo2/x"), root));
        assert!(!is_inside(Path::new("/r/other"), root));
        assert!(is_inside(Path::new("/r/repo/"), Path::new("/r")));
    }

    #[test]
    fn names_are_one_plain_component() {
        for ok in ["bold-fox", "a1", "x_y.z", "A"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "", ".", "..", ".hidden", "-x", "a/b", "a\\b", "a..b", "x.lock", "a b", "é", "x.",
            "a:b",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(!valid_name(&"a".repeat(MAX_NAME + 1)));
        assert!(valid_name(&"a".repeat(MAX_NAME)));
    }

    #[test]
    fn ttl_units_and_bounds() {
        assert_eq!(parse_ttl("90s"), Some(90));
        assert_eq!(parse_ttl("7d"), Some(7 * 86_400));
        assert_eq!(parse_ttl("2w"), Some(2 * 604_800));
        assert_eq!(parse_ttl("365d"), Some(MAX_TTL_SECS));
        for bad in [
            "",
            "d",
            "0d",
            "7",
            "7x",
            "-1d",
            "1.5d",
            "366d",
            "99999999999d",
            "+7d",
        ] {
            assert_eq!(parse_ttl(bad), None, "{bad}");
        }
    }

    #[test]
    fn occupancy_tells_missing_empty_and_used() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("w");
        assert_eq!(occupancy(&p).unwrap(), Occupancy::Missing);
        std::fs::create_dir(&p).unwrap();
        assert_eq!(occupancy(&p).unwrap(), Occupancy::EmptyDir);
        std::fs::write(p.join("f"), "x").unwrap();
        assert_eq!(occupancy(&p).unwrap(), Occupancy::Occupied);
        let f = t.path().join("file");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(occupancy(&f).unwrap(), Occupancy::Occupied);
    }

    #[test]
    fn dirs_below_a_boundary_are_made_and_a_link_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let boundary = t.path().join("repo");
        std::fs::create_dir(&boundary).unwrap();
        let dir = boundary.join(".claude").join("worktrees");
        make_dirs_below(&boundary, &dir).unwrap();
        assert!(dir.is_dir());
        make_dirs_below(&boundary, &dir).unwrap();
        assert!(make_dirs_below(&boundary, &t.path().join("elsewhere")).is_err());

        let outside = t.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let hop = boundary.join("hop");
        if make_dir_link(&hop, &outside).is_ok() {
            let err = make_dirs_below(&boundary, &hop.join("deeper")).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(!outside.join("deeper").exists());
        }
    }

    #[test]
    fn removing_a_link_leaves_its_target_and_a_tree_goes_whole() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), "x").unwrap();
        let link = t.path().join("link");
        if make_dir_link(&link, &target).is_ok() {
            assert!(is_link_at(&link));
            remove_tree(&link).unwrap();
            assert!(!link.exists() && target.join("keep").is_file());
        }
        let tree = t.path().join("tree");
        std::fs::create_dir_all(tree.join("a").join("b")).unwrap();
        std::fs::write(tree.join("a").join("f"), "x").unwrap();
        let inner = tree.join("a").join("hop");
        let _ = make_dir_link(&inner, &target);
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert!(
            target.join("keep").is_file(),
            "a link inside the tree was followed"
        );
        remove_tree(&tree).unwrap();
    }
}
