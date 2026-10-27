//! The working copy of a run: a directory the agent edits and runs commands in.
//!
//! The base tree of the run is written into `<work_dir>/run-<id>-<random>/tree`. Tools read and
//! write there; a command runs there (in the sandbox); a commit turns the difference between the
//! directory and the base tree into the file operations of one `Repo.Commit`. Rules:
//!
//! * A path a tool names is relative, made of plain names, and every part that already exists must
//!   be a real directory or file, never a link: the copy is confined to its directory even if a
//!   command made a link in it.
//! * Links are not part of a working copy. The base tree's links are not written, and links found
//!   on disk are ignored by the commit.
//! * The copy is bounded: files, total bytes and bytes per file ([`Limits`]). A tree that does not
//!   fit is refused before anything is written.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use asc_core::id::TreeId;
use bytes::Bytes;
use similar::TextDiff;
use tonic::Status;

use super::repo::{RepoView, TreeFile, WalkError};
use crate::registry::RepoEntry;

/// Bounds of one working copy.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_files: usize,
    pub max_bytes: u64,
    pub max_file_bytes: u64,
}

/// Why a path or a write was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkError {
    #[error("{0}")]
    Path(String),
    #[error("{0}")]
    Limit(String),
    #[error("no such file: {0}")]
    Missing(String),
    #[error("{0}")]
    Io(String),
}

fn io(e: &std::io::Error) -> WorkError {
    WorkError::Io(format!("file system error: {}", e.kind()))
}

/// Names Windows reserves for devices.
const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A path a tool may name: relative, `/`-separated plain names, bounded. Returns it normalised.
pub fn clean_rel(rel: &str) -> Result<String, WorkError> {
    let bad = |m: &str| Err(WorkError::Path(m.to_owned()));
    if rel.is_empty() || rel.len() > 1024 {
        return bad("a path is 1 to 1024 bytes");
    }
    if rel.chars().any(char::is_control) || rel.contains('\\') {
        return bad("a path holds a control character or a backslash");
    }
    if rel.starts_with('/') {
        return bad("a path is relative to the repository root");
    }
    let trimmed = rel.trim_end_matches('/');
    let mut parts = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return bad("a path has an empty, `.` or `..` part");
        }
        let lower = seg.to_ascii_lowercase();
        let stem = lower.split('.').next().unwrap_or("");
        if cfg!(windows)
            && (seg.contains(':')
                || seg.ends_with('.')
                || seg.ends_with(' ')
                || seg.contains(['<', '>', '"', '|', '?', '*'])
                || RESERVED.contains(&stem))
        {
            return bad("that name cannot be written on this server");
        }
        parts.push(seg);
    }
    Ok(parts.join("/"))
}

/// A changed file, ready for a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    Put {
        path: String,
        content: Bytes,
        exec: bool,
    },
    Delete {
        path: String,
    },
}

impl Edit {
    pub fn path(&self) -> &str {
        match self {
            Self::Put { path, .. } | Self::Delete { path } => path,
        }
    }
}

struct DiskFile {
    size: u64,
    exec: bool,
}

/// The working copy.
pub struct Workspace {
    dir: PathBuf,
    tree: PathBuf,
    base: BTreeMap<String, TreeFile>,
    view: Arc<RepoView>,
    entry: Arc<RepoEntry>,
    limits: Limits,
}

impl Workspace {
    /// Creates the directory and writes the files of `root` into it.
    pub async fn create(
        view: Arc<RepoView>,
        entry: Arc<RepoEntry>,
        root: Option<TreeId>,
        work_dir: &Path,
        run_id: u64,
        limits: Limits,
    ) -> Result<Self, Status> {
        let mut raw = [0u8; 6];
        getrandom::fill(&mut raw).map_err(|_| Status::internal("internal error"))?;
        let tag: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let dir = work_dir.join(format!("run-{run_id}-{tag}"));
        let tree = dir.join("tree");
        tokio::fs::create_dir_all(&tree).await.map_err(|e| {
            Status::unavailable(format!("cannot create the working directory: {}", e.kind()))
        })?;
        let files = match root {
            Some(r) => view
                .walk(&entry, r, limits.max_files, limits.max_bytes)
                .await
                .map_err(|e| match e {
                    WalkError::TooManyFiles => Status::failed_precondition(format!(
                        "the repository has more than {} files; an agent run cannot copy it",
                        limits.max_files
                    )),
                    WalkError::TooManyBytes => Status::failed_precondition(format!(
                        "the repository is larger than {} bytes; an agent run cannot copy it",
                        limits.max_bytes
                    )),
                    WalkError::Store(_) => Status::internal("internal error"),
                })?,
            None => Vec::new(),
        };
        let ws = Self {
            dir,
            tree,
            base: files.iter().map(|f| (f.path.clone(), f.clone())).collect(),
            view,
            entry,
            limits,
        };
        if let Err(e) = ws.materialize(&files).await {
            ws.cleanup().await;
            return Err(e);
        }
        Ok(ws)
    }

    async fn materialize(&self, files: &[TreeFile]) -> Result<(), Status> {
        let engine = self.entry.backend.engine();
        for f in files {
            // A name this server cannot write (a Windows device name, a colon) is left out of the
            // copy and so cannot be edited; it stays in the base tree and in every commit.
            let Ok(rel) = clean_rel(&f.path) else {
                continue;
            };
            let path = self.tree.join(&rel);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|_| Status::internal("internal error"))?;
            }
            let data = engine
                .read_file(f.id, f.size)
                .await
                .map_err(|e| crate::update::internal("workspace: read", &e))?;
            tokio::fs::write(&path, &data)
                .await
                .map_err(|_| Status::internal("internal error"))?;
            #[cfg(unix)]
            if f.exec {
                use std::os::unix::fs::PermissionsExt as _;
                let _ =
                    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).await;
            }
        }
        Ok(())
    }

    /// The directory commands run in.
    pub fn tree(&self) -> &Path {
        &self.tree
    }

    /// The directory that holds the copy and the commands' scratch space.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Deletes the working directory.
    pub async fn cleanup(&self) {
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }

    /// The absolute path of `rel` in the copy. No existing part of it may be a link.
    pub async fn resolve(&self, rel: &str) -> Result<PathBuf, WorkError> {
        let clean = clean_rel(rel)?;
        let mut cur = self.tree.clone();
        for seg in clean.split('/') {
            cur.push(seg);
            match tokio::fs::symlink_metadata(&cur).await {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(WorkError::Path(format!("{clean} goes through a link")));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(&e)),
            }
        }
        Ok(cur)
    }

    /// The text of a file, at most `max` bytes.
    pub async fn read(&self, rel: &str, max: u64) -> Result<Vec<u8>, WorkError> {
        let p = self.resolve(rel).await?;
        let meta = tokio::fs::metadata(&p).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                WorkError::Missing(rel.to_owned())
            } else {
                io(&e)
            }
        })?;
        if !meta.is_file() {
            return Err(WorkError::Path(format!("{rel} is not a file")));
        }
        if meta.len() > max {
            return Err(WorkError::Limit(format!(
                "{rel} is larger than {max} bytes"
            )));
        }
        tokio::fs::read(&p).await.map_err(|e| io(&e))
    }

    /// Writes (creating or replacing) a file. Refused when it would take the copy past a bound.
    pub async fn write(&self, rel: &str, content: &[u8]) -> Result<(), WorkError> {
        if content.len() as u64 > self.limits.max_file_bytes {
            return Err(WorkError::Limit(format!(
                "a file is at most {} bytes",
                self.limits.max_file_bytes
            )));
        }
        let p = self.resolve(rel).await?;
        if let Ok(m) = tokio::fs::metadata(&p).await
            && m.is_dir()
        {
            return Err(WorkError::Path(format!("{rel} is a directory")));
        }
        let (files, bytes) = self.usage().await?;
        let old = tokio::fs::metadata(&p).await.map_or(0, |m| m.len());
        let is_new = !p.exists();
        if is_new && files >= self.limits.max_files {
            return Err(WorkError::Limit(format!(
                "the working copy holds at most {} files",
                self.limits.max_files
            )));
        }
        if bytes
            .saturating_sub(old)
            .saturating_add(content.len() as u64)
            > self.limits.max_bytes
        {
            return Err(WorkError::Limit(format!(
                "the working copy holds at most {} bytes",
                self.limits.max_bytes
            )));
        }
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io(&e))?;
        }
        // A directory made by create_dir_all could, through a race with a command, be a link.
        let again = self.resolve(rel).await?;
        tokio::fs::write(&again, content).await.map_err(|e| io(&e))
    }

    /// Deletes a file.
    pub async fn delete(&self, rel: &str) -> Result<(), WorkError> {
        let p = self.resolve(rel).await?;
        match tokio::fs::metadata(&p).await {
            Ok(m) if m.is_file() => tokio::fs::remove_file(&p).await.map_err(|e| io(&e)),
            Ok(_) => Err(WorkError::Path(format!("{rel} is not a file"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(WorkError::Missing(rel.to_owned()))
            }
            Err(e) => Err(io(&e)),
        }
    }

    /// Moves a file.
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), WorkError> {
        let data = self.read(from, self.limits.max_file_bytes).await?;
        let src = self.resolve(from).await?;
        let dest = self.resolve(to).await?;
        if tokio::fs::metadata(&dest).await.is_ok() {
            return Err(WorkError::Path(format!("{to} exists")));
        }
        self.write(to, &data).await?;
        tokio::fs::remove_file(&src).await.map_err(|e| io(&e))
    }

    /// The entries of a directory (`""` is the root): `(name, is_dir, size)`.
    pub async fn list(&self, rel: &str) -> Result<Vec<(String, bool, u64)>, WorkError> {
        let p = if rel.is_empty() {
            self.tree.clone()
        } else {
            self.resolve(rel).await?
        };
        let mut rd = tokio::fs::read_dir(&p).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                WorkError::Missing(rel.to_owned())
            } else {
                io(&e)
            }
        })?;
        let mut out = Vec::new();
        while let Some(e) = rd.next_entry().await.map_err(|e| io(&e))? {
            let Ok(t) = e.file_type().await else { continue };
            if t.is_symlink() {
                continue;
            }
            let size = e.metadata().await.map_or(0, |m| m.len());
            out.push((
                e.file_name().to_string_lossy().into_owned(),
                t.is_dir(),
                size,
            ));
        }
        out.sort();
        Ok(out)
    }

    /// Every file of the copy with its size and executable bit; links are skipped.
    async fn disk(&self) -> Result<BTreeMap<String, DiskFile>, WorkError> {
        let root = self.tree.clone();
        let limits = self.limits;
        tokio::task::spawn_blocking(move || {
            let mut out = BTreeMap::new();
            let mut bytes = 0u64;
            let mut stack = vec![(root.clone(), String::new())];
            while let Some((dir, prefix)) = stack.pop() {
                for e in std::fs::read_dir(&dir).map_err(|e| io(&e))? {
                    let e = e.map_err(|e| io(&e))?;
                    let t = e.file_type().map_err(|e| io(&e))?;
                    let name = e.file_name().to_string_lossy().into_owned();
                    let rel = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    if t.is_symlink() {
                        continue;
                    }
                    if t.is_dir() {
                        stack.push((e.path(), rel));
                    } else if t.is_file() {
                        let m = e.metadata().map_err(|e| io(&e))?;
                        bytes = bytes.saturating_add(m.len());
                        if out.len() >= limits.max_files || bytes > limits.max_bytes {
                            return Err(WorkError::Limit(
                                "the working copy grew past its bounds".into(),
                            ));
                        }
                        #[cfg(unix)]
                        let exec = {
                            use std::os::unix::fs::PermissionsExt as _;
                            m.permissions().mode() & 0o111 != 0
                        };
                        #[cfg(not(unix))]
                        let exec = false;
                        out.insert(
                            rel,
                            DiskFile {
                                size: m.len(),
                                exec,
                            },
                        );
                    }
                }
            }
            Ok(out)
        })
        .await
        .map_err(|_| WorkError::Io("internal error".into()))?
    }

    /// Every file of the copy with its size, sorted by path.
    pub async fn files(&self) -> Result<Vec<(String, u64)>, WorkError> {
        Ok(self
            .disk()
            .await?
            .into_iter()
            .map(|(p, f)| (p, f.size))
            .collect())
    }

    /// The root tree of a committed changeset (32 bytes), if it can be read.
    pub async fn root_of_commit(&self, changeset: &[u8]) -> Option<TreeId> {
        let cs = super::repo::changeset_id(changeset)?;
        self.view.root_of(&self.entry, cs).await.ok()
    }

    async fn usage(&self) -> Result<(usize, u64), WorkError> {
        let d = self.disk().await?;
        Ok((d.len(), d.values().map(|f| f.size).sum()))
    }

    /// The edits that turn the base tree into the directory, sorted by path. Files whose names
    /// the copy could not hold stay as they are. At most `max_ops` edits and no file over the
    /// per-file bound; past either the call fails, so a commit is never silently partial.
    pub async fn edits(&self, max_ops: usize) -> Result<Vec<Edit>, WorkError> {
        let disk = self.disk().await?;
        let engine = self.entry.backend.engine();
        let mut out = Vec::new();
        for (path, d) in &disk {
            let base = self.base.get(path);
            let exec = if cfg!(unix) {
                d.exec
            } else {
                base.is_some_and(|b| b.exec)
            };
            if d.size > self.limits.max_file_bytes {
                return Err(WorkError::Limit(format!(
                    "{path} is larger than {} bytes",
                    self.limits.max_file_bytes
                )));
            }
            let changed = match base {
                None => true,
                Some(b) if b.size != d.size || b.exec != exec => true,
                Some(b) => {
                    let old = engine
                        .read_file(b.id, b.size)
                        .await
                        .map_err(|e| WorkError::Io(format!("cannot read the base file: {e}")))?;
                    let new = tokio::fs::read(self.tree.join(path))
                        .await
                        .map_err(|e| io(&e))?;
                    old[..] != new[..]
                }
            };
            if changed {
                let new = tokio::fs::read(self.tree.join(path))
                    .await
                    .map_err(|e| io(&e))?;
                out.push(Edit::Put {
                    path: path.clone(),
                    content: Bytes::from(new),
                    exec,
                });
            }
        }
        for path in self.base.keys() {
            // Only paths the copy could hold are deleted by absence.
            if !disk.contains_key(path) && clean_rel(path).is_ok() {
                out.push(Edit::Delete { path: path.clone() });
            }
        }
        out.sort_by(|a, b| a.path().cmp(b.path()));
        if out.len() > max_ops {
            return Err(WorkError::Limit(format!(
                "a change holds at most {max_ops} file edits"
            )));
        }
        Ok(out)
    }

    /// A unified diff of one changed path, or of all of them when `only` is `None`.
    pub async fn diff(&self, only: Option<&str>, max_bytes: usize) -> Result<String, WorkError> {
        let edits = self.edits(usize::MAX).await?;
        let engine = self.entry.backend.engine();
        let mut out = String::new();
        for e in edits {
            if only.is_some_and(|o| o != e.path()) {
                continue;
            }
            let (old, new): (Vec<u8>, Vec<u8>) = match &e {
                Edit::Put { path, content, .. } => {
                    let old = match self.base.get(path) {
                        Some(b) => engine
                            .read_file(b.id, b.size)
                            .await
                            .map(|b| b.to_vec())
                            .map_err(|e| {
                                WorkError::Io(format!("cannot read the base file: {e}"))
                            })?,
                        None => Vec::new(),
                    };
                    (old, content.to_vec())
                }
                Edit::Delete { path } => {
                    let b = &self.base[path];
                    let old = engine
                        .read_file(b.id, b.size)
                        .await
                        .map(|b| b.to_vec())
                        .map_err(|e| WorkError::Io(format!("cannot read the base file: {e}")))?;
                    (old, Vec::new())
                }
            };
            let path = e.path();
            if scrapes_as_binary(&old) || scrapes_as_binary(&new) {
                out.push_str(&format!(
                    "Binary file {path} changed ({} -> {} bytes)\n",
                    old.len(),
                    new.len()
                ));
            } else {
                let (o, n) = (String::from_utf8_lossy(&old), String::from_utf8_lossy(&new));
                let d = TextDiff::from_lines(&*o, &*n);
                out.push_str(
                    &d.unified_diff()
                        .context_radius(3)
                        .header(&format!("a/{path}"), &format!("b/{path}"))
                        .to_string(),
                );
            }
            if out.len() > max_bytes {
                out.truncate(max_bytes);
                out.push_str("\n[... truncated]\n");
                break;
            }
        }
        Ok(out)
    }

    /// The paths that differ, as `A path`, `M path`, `D path` lines.
    pub async fn status(&self) -> Result<Vec<String>, WorkError> {
        let edits = self.edits(usize::MAX).await?;
        Ok(edits
            .iter()
            .map(|e| match e {
                Edit::Put { path, .. } if self.base.contains_key(path) => format!("M {path}"),
                Edit::Put { path, .. } => format!("A {path}"),
                Edit::Delete { path } => format!("D {path}"),
            })
            .collect())
    }

    /// Makes the tree of a committed changeset the new base.
    pub async fn rebase_to(&mut self, root: TreeId) -> Result<(), Status> {
        let files = self
            .view
            .walk(
                &self.entry,
                root,
                self.limits.max_files.saturating_mul(2),
                self.limits.max_bytes.saturating_mul(2),
            )
            .await
            .map_err(|_| Status::internal("internal error"))?;
        self.base = files.into_iter().map(|f| (f.path.clone(), f)).collect();
        Ok(())
    }

    /// Paths of the base tree.
    pub fn base_paths(&self) -> BTreeSet<&str> {
        self.base.keys().map(String::as_str).collect()
    }
}

fn scrapes_as_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_plain_relative_names() {
        assert_eq!(clean_rel("src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(clean_rel("dir/").unwrap(), "dir");
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a/./b",
            "a\\b",
            "a\0b",
            "a\nb",
            &"x".repeat(1025),
            "..",
            ".",
        ] {
            assert!(clean_rel(bad).is_err(), "{bad:?}");
        }
        if cfg!(windows) {
            for bad in ["con", "a/NUL.txt", "a:b", "x.", "x ", "a?b", "COM1"] {
                assert!(clean_rel(bad).is_err(), "{bad:?}");
            }
        } else {
            assert!(clean_rel("a:b").is_ok());
        }
    }
}
