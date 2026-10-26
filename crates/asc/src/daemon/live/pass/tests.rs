//! Tests of the pure parts of a live pass. The pass itself (fetch, restack, move) is tested
//! through the built binary in `tests/live_cli.rs`.

use super::*;

fn path(p: &str) -> RepoPath {
    RepoPath::new(p.as_bytes().to_vec()).unwrap()
}

fn untracked(p: &str, dir: bool) -> Untracked {
    Untracked { path: path(p), dir }
}

#[test]
fn an_untracked_path_in_the_way_of_the_move_collides() {
    let changed = [path("src/a.rs"), path("docs/new/guide.md"), path("top.txt")];
    // The same file, a directory that holds a changed file, and a file where a directory goes.
    assert!(collides(&[untracked("top.txt", false)], &changed));
    assert!(collides(&[untracked("src", true)], &changed));
    assert!(collides(&[untracked("docs", false)], &changed));
    assert!(collides(&[untracked("src/a.rs", false)], &changed));
    // Elsewhere, siblings and look-alikes do not.
    assert!(!collides(&[untracked("src/b.rs", false)], &changed));
    assert!(!collides(&[untracked("srcs", true)], &changed));
    assert!(!collides(&[untracked("top.txt.bak", false)], &changed));
    assert!(!collides(&[untracked("src/a.rs/x", false)], &changed));
    assert!(!collides(&[], &changed));
    assert!(!collides(&[untracked("top.txt", false)], &[]));
}

#[test]
fn an_untracked_directory_matches_what_is_written_below_it() {
    let changed = [path("vendor/lib/x.c")];
    assert!(collides(&[untracked("vendor", true)], &changed));
    assert!(collides(&[untracked("vendor/lib", true)], &changed));
    assert!(!collides(&[untracked("vendor/other", true)], &changed));
}

#[test]
fn operations_in_progress_are_read_from_the_git_directory() {
    let dir = tempfile::tempdir().unwrap();
    let git = dir.path();
    assert_eq!(operation_of(git), None);
    for (marker, want) in [
        ("rebase-merge", InProgress::Rebase),
        ("rebase-apply", InProgress::Rebase),
        ("asc-stack-op.json", InProgress::Rebase),
        ("MERGE_HEAD", InProgress::Merge),
        ("CHERRY_PICK_HEAD", InProgress::CherryPick),
        ("REVERT_HEAD", InProgress::CherryPick),
        ("sequencer", InProgress::CherryPick),
        ("BISECT_LOG", InProgress::Bisect),
    ] {
        let p = git.join(marker);
        std::fs::write(&p, b"").unwrap();
        assert_eq!(operation_of(git), Some(want), "{marker}");
        std::fs::remove_file(&p).unwrap();
    }
}

#[test]
fn globs_match_the_immutable_default() {
    let pat = b"refs/remotes/*/release/*";
    assert!(glob(pat, b"refs/remotes/origin/release/1.0"));
    assert!(!glob(pat, b"refs/remotes/origin/master"));
    assert!(!glob(pat, b"refs/heads/release/1.0"));
    assert!(glob(b"*", b""));
    assert!(!glob(b"a", b"ab"));
}

#[test]
fn a_line_is_one_line_without_control_characters() {
    let d = Done::new("notice", "a\nb\u{1b}[31m");
    assert_eq!(d.word(), "notice");
    assert_eq!(d.detail(), "a\nb\u{1b}[31m");
    let line = d.describe();
    assert!(!line.chars().any(char::is_control), "{line:?}");
    assert!(line.starts_with("notice: a b"));
    assert_eq!(Done::new("fetched", "").describe(), "fetched");
}

#[test]
fn a_move_is_blocked_for_the_reasons_the_notice_names() {
    for b in [
        MoveBlock::Dirty,
        MoveBlock::CollidingUntracked,
        MoveBlock::Operation(InProgress::Bisect),
        MoveBlock::UnresolvedConflicts,
        MoveBlock::IndexLocked,
    ] {
        assert!(!block_text(b).is_empty());
    }
    assert_eq!(plural(1), "");
    assert_eq!(plural(0), "s");
    assert_eq!(plural(2), "s");
}
