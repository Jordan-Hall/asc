//! `status` for a plain working copy (SPEC §9.5, §10.4): staged rows against HEAD, the work tree
//! against the effective tree using the synthetic index as the stat cache, and untracked paths.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use asc_core::git::BlobOidHasher;
use asc_core::id::GitOid;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_engine::Engine;
use asc_git::index::{IndexEntry, IndexFile, StatTime};
use asc_state::StateDb;
use asc_state::state::stage::{self, StageRow, StageSet};
use asc_state::state::workspaces::Workspace;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use futures::TryStreamExt;

use crate::ignore::IgnoreStack;
use crate::plain::{Error, Result, Wc, fs_path};

/// Bytes checked for NUL by git's binary heuristic.
const BINARY_PROBE: usize = 8000;

/// How a path's line endings are normalised on ingest (SPEC §2.4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    /// No conversion.
    Keep,
    /// `text` or `eol=*`: CRLF becomes LF.
    Text,
    /// `text=auto`: as `Text` unless the content looks binary or has a lone CR.
    Auto,
}

/// Attribute lookup for ingest normalisation. The attribute-aware implementation lives with
/// `asc add`; status only needs the answer per path.
pub trait EolRules {
    fn eol(&self, path: &RepoPath) -> Eol;
}

/// No attributes: nothing is normalised.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEol;

impl EolRules for NoEol {
    fn eol(&self, _: &RepoPath) -> Eol {
        Eol::Keep
    }
}

/// What differs for one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    TypeChanged,
}

impl ChangeKind {
    /// git's status letter.
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::TypeChanged => 'T',
        }
    }
}

/// One changed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChange {
    pub path: RepoPath,
    pub kind: ChangeKind,
}

/// An untracked file, or an untracked directory (`dir`) reported as one entry (`?? <dir>/`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Untracked {
    pub path: RepoPath,
    pub dir: bool,
}

/// The result of [`compute`]; every list is sorted by path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Stage rows against HEAD.
    pub staged: Vec<PathChange>,
    /// Work tree against the staged tree.
    pub unstaged: Vec<PathChange>,
    /// Paths with conflict stages.
    pub unmerged: Vec<RepoPath>,
    pub untracked: Vec<Untracked>,
    /// Directories (`""` for the root) whose `.gitignore` differs from the `.ascignore` that
    /// shadows it; print [`crate::ignore::shadow_warning`] for each.
    pub shadowed: Vec<String>,
}

// ---------------------------------------------------------------- probing

/// The work tree at one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    Missing,
    Dir,
    File {
        oid: GitOid,
        symlink: bool,
        /// `None` where the file system has no exec bit.
        exec: Option<bool>,
    },
}

fn stat_matches(e: &IndexEntry, meta: &fs::Metadata, index_mtime: Option<SystemTime>) -> bool {
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let m = StatTime::from_system_time(modified);
    if e.size != meta.len() as u32 || e.mtime != m {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if e.ino != meta.ino() as u32 {
            return false;
        }
    }
    // An entry as new as the index file may predate a same-tick write: hash it.
    index_mtime.is_none_or(|t| e.mtime.to_system_time() < t)
}

#[cfg(unix)]
fn exec_bit(meta: &fs::Metadata) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn exec_bit(_: &fs::Metadata) -> Option<bool> {
    None
}

/// Looks at the work tree at `path`, hashing the content only when the stat cache `cached`
/// (the index entry) does not vouch for it.
pub(crate) fn probe(
    root: &Path,
    eol: &dyn EolRules,
    path: &RepoPath,
    cached: Option<&IndexEntry>,
    index_mtime: Option<SystemTime>,
) -> io::Result<Probe> {
    let abs = fs_path(root, path);
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
        Err(_) => return Ok(Probe::Missing),
    };
    if meta.is_dir() {
        return Ok(Probe::Dir);
    }
    let symlink = meta.file_type().is_symlink();
    let exec = exec_bit(&meta);
    if let Some(e) = cached.filter(|e| stat_matches(e, &meta, index_mtime)) {
        return Ok(Probe::File {
            oid: e.oid,
            symlink,
            exec,
        });
    }
    let oid = if symlink {
        hash_bytes(&link_target(&abs)?)?
    } else {
        hash_file(&abs, eol.eol(path))?
    };
    Ok(Probe::File { oid, symlink, exec })
}

fn link_target(abs: &Path) -> io::Result<Vec<u8>> {
    let t = fs::read_link(abs)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(t.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(t.to_string_lossy().replace('\\', "/").into_bytes())
    }
}

fn hash_bytes(data: &[u8]) -> io::Result<GitOid> {
    let mut h = BlobOidHasher::new(data.len() as u64);
    h.update(data);
    h.finalize().map_err(io::Error::other)
}

/// Streams a file through the blob hasher after the ingest normalisation `eol`.
pub(crate) fn hash_file(path: &Path, eol: Eol) -> io::Result<GitOid> {
    let mut buf = vec![0u8; 64 * 1024];
    let (convert, size) = match eol {
        Eol::Keep => (false, fs::metadata(path)?.len()),
        Eol::Text | Eol::Auto => {
            let s = scan(path, &mut buf)?;
            let convert = eol == Eol::Text || (!s.nul && !s.lone_cr);
            (convert, s.total - if convert { s.crlf } else { 0 })
        }
    };
    let mut hasher = BlobOidHasher::new(size);
    let mut file = fs::File::open(path)?;
    let mut pending_cr = false;
    let mut out = Vec::with_capacity(buf.len());
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if !convert {
            hasher.update(&buf[..n]);
            continue;
        }
        out.clear();
        for &b in &buf[..n] {
            if pending_cr {
                pending_cr = false;
                if b != b'\n' {
                    out.push(b'\r');
                }
            }
            if b == b'\r' {
                pending_cr = true;
            } else {
                out.push(b);
            }
        }
        hasher.update(&out);
    }
    if pending_cr {
        hasher.update(b"\r");
    }
    hasher.finalize().map_err(io::Error::other)
}

struct Scan {
    total: u64,
    crlf: u64,
    lone_cr: bool,
    nul: bool,
}

fn scan(path: &Path, buf: &mut [u8]) -> io::Result<Scan> {
    let mut file = fs::File::open(path)?;
    let mut s = Scan {
        total: 0,
        crlf: 0,
        lone_cr: false,
        nul: false,
    };
    let mut pending_cr = false;
    loop {
        let n = file.read(buf)?;
        if n == 0 {
            break;
        }
        for (i, &b) in buf[..n].iter().enumerate() {
            if s.total + (i as u64) < BINARY_PROBE as u64 && b == 0 {
                s.nul = true;
            }
            if pending_cr {
                pending_cr = false;
                if b == b'\n' {
                    s.crlf += 1;
                } else {
                    s.lone_cr = true;
                }
            }
            if b == b'\r' {
                pending_cr = true;
            }
        }
        s.total += n as u64;
    }
    s.lone_cr |= pending_cr;
    Ok(s)
}

// ---------------------------------------------------------------- compute

/// What a path is staged as: its kind and git oid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Eff {
    kind: Kind,
    git: GitOid,
}

fn class(k: Kind) -> u8 {
    match k {
        Kind::File | Kind::Exec => 0,
        Kind::Symlink => 1,
        Kind::Gitlink => 2,
        Kind::Dir => 3,
    }
}

fn differs(a: Eff, b: Eff) -> Option<ChangeKind> {
    if class(a.kind) != class(b.kind) {
        Some(ChangeKind::TypeChanged)
    } else if a.kind != b.kind || a.git != b.git {
        Some(ChangeKind::Modified)
    } else {
        None
    }
}

fn path_key(p: &RepoPath) -> Vec<u8> {
    p.as_bytes().to_vec()
}

/// Everything status learns while walking the effective tree.
#[derive(Default)]
struct Walk {
    status: Status,
    tracked_files: HashSet<Vec<u8>>,
    tracked_dirs: HashSet<Vec<u8>>,
}

impl Walk {
    fn track(&mut self, p: &RepoPath) {
        let mut cur = p.parent();
        while let Some(d) = cur {
            if !self.tracked_dirs.insert(path_key(&d)) {
                break;
            }
            cur = d.parent();
        }
        self.tracked_files.insert(path_key(p));
    }
}

/// Computes the status of the plain workspace `ws` (SPEC §9.5). Untracked paths come from one
/// walk that honours the ignore rules, redirections and nested workspaces.
pub async fn compute<O: Objects, R: Refs>(
    wc: &Wc<'_, O, R>,
    db: &StateDb,
    ws: &Workspace,
) -> Result<Status> {
    let head = match ws.ws.base_cs {
        Some(cs) => wc.engine.get_changeset(cs).await?.root,
        None => Engine::<O, R>::empty_tree_id(),
    };
    let mut rows: HashMap<Vec<u8>, Vec<StageRow>> = HashMap::new();
    for r in stage::overlay(db, ws.ws.id, StageSet::Default).await? {
        rows.entry(r.path.clone()).or_default().push(r);
    }
    let index = IndexFile::read_or_empty(wc.index_path)?;
    let cache: HashMap<&[u8], &IndexEntry> = index
        .entries
        .iter()
        .map(|e| (e.path.as_bytes(), e))
        .collect();

    let mut w = Walk::default();
    let mut walk = wc.engine.walk(head, None);
    while let Some((path, entry)) = walk.try_next().await? {
        let head_eff = Eff {
            kind: entry.kind,
            git: entry.git,
        };
        let eff = match rows.remove(path.as_bytes()) {
            None => Some(head_eff),
            Some(r) => staged_row(&mut w, wc, &path, Some(head_eff), &r)?,
        };
        if let Some(eff) = eff {
            unstaged(&mut w, wc, &path, eff, &cache, &index, false)?;
        }
    }
    drop(walk);
    let mut rest: Vec<_> = rows.into_iter().collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, r) in rest {
        let Ok(path) = RepoPath::new(key) else {
            continue;
        };
        let intent = r.iter().any(|x| x.intent_to_add);
        if let Some(eff) = staged_row(&mut w, wc, &path, None, &r)? {
            unstaged(&mut w, wc, &path, eff, &cache, &index, intent)?;
        }
    }
    untracked(wc, &mut w)?;
    let mut s = w.status;
    s.staged.sort_by(|a, b| a.path.cmp(&b.path));
    s.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    s.unmerged.sort();
    s.untracked.sort_by(|a, b| a.path.cmp(&b.path));
    s.shadowed.sort();
    s.shadowed.dedup();
    Ok(s)
}

/// Applies the rows of one path to `head`: records the staged change and returns what the path
/// is staged as (`None` for a staged delete or a conflict).
fn staged_row<O: Objects, R: Refs>(
    w: &mut Walk,
    _wc: &Wc<'_, O, R>,
    path: &RepoPath,
    head: Option<Eff>,
    rows: &[StageRow],
) -> Result<Option<Eff>> {
    if rows.iter().any(|r| r.stage > 0) {
        w.status.unmerged.push(path.clone());
        w.track(path);
        return Ok(None);
    }
    let Some(r) = rows.iter().find(|r| r.stage == 0) else {
        return Ok(head);
    };
    let new = match (r.file, r.git) {
        (Some(_), Some(git)) => Some(Eff { kind: r.kind, git }),
        _ => None,
    };
    let change = match (head, new) {
        (Some(_), None) => Some(ChangeKind::Deleted),
        (None, Some(_)) if r.intent_to_add => None,
        (None, Some(_)) => Some(ChangeKind::Added),
        (Some(a), Some(b)) => differs(a, b),
        (None, None) => None,
    };
    if let Some(kind) = change {
        w.status.staged.push(PathChange {
            path: path.clone(),
            kind,
        });
    }
    Ok(new)
}

/// Compares the work tree with the staged state `eff` of `path` and marks the path tracked.
fn unstaged<O: Objects, R: Refs>(
    w: &mut Walk,
    wc: &Wc<'_, O, R>,
    path: &RepoPath,
    eff: Eff,
    cache: &HashMap<&[u8], &IndexEntry>,
    index: &IndexFile,
    intent_to_add: bool,
) -> Result<()> {
    w.track(path);
    let cached = cache.get(path.as_bytes()).copied();
    if cached.is_some_and(|e| e.flags.skip_worktree) || eff.kind == Kind::Gitlink {
        return Ok(());
    }
    let kind = if intent_to_add {
        Some(ChangeKind::Added)
    } else {
        match probe(wc.root, wc.eol, path, cached, index.mtime).map_err(|source| Error::Io {
            path: fs_path(wc.root, path),
            source,
        })? {
            Probe::Missing | Probe::Dir => Some(ChangeKind::Deleted),
            Probe::File { oid, symlink, exec } => {
                let want_link = eff.kind == Kind::Symlink;
                // Without symlink support a link is a regular file holding its target.
                let as_file = want_link && !symlink && !wc.symlinks;
                if want_link != symlink && !as_file {
                    Some(ChangeKind::TypeChanged)
                } else if oid != eff.git
                    || exec.is_some_and(|x| !want_link && x != (eff.kind == Kind::Exec))
                {
                    Some(ChangeKind::Modified)
                } else {
                    None
                }
            }
        }
    };
    if let Some(kind) = kind {
        w.status.unstaged.push(PathChange {
            path: path.clone(),
            kind,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------- untracked

struct Finder<'a> {
    redirections: &'a crate::redirect::Redirections,
    tracked_files: &'a HashSet<Vec<u8>>,
    tracked_dirs: &'a HashSet<Vec<u8>>,
    ignore: IgnoreStack,
    untracked: Vec<Untracked>,
    shadowed: Vec<String>,
}

#[cfg(unix)]
fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    n.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    n.to_string_lossy().into_owned().into_bytes()
}

fn untracked<O: Objects, R: Refs>(wc: &Wc<'_, O, R>, w: &mut Walk) -> Result<()> {
    let mut f = Finder {
        redirections: wc.redirections,
        tracked_files: &w.tracked_files,
        tracked_dirs: &w.tracked_dirs,
        ignore: IgnoreStack::new(wc.root, wc.ignore),
        untracked: Vec::new(),
        shadowed: Vec::new(),
    };
    let io_err = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    f.dir(None, wc.root, true).map_err(io_err(wc.root))?;
    w.status.untracked = f.untracked;
    w.status.shadowed = f.shadowed;
    Ok(())
}

impl Finder<'_> {
    /// Walks `abs` (below repository path `rel`). With `report`, collects untracked entries;
    /// without, only answers whether the directory holds anything visible, stopping at the first.
    fn dir(&mut self, rel: Option<&RepoPath>, abs: &Path, report: bool) -> io::Result<bool> {
        if self.ignore.push_dir(abs)? {
            let dir = rel.map(|r| String::from_utf8_lossy(r.as_bytes()).into_owned());
            self.shadowed.push(dir.unwrap_or_default());
        }
        let found = self.entries(rel, abs, report);
        self.ignore.pop_dir();
        found
    }

    fn entries(&mut self, rel: Option<&RepoPath>, abs: &Path, report: bool) -> io::Result<bool> {
        let mut visible = false;
        for entry in fs::read_dir(abs)? {
            let entry = entry?;
            let name = name_bytes(&entry.file_name());
            if name == b".git" || self.redirections.is_redirected_name(&name) {
                continue;
            }
            let Ok(child) = (match rel {
                Some(r) => RepoPath::new([r.as_bytes(), b"/", &name].concat()),
                None => RepoPath::new(name),
            }) else {
                continue;
            };
            let child_abs = entry.path();
            let is_dir = entry.file_type()?.is_dir();
            if self.ignore.is_ignored(&child_abs, is_dir) {
                continue;
            }
            let key = child.as_bytes();
            if !is_dir {
                if self.tracked_files.contains(key) {
                    continue;
                }
                visible = true;
                if report {
                    self.untracked.push(Untracked {
                        path: child,
                        dir: false,
                    });
                } else {
                    return Ok(true);
                }
            } else if self.tracked_files.contains(key) {
                // A gitlink: not descended.
            } else if self.tracked_dirs.contains(key) {
                visible |= self.dir(Some(&child), &child_abs, report)?;
                if visible && !report {
                    return Ok(true);
                }
            } else if child_abs.join(".git").exists()
                || self.dir(Some(&child), &child_abs, false)?
            {
                // A nested workspace, or a directory with something to show.
                visible = true;
                if report {
                    self.untracked.push(Untracked {
                        path: child,
                        dir: true,
                    });
                } else {
                    return Ok(true);
                }
            }
        }
        Ok(visible)
    }
}

/// The work tree path of an untracked entry, for callers that print or delete it.
pub fn untracked_abs(root: &Path, u: &Untracked) -> PathBuf {
    fs_path(root, &u.path)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use asc_core::codec::Enveloped;
    use asc_core::git::blob_oid;
    use asc_core::id::{ChangesetId, FileId};
    use asc_core::object::{Changeset, SortedMap};
    use asc_core::sig::Signature;
    use asc_state::state::repos::{self, NewRepo, RemoteKind};
    use asc_state::state::workspaces::{self, Mode as WsMode, NewWorkspace};
    use asc_state::{AscHome, RepoId, WorkspaceId};
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;

    use super::*;
    use crate::plain::testutil::*;
    use crate::plain::{Mode, checkout};

    const F: Kind = Kind::File;

    /// A state db with one plain workspace whose base changeset has `root`.
    async fn workspace(fx: &Fixture, root: asc_core::id::TreeId) -> (StateDb, Workspace) {
        let home = AscHome::open(fx._dir.path().join("home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let repo = repos::insert(
            &db,
            &NewRepo {
                id: RepoId::new(),
                name: "r".into(),
                ns: vec![1],
                remote_url: None,
                remote_kind: RemoteKind::Server,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        let sig = Signature::new(b"T".to_vec(), b"t@example.com".to_vec(), 1, 0).unwrap();
        let cs = Changeset {
            parents: vec![],
            root,
            author: sig.clone(),
            committer: sig,
            message: b"m".to_vec(),
            extra: SortedMap::new(vec![]).unwrap(),
            changes: SortedMap::new(vec![]).unwrap(),
        };
        let env = cs.to_envelope();
        let id = ChangesetId::of(&env);
        fx.store
            .put(&ObjKey::Changeset(id), encode_stored(&env))
            .await
            .unwrap();
        let new = NewWorkspace {
            id: WorkspaceId::new(),
            repo,
            path: "w".into(),
            mount: "w".into(),
            mode: WsMode::Plain,
            head: "master".into(),
            base_cs: Some(id),
            sparse: None,
            ephemeral_until: None,
        };
        workspaces::insert(&db, &new).await.unwrap();
        let ws = workspaces::get(&db, new.id).await.unwrap().unwrap();
        (db, ws)
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git") // lint:allow-raw-git
            .args(["-c", "core.autocrlf=false", "-c", "user.name=t"])
            .args(["-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    const TREE: &[(&str, Kind, &[u8])] = &[
        (".gitignore", F, b"*.log\nbuild/\n"),
        ("a.txt", F, b"one\n"),
        ("d/b.txt", F, b"two\n"),
        ("d/c.txt", F, b"three\n"),
        ("e/f.txt", F, b"four\n"),
        ("keep.txt", F, b"keep\n"),
    ];

    /// Edits applied to a materialised copy of `TREE`; the same on both sides.
    fn mutate(root: &Path) {
        let w = |rel: &str, data: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        };
        w("a.txt", "one but longer\n");
        fs::remove_file(root.join("d/c.txt")).unwrap();
        w("new.txt", "n\n");
        w("newdir/x.txt", "x\n");
        w("newdir/deep/y.txt", "y\n");
        w("debug.log", "ignored\n");
        w("build/out.o", "ignored\n");
        w("onlyignored/z.log", "ignored\n");
        w("scratch.txt", "excluded\n");
        w("e/extra.txt", "e\n");
        fs::create_dir_all(root.join("emptydir")).unwrap();
    }

    /// Lines in `git status --porcelain=v2` shape, paths only.
    fn from_git(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = git(
            dir,
            &["status", "--porcelain=v2", "--untracked-files=normal"],
        )
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(9, ' ').collect();
            match f[0] {
                "1" => Some(format!("{} {}", f[1], f[8])),
                "?" => Some(format!("? {}", &l[2..])),
                _ => None,
            }
        })
        .collect();
        v.sort();
        v
    }

    fn from_status(s: &Status) -> Vec<String> {
        let text = |p: &RepoPath| String::from_utf8_lossy(p.as_bytes()).into_owned();
        let mut staged: HashMap<String, char> = HashMap::new();
        for c in &s.staged {
            staged.insert(text(&c.path), c.kind.letter());
        }
        let mut v: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for c in &s.unstaged {
            let path = text(&c.path);
            let x = staged.get(&path).copied().unwrap_or('.');
            v.push(format!("{x}{} {path}", c.kind.letter()));
            seen.insert(path);
        }
        for (path, x) in staged {
            if !seen.contains(&path) {
                v.push(format!("{x}. {path}"));
            }
        }
        for u in &s.untracked {
            v.push(format!(
                "? {}{}",
                text(&u.path),
                if u.dir { "/" } else { "" }
            ));
        }
        v.sort();
        v
    }

    #[tokio::test]
    async fn status_equals_git_status_on_the_same_tree() {
        let fx = Fixture::new();
        let tree = fx.tree(TREE).await;
        checkout(&fx.wc(), fx.empty(), tree, Mode::Safe)
            .await
            .unwrap();
        mutate(&fx.root);
        fs::create_dir_all(fx.root.join(".asc/info")).unwrap();
        fs::write(fx.root.join(".asc/info/exclude"), "scratch.txt\n").unwrap();

        let reference = fx._dir.path().join("ref");
        fs::create_dir_all(&reference).unwrap();
        git(&reference, &["init", "-q"]);
        for (name, _, data) in TREE {
            let p = reference.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        }
        git(&reference, &["add", "-A"]);
        git(&reference, &["commit", "-q", "-m", "init"]);
        mutate(&reference);
        fs::write(reference.join(".git/info/exclude"), "scratch.txt\n").unwrap();

        let (db, ws) = workspace(&fx, tree).await;
        let st = compute(&fx.wc(), &db, &ws).await.unwrap();
        let ours = from_status(&st);
        let theirs = from_git(&reference);
        assert_eq!(ours, theirs);
        assert!(ours.contains(&"? newdir/".to_string()));
        assert!(!ours.iter().any(|l| l.contains(".asc")), "{ours:?}");
        assert!(st.shadowed.is_empty());
    }

    #[tokio::test]
    async fn clean_checkout_is_clean_and_hashes_nothing_it_can_skip() {
        let fx = Fixture::new();
        let tree = fx.tree(TREE).await;
        checkout(&fx.wc(), fx.empty(), tree, Mode::Safe)
            .await
            .unwrap();
        let (db, ws) = workspace(&fx, tree).await;
        assert_eq!(
            compute(&fx.wc(), &db, &ws).await.unwrap(),
            Status::default()
        );
    }

    #[tokio::test]
    async fn staged_rows_are_compared_with_head() {
        let fx = Fixture::new();
        let tree = fx.tree(TREE).await;
        checkout(&fx.wc(), fx.empty(), tree, Mode::Safe)
            .await
            .unwrap();
        let (db, ws) = workspace(&fx, tree).await;
        fs::write(fx.root.join("s.txt"), b"staged\n").unwrap();
        let rows = [
            StageRow {
                path: b"s.txt".to_vec(),
                stage: 0,
                kind: F,
                file: Some(FileId::of(b"staged\n")),
                size: 7,
                git: Some(blob_oid(b"staged\n")),
                intent_to_add: false,
            },
            StageRow::from_leaf(b"keep.txt".to_vec(), None),
        ];
        stage::upsert(&db, ws.ws.id, StageSet::Default, &rows)
            .await
            .unwrap();
        let st = compute(&fx.wc(), &db, &ws).await.unwrap();
        let got = from_status(&st);
        assert_eq!(got, ["? keep.txt", "A. s.txt", "D. keep.txt"]);
        assert!(st.unstaged.is_empty());
    }

    #[tokio::test]
    async fn differing_shadowed_gitignore_is_reported_and_identical_is_not() {
        for (asc, expect) in [(&b"b\n"[..], vec![String::new()]), (&b"a\n"[..], vec![])] {
            let fx = Fixture::new();
            let tree = fx
                .tree(&[(".gitignore", F, b"a\n"), (".ascignore", F, asc)])
                .await;
            checkout(&fx.wc(), fx.empty(), tree, Mode::Safe)
                .await
                .unwrap();
            let (db, ws) = workspace(&fx, tree).await;
            let st = compute(&fx.wc(), &db, &ws).await.unwrap();
            assert_eq!(st.shadowed, expect);
        }
    }

    #[tokio::test]
    async fn redirected_names_and_nested_workspaces_are_not_descended() {
        let mut fx = Fixture::new();
        fx.redirections = crate::redirect::Redirections::parse("node_modules\n");
        let tree = fx.tree(&[("a.txt", F, b"a")]).await;
        checkout(&fx.wc(), fx.empty(), tree, Mode::Safe)
            .await
            .unwrap();
        fs::create_dir_all(fx.root.join("node_modules/pkg")).unwrap();
        fs::write(fx.root.join("node_modules/pkg/i.js"), b"x").unwrap();
        fs::create_dir_all(fx.root.join("nested/src")).unwrap();
        fs::write(fx.root.join("nested/.git"), b"gitdir: elsewhere").unwrap();
        fs::write(fx.root.join("nested/src/m.rs"), b"x").unwrap();
        let (db, ws) = workspace(&fx, tree).await;
        let st = compute(&fx.wc(), &db, &ws).await.unwrap();
        let got: Vec<_> = st
            .untracked
            .iter()
            .map(|u| {
                (
                    String::from_utf8_lossy(u.path.as_bytes()).into_owned(),
                    u.dir,
                )
            })
            .collect();
        assert_eq!(got, [("nested".to_string(), true)]);
    }

    #[test]
    fn eol_normalisation_hashes_like_git_add() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("t.txt");
        fs::write(&f, b"a\r\nb\r\nc").unwrap();
        assert_eq!(hash_file(&f, Eol::Text).unwrap(), blob_oid(b"a\nb\nc"));
        assert_eq!(hash_file(&f, Eol::Auto).unwrap(), blob_oid(b"a\nb\nc"));
        assert_eq!(hash_file(&f, Eol::Keep).unwrap(), blob_oid(b"a\r\nb\r\nc"));
        fs::write(&f, b"a\rb\r\n").unwrap();
        assert_eq!(hash_file(&f, Eol::Auto).unwrap(), blob_oid(b"a\rb\r\n"));
        assert_eq!(hash_file(&f, Eol::Text).unwrap(), blob_oid(b"a\rb\n"));
        fs::write(&f, b"\0bin\r\n").unwrap();
        assert_eq!(hash_file(&f, Eol::Auto).unwrap(), blob_oid(b"\0bin\r\n"));
    }
}
