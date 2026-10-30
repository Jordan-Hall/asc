//! Test fixtures shared by the merge, rebase and land tests.

use std::sync::Arc;

use asc_core::id::{ChangesetId, TreeId};
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_store::bucket::BucketObjects;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use async_trait::async_trait;

use crate::Engine;
use crate::commit::{NewChangeset, PathPolicy};

pub struct NoRefs;

#[async_trait]
impl Refs for NoRefs {
    async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
        Ok(None)
    }
    async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
        Ok(Vec::new())
    }
    async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
        Err(RefError::Corrupt("unused".into()))
    }
    async fn log(
        &self,
        _: RepoKey,
        _: &RefName,
        _: u32,
    ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
        Ok(Vec::new())
    }
}

pub type TestEngine = Engine<BucketObjects, NoRefs>;

pub fn engine() -> TestEngine {
    Engine::new(
        Arc::new(BucketObjects::memory()),
        Arc::new(NoRefs),
        RepoKey(1),
    )
}

pub fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

pub fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap()
}

/// A leaf to write: `(path, kind, content)`.
pub type Leaf<'a> = (&'a str, Kind, &'a [u8]);

/// Stores the leaves and returns the tree holding exactly them.
pub async fn tree_of(e: &TestEngine, leaves: &[Leaf<'_>]) -> TreeId {
    let mut changes = Vec::new();
    for (path, kind, content) in leaves {
        let blob = e.store_blob(content).await.unwrap();
        changes.push((
            p(path),
            Change::write(*kind, blob.file, blob.size, blob.git, None).unwrap(),
        ));
    }
    e.apply(TestEngine::empty_tree_id(), &changes)
        .await
        .unwrap()
}

/// Shorthand for regular files.
pub async fn files(e: &TestEngine, leaves: &[(&str, &str)]) -> TreeId {
    let leaves: Vec<Leaf> = leaves
        .iter()
        .map(|(p, c)| (*p, Kind::File, c.as_bytes()))
        .collect();
    tree_of(e, &leaves).await
}

/// Stores a changeset over `root` with the given parents and message.
pub async fn commit(
    e: &TestEngine,
    parents: &[ChangesetId],
    root: TreeId,
    message: &str,
) -> ChangesetId {
    e.create_changeset(
        NewChangeset {
            parents: parents.to_vec(),
            author: sig(),
            committer: sig(),
            message: message.as_bytes().to_vec(),
            extra: Vec::new(),
            root,
            copies: Vec::new(),
        },
        PathPolicy::Native {
            windows_safe: false,
        },
    )
    .await
    .unwrap()
}

/// The content of the file at `path` in `root`.
pub async fn read(e: &TestEngine, root: TreeId, path: &str) -> Option<Vec<u8>> {
    let entry = e.lookup(root, &p(path)).await.unwrap()?;
    Some(e.read_blob(&entry).await.unwrap().to_vec())
}

// ---------------------------------------------------------------- git golden helpers

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The git the tests compare with: the pinned build `ASC_REAL_GIT` names (docs/git-fidelity.md),
/// else the first `git` on `PATH`. A runner's own git moves on its own, and the oracle must not.
pub fn git_program() -> std::ffi::OsString {
    std::env::var_os("ASC_REAL_GIT")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "git".into())
}

/// False when `git` cannot be spawned; golden tests then skip.
pub fn git_available() -> bool {
    Command::new(git_program())
        .arg("--version")
        .output()
        .is_ok() // lint:allow-raw-git
}

/// `crates/asc-engine/tests/merge_golden`.
pub fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/merge_golden")
}

/// Sorted child directory names of `dir`.
pub fn case_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every file under `dir` as `relative/path -> bytes`.
pub fn read_dir_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap();
                out.insert(
                    rel.to_string_lossy().replace('\\', "/"),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    if dir.is_dir() {
        walk(dir, dir, &mut out);
    }
    out
}

/// Runs git in `dir` with extra environment and returns its output (any exit status).
pub fn git(dir: &Path, envs: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(git_program()); // lint:allow-raw-git
    cmd.current_dir(dir)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        // Never discover a repository above `dir`: a checkout the user does not own makes a newer
        // git die before `merge-file`, which needs no repository.
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(dir))
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@x")
        .args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// Like [`git`] but the command must succeed; returns stdout.
pub fn git_ok(dir: &Path, envs: &[(&str, &str)], args: &[&str]) -> Vec<u8> {
    let out = git(dir, envs, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// A base changeset with `a` (five lines), `b` and `c`.
pub async fn fixture() -> (TestEngine, ChangesetId) {
    let e = engine();
    let root = files(&e, &[("a", "1\n2\n3\n4\n5\n"), ("b", "b\n"), ("c", "c\n")]).await;
    let base = commit(&e, &[], root, "base").await;
    (e, base)
}

/// A child of `parent` that writes `edits` (regular files) over its root.
pub async fn child(
    e: &TestEngine,
    parent: ChangesetId,
    edits: &[(&str, &str)],
    extra: Vec<(Vec<u8>, Vec<u8>)>,
) -> ChangesetId {
    let parent_root = e.get_changeset(parent).await.unwrap().root;
    let mut changes = Vec::new();
    for (path, content) in edits {
        let blob = e.store_blob(content.as_bytes()).await.unwrap();
        changes.push((
            p(path),
            Change::write(Kind::File, blob.file, blob.size, blob.git, None).unwrap(),
        ));
    }
    let root = e.apply(parent_root, &changes).await.unwrap();
    e.create_changeset(
        NewChangeset {
            parents: vec![parent],
            author: sig(),
            committer: sig(),
            message: b"edit".to_vec(),
            extra,
            root,
            copies: Vec::new(),
        },
        PathPolicy::Native {
            windows_safe: false,
        },
    )
    .await
    .unwrap()
}
