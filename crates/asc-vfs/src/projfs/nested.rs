//! Nested worktree paths (SPEC §9.6 "Nested paths", spike S11).
//!
//! ProjFS cannot virtualise a directory inside another virtualization root, and Claude Code's
//! default worktree path is `<repo>/.claude/worktrees/<name>`, inside the parent workspace. A
//! workspace whose requested path lies inside another workspace's root is therefore mounted at
//! `$ASC_HOME/wt/<ws-id>` and a directory junction at the requested path points there. The
//! workspace row keeps the requested path as `path` and the real root as `mount`. S11 is that a
//! junction inside one root, pointing to another root, is served by the second provider. Where a
//! junction cannot be made the caller uses plain mode ([`Placement::Plain`]).

use std::io;
use std::path::{Component, Path, PathBuf};

/// Longest workspace id accepted as a directory name.
const MAX_ID: usize = 64;

/// Where a workspace is mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The requested path is not inside another root: mount it there.
    Direct,
    /// Mount at `mount` and link the requested path to it.
    Junction { mount: PathBuf },
    /// No junction can be made: the caller falls back to plain mode.
    Plain,
}

/// `$ASC_HOME/wt/<ws-id>`. The id is a uuid in text form; anything else is refused, so a hostile id
/// cannot climb out of `wt`.
pub fn mount_root(asc_home: &Path, ws_id: &str) -> io::Result<PathBuf> {
    let ok = !ws_id.is_empty()
        && ws_id.len() <= MAX_ID
        && ws_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !ok {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a workspace id is letters, digits and hyphens",
        ));
    }
    Ok(asc_home.join("wt").join(ws_id))
}

/// The normal components of `p`, case-folded (Windows paths are case-insensitive), with the
/// verbatim prefix `\\?\` dropped.
fn parts(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            Component::Prefix(pre) => {
                let s = pre.as_os_str().to_string_lossy().to_lowercase();
                Some(s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned())
            }
            _ => None,
        })
        .collect()
}

/// Whether `requested` lies strictly inside one of the `roots` (other workspaces' roots).
/// Lexical: `..` is refused by the caller's path normalisation, and a link in the way is the
/// caller's business (`SafeRoot`).
pub fn is_nested(requested: &Path, roots: &[PathBuf]) -> bool {
    let want = parts(requested);
    roots.iter().any(|r| {
        let root = parts(r);
        !root.is_empty() && want.len() > root.len() && want[..root.len()] == root[..]
    })
}

/// Creates the junction `link` -> `mount`. `link` must not exist.
pub fn link(link: &Path, mount: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let (l, m) = (link.to_string_lossy(), mount.to_string_lossy());
    // `cmd` parses these; the characters that it treats specially are refused.
    if l.contains(['%', '"', '^', '&', '|', '<', '>', '\n', '\r'])
        || m.contains(['%', '"', '^', '&', '|', '<', '>', '\n', '\r'])
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path has characters that cannot be used for a junction",
        ));
    }
    if std::fs::symlink_metadata(link).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = std::process::Command::new("cmd")
        .raw_arg(format!("/D /C mklink /J \"{l}\" \"{m}\""))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other("mklink /J failed"));
    }
    Ok(())
}

/// Removes the junction at `link` without touching its target. Anything that is not a reparse
/// point is left alone.
pub fn unlink(link: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    let m = std::fs::symlink_metadata(link)?;
    if m.file_attributes() & super::ffi::FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a junction or link",
        ));
    }
    std::fs::remove_dir(link)
}

/// Decides and prepares the placement of a workspace at `requested`, given the roots of the other
/// workspaces. For a nested path the mount directory is created and the junction is made.
pub fn place(
    asc_home: &Path,
    ws_id: &str,
    requested: &Path,
    others: &[PathBuf],
) -> io::Result<Placement> {
    if !is_nested(requested, others) {
        return Ok(Placement::Direct);
    }
    let mount = mount_root(asc_home, ws_id)?;
    std::fs::create_dir_all(&mount)?;
    match link(requested, &mount) {
        Ok(()) => Ok(Placement::Junction { mount }),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            let _ = std::fs::remove_dir(&mount);
            Ok(Placement::Plain)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn mount_root_is_under_wt() {
        let r = mount_root(
            Path::new(r"C:\home"),
            "6b1f3a90-1122-4c33-8455-010203040506",
        )
        .unwrap();
        assert_eq!(r, p(r"C:\home\wt\6b1f3a90-1122-4c33-8455-010203040506"));
        for bad in ["", "..", "a/b", r"a\b", "a b", ".", &"x".repeat(65)] {
            assert!(mount_root(Path::new(r"C:\home"), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn nested_is_strictly_inside_and_case_insensitive() {
        let roots = [p(r"D:\Repos\ASC")];
        assert!(is_nested(&p(r"D:\repos\asc\.claude\worktrees\a"), &roots));
        assert!(is_nested(&p(r"\\?\D:\Repos\ASC\x"), &roots));
        assert!(!is_nested(&p(r"D:\Repos\ASC"), &roots));
        assert!(!is_nested(&p(r"D:\Repos\Asc2\x"), &roots));
        assert!(!is_nested(&p(r"E:\Repos\ASC\x"), &roots));
        assert!(!is_nested(&p(r"D:\Repos\ASC\x"), &[]));
    }

    #[test]
    fn place_is_direct_outside_other_roots() {
        let dir = tempfile::tempdir().unwrap();
        let got = place(
            dir.path(),
            "abc",
            &dir.path().join("w"),
            &[p(r"Z:\elsewhere")],
        )
        .unwrap();
        assert_eq!(got, Placement::Direct);
    }

    #[test]
    fn link_makes_a_junction_and_unlink_keeps_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("f"), b"x").unwrap();
        let l = dir.path().join("parent").join("link");
        link(&l, &target).unwrap();
        assert_eq!(std::fs::read(l.join("f")).unwrap(), b"x");
        assert!(
            link(&l, &target).is_err(),
            "an existing link is not replaced"
        );
        unlink(&l).unwrap();
        assert!(target.join("f").is_file(), "the target survives");
        assert!(unlink(&target).is_err(), "a real directory is not removed");
    }

    #[test]
    fn link_refuses_shell_metacharacters() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("a&b");
        assert!(link(&bad, dir.path()).is_err());
    }
}

#[cfg(test)]
mod virtual_tests {
    use super::*;
    use crate::projfs::switch::lab::Lab;
    use crate::projfs::switch::tests::V1;

    const OTHER: &[(&str, &[u8])] = &[("README", b"nested readme"), ("dir/f.txt", b"f")];
    const GUID_B: [u8; 16] = [9, 9, 9, 9, 8, 8, 7, 7, 6, 6, 5, 5, 4, 4, 3, 3];

    /// Spike S11: a junction inside one virtualization root, pointing at another root, is served
    /// by the second provider.
    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn s11_a_junction_in_one_root_serves_another_root() {
        let Some(outer) = Lab::new(V1) else { return };
        let Some(inner) = Lab::with_guid(GUID_B, OTHER) else {
            return;
        };
        let l = outer.at("worktrees/wt1");
        link(&l, &inner.root()).unwrap();
        assert_eq!(std::fs::read(l.join("README")).unwrap(), b"nested readme");
        assert_eq!(std::fs::read(l.join("dir").join("f.txt")).unwrap(), b"f");
        // Writes through the junction reach the inner workspace's journal, not the outer one.
        std::fs::write(l.join("new.txt"), b"n").unwrap();
        assert!(inner.root().join("new.txt").is_file());
        // The outer root still serves its own files.
        assert_eq!(std::fs::read(outer.at("a.txt")).unwrap(), b"alpha");
        unlink(&l).unwrap();
        assert!(inner.root().join("README").is_file());
    }

    #[test]
    #[ignore = "needs Windows with Client-ProjFS enabled"]
    fn place_mounts_under_wt_and_links_the_requested_path() {
        let Some(outer) = Lab::new(V1) else { return };
        let home = tempfile::tempdir().unwrap();
        let requested = outer.at(".claude/worktrees/agent");
        let got = place(
            home.path(),
            "11112222-3333-4444-5555-666677778888",
            &requested,
            &[outer.root()],
        )
        .unwrap();
        let mount = home
            .path()
            .join("wt")
            .join("11112222-3333-4444-5555-666677778888");
        assert_eq!(
            got,
            Placement::Junction {
                mount: mount.clone()
            }
        );
        std::fs::write(mount.join("f"), b"x").unwrap();
        assert_eq!(std::fs::read(requested.join("f")).unwrap(), b"x");
        unlink(&requested).unwrap();
    }
}
