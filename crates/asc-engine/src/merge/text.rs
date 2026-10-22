//! Text three-way merge over the `gix-merge` builtin driver (SPEC §4.4).

use bstr::BStr;
use gix_imara_diff::InternedInput;
use gix_merge::blob::Resolution;
use gix_merge::blob::builtin_driver::text::{self, Conflict, ConflictStyle as GixStyle, Labels};

use super::{ConflictStyle, Favor, MergeOptions};

/// Bytes git inspects for a NUL when it decides a buffer is binary.
const BINARY_PROBE: usize = 8000;

/// Git's `buffer_is_binary`: a NUL in the first 8000 bytes.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_PROBE)].contains(&0)
}

/// A text merge result.
pub struct TextMerge {
    pub bytes: Vec<u8>,
    /// Conflict markers remain in `bytes`.
    pub conflicted: bool,
}

/// Merges `ours` and `theirs` over `base` line by line. `union` keeps both sides on a conflict
/// (`merge=union`); otherwise `opts.favor` decides whether a conflict is kept or resolved.
pub fn merge_text(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    opts: &MergeOptions,
    union: bool,
) -> TextMerge {
    let conflict = if union {
        Conflict::ResolveWithUnion
    } else {
        match opts.favor {
            Favor::None => Conflict::Keep {
                style: match opts.style {
                    ConflictStyle::Merge => GixStyle::Merge,
                    ConflictStyle::Diff3 => GixStyle::Diff3,
                    ConflictStyle::ZealousDiff3 => GixStyle::ZealousDiff3,
                },
                marker_size: opts.marker_size,
            },
            Favor::Ours => Conflict::ResolveWithOurs,
            Favor::Theirs => Conflict::ResolveWithTheirs,
        }
    };
    let labels = Labels {
        ancestor: Some(BStr::new(opts.labels.base.as_bytes())),
        current: Some(BStr::new(opts.labels.ours.as_bytes())),
        other: Some(BStr::new(opts.labels.theirs.as_bytes())),
    };
    let mut input = InternedInput::default();
    let mut bytes = Vec::new();
    let resolution = gix_merge::blob::builtin_driver::text(
        &mut bytes,
        &mut input,
        labels,
        ours,
        base,
        theirs,
        text::Options {
            conflict,
            ..Default::default()
        },
    );
    TextMerge {
        bytes,
        conflicted: resolution == Resolution::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> MergeOptions {
        MergeOptions::default()
    }

    #[test]
    fn binary_probe_stops_at_8000_bytes() {
        let mut data = vec![b'a'; 9000];
        assert!(!is_binary(&data));
        data[100] = 0;
        assert!(is_binary(&data));
        data[100] = b'a';
        data[8500] = 0;
        assert!(!is_binary(&data));
    }

    #[test]
    fn disjoint_edits_merge_cleanly() {
        let got = merge_text(b"a\nb\nc\n", b"A\nb\nc\n", b"a\nb\nC\n", &opts(), false);
        assert!(!got.conflicted);
        assert_eq!(got.bytes, b"A\nb\nC\n");
    }

    #[test]
    fn overlapping_edits_get_markers() {
        let got = merge_text(b"a\n", b"x\n", b"y\n", &opts(), false);
        assert!(got.conflicted);
        assert_eq!(got.bytes, b"<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\n");
    }

    /// Every `merge_golden/text/<case>/{base,ours,theirs}` merges byte-for-byte like
    /// `git merge-file -p` in each conflict style.
    #[test]
    fn matches_git_merge_file() {
        use crate::merge::testutil::{case_names, git, git_available, golden_root};
        if !git_available() {
            return;
        }
        let root = golden_root().join("text");
        let cases = case_names(&root);
        assert!(!cases.is_empty());
        for case in cases {
            let dir = root.join(&case);
            let read = |n: &str| std::fs::read(dir.join(n)).unwrap();
            let (base, ours, theirs) = (read("base"), read("ours"), read("theirs"));
            for (style, flag) in [
                (ConflictStyle::Merge, None),
                (ConflictStyle::Diff3, Some("--diff3")),
                (ConflictStyle::ZealousDiff3, Some("--zdiff3")),
            ] {
                let mut args = vec![
                    "merge-file",
                    "-p",
                    "-L",
                    "ours",
                    "-L",
                    "base",
                    "-L",
                    "theirs",
                ];
                args.extend(flag);
                args.extend(["ours", "base", "theirs"]);
                let want = git(&dir, &[], &args);
                let got = merge_text(
                    &base,
                    &ours,
                    &theirs,
                    &MergeOptions { style, ..opts() },
                    false,
                );
                assert_eq!(
                    String::from_utf8_lossy(&got.bytes),
                    String::from_utf8_lossy(&want.stdout),
                    "{case} {style:?}"
                );
                assert_eq!(got.conflicted, !want.status.success(), "{case} {style:?}");
            }
        }
    }

    #[test]
    fn union_and_favor_resolve() {
        let u = merge_text(b"a\n", b"x\n", b"y\n", &opts(), true);
        assert!(!u.conflicted);
        assert_eq!(u.bytes, b"x\ny\n");
        let o = MergeOptions {
            favor: Favor::Theirs,
            ..opts()
        };
        let t = merge_text(b"a\n", b"x\n", b"y\n", &o, false);
        assert!(!t.conflicted);
        assert_eq!(t.bytes, b"y\n");
    }
}
