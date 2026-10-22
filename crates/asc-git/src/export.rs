//! Git export (SPEC §14.2): renders asc objects as git objects into a target object database.
//!
//! Blobs are file bytes, trees use the stored git override when the canonical render does not
//! hash to the recorded oid, and commits and tags use the raw bytes (`git.commit`, `git.tag`)
//! when present. Objects the target already has are skipped, and a present tree or commit is
//! taken to have its closure, as in git. Objects are written children first, so an interrupted
//! export never leaves an object whose references are missing.
//!
//! Output is loose objects, or one undeltified pack indexed by `git index-pack` once more than
//! [`PACK_THRESHOLD`] objects are pending. Refs move through `git update-ref --stdin`.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use asc_core::codec::{CodecError, decode_envelope};
use asc_core::git::{GitError, ObjectKind, blob_oid, export_commit, export_tag, object_oid};
use asc_core::git::{render_tree, tree_oid};
use asc_core::id::{ChangesetId, FileId, GitOid, TagId, TreeId};
use asc_core::object::{Kind, TagRecord};
use asc_core::path::MAX_TREE_DEPTH;
use asc_engine::Engine;
use asc_engine::{commit, derive, diff, tree};
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use asc_store::refs::Refs;
use sha1::{Digest, Sha1};

use crate::odb::{Odb, OdbError};
use crate::realgit::{RealGit, RealGitError};

/// More pending objects than this go into one pack instead of loose files.
pub const PACK_THRESHOLD: usize = 1000;

/// Pending object bytes above this switch to a pack too, which bounds memory.
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;

/// Why an export failed.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Derive(#[from] derive::Error),
    #[error(transparent)]
    Content(#[from] diff::Error),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("stored object does not decode: {0}")]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Odb(#[from] OdbError),
    #[error(transparent)]
    RealGit(#[from] RealGitError),
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: std::io::Error,
    },
    #[error("git {command} failed ({status}): {stderr}")]
    GitFailed {
        command: &'static str,
        status: String,
        stderr: String,
    },
    #[error("tag {0} is missing")]
    MissingTag(TagId),
    #[error("git tree override {0} is missing")]
    MissingOverride(GitOid),
    #[error("tree {0} nests deeper than {MAX_TREE_DEPTH} levels")]
    TreeTooDeep(TreeId),
    #[error("git tree override {oid} does not describe its tree: {source}")]
    BadOverride {
        oid: GitOid,
        source: crate::tree_override::OverrideError,
    },
    #[error("file {file} hashes to {actual}, recorded as {expected}")]
    BlobMismatch {
        file: FileId,
        expected: GitOid,
        actual: GitOid,
    },
    #[error("changeset {id} renders as commit {actual}, derived as {expected}")]
    CommitMismatch {
        id: ChangesetId,
        expected: GitOid,
        actual: GitOid,
    },
    #[error("ref name {0:?} cannot be passed to update-ref")]
    BadRefName(String),
    #[error("pack would hold more than {} objects", u32::MAX)]
    PackFull,
}

fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> ExportError {
    move |source| ExportError::Io { what, source }
}

/// Something to export, with everything reachable from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportRoot {
    Changeset(ChangesetId),
    Tag(TagId),
}

/// What an export did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Git objects written.
    pub written: u64,
    /// Objects (and, for trees and commits, their closures) the target already had.
    pub skipped: u64,
    /// Whether the objects went into a pack rather than loose files.
    pub packed: bool,
    /// The git oid of each requested root, in order.
    pub tips: Vec<GitOid>,
}

/// Exports asc objects into a git repository.
pub struct Exporter<'a, O: Objects, R: Refs> {
    engine: &'a Engine<O, R>,
    objects: &'a O,
    git: &'a RealGit,
    git_dir: PathBuf,
}

impl<'a, O: Objects, R: Refs> Exporter<'a, O, R> {
    /// An exporter for the git directory `git_dir` (a bare repository or a `.git`).
    /// `objects` is the store `engine` reads from.
    pub fn new(
        engine: &'a Engine<O, R>,
        objects: &'a O,
        git: &'a RealGit,
        git_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            engine,
            objects,
            git,
            git_dir: git_dir.into(),
        }
    }

    /// Writes every object the roots need that `target` lacks. `target` is the object database
    /// of `git_dir`.
    pub async fn export(
        &self,
        roots: &[ExportRoot],
        target: &Odb,
    ) -> Result<ExportReport, ExportError> {
        let mut run = Run {
            target,
            sink: Sink::new(target, self.git_dir.join("objects").join("pack")),
            seen: HashSet::new(),
            visited: HashSet::new(),
            report: ExportReport::default(),
        };
        for root in roots {
            let tip = match *root {
                ExportRoot::Changeset(id) => {
                    self.export_changeset(id, &mut run).await?;
                    self.engine.git_commit_of(id).await?
                }
                ExportRoot::Tag(id) => self.export_tag(id, &mut run).await?,
            };
            run.report.tips.push(tip);
        }
        let Run {
            sink, mut report, ..
        } = run;
        let (written, packed) = sink.finish(self)?;
        report.written = written;
        report.packed = packed;
        Ok(report)
    }

    /// Sets refs in one transaction through `git update-ref --stdin`.
    pub fn set_refs(&self, refs: &[(&str, GitOid)]) -> Result<(), ExportError> {
        if refs.is_empty() {
            return Ok(());
        }
        let mut input = Vec::new();
        for (name, oid) in refs {
            if name.is_empty() || name.contains(['\0', '\n']) {
                return Err(ExportError::BadRefName((*name).to_owned()));
            }
            // `-z` format: update SP <ref> NUL <new> NUL <old> NUL; an empty old skips the check.
            input.extend_from_slice(format!("update {name}\0{oid}\0\0").as_bytes());
        }
        let mut cmd = self.git.command()?;
        cmd.arg("--git-dir")
            .arg(&self.git_dir)
            .args(["update-ref", "--stdin", "-z"]);
        self.run_git(cmd, "update-ref", Some(&input), None)
    }

    fn run_git(
        &self,
        mut cmd: std::process::Command,
        command: &'static str,
        input: Option<&[u8]>,
        stdin_file: Option<File>,
    ) -> Result<(), ExportError> {
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        cmd.stdin(match (&input, stdin_file) {
            (_, Some(f)) => Stdio::from(f),
            (Some(_), None) => Stdio::piped(),
            (None, None) => Stdio::null(),
        });
        let mut child = cmd.spawn().map_err(io("cannot run git"))?;
        if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
            // A failing git closes the pipe early; its exit status explains why.
            let _ = stdin.write_all(bytes);
        }
        let out = child.wait_with_output().map_err(io("waiting for git"))?;
        if out.status.success() {
            return Ok(());
        }
        Err(ExportError::GitFailed {
            command,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }

    /// Exports a tag object after its target's closure; returns the tag's git oid.
    async fn export_tag(&self, id: TagId, run: &mut Run<'_>) -> Result<GitOid, ExportError> {
        let data = self
            .objects
            .get(&ObjKey::Tag(id))
            .await?
            .ok_or(ExportError::MissingTag(id))?;
        let rec: TagRecord = decode_envelope(&data)?;
        self.export_changeset(rec.target, run).await?;
        let target_git = self.engine.git_commit_of(rec.target).await?;
        let raw = export_tag(&rec, target_git)?;
        let oid = object_oid(ObjectKind::Tag, &raw);
        run.put(ObjectKind::Tag, oid, Bytes::from(raw))?;
        Ok(oid)
    }

    /// Exports the unexported part of a changeset's ancestry, parents first.
    async fn export_changeset(
        &self,
        root: ChangesetId,
        run: &mut Run<'_>,
    ) -> Result<(), ExportError> {
        let mut stack = vec![(root, false)];
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                self.emit_commit(id, run).await?;
                continue;
            }
            if !run.visited.insert(id) {
                continue;
            }
            let git = self.engine.git_commit_of(id).await?;
            if run.target.contains(git) {
                run.report.skipped += 1;
                continue;
            }
            stack.push((id, true));
            let cs = self.engine.get_changeset(id).await?;
            stack.extend(cs.parents.iter().rev().map(|&p| (p, false)));
        }
        Ok(())
    }

    async fn emit_commit(&self, id: ChangesetId, run: &mut Run<'_>) -> Result<(), ExportError> {
        let cs = self.engine.get_changeset(id).await?;
        let root = self.engine.get_tree(cs.root).await?;
        let root_git = tree_oid(&root);
        self.export_tree(cs.root, None, run).await?;
        let mut parents = Vec::with_capacity(cs.parents.len());
        for &p in &cs.parents {
            parents.push(self.engine.git_commit_of(p).await?);
        }
        let raw = export_commit(&cs, root_git, &parents)?;
        let oid = object_oid(ObjectKind::Commit, &raw);
        let expected = self.engine.git_commit_of(id).await?;
        if oid != expected {
            return Err(ExportError::CommitMismatch {
                id,
                expected,
                actual: oid,
            });
        }
        run.put(ObjectKind::Commit, oid, Bytes::from(raw))
    }

    /// Exports a tree and what it holds, children first. `expected` is the git oid the parent
    /// entry records; when the canonical render differs, the stored override is the body.
    async fn export_tree(
        &self,
        root: TreeId,
        expected: Option<GitOid>,
        run: &mut Run<'_>,
    ) -> Result<(), ExportError> {
        enum Work {
            Visit(TreeId, Option<GitOid>, usize),
            Emit(GitOid, Bytes),
        }
        let mut stack = vec![Work::Visit(root, expected, 0)];
        while let Some(work) = stack.pop() {
            let (id, expected, depth) = match work {
                Work::Emit(oid, body) => {
                    run.sink.push(ObjectKind::Tree, oid, body)?;
                    continue;
                }
                Work::Visit(id, expected, depth) => (id, expected, depth),
            };
            // The walk is iterative, but the bound keeps the work stack and a corrupt store's
            // pathological nesting finite.
            if depth > MAX_TREE_DEPTH {
                return Err(ExportError::TreeTooDeep(id));
            }
            let tree = self.engine.get_tree(id).await?;
            let rendered = render_tree(&tree);
            let mut oid = object_oid(ObjectKind::Tree, &rendered);
            let mut body = Bytes::from(rendered);
            if let Some(want) = expected.filter(|&w| w != oid) {
                body = self
                    .objects
                    .get(&ObjKey::GitTreeOverride(want))
                    .await?
                    .ok_or(ExportError::MissingOverride(want))?;
                crate::tree_override::verify(&body, &tree)
                    .map_err(|source| ExportError::BadOverride { oid: want, source })?;
                oid = want;
            }
            if !run.seen.insert(oid) || run.target.contains(oid) {
                run.report.skipped += 1;
                continue;
            }
            stack.push(Work::Emit(oid, body));
            for e in tree.entries() {
                match e.kind {
                    Kind::Dir => stack.push(Work::Visit(
                        TreeId::from_bytes(e.id),
                        Some(e.git),
                        depth + 1,
                    )),
                    Kind::Gitlink => {}
                    Kind::File | Kind::Exec | Kind::Symlink => {
                        if !run.seen.insert(e.git) || run.target.contains(e.git) {
                            run.report.skipped += 1;
                            continue;
                        }
                        let file = FileId::from_bytes(e.id);
                        let bytes = self.engine.read_file(file, e.size).await?;
                        let actual = blob_oid(&bytes);
                        if actual != e.git {
                            return Err(ExportError::BlobMismatch {
                                file,
                                expected: e.git,
                                actual,
                            });
                        }
                        run.sink.push(ObjectKind::Blob, e.git, bytes)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// State of one export call.
struct Run<'t> {
    target: &'t Odb,
    sink: Sink<'t>,
    /// Oids already queued, so nothing is written twice.
    seen: HashSet<GitOid>,
    /// Changesets already walked.
    visited: HashSet<ChangesetId>,
    report: ExportReport,
}

impl Run<'_> {
    /// Queues a commit or tag unless the target has it.
    fn put(&mut self, kind: ObjectKind, oid: GitOid, body: Bytes) -> Result<(), ExportError> {
        if !self.seen.insert(oid) || self.target.contains(oid) {
            self.report.skipped += 1;
            return Ok(());
        }
        self.sink.push(kind, oid, body)
    }
}

/// Collects objects as loose files or one pack.
struct Sink<'t> {
    odb: &'t Odb,
    pack_dir: PathBuf,
    pending: Vec<(ObjectKind, GitOid, Bytes)>,
    pending_bytes: usize,
    pack: Option<PackFile>,
    written: u64,
}

impl<'t> Sink<'t> {
    fn new(odb: &'t Odb, pack_dir: PathBuf) -> Self {
        Self {
            odb,
            pack_dir,
            pending: Vec::new(),
            pending_bytes: 0,
            pack: None,
            written: 0,
        }
    }

    fn push(&mut self, kind: ObjectKind, oid: GitOid, body: Bytes) -> Result<(), ExportError> {
        self.written += 1;
        if let Some(pack) = &mut self.pack {
            return pack.add(kind, &body);
        }
        self.pending_bytes += body.len();
        self.pending.push((kind, oid, body));
        if self.pending.len() > PACK_THRESHOLD || self.pending_bytes > MAX_PENDING_BYTES {
            let mut pack = PackFile::create(&self.pack_dir)?;
            for (kind, _, body) in self.pending.drain(..) {
                pack.add(kind, &body)?;
            }
            self.pending_bytes = 0;
            self.pack = Some(pack);
        }
        Ok(())
    }

    /// Makes the objects visible: loose writes, or `git index-pack` of the pack.
    fn finish<O: Objects, R: Refs>(
        self,
        exporter: &Exporter<'_, O, R>,
    ) -> Result<(u64, bool), ExportError> {
        match self.pack {
            Some(pack) => {
                let file = pack.finish()?;
                let mut cmd = exporter.git.command()?;
                cmd.arg("--git-dir")
                    .arg(&exporter.git_dir)
                    .args(["index-pack", "--stdin"]);
                let stdin = File::open(file.path()).map_err(io("cannot read pack"))?;
                exporter.run_git(cmd, "index-pack", None, Some(stdin))?;
                Ok((self.written, true))
            }
            None => {
                for (kind, oid, body) in &self.pending {
                    let got = self.odb.write_loose(*kind, body)?;
                    debug_assert_eq!(got, *oid);
                }
                Ok((self.written, false))
            }
        }
    }
}

/// A pack being written to a temporary file; removed on drop.
struct PackFile {
    out: BufWriter<File>,
    path: PathBuf,
    count: u32,
}

impl PackFile {
    fn create(dir: &Path) -> Result<Self, ExportError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = dir.join(format!("tmp_asc_{}_{nanos}.pack", std::process::id()));
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io("cannot create pack"))?;
        let mut out = BufWriter::new(file);
        // Header placeholder; the object count is patched in `finish`.
        out.write_all(&pack_header(0)).map_err(io("writing pack"))?;
        Ok(Self {
            out,
            path,
            count: 0,
        })
    }

    /// Appends one undeltified entry.
    fn add(&mut self, kind: ObjectKind, body: &[u8]) -> Result<(), ExportError> {
        use gix::odb::pack::data::entry::Header;
        let header = match kind {
            ObjectKind::Blob => Header::Blob,
            ObjectKind::Tree => Header::Tree,
            ObjectKind::Commit => Header::Commit,
            ObjectKind::Tag => Header::Tag,
        };
        let mut z =
            gix::zlib::stream::deflate::Write::new(Vec::new(), gix::zlib::Compression::DEFAULT);
        z.write_all(body)
            .and_then(|()| z.flush())
            .map_err(io("compressing pack entry"))?;
        header
            .write_to(body.len() as u64, &mut self.out)
            .and_then(|_| self.out.write_all(&z.into_inner()))
            .map_err(io("writing pack"))?;
        self.count = self.count.checked_add(1).ok_or(ExportError::PackFull)?;
        Ok(())
    }

    /// Patches the header and appends the SHA-1 trailer. The file is removed when the
    /// returned guard drops.
    fn finish(mut self) -> Result<PackGuard, ExportError> {
        self.out.flush().map_err(io("writing pack"))?;
        let mut file = self.out.get_ref().try_clone().map_err(io("writing pack"))?;
        file.seek(SeekFrom::Start(0)).map_err(io("writing pack"))?;
        file.write_all(&pack_header(self.count))
            .map_err(io("writing pack"))?;
        file.seek(SeekFrom::Start(0)).map_err(io("writing pack"))?;
        let mut sha = Sha1::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(io("reading pack"))?;
            if n == 0 {
                break;
            }
            sha.update(&buf[..n]);
        }
        file.write_all(&sha.finalize())
            .and_then(|()| file.sync_all())
            .map_err(io("writing pack"))?;
        Ok(PackGuard(std::mem::take(&mut self.path)))
    }
}

impl Drop for PackFile {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Deletes the temporary pack when dropped.
struct PackGuard(PathBuf);

impl PackGuard {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PackGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn pack_header(count: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[..4].copy_from_slice(b"PACK");
    h[4..8].copy_from_slice(&2u32.to_be_bytes());
    h[8..].copy_from_slice(&count.to_be_bytes());
    h
}
