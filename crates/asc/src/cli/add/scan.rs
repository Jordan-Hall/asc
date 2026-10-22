//! Plans `add`: which paths to stage, which to unstage, and which are ignored. Tracked paths
//! are compared with the work tree through the synthetic index as stat cache, hashing only
//! stat-dirty files; new files come from one walk that honours the ignore rules (SPEC §2.4.2).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use asc_core::id::GitOid;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_git::index::{IndexEntry, IndexFile, StatTime};
use asc_state::state::stage::Leaf;
use asc_wc::ignore::{IgnoreSources, IgnoreStack, shadow_warning};
use asc_wc::plain::fs_path;
use asc_wc::redirect::Redirections;
use asc_wc::status::Eol;

use super::attrs::{Attrs, PathAttrs};
use super::convert;
use super::spec::Spec;
use super::target::{Staged, Target, Tracked};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};

/// The flags that shape a plan.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    /// `-u`: tracked paths only.
    pub update: bool,
    /// `-N`: new paths become intent-to-add entries; tracked paths are left alone.
    pub intent: bool,
    /// `-f`: ignored paths are added too.
    pub force: bool,
}

/// One change to make.
#[derive(Clone, Debug)]
pub enum Op {
    /// Read `path` from the work tree and stage it.
    Add {
        path: RepoPath,
        attrs: PathAttrs,
        ita: bool,
        /// What the path is staged as now (a work tree without an exec bit keeps its mode).
        prev: Option<Kind>,
    },
    /// Stage the removal of `path`.
    Remove(RepoPath),
}

/// The result of planning.
#[derive(Debug, Default)]
pub struct Plan {
    pub ops: Vec<Op>,
    /// Ignored paths a pathspec named.
    pub ignored: Vec<RepoPath>,
    /// Directories whose `.gitignore` a differing `.ascignore` shadows.
    pub shadowed: Vec<String>,
    /// The first path stored natively despite `filter=lfs`.
    pub lfs: Option<RepoPath>,
}

impl Plan {
    /// Prints the SPEC §2.4.2 shadow warnings and the §2.4.1 lfs warning, once each.
    pub fn warn(&mut self) {
        self.shadowed.sort();
        self.shadowed.dedup();
        for dir in &self.shadowed {
            eprintln!("{}", shadow_warning(dir));
        }
        if let Some(path) = &self.lfs {
            eprintln!("warning: asc: ignoring filter=lfs for {path} (large files are native)");
        }
    }

    fn note_lfs(&mut self, path: &RepoPath, attrs: &PathAttrs) {
        if attrs.lfs && self.lfs.is_none() {
            self.lfs = Some(path.clone());
        }
    }
}

/// The ignore sources of the workspace (SPEC §2.4.2).
pub fn ignore_sources(ctx: &Ctx, ws: &WsCtx) -> IgnoreSources {
    let root = ws.projection.toplevel();
    let asc_excludes = ctx
        .config
        .path("asc.excludesFile")
        .or_else(|| std::env::home_dir().map(|h| h.join(".config").join("asc").join("ignore")))
        .unwrap_or_default();
    IgnoreSources {
        asc_excludes,
        core_excludes: ctx.config.path("core.excludesFile"),
        local_exclude: root.join(".asc").join("info").join("exclude"),
        common_exclude: ws.projection.common_dir().join("info").join("exclude"),
    }
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> CliError + '_ {
    move |e| CliError::Fatal(format!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------- probing

/// The work tree at one path.
enum Probe {
    Missing,
    Dir,
    /// Neither a file, a symlink nor a directory: never read.
    Other,
    File { git: GitOid, symlink: bool, exec: Option<bool> },
}

fn stat_matches(e: &IndexEntry, meta: &fs::Metadata, index_mtime: Option<SystemTime>) -> bool {
    let Ok(modified) = meta.modified() else { return false };
    if e.size != meta.len() as u32 || e.mtime != StatTime::from_system_time(modified) {
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

/// Looks at `path` in the work tree, hashing the content only when the stat cache `cached`
/// does not vouch for it. `eol` is asked only when a hash is needed.
fn probe(
    root: &Path,
    path: &RepoPath,
    cached: Option<&IndexEntry>,
    index_mtime: Option<SystemTime>,
    eol: impl FnOnce() -> Eol,
    filemode: bool,
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
    if !symlink && !meta.is_file() {
        return Ok(Probe::Other);
    }
    let exec = convert::exec_bit(&meta).filter(|_| filemode);
    if let Some(e) = cached.filter(|e| stat_matches(e, &meta, index_mtime)) {
        return Ok(Probe::File { git: e.oid, symlink, exec });
    }
    let git = if symlink {
        convert::bytes_oid(&convert::link_target(&abs)?)?
    } else {
        convert::blob_oid(&abs, eol())?
    };
    Ok(Probe::File { git, symlink, exec })
}

/// Whether the work tree differs from what is staged.
fn changed(leaf: &Leaf, git: GitOid, symlink: bool, exec: Option<bool>, core_symlinks: bool) -> bool {
    let want_link = leaf.kind == Kind::Symlink;
    // Without symlink support a link is a regular file holding its target.
    let as_file = want_link && !symlink && !core_symlinks;
    if want_link != symlink && !as_file {
        return true;
    }
    git != leaf.git || exec.is_some_and(|x| !want_link && x != (leaf.kind == Kind::Exec))
}

// ---------------------------------------------------------------- planning

/// Plans the staging of the paths `spec` selects.
pub async fn plan(
    ctx: &Ctx,
    target: &Target,
    spec: &Spec,
    opts: Opts,
    attrs: &mut Attrs,
) -> Result<Plan> {
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel();
    let index_path = ws.projection.git_dir().join("index");
    let index = IndexFile::read_or_empty(&index_path).unwrap_or_default();
    let cache: HashMap<&[u8], &IndexEntry> =
        index.entries.iter().map(|e| (e.path.as_bytes(), e)).collect();
    let filemode = ctx.config.bool("core.filemode").unwrap_or(cfg!(unix));
    let core_symlinks = ctx.config.bool("core.symlinks").unwrap_or(cfg!(unix));

    let mut plan = Plan::default();
    let mut seen = vec![false; spec.includes().len()];
    let mut tracked: HashSet<Vec<u8>> = HashSet::new();
    let mut lookup_error: Option<CliError> = None;

    target
        .tracked(ws, spec, |path, t: Tracked| {
            spec.mark(path.as_bytes(), &mut seen);
            tracked.insert(path.as_bytes().to_vec());
            if opts.intent {
                return Ok(());
            }
            let (leaf, ita) = match &t.staged {
                Staged::Leaf { leaf, ita } => (Some(leaf), *ita),
                Staged::Unmerged | Staged::Absent => (None, false),
            };
            if leaf.is_some_and(|l| l.kind == Kind::Gitlink) {
                return Ok(());
            }
            let cached = cache.get(path.as_bytes()).copied();
            if cached.is_some_and(|e| e.flags.skip_worktree) {
                return Ok(());
            }
            let eol = || attrs.lookup(path).map_or(Eol::Keep, |a| a.eol);
            let found = probe(root, path, cached, index.mtime, eol, filemode)
                .map_err(io_err(&fs_path(root, path)))?;
            let dirty = match (&found, leaf) {
                (Probe::Missing | Probe::Dir, _) => {
                    plan.ops.push(Op::Remove(path.clone()));
                    return Ok(());
                }
                (Probe::Other, _) => return Ok(()),
                (Probe::File { .. }, None) => true,
                (Probe::File { git, symlink, exec }, Some(l)) => {
                    ita || changed(l, *git, *symlink, *exec, core_symlinks)
                }
            };
            if dirty {
                match attrs.lookup(path) {
                    Ok(a) => {
                        plan.note_lfs(path, &a);
                        plan.ops.push(Op::Add {
                            path: path.clone(),
                            attrs: a,
                            ita: false,
                            prev: leaf.map(|l| l.kind),
                        });
                    }
                    Err(e) if lookup_error.is_none() => {
                        lookup_error = Some(CliError::Fatal(e.to_string()));
                    }
                    Err(_) => {}
                }
            }
            Ok(())
        })
        .await?;
    if let Some(e) = lookup_error {
        return Err(e);
    }

    if !opts.update {
        let sources = ignore_sources(ctx, ws);
        let mut walker = Walker {
            spec,
            opts,
            redirections: Redirections::load(root).map_err(io_err(root))?,
            ignore: IgnoreStack::new(root, &sources),
            tracked: &tracked,
            seen: &mut seen,
            plan: &mut plan,
            attrs,
        };
        walker.dir(None, root)?;
    }

    for (item, hit) in spec.includes().iter().zip(&seen) {
        let literal = item.literal();
        let exists = literal.is_some_and(|p| {
            p.is_empty() || fs::symlink_metadata(fs_path_raw(root, p)).is_ok()
        });
        if !hit && !exists {
            return Err(CliError::Fatal(format!(
                "pathspec '{}' did not match any files",
                item.raw()
            )));
        }
    }
    Ok(plan)
}

fn fs_path_raw(root: &Path, repo_path: &[u8]) -> PathBuf {
    root.join(String::from_utf8_lossy(repo_path).as_ref())
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

/// Walks the work tree for untracked paths the spec selects.
struct Walker<'a> {
    spec: &'a Spec,
    opts: Opts,
    redirections: Redirections,
    ignore: IgnoreStack,
    tracked: &'a HashSet<Vec<u8>>,
    seen: &'a mut [bool],
    plan: &'a mut Plan,
    attrs: &'a mut Attrs,
}

impl Walker<'_> {
    fn dir(&mut self, rel: Option<&RepoPath>, abs: &Path) -> Result<()> {
        if self.ignore.push_dir(abs).map_err(io_err(abs))? {
            let dir = rel.map(|r| String::from_utf8_lossy(r.as_bytes()).into_owned());
            self.plan.shadowed.push(dir.unwrap_or_default());
        }
        let done = self.entries(rel, abs);
        self.ignore.pop_dir();
        done
    }

    /// Whether `abs` is skipped as ignored. `-f` lifts the user's rules, never the implicit
    /// ones (`.git`, `.asc/info/`).
    fn ignored(&self, abs: &Path, is_dir: bool) -> bool {
        match self.ignore.explain(abs, is_dir) {
            Some(hit) if !hit.negated => !self.opts.force || hit.file.is_none(),
            _ => false,
        }
    }

    fn entries(&mut self, rel: Option<&RepoPath>, abs: &Path) -> Result<()> {
        let mut list = fs::read_dir(abs)
            .and_then(|d| d.collect::<io::Result<Vec<_>>>())
            .map_err(io_err(abs))?;
        list.sort_by_key(fs::DirEntry::file_name);
        for entry in list {
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
            let kind = entry.file_type().map_err(io_err(&child_abs))?;
            let key = child.as_bytes();
            if kind.is_dir() {
                if !self.spec.may_contain_dir(key) || self.tracked.contains(key) {
                    continue; // outside the spec, or a gitlink
                }
                if self.ignored(&child_abs, true) {
                    if self.spec.exact(key) {
                        self.named_ignored(child);
                    }
                    continue;
                }
                if child_abs.join(".git").exists() {
                    continue; // a nested workspace is never descended
                }
                self.dir(Some(&child), &child_abs)?;
                continue;
            }
            if !self.spec.matches(key) || self.tracked.contains(key) {
                continue;
            }
            if self.ignored(&child_abs, false) {
                if self.spec.exact(key) {
                    self.named_ignored(child);
                }
                continue;
            }
            self.spec.mark(key, self.seen);
            if !kind.is_file() && !kind.is_symlink() {
                continue;
            }
            let attrs = self.attrs.lookup(&child).map_err(|e| CliError::Fatal(e.to_string()))?;
            self.plan.note_lfs(&child, &attrs);
            self.plan.ops.push(Op::Add { path: child, attrs, ita: self.opts.intent, prev: None });
        }
        Ok(())
    }

    fn named_ignored(&mut self, path: RepoPath) {
        self.spec.mark(path.as_bytes(), self.seen);
        self.plan.ignored.push(path);
    }
}
