//! Golden corpus: file pairs from real history, compared with `git diff --no-index` output
//! (patch and numstat) for every algorithm. `tests/diff_golden/exceptions.txt` lists the
//! `<id> <algorithm>` pairs that are known to differ; it must be empty at the phase-1 exit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use asc_core::git::blob_oid;
use asc_core::id::FileId;
use asc_core::object::{Entry, Kind};
use asc_core::path::{Name, RepoPath};

use super::{Algorithm, DiffCtx, DiffOptions, NoAttributes, Status, TreeChange};
use super::{write_numstat, write_patch};
use crate::diff::tests_support::MemSource;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diff_golden")
}

fn entry(content: &[u8]) -> Entry {
    Entry {
        name: Name::new("file").unwrap(),
        kind: Kind::File,
        id: *FileId::of(content).as_bytes(),
        size: content.len() as u64,
        git: blob_oid(content),
    }
}

fn change(old: &[u8], new: &[u8]) -> TreeChange {
    let path = RepoPath::new("file").unwrap();
    TreeChange {
        old_path: path.clone(),
        path,
        old: Some(entry(old)),
        new: Some(entry(new)),
        status: Status::Modified,
    }
}

fn pair_ids() -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(dir().join("pairs"))
        .expect("golden pairs exist")
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".old").map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

fn exceptions() -> BTreeSet<String> {
    std::fs::read_to_string(dir().join("exceptions.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Ids (as `<id> <algorithm>`) whose patch or numstat differs from git's.
async fn mismatches(algorithm: Algorithm, label: &str) -> Vec<String> {
    let mut bad = Vec::new();
    let opts = DiffOptions {
        algorithm,
        ..DiffOptions::default()
    };
    for id in pair_ids() {
        let old = std::fs::read(dir().join(format!("pairs/{id}.old"))).unwrap();
        let new = std::fs::read(dir().join(format!("pairs/{id}.new"))).unwrap();
        let mut src = MemSource::default();
        src.add(&old);
        src.add(&new);
        let ctx = DiffCtx {
            opts: &opts,
            content: &src,
            attrs: &NoAttributes,
        };
        let changes = [change(&old, &new)];
        let (mut patch, mut numstat) = (Vec::new(), Vec::new());
        write_patch(&mut patch, &ctx, &changes).await.unwrap();
        write_numstat(&mut numstat, &ctx, &changes).await.unwrap();
        let want_patch = std::fs::read(dir().join(format!("expected/{id}.{label}.patch"))).unwrap();
        let want_numstat =
            std::fs::read(dir().join(format!("expected/{id}.{label}.numstat"))).unwrap();
        if patch != want_patch || numstat != want_numstat {
            if let Some(out) = std::env::var_os("ASC_GOLDEN_DUMP") {
                let out = Path::new(&out);
                std::fs::write(out.join(format!("{id}.{label}.patch")), &patch).unwrap();
                std::fs::write(out.join(format!("{id}.{label}.numstat")), &numstat).unwrap();
            }
            bad.push(format!("{id} {label}"));
        }
    }
    bad
}

async fn check(algorithm: Algorithm, label: &str) {
    assert!(pair_ids().len() >= 200, "the corpus holds 200 pairs");
    let bad: BTreeSet<String> = mismatches(algorithm, label).await.into_iter().collect();
    let known: BTreeSet<String> = exceptions()
        .into_iter()
        .filter(|e| e.ends_with(label))
        .collect();
    assert_eq!(
        bad, known,
        "{label}: differing pairs vs recorded exceptions"
    );
}

#[tokio::test]
async fn myers_matches_git() {
    check(Algorithm::Myers, "myers").await;
}

#[tokio::test]
async fn histogram_matches_git() {
    check(Algorithm::Histogram, "histogram").await;
}

#[tokio::test]
async fn patience_matches_git() {
    check(Algorithm::Patience, "patience").await;
}

#[tokio::test]
async fn minimal_matches_git() {
    check(Algorithm::Minimal, "minimal").await;
}

/// Our patches apply to the old file with `git apply --check`.
#[tokio::test]
async fn own_patches_apply() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    for algorithm in [Algorithm::Myers, Algorithm::Histogram] {
        let opts = DiffOptions {
            algorithm,
            ..DiffOptions::default()
        };
        for id in pair_ids() {
            let old = std::fs::read(dir().join(format!("pairs/{id}.old"))).unwrap();
            let new = std::fs::read(dir().join(format!("pairs/{id}.new"))).unwrap();
            let mut src = MemSource::default();
            src.add(&old);
            src.add(&new);
            let ctx = DiffCtx {
                opts: &opts,
                content: &src,
                attrs: &NoAttributes,
            };
            let mut patch = Vec::new();
            write_patch(&mut patch, &ctx, &[change(&old, &new)])
                .await
                .unwrap();
            std::fs::write(work.path().join("file"), &old).unwrap();
            let mut child = Command::new("git")
                .args(["apply", "--check", "-"])
                .current_dir(work.path())
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&patch).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{id} {algorithm:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
