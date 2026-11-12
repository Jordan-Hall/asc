//! Linux mount facts virtual CI checks before it mounts (SPEC §12.3 "Virtual CI"): whether the
//! mount a directory sits on propagates new mounts to its peers (`shared:` in
//! `/proc/self/mountinfo`), whether `/etc/fuse.conf` lets a user other than root mount with
//! `allow_other`, and the effective uid from `/proc/self/status`. The parsers take the files'
//! text, so they are tested on every platform.

use std::path::{Component, Path};

/// One line of `/proc/self/mountinfo` that matters here.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    point: Vec<String>,
    shared: bool,
}

/// Undoes the octal escapes (`\040` for a space) the kernel writes in mount points.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && (b'0'..=b'3').contains(&b[i + 1])
            && b[i + 2..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn components(p: &str) -> Vec<String> {
    Path::new(p)
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn parse_line(line: &str) -> Option<Entry> {
    let fields: Vec<&str> = line.split(' ').collect();
    // id, parent, major:minor, root, mount point, options, optional fields..., "-", fs type...
    let point = fields.get(4)?;
    let sep = fields.iter().skip(6).position(|f| *f == "-")? + 6;
    let shared = fields[6..sep].iter().any(|f| f.starts_with("shared:"));
    Some(Entry {
        point: components(&unescape(point)),
        shared,
    })
}

/// Whether the mount `dir` sits on is shared, from `mountinfo` (the text of
/// `/proc/self/mountinfo`); `None` when no line covers it. `dir` should be absolute and free of
/// symbolic links. Of the mount points that are `dir` or one of its parents, the longest wins,
/// and of equal ones the last line (the mount on top).
pub fn shared_propagation(mountinfo: &str, dir: &Path) -> Option<bool> {
    let want = components(&dir.to_string_lossy());
    let mut best: Option<Entry> = None;
    for e in mountinfo.lines().filter_map(parse_line) {
        if want.starts_with(&e.point)
            && best.as_ref().is_none_or(|b| e.point.len() >= b.point.len())
        {
            best = Some(e);
        }
    }
    best.map(|e| e.shared)
}

/// Whether `/etc/fuse.conf` (its text) has an uncommented `user_allow_other`.
pub fn user_allow_other(fuse_conf: &str) -> bool {
    fuse_conf
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .any(|l| l == "user_allow_other")
}

/// The effective uid in `/proc/self/status` (its text): the second number of `Uid:`.
pub fn effective_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
30 22 0:25 / /proc rw,nosuid - proc proc rw
41 22 8:1 /srv/runner /srv/runner rw,relatime shared:7 - ext4 /dev/sda1 rw
42 22 8:2 / /work rw,relatime - ext4 /dev/sdb rw
43 42 8:2 /a\\040b /work/a\\040b rw master:3 - ext4 /dev/sdb rw
44 42 8:2 / /work/over rw - ext4 /dev/sdb rw
45 42 8:2 / /work/over rw shared:9 - ext4 /dev/sdb rw
";

    #[test]
    fn the_longest_covering_mount_decides() {
        let p = |s: &str| shared_propagation(INFO, Path::new(s));
        assert_eq!(p("/srv/runner/_work/r/r"), Some(true));
        assert_eq!(p("/srv/runner"), Some(true));
        assert_eq!(p("/work/job"), Some(false));
        // A name that only starts like a mount point is not under it.
        assert_eq!(
            p("/srv/runnerx/job"),
            Some(true),
            "falls back to /, which is shared"
        );
        assert_eq!(
            p("/work/a b/c"),
            Some(false),
            "master: alone is a slave, not shared"
        );
        assert_eq!(p("/work/over/x"), Some(true), "the mount on top wins");
    }

    #[test]
    fn no_covering_line_is_unknown() {
        assert_eq!(shared_propagation("", Path::new("/x")), None);
        assert_eq!(shared_propagation("garbage line\n", Path::new("/x")), None);
    }

    #[test]
    fn fuse_conf_needs_an_uncommented_line() {
        assert!(user_allow_other("# mount_max = 1000\nuser_allow_other\n"));
        assert!(user_allow_other("  user_allow_other  # for CI\n"));
        assert!(!user_allow_other("#user_allow_other\n"));
        assert!(!user_allow_other(""));
    }

    #[test]
    fn effective_uid_is_the_second_number() {
        assert_eq!(effective_uid("Name:\tx\nUid:\t1000\t0\t0\t0\n"), Some(0));
        assert_eq!(effective_uid("Uid:\t1001\t1001\t1001\t1001\n"), Some(1001));
        assert_eq!(effective_uid("Name:\tx\n"), None);
    }
}
