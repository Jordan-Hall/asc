//! `status --porcelain=v2` text (git's `wt_status_print_porcelain_v2` layout, SPEC §11.2).

use asc_core::id::GitOid;

use super::{quote_path, relativize};

/// One side of a path: its git mode and blob id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Side {
    pub mode: u32,
    pub oid: GitOid,
}

/// One reported path: an ordinary change, a rename or an unmerged path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub path: Vec<u8>,
    /// Rename or copy source and its similarity score.
    pub orig: Option<(Vec<u8>, u8)>,
    pub x: u8,
    pub y: u8,
    pub head: Option<Side>,
    pub index: Option<Side>,
    /// Worktree mode.
    pub work: u32,
    /// Stages 1 to 3 of an unmerged path.
    pub stages: Option<[Option<Side>; 3]>,
}

impl Row {
    /// A clean row for `path` (`..` status, nothing on either side).
    #[must_use]
    pub fn new(path: Vec<u8>) -> Self {
        Self { path, orig: None, x: b'.', y: b'.', head: None, index: None, work: 0, stages: None }
    }

    fn submodule(&self) -> bool {
        [self.head, self.index].iter().flatten().any(|s| s.mode == 0o160000) || self.work == 0o160000
    }

    /// The `<sub>` field.
    fn sub(&self) -> &'static str {
        if !self.submodule() {
            return "N...";
        }
        if self.head.is_some() && self.index.is_some() && self.head != self.index { "SC.." } else { "S..." }
    }
}

/// How HEAD is attached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Detached,
}

/// The state of the branch against its upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    Gone,
    Counts { ahead: usize, behind: usize },
    /// `--no-ahead-behind` and the tips differ.
    Different,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    pub name: String,
    pub track: Track,
}

/// The `# branch.*` header data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub head: Head,
    /// `None` on an unborn branch.
    pub oid: Option<GitOid>,
    pub upstream: Option<Upstream>,
}

/// How paths and terminators are printed.
#[derive(Clone, Debug, Default)]
pub struct Opts {
    /// `-z`: NUL terminators, no quoting.
    pub z: bool,
    /// Show paths relative to `prefix`.
    pub relative: bool,
    /// The current directory below the top level (`""` or `a/b/`).
    pub prefix: Vec<u8>,
    /// `core.quotePath`.
    pub quote_high: bool,
}

impl Opts {
    fn path(&self, p: &[u8]) -> Vec<u8> {
        let shown = if self.relative { relativize(p, &self.prefix) } else { p.to_vec() };
        if self.z { shown } else { quote_path(&shown, false, self.quote_high) }
    }

    fn term(&self, out: &mut Vec<u8>) {
        out.push(if self.z { 0 } else { b'\n' });
    }
}

fn mode_of(side: Option<Side>) -> u32 {
    side.map_or(0, |s| s.mode)
}

fn oid_of(side: Option<Side>) -> GitOid {
    side.map_or_else(|| GitOid::from_bytes([0; 20]), |s| s.oid)
}

/// The porcelain v2 text: the optional branch headers, the changed rows, then `?` untracked and
/// `!` ignored paths.
#[must_use]
pub fn porcelain_v2(
    branch: Option<&Branch>,
    rows: &[Row],
    untracked: &[Vec<u8>],
    ignored: &[Vec<u8>],
    o: &Opts,
) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(b) = branch {
        let mut header = |s: String| {
            out.extend_from_slice(s.as_bytes());
            o.term(&mut out);
        };
        header(format!("# branch.oid {}", b.oid.map_or("(initial)".to_owned(), |x| x.to_string())));
        header(format!(
            "# branch.head {}",
            match &b.head {
                Head::Branch(n) => n.as_str(),
                Head::Detached => "(detached)",
            }
        ));
        if let Some(up) = &b.upstream {
            header(format!("# branch.upstream {}", up.name));
            match up.track {
                Track::Gone => {}
                Track::Different => header("# branch.ab +? -?".into()),
                Track::Counts { ahead, behind } => header(format!("# branch.ab +{ahead} -{behind}")),
            }
        }
    }
    for row in rows {
        let xy = format!("{}{}", row.x as char, row.y as char);
        let path = o.path(&row.path);
        if let Some(st) = &row.stages {
            out.extend_from_slice(
                format!(
                    "u {xy} {} {:06o} {:06o} {:06o} {:06o} {} {} {} ",
                    row.sub(),
                    mode_of(st[0]),
                    mode_of(st[1]),
                    mode_of(st[2]),
                    row.work,
                    oid_of(st[0]),
                    oid_of(st[1]),
                    oid_of(st[2]),
                )
                .as_bytes(),
            );
            out.extend_from_slice(&path);
            o.term(&mut out);
            continue;
        }
        let common = format!(
            "{xy} {} {:06o} {:06o} {:06o} {} {}",
            row.sub(),
            mode_of(row.head),
            mode_of(row.index),
            row.work,
            oid_of(row.head),
            oid_of(row.index),
        );
        match &row.orig {
            Some((orig, score)) => {
                out.extend_from_slice(format!("2 {common} R{score} ").as_bytes());
                out.extend_from_slice(&path);
                out.push(if o.z { 0 } else { b'\t' });
                out.extend_from_slice(&o.path(orig));
            }
            None => {
                out.extend_from_slice(format!("1 {common} ").as_bytes());
                out.extend_from_slice(&path);
            }
        }
        o.term(&mut out);
    }
    for (mark, items) in [(b'?', untracked), (b'!', ignored)] {
        for item in items {
            out.push(mark);
            out.push(b' ');
            out.extend_from_slice(&o.path(item));
            o.term(&mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> GitOid {
        GitOid::from_bytes([n; 20])
    }

    fn side(mode: u32, n: u8) -> Option<Side> {
        Some(Side { mode, oid: oid(n) })
    }

    fn opts() -> Opts {
        Opts { quote_high: true, ..Opts::default() }
    }

    #[test]
    fn ordinary_rename_and_untracked() {
        let mut m = Row::new(b"a.txt".to_vec());
        m.x = b'M';
        m.head = side(0o100644, 1);
        m.index = side(0o100644, 2);
        m.work = 0o100644;
        let mut r = Row::new(b"new name".to_vec());
        r.x = b'R';
        r.orig = Some((b"old".to_vec(), 100));
        r.head = side(0o100644, 3);
        r.index = side(0o100644, 3);
        r.work = 0o100644;
        let text = porcelain_v2(None, &[m, r], &[b"u\xc3\xa9".to_vec()], &[b"t.o".to_vec()], &opts());
        let want = format!(
            "1 M. N... 100644 100644 100644 {} {} a.txt\n\
             2 R. N... 100644 100644 100644 {} {} R100 new name\told\n\
             ? \"u\\303\\251\"\n! t.o\n",
            oid(1),
            oid(2),
            oid(3),
            oid(3)
        );
        assert_eq!(String::from_utf8(text).unwrap(), want);
    }

    #[test]
    fn unmerged_and_submodule() {
        let mut u = Row::new(b"c".to_vec());
        u.x = b'U';
        u.y = b'U';
        u.work = 0o100644;
        u.stages = Some([side(0o100644, 1), side(0o100644, 2), None]);
        let mut s = Row::new(b"sm".to_vec());
        s.y = b'M';
        s.head = side(0o160000, 4);
        s.index = side(0o160000, 5);
        s.work = 0o160000;
        let text = String::from_utf8(porcelain_v2(None, &[u, s], &[], &[], &opts())).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            format!("u UU N... 100644 100644 000000 100644 {} {} {} c", oid(1), oid(2), oid(0))
        );
        assert!(lines[1].starts_with("1 .M SC.. 160000 160000 160000 "));
    }

    #[test]
    fn branch_headers_and_nul() {
        let b = Branch {
            head: Head::Branch("master".into()),
            oid: Some(oid(9)),
            upstream: Some(Upstream {
                name: "origin/master".into(),
                track: Track::Counts { ahead: 2, behind: 1 },
            }),
        };
        let o = Opts { z: true, ..opts() };
        let text = porcelain_v2(Some(&b), &[], &[b"a b".to_vec()], &[], &o);
        let want = format!(
            "# branch.oid {}\0# branch.head master\0# branch.upstream origin/master\0# branch.ab +2 -1\0? a b\0",
            oid(9)
        );
        assert_eq!(text, want.as_bytes());
        let init = Branch { head: Head::Detached, oid: None, upstream: None };
        let text = porcelain_v2(Some(&init), &[], &[], &[], &opts());
        assert_eq!(text, b"# branch.oid (initial)\n# branch.head (detached)\n");
    }

    #[test]
    fn relative_to_prefix() {
        let mut m = Row::new(b"top.txt".to_vec());
        m.y = b'M';
        let o = Opts { relative: true, prefix: b"sub/".to_vec(), ..opts() };
        let text = porcelain_v2(None, &[m], &[], &[], &o);
        assert!(text.ends_with(b" ../top.txt\n"));
    }
}
