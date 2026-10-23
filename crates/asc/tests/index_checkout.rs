//! U61p: a branch switch keeps the synthetic `.git/index` (SPEC §9.4) in step with the commit it
//! lands on and records the stat of the files it wrote, so turbo (which trusts a stat-clean
//! entry's oid and hashes the rest) neither reads a stale oid nor hashes a freshly checked out
//! file.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use asc_core::git::blob_oid;
use asc_git::index::{IndexFile, StatTime};
use support::Sandbox;

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> String {
    let out = sb
        .asc(cwd)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(args)
        .output()
        .expect("spawn asc");
    assert!(
        out.status.success(),
        "asc {args:?} failed: {}{}",
        support::text(&out.stdout),
        support::text(&out.stderr)
    );
    support::text(&out.stdout)
}

/// The synthetic index of the workspace at `root`, found through its gitfile.
fn index_of(root: &Path) -> IndexFile {
    let gitfile = fs::read_to_string(root.join(".git")).expect("gitfile");
    let dir = PathBuf::from(gitfile.trim().trim_start_matches("gitdir:").trim());
    IndexFile::read(&dir.join("index")).expect("index")
}

#[test]
fn checking_out_a_branch_exports_its_oids_with_the_stat_of_the_files_written() {
    let Some(_) = support::asc_binary() else {
        return;
    };
    let sb = Sandbox::new();
    let root = sb.dir("ws");
    asc(&sb, &root, &["init"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    fs::write(root.join("b.txt"), "bee\n").unwrap();
    asc(&sb, &root, &["add", "-A"]);
    asc(&sb, &root, &["commit", "-m", "base"]);
    asc(&sb, &root, &["checkout", "-b", "other"]);
    fs::write(root.join("a.txt"), "other\n").unwrap();
    asc(&sb, &root, &["add", "-A"]);
    asc(&sb, &root, &["commit", "-m", "other"]);
    asc(&sb, &root, &["checkout", "master"]);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "one\n");

    let idx = index_of(&root);
    let a = idx
        .entries
        .iter()
        .find(|e| e.path.as_bytes() == b"a.txt")
        .expect("a.txt is exported");
    assert_eq!(
        a.oid,
        blob_oid(b"one\n"),
        "the index names the blob of the branch checked out, not of the one left"
    );
    for e in &idx.entries {
        let name = String::from_utf8_lossy(e.path.as_bytes()).into_owned();
        let meta = fs::metadata(root.join(&name)).unwrap();
        assert_eq!(u64::from(e.size), meta.len(), "{name}: size");
        assert_eq!(
            e.mtime,
            StatTime::from_system_time(meta.modified().unwrap()),
            "{name}: stat-clean, so a reader hashes nothing"
        );
        assert!(
            idx.mtime.unwrap() >= e.mtime.to_system_time() + std::time::Duration::from_secs(2),
            "{name}: not racy"
        );
    }
}
