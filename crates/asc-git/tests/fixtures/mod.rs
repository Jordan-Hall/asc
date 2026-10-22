//! Fixture builders for the import tests: repositories built with real git, plus in-memory
//! stand-ins for the git map and the ref store.
//!
//! Odd objects (unsorted trees, odd modes, hand-written signatures) are written with
//! `git hash-object --literally`, so the bytes are exactly what the test spells out.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::git::ObjectKind;
use asc_core::id::GitOid;
use asc_core::path::RefName;
use asc_engine::derive::{self, GitMapRow, GitMapSink};
use asc_git::import::{ImportMap, Tip};
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{self, Objects};
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};

/// A bare git repository in a temp directory.
pub struct Repo {
    pub dir: tempfile::TempDir,
}

impl Repo {
    pub fn init() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Self { dir };
        repo.git(&["init", "-q", "--bare"], b"");
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Runs git in the repository and returns stdout.
    pub fn git(&self, args: &[&str], stdin: &[u8]) -> Vec<u8> {
        let mut child = Command::new("git") // lint:allow-raw-git
            .args(args)
            .current_dir(self.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin)
            .expect("write stdin");
        let out = child.wait_with_output().expect("git output");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn put(&self, kind: &str, body: &[u8]) -> GitOid {
        let out = self.git(
            &["hash-object", "-w", "--literally", "-t", kind, "--stdin"],
            body,
        );
        String::from_utf8(out)
            .expect("utf8")
            .trim()
            .parse()
            .expect("oid")
    }

    pub fn blob(&self, bytes: &[u8]) -> GitOid {
        self.put("blob", bytes)
    }

    /// A tree with its entries exactly in the order and with the modes given.
    pub fn tree_literal(&self, entries: &[(&str, &[u8], GitOid)]) -> GitOid {
        let mut body = Vec::new();
        for (mode, name, oid) in entries {
            body.extend_from_slice(mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(name);
            body.push(0);
            body.extend_from_slice(oid.as_bytes());
        }
        self.put("tree", &body)
    }

    /// A canonical tree: modes as given, entries sorted the way git sorts them.
    pub fn tree(&self, entries: &[(&str, &[u8], GitOid)]) -> GitOid {
        let mut sorted = entries.to_vec();
        let key = |e: &(&str, &[u8], GitOid)| {
            let mut k = e.1.to_vec();
            if e.0 == "40000" {
                k.push(b'/');
            }
            k
        };
        sorted.sort_by_key(key);
        self.tree_literal(&sorted)
    }

    /// A commit with hand-written identity lines and extra header lines (each ending in LF).
    pub fn commit_raw(
        &self,
        tree: GitOid,
        parents: &[GitOid],
        author: &[u8],
        extra_headers: &[u8],
        message: &[u8],
    ) -> GitOid {
        let mut body = format!("tree {tree}\n").into_bytes();
        for p in parents {
            body.extend_from_slice(format!("parent {p}\n").as_bytes());
        }
        for key in ["author", "committer"] {
            body.extend_from_slice(key.as_bytes());
            body.push(b' ');
            body.extend_from_slice(author);
            body.push(b'\n');
        }
        body.extend_from_slice(extra_headers);
        body.push(b'\n');
        body.extend_from_slice(message);
        self.put("commit", &body)
    }

    pub fn commit(&self, tree: GitOid, parents: &[GitOid], message: &str) -> GitOid {
        self.commit_raw(
            tree,
            parents,
            b"Au Thor <au@example.com> 1700000000 +0000",
            b"",
            message.as_bytes(),
        )
    }

    /// An annotated tag object of `kind` (`commit`, `tree`, `blob`, `tag`).
    pub fn tag_object(&self, object: GitOid, kind: &str, name: &str) -> GitOid {
        let body = format!(
            "object {object}\ntype {kind}\ntag {name}\ntagger Ta Gger <t@example.com> 1700000100 +0000\n\nrelease {name}\n"
        );
        self.put("tag", body.as_bytes())
    }

    pub fn set_ref(&self, name: &str, oid: GitOid) {
        self.git(&["update-ref", name, &oid.to_string()], b"");
    }

    /// All refs under `refs/`, as import tips.
    pub fn tips(&self) -> Vec<Tip> {
        let out = self.git(&["for-each-ref", "--format=%(refname) %(objectname)"], b"");
        String::from_utf8(out)
            .expect("utf8")
            .lines()
            .map(|l| {
                let (name, oid) = l.split_once(' ').expect("ref line");
                Tip {
                    name: name.as_bytes().to_vec(),
                    oid: oid.parse().expect("oid"),
                }
            })
            .collect()
    }

    /// `git ls-tree -r` of a commit as `(path, blob oid)` for non-directory entries.
    pub fn ls_tree(&self, commit: GitOid) -> BTreeMap<Vec<u8>, GitOid> {
        let out = self.git(&["ls-tree", "-r", "-z", &commit.to_string()], b"");
        out.split(|&c| c == 0)
            .filter(|l| !l.is_empty())
            .map(|l| {
                let tab = l.iter().position(|&c| c == b'\t').expect("tab");
                let head = std::str::from_utf8(&l[..tab]).expect("utf8");
                let oid = head.split(' ').nth(2).expect("oid");
                (l[tab + 1..].to_vec(), oid.parse().expect("oid"))
            })
            .collect()
    }

    pub fn rev_list_all(&self) -> Vec<GitOid> {
        let out = self.git(&["rev-list", "--all"], b"");
        String::from_utf8(out)
            .expect("utf8")
            .lines()
            .map(|l| l.parse().expect("oid"))
            .collect()
    }
}

/// What the main fixture contains, by name.
pub struct Fixture {
    pub repo: Repo,
    pub head: GitOid,
    pub unsorted_root: GitOid,
    pub unsorted_sub: GitOid,
    pub non_utf8: GitOid,
    pub signed: GitOid,
    pub octopus: GitOid,
    pub with_gitlink: GitOid,
}

fn entry<'a>(mode: &'a str, name: &'a str, oid: GitOid) -> (&'a str, &'a [u8], GitOid) {
    (mode, name.as_bytes(), oid)
}

/// A repository with every odd shape the importer must handle.
pub fn main_fixture() -> Fixture {
    let r = Repo::init();
    let readme = r.blob(b"# project\n");
    let app = r.blob(b"fn app() {}\n");
    let lib = r.blob(b"pub fn lib() {}\n");
    let lfs =
        r.blob(b"version https://git-lfs.github.com/spec/v1\noid sha256:4d7a2146\nsize 12345\n");
    let con = r.blob(b"con\n");
    let upper = r.blob(b"upper\n");
    let lower = r.blob(b"lower\n");
    let gitlink_target = GitOid::from_bytes([0x11; 20]);

    let src = r.tree(&[
        entry("100644", "app.rs", app),
        entry("100644", "lib.rs", lib),
    ]);
    let win = r.tree(&[
        entry("100644", "CON", con),
        entry("100644", "A.ts", upper),
        entry("100644", "a.ts", lower),
    ]);
    let t1 = r.tree(&[
        entry("100644", "README.md", readme),
        entry("100644", "lfs.bin", lfs),
        entry("40000", "src", src),
        entry("40000", "win", win),
        entry("160000", "vendor", gitlink_target),
    ]);
    let c1 = r.commit(t1, &[], "initial\n");

    // c2: edit a file, add an executable and a symlink.
    let app2 = r.blob(b"fn app() { run(); }\n");
    let script = r.blob(b"#!/bin/sh\necho hi\n");
    let link = r.blob(b"README.md");
    let src2 = r.tree(&[
        entry("100644", "app.rs", app2),
        entry("100644", "lib.rs", lib),
    ]);
    let t2 = r.tree(&[
        entry("100644", "README.md", readme),
        entry("100644", "lfs.bin", lfs),
        entry("40000", "src", src2),
        entry("40000", "win", win),
        entry("160000", "vendor", gitlink_target),
        entry("100755", "run.sh", script),
        entry("120000", "link", link),
    ]);
    let c2 = r.commit(t2, &[c1], "edit, exec, symlink\n");

    // c3: signed (the signature block is spelled out; git never verifies it here).
    let readme3 = r.blob(b"# project\n\nsigned\n");
    let t3 = r.tree(&[
        entry("100644", "README.md", readme3),
        entry("100644", "lfs.bin", lfs),
        entry("40000", "src", src2),
        entry("40000", "win", win),
        entry("160000", "vendor", gitlink_target),
        entry("100755", "run.sh", script),
        entry("120000", "link", link),
    ]);
    let signed = r.commit_raw(
        t3,
        &[c2],
        b"Au Thor <au@example.com> 1700000200 +0000",
        b"gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n =abcd\n -----END PGP SIGNATURE-----\n",
        b"signed\n",
    );

    // Two side branches off c1, then a merge and an octopus.
    let side_a = r.blob(b"side a\n");
    let side_b = r.blob(b"side b\n");
    let ta = r.tree(&[
        entry("100644", "README.md", readme),
        entry("100644", "lfs.bin", lfs),
        entry("40000", "src", src),
        entry("40000", "win", win),
        entry("160000", "vendor", gitlink_target),
        entry("100644", "side-a.txt", side_a),
    ]);
    let tb = r.tree(&[
        entry("100644", "README.md", readme),
        entry("100644", "lfs.bin", lfs),
        entry("40000", "src", src),
        entry("40000", "win", win),
        entry("160000", "vendor", gitlink_target),
        entry("100644", "side-b.txt", side_b),
    ]);
    let ca = r.commit(ta, &[c1], "side a\n");
    let cb = r.commit(tb, &[c1], "side b\n");
    let tm = r.tree(&[
        entry("100644", "README.md", readme3),
        entry("100644", "lfs.bin", lfs),
        entry("40000", "src", src2),
        entry("40000", "win", win),
        entry("160000", "vendor", gitlink_target),
        entry("100755", "run.sh", script),
        entry("120000", "link", link),
        entry("100644", "side-a.txt", side_a),
    ]);
    let merge = r.commit(tm, &[signed, ca], "merge\n");
    let to = r.tree(&[
        entry("100644", "README.md", readme3),
        entry("100644", "lfs.bin", lfs),
        entry("40000", "src", src2),
        entry("40000", "win", win),
        entry("160000", "vendor", gitlink_target),
        entry("100755", "run.sh", script),
        entry("120000", "link", link),
        entry("100644", "side-a.txt", side_a),
        entry("100644", "side-b.txt", side_b),
    ]);
    let octopus = r.commit(to, &[merge, cb, c2], "octopus\n");

    // An unsorted root tree with an odd mode (100664), reversed from git's order.
    let notes = r.blob(b"notes\n");
    let root_entries = [
        entry("160000", "vendor", gitlink_target),
        entry("40000", "win", win),
        entry("40000", "src", src2),
        entry("120000", "link", link),
        entry("100664", "notes.txt", notes),
        entry("100644", "lfs.bin", lfs),
        entry("100755", "run.sh", script),
        entry("100644", "README.md", readme3),
        entry("100644", "side-b.txt", side_b),
        entry("100644", "side-a.txt", side_a),
    ];
    let t_odd = r.tree_literal(&root_entries);
    let unsorted_root = r.commit(t_odd, &[octopus], "unsorted root tree\n");

    // A canonical root whose `src` subtree is unsorted, with an odd mode inside.
    let src_odd = r.tree_literal(&[
        entry("100664", "lib.rs", lib),
        entry("100644", "app.rs", app2),
    ]);
    let t_sub = r.tree(&[
        entry("100644", "README.md", readme3),
        entry("100644", "lfs.bin", lfs),
        entry("40000", "src", src_odd),
        entry("40000", "win", win),
        entry("160000", "vendor", gitlink_target),
        entry("100755", "run.sh", script),
        entry("120000", "link", link),
        entry("100644", "side-a.txt", side_a),
        entry("100644", "side-b.txt", side_b),
        entry("100644", "notes.txt", notes),
    ]);
    let unsorted_sub = r.commit(t_sub, &[unsorted_root], "unsorted subtree\n");

    // A Latin-1 author and an `encoding` header.
    let t_enc = r.tree(&[
        entry("100644", "README.md", readme3),
        entry("100644", "notes.txt", notes),
    ]);
    let non_utf8 = r.commit_raw(
        t_enc,
        &[unsorted_sub],
        b"Jos\xe9 Author <jose@example.com> 1700000300 +0100",
        b"encoding ISO-8859-1\n",
        b"caf\xe9 au lait\n",
    );

    // A 5 MiB file (chunked in the store), then a file that becomes a directory.
    let big_bytes: Vec<u8> = (0..5 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let big = r.blob(&big_bytes);
    let x_file = r.blob(b"x\n");
    let t_big = r.tree(&[
        entry("100644", "README.md", readme3),
        entry("100644", "big.bin", big),
        entry("100644", "x", x_file),
    ]);
    let with_big = r.commit(t_big, &[non_utf8], "big file\n");
    let x_inner = r.blob(b"y\n");
    let x_dir = r.tree(&[entry("100644", "y", x_inner)]);
    let t_x = r.tree(&[
        entry("100644", "README.md", readme3),
        entry("100644", "big.bin", big),
        entry("40000", "x", x_dir),
        entry("160000", "vendor", GitOid::from_bytes([0x22; 20])),
    ]);
    let head = r.commit(t_x, &[with_big], "file becomes directory, gitlink moves\n");

    r.set_ref("refs/heads/master", head);
    r.set_ref("refs/heads/side", cb);
    r.set_ref("refs/heads/octopus", octopus);

    // Tags: one importable, three that cannot be represented.
    let v1 = r.tag_object(head, "commit", "v1");
    r.set_ref("refs/tags/v1", v1);
    r.set_ref("refs/tags/of-tree", r.tag_object(t1, "tree", "of-tree"));
    r.set_ref("refs/tags/of-blob", r.tag_object(readme, "blob", "of-blob"));
    r.set_ref("refs/tags/of-tag", r.tag_object(v1, "tag", "of-tag"));

    Fixture {
        repo: r,
        head,
        unsorted_root,
        unsorted_sub,
        non_utf8,
        signed,
        octopus,
        with_gitlink: c1,
    }
}

/// A short linear history with a branch, for the crash test.
pub fn small_fixture() -> Repo {
    let r = Repo::init();
    let mut parent: Option<GitOid> = None;
    let mut dirs: Vec<(String, GitOid)> = Vec::new();
    for i in 0..10u32 {
        let file = r.blob(format!("file {i}\n").as_bytes());
        let shared = r.blob(b"shared\n");
        let sub = r.tree(&[
            entry("100644", &format!("f{i}.txt"), file),
            entry("100644", "shared.txt", shared),
        ]);
        dirs.push((format!("d{}", i % 3), sub));
        let mut entries: Vec<(&str, &[u8], GitOid)> = vec![entry("100644", "top.txt", file)];
        for (name, oid) in dirs.iter().rev().take(3) {
            if !entries.iter().any(|e| e.1 == name.as_bytes()) {
                entries.push(("40000", name.as_bytes(), *oid));
            }
        }
        let tree = r.tree(&entries);
        let parents: Vec<GitOid> = parent.into_iter().collect();
        let c = r.commit(tree, &parents, &format!("commit {i}\n"));
        parent = Some(c);
    }
    if let Some(tip) = parent {
        r.set_ref("refs/heads/master", tip);
        r.set_ref("refs/tags/t", r.tag_object(tip, "commit", "t"));
    }
    r
}

// ------------------------------------------------------------ in-memory stand-ins

/// A ref store with no refs; import never touches refs.
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

/// Counts writes and fails every write after the first `limit`: a killed process.
pub struct Killer {
    writes: AtomicUsize,
    limit: AtomicUsize,
}

impl Killer {
    pub fn new(limit: usize) -> Self {
        Self {
            writes: AtomicUsize::new(0),
            limit: AtomicUsize::new(limit),
        }
    }

    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::SeqCst);
    }

    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    /// Counts one write; fails when the limit is spent.
    fn tick(&self) -> bool {
        self.writes.fetch_add(1, Ordering::SeqCst) < self.limit.load(Ordering::SeqCst)
    }
}

/// The git map in memory, optionally killed.
/// Rows keyed by `(git oid bytes, kind byte)`.
pub type MapRows = BTreeMap<(Vec<u8>, u8), [u8; 32]>;

pub struct MemMap {
    rows: Mutex<MapRows>,
    killer: std::sync::Arc<Killer>,
}

fn kind_byte(k: ObjectKind) -> u8 {
    match k {
        ObjectKind::Blob => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Commit => 3,
        ObjectKind::Tag => 4,
    }
}

impl MemMap {
    pub fn new(killer: std::sync::Arc<Killer>) -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            killer,
        }
    }

    pub fn dump(&self) -> MapRows {
        self.rows.lock().expect("map lock").clone()
    }

    pub fn lookup(&self, git: GitOid, kind: ObjectKind) -> Option<[u8; 32]> {
        self.dump()
            .get(&(git.as_bytes().to_vec(), kind_byte(kind)))
            .copied()
    }
}

#[async_trait]
impl GitMapSink for MemMap {
    async fn put(&self, rows: &[GitMapRow]) -> derive::Result<()> {
        if !self.killer.tick() {
            return Err(derive::Error::Sink("killed".into()));
        }
        let mut map = self.rows.lock().expect("map lock");
        for r in rows {
            map.insert((r.git.as_bytes().to_vec(), kind_byte(r.kind)), r.asc);
        }
        Ok(())
    }
}

#[async_trait]
impl ImportMap for MemMap {
    async fn get(&self, git: GitOid, kind: ObjectKind) -> derive::Result<Option<[u8; 32]>> {
        Ok(self.lookup(git, kind))
    }
}

/// An `Objects` that dies after the killer's write budget.
pub struct KillObjects<O> {
    pub inner: std::sync::Arc<O>,
    pub killer: std::sync::Arc<Killer>,
}

#[async_trait]
impl<O: Objects> Objects for KillObjects<O> {
    async fn get(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        self.inner.get(key).await
    }
    async fn get_stored(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        self.inner.get_stored(key).await
    }
    async fn get_range(
        &self,
        key: &ObjKey,
        r: std::ops::Range<u64>,
    ) -> objects::Result<Option<Bytes>> {
        self.inner.get_range(key, r).await
    }
    async fn put(&self, key: &ObjKey, stored: Bytes) -> objects::Result<()> {
        if !self.killer.tick() {
            return Err(objects::Error::Url("killed".into()));
        }
        self.inner.put(key, stored).await
    }
    async fn has_many(&self, keys: &[ObjKey]) -> objects::Result<Vec<bool>> {
        self.inner.has_many(keys).await
    }
    async fn delete(&self, key: &ObjKey) -> objects::Result<()> {
        self.inner.delete(key).await
    }
    fn list(
        &self,
        prefix: KeyPrefix,
    ) -> BoxStream<'_, objects::Result<(ObjKey, std::time::SystemTime)>> {
        self.inner.list(prefix)
    }
}

/// Every stored object as `key path -> stored bytes`.
pub async fn dump_store<O: Objects>(objects: &O) -> BTreeMap<String, Bytes> {
    let keys: Vec<ObjKey> = objects
        .list(KeyPrefix::all())
        .map(|r| r.map(|(k, _)| k))
        .try_collect()
        .await
        .expect("list");
    let mut out = BTreeMap::new();
    for key in keys {
        let bytes = objects
            .get_stored(&key)
            .await
            .expect("get_stored")
            .expect("listed key exists");
        out.insert(key.path(), bytes);
    }
    out
}

pub fn repo_key() -> RepoKey {
    RepoKey(1)
}
