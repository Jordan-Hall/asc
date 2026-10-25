//! `asc restore [-s <tree>] [-S] [-W] [-q] [--] <pathspec>…` (SPEC §4.4). It also holds the
//! helpers `reset` shares: revision lookup, the effective index tree, pathspecs, staging rows
//! from a tree, and the forced working-tree sync.
//!
//! The working tree is written through `asc_wc::plain::checkout` in `Force` mode between two
//! small synthetic trees that differ exactly at the paths to rewrite, so only those paths are
//! touched and the write path (temp file, rename, symlink and case checks) is the one `switch`
//! uses.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_core::git::BlobOidHasher;
use asc_core::id::{ChangesetId, FileId, GitOid, TreeId};
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::RepoPath;
use asc_engine::history::HistoryApi;
use asc_engine::tree::Pathspec;
use asc_state::state::stage::{self, Leaf, StageRow, StageSet};
use asc_store::cachedir::CacheDir;
use asc_store::refs::Actor;
use asc_store::sqlrefs::SqlRefs;
use asc_store::storeset::StoreSet;
use asc_wc::SafeRoot;
use asc_wc::ignore::IgnoreSources;
use asc_wc::plain::{self, Mode, Sparse, Wc};
use asc_wc::redirect::Redirections;
use asc_wc::status::NoEol;
use futures::TryStreamExt;

use super::common::{CliEngine, CliError, Ctx, ObjectStack, Result, WsCtx};
use crate::dispatch::Program;
use crate::ext;
use crate::native::NativeSpec;
use crate::refs_guard::ReadOnlyRefs;

#[derive(clap::Parser, Debug)]
#[command(
    name = "restore",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// Restore from this tree-ish instead of the index (or HEAD with `--staged`).
    #[arg(short = 's', long = "source")]
    pub source: Option<String>,
    /// Restore the index.
    #[arg(short = 'S', long = "staged")]
    pub staged: bool,
    /// Restore the working tree (the default when `--staged` is absent).
    #[arg(short = 'W', long = "worktree")]
    pub worktree: bool,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// The paths to restore.
    pub paths: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "restore",
    flags: &[
        "-s",
        "--source",
        "-S",
        "--staged",
        "-W",
        "--worktree",
        "-q",
        "--quiet",
    ],
    placeholders: &[],
}];

/// The program name git-style messages use.
pub(crate) fn tool(ctx: &Ctx) -> &'static str {
    if ctx.prog == Program::Git {
        "git"
    } else {
        "asc"
    }
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// The root tree of `commit`, or the empty tree for an unborn HEAD.
pub(crate) async fn root_of(ws: &WsCtx, commit: Option<ChangesetId>) -> Result<TreeId> {
    Ok(match commit {
        Some(c) => ws.engine.get_changeset(c).await?.root,
        None => CliEngine::empty_tree_id(),
    })
}

/// `spec` as a commit: `HEAD` and `@` name the workspace's HEAD (the engine's refs do not hold
/// it), anything else goes through the engine's revision resolver. `None` when it does not
/// resolve (an unborn HEAD included).
pub(crate) async fn resolve_commit(ctx: &Ctx, spec: &str) -> Result<Option<ChangesetId>> {
    let ws = ctx.ws()?;
    let head_len = if spec.starts_with("HEAD") {
        4
    } else {
        usize::from(spec.starts_with('@'))
    };
    let rewritten = match (head_len, spec.as_bytes().get(head_len)) {
        (0, _) => spec.to_owned(),
        (_, None | Some(b'~' | b'^')) => match ws.head_commit().await? {
            Some(h) => format!("{h}{}", &spec[head_len..]),
            None => return Ok(None),
        },
        _ => spec.to_owned(),
    };
    let found = ws.engine.resolve(&[rewritten]).await;
    match found {
        Ok(mut v) => Ok(v.pop().flatten()),
        Err(asc_engine::graph::Error::Engine(_)) | Err(asc_engine::graph::Error::Missing(_)) => {
            Ok(None)
        }
        // `master~1` on a root commit names nothing; git reports it as an unknown revision.
        Err(asc_engine::graph::Error::Remote(m)) if m.contains(" has no parent number ") => Ok(None),
        Err(e) => Err(fatal(e)),
    }
}

fn leaf_of(e: &Entry) -> Leaf {
    Leaf {
        kind: e.kind,
        file: FileId::from_bytes(e.id),
        size: e.size,
        git: e.git,
    }
}

fn leaf_of_change(change: &Change) -> Option<Leaf> {
    match change {
        Change::Write {
            kind,
            file,
            size,
            git,
            ..
        } => Some(Leaf {
            kind: *kind,
            file: *file,
            size: *size,
            git: *git,
        }),
        Change::Gitlink { commit } => Some(Leaf {
            kind: Kind::Gitlink,
            file: FileId::from_bytes([0; 32]),
            size: 0,
            git: *commit,
        }),
        Change::Delete => None,
    }
}

/// The tree the index holds: HEAD's tree with the stage-0 rows applied. Also returns the paths
/// with conflict stages (they are not part of the tree).
pub(crate) async fn index_tree(
    ws: &WsCtx,
    head_root: TreeId,
) -> Result<(TreeId, BTreeSet<Vec<u8>>)> {
    let rows = stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await?;
    let mut changes = Vec::new();
    let mut unmerged = BTreeSet::new();
    for r in rows {
        if r.stage > 0 {
            unmerged.insert(r.path);
            continue;
        }
        let path = RepoPath::new(r.path).map_err(fatal)?;
        let change = match (r.file, r.git) {
            (Some(file), Some(git)) if r.kind == Kind::Gitlink => {
                let _ = file;
                Change::Gitlink { commit: git }
            }
            (Some(file), Some(git)) => {
                Change::write(r.kind, file, r.size, git, None).map_err(fatal)?
            }
            _ => Change::Delete,
        };
        changes.push((path, change));
    }
    Ok((ws.engine.apply(head_root, &changes).await?, unmerged))
}

/// The user's pathspec arguments, resolved against the current directory.
pub(crate) struct Spec {
    originals: Vec<String>,
    resolved: Vec<Vec<u8>>,
    pub(crate) spec: Pathspec,
}

impl Spec {
    /// Parses `items` (relative to the current directory) into one pathspec. An item that names
    /// the repository root selects everything.
    pub(crate) fn new(ctx: &Ctx, items: &[OsString]) -> Result<Self> {
        let ws = ctx.ws()?;
        let prefix = cwd_prefix(ws.projection.toplevel(), &ctx.cwd);
        let literal = ctx.env.pathspec.literal;
        let mut originals = Vec::new();
        let mut resolved = Vec::new();
        let mut everything = false;
        for item in items {
            let text = item.to_string_lossy().into_owned();
            let (magic, rest, top) = if literal {
                (String::new(), text.as_str(), false)
            } else {
                split_magic(&text)
            };
            let joined = join_prefix(if top { "" } else { &prefix }, rest).ok_or_else(|| {
                CliError::Fatal(format!(
                    "{text}: '{text}' is outside repository at '{}'",
                    ws.projection
                        .toplevel()
                        .display()
                        .to_string()
                        .replace('\\', "/")
                ))
            })?;
            let exclude =
                magic.contains("exclude") || magic.starts_with(":!") || magic.starts_with(":^");
            if joined.is_empty() && !exclude {
                everything = true;
                continue;
            }
            originals.push(text);
            resolved.push(format!("{magic}{joined}").into_bytes());
        }
        if everything {
            resolved.retain(|r| {
                r.starts_with(b":!")
                    || r.starts_with(b":^")
                    || r.windows(7).any(|w| w == b"exclude")
            });
            originals.clear();
        }
        let spec = Pathspec::parse(&resolved).map_err(fatal)?;
        Ok(Self {
            originals,
            resolved,
            spec,
        })
    }

    /// The first original item that selects nothing in any of `roots`.
    pub(crate) async fn first_unmatched(
        &self,
        ws: &WsCtx,
        roots: &[TreeId],
    ) -> Result<Option<String>> {
        for (orig, res) in self.originals.iter().zip(&self.resolved) {
            if res.starts_with(b":!") || res.starts_with(b":^") {
                continue;
            }
            let one = Pathspec::parse([res]).map_err(fatal)?;
            let mut hit = false;
            for root in roots {
                if ws
                    .engine
                    .walk(*root, Some(&one))
                    .try_next()
                    .await?
                    .is_some()
                {
                    hit = true;
                    break;
                }
            }
            if !hit {
                return Ok(Some(orig.clone()));
            }
        }
        Ok(None)
    }
}

/// The current directory below the work tree root, `/`-separated; empty at the root.
fn cwd_prefix(top: &Path, cwd: &Path) -> String {
    let norm = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let (t, c) = (norm(top), norm(cwd));
    let canon = |p: &Path| fs::canonicalize(p).ok().map(|c| norm(&c));
    let rel = |t: &str, c: &str| {
        let t = t.trim_end_matches('/');
        c.strip_prefix(t).map(|r| r.trim_matches('/').to_owned())
    };
    rel(&t, &c)
        .or_else(|| match (canon(top), canon(cwd)) {
            (Some(t), Some(c)) => rel(&t, &c),
            _ => None,
        })
        .unwrap_or_default()
}

/// Splits pathspec magic (`:(…)`, `:!`, `:^`, `:/`) from the path; the flag says "top-level".
fn split_magic(text: &str) -> (String, &str, bool) {
    if let Some(after) = text.strip_prefix(":(")
        && let Some(close) = after.find(')')
    {
        let magic = format!(":({})", &after[..close]);
        let top = after[..close].split(',').any(|w| w == "top");
        return (magic, &after[close + 1..], top);
    }
    for m in [":!", ":^"] {
        if let Some(rest) = text.strip_prefix(m) {
            return (m.to_owned(), rest, false);
        }
    }
    match text.strip_prefix(":/") {
        Some(rest) => (String::new(), rest, true),
        None => (String::new(), text, false),
    }
}

/// `prefix/item` with `.` and `..` folded; `None` when it climbs above the root.
fn join_prefix(prefix: &str, item: &str) -> Option<String> {
    let item = item.replace('\\', "/");
    let mut parts: Vec<&str> = prefix.split('/').filter(|s| !s.is_empty()).collect();
    for comp in item.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join("/"))
}

/// Moves stage set 0 at the paths `spec` selects to what tree `source` holds there: a row when
/// that differs from HEAD, no row when it equals HEAD. Conflicted paths are resolved to the
/// source's entry. Returns the changed paths, after telling the synthetic index.
pub(crate) async fn stage_from_tree(
    ws: &WsCtx,
    index: TreeId,
    unmerged: &BTreeSet<Vec<u8>>,
    source: TreeId,
    head_root: TreeId,
    spec: &Spec,
) -> Result<Vec<RepoPath>> {
    let mut desired: BTreeMap<RepoPath, Option<Leaf>> = BTreeMap::new();
    for (path, change) in ws.engine.leafdiff(index, source, Some(&spec.spec)).await? {
        desired.insert(path, leaf_of_change(&change));
    }
    for raw in unmerged {
        let path = RepoPath::new(raw.clone()).map_err(fatal)?;
        if spec.spec.matches(&path) {
            let leaf = ws.engine.lookup(source, &path).await?.map(|e| leaf_of(&e));
            desired.insert(path, leaf);
        }
    }
    let (id, mut rows, mut changed) = (ws.workspace.ws.id, Vec::new(), Vec::new());
    for (path, want) in desired {
        let head = ws
            .engine
            .lookup(head_root, &path)
            .await?
            .map(|e| leaf_of(&e));
        stage::remove(&ws.db, id, StageSet::Default, path.as_bytes()).await?;
        if head != want {
            rows.push(StageRow::from_leaf(path.as_bytes().to_vec(), want.as_ref()));
        }
        changed.push(path);
    }
    stage::upsert(&ws.db, id, StageSet::Default, &rows).await?;
    // The staged content comes from a tree, not from the files, so no stat may be trusted: the
    // sink patches every changed entry (previously staged paths and the new rows) stat-dirty.
    ext::index_sink()
        .on_stage_changed(id, &[])
        .await
        .map_err(fatal)?;
    Ok(changed)
}

/// The pieces a working-copy operation borrows.
pub(crate) struct WcParts {
    store: StoreSet,
    redirections: Redirections,
    ignore: IgnoreSources,
    sparse: Option<Sparse>,
    symlinks: bool,
    ignore_case: bool,
    root: PathBuf,
    index: PathBuf,
}

impl WcParts {
    /// The parts for the workspace, with `index` as the stat-cache file.
    pub(crate) fn new(ctx: &Ctx, ws: &WsCtx, index: PathBuf) -> Result<Self> {
        let root = ws.projection.toplevel().to_owned();
        let cache = ctx
            .config
            .path("asc.cache.dir")
            .unwrap_or_else(|| ws.home.cache());
        let store = StoreSet {
            store: CacheDir::open(ws.home.store(), ws.home.tmp()).map_err(fatal)?,
            cache: CacheDir::open(cache, ws.home.tmp()).map_err(fatal)?,
        };
        let asc_excludes = ctx
            .config
            .path("asc.excludesFile")
            .or_else(|| std::env::home_dir().map(|h| h.join(".config").join("asc").join("ignore")))
            .unwrap_or_else(|| root.join(".asc").join("no-global-ignore"));
        let ignore = IgnoreSources {
            asc_excludes,
            core_excludes: ctx.config.path("core.excludesFile"),
            local_exclude: root.join(".asc").join("info").join("exclude"),
            common_exclude: ws.projection.common_dir().join("info").join("exclude"),
        };
        Ok(Self {
            store,
            redirections: Redirections::load(&root).unwrap_or_default(),
            ignore,
            sparse: ws.workspace.ws.sparse.as_deref().map(Sparse::parse),
            symlinks: ctx.config.bool("core.symlinks").unwrap_or(!cfg!(windows)),
            ignore_case: ctx.ignore_case(),
            root,
            index,
        })
    }

    pub(crate) fn wc<'a>(
        &'a self,
        engine: &'a CliEngine,
    ) -> Wc<'a, ObjectStack, ReadOnlyRefs<SqlRefs>> {
        Wc {
            engine,
            store: &self.store,
            root: &self.root,
            index_path: &self.index,
            symlinks: self.symlinks,
            ignore_case: self.ignore_case,
            sparse: self.sparse.as_ref(),
            redirections: &self.redirections,
            ignore: &self.ignore,
            eol: &NoEol,
        }
    }
}

/// A stat-cache file that is removed when dropped.
struct ScratchIndex(PathBuf);

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Forces the working tree at `paths` to what `target` holds there (absent paths are deleted).
/// With `real_index` the synthetic `.git/index` is updated for those paths; otherwise a scratch
/// copy takes the stat-cache writes.
pub(crate) async fn sync_worktree(
    ctx: &Ctx,
    target: TreeId,
    paths: &[RepoPath],
    real_index: bool,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let ws = ctx.ws()?;
    let real = ws.projection.git_dir().join("index");
    let (index, _scratch) = if real_index {
        (real, None)
    } else {
        let scratch = ws
            .home
            .tmp()
            .join(format!("restore-index-{}", std::process::id()));
        if real.exists() {
            fs::copy(&real, &scratch)?;
        }
        (scratch.clone(), Some(ScratchIndex(scratch)))
    };
    let parts = WcParts::new(ctx, ws, index)?;
    let wc = parts.wc(&ws.engine);
    let sentinel = Change::write(
        Kind::File,
        FileId::from_bytes([0xA5; 32]),
        0,
        GitOid::from_bytes([0xA5; 20]),
        None,
    )
    .map_err(fatal)?;
    let (mut gone, mut put, mut real_changes) = (Vec::new(), Vec::new(), Vec::new());
    for path in paths {
        match ws.engine.lookup(target, path).await? {
            Some(e) if e.kind == Kind::Dir => {}
            Some(e) => {
                let change = match e.kind {
                    Kind::Gitlink => Change::Gitlink { commit: e.git },
                    kind => Change::write(kind, FileId::from_bytes(e.id), e.size, e.git, None)
                        .map_err(fatal)?,
                };
                put.push((path.clone(), sentinel.clone()));
                real_changes.push((path.clone(), change));
            }
            None => gone.push((path.clone(), sentinel.clone())),
        }
    }
    let empty = CliEngine::empty_tree_id();
    for (from, to) in [
        (ws.engine.apply(empty, &gone).await?, empty),
        (
            ws.engine.apply(empty, &put).await?,
            ws.engine.apply(empty, &real_changes).await?,
        ),
    ] {
        plain::checkout(&wc, from, to, Mode::Force)
            .await
            .map_err(fatal)?;
    }
    Ok(())
}

/// Whether the file at `path` differs from `entry` (size first, then the blob hash).
fn differs(gate: &SafeRoot, path: &RepoPath, entry: &Entry) -> bool {
    // Beyond a link the file is outside the work tree: it reads as missing, so it differs.
    let Some(abs) = gate.resolve(path) else {
        return true;
    };
    let Ok(meta) = fs::symlink_metadata(&abs) else {
        return true;
    };
    match entry.kind {
        Kind::Gitlink | Kind::Dir => false,
        Kind::Symlink => true,
        _ if !meta.is_file() || meta.len() != entry.size => true,
        _ => {
            let Ok(mut file) = fs::File::open(&abs) else {
                return true;
            };
            let mut hasher = BlobOidHasher::new(entry.size);
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        hasher.update(&buf[..n]);
                    }
                    Err(_) => return true,
                }
            }
            hasher.finalize().ok() != Some(entry.git)
        }
    }
}

/// The paths `spec` selects whose working file must change to match `target`: those the index
/// and `target` differ at, plus those present in `target` whose file differs.
pub(crate) async fn worktree_paths(
    ws: &WsCtx,
    index: TreeId,
    target: TreeId,
    spec: &Spec,
) -> Result<BTreeSet<RepoPath>> {
    let mut out: BTreeSet<RepoPath> = ws
        .engine
        .leafdiff(index, target, Some(&spec.spec))
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let gate = SafeRoot::new(ws.projection.toplevel());
    let mut walk = ws.engine.walk(target, Some(&spec.spec));
    while let Some((path, entry)) = walk.try_next().await? {
        if !out.contains(&path) && differs(&gate, &path, &entry) {
            out.insert(path);
        }
    }
    Ok(out)
}

/// The actor of a local ref update.
pub(crate) fn actor() -> Actor {
    let id = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "local".into());
    Actor {
        id,
        via: "cli".into(),
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if a.paths.is_empty() {
        return Err(CliError::Fatal(
            "you must specify path(s) to restore".into(),
        ));
    }
    let worktree = a.worktree || !a.staged;
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    let (index, unmerged) = index_tree(ws, head_root).await?;
    let source = match &a.source {
        Some(s) => {
            let cs = resolve_commit(ctx, s)
                .await?
                .ok_or_else(|| CliError::Fatal(format!("could not resolve {s}")))?;
            Some(root_of(ws, Some(cs)).await?)
        }
        None => None,
    };
    let stage_source = source.unwrap_or(head_root);
    let wt_source = source.unwrap_or(if a.staged { head_root } else { index });

    let spec = Spec::new(ctx, &a.paths)?;
    if let Some(bad) = spec
        .first_unmatched(ws, &[index, stage_source, wt_source])
        .await?
    {
        return Err(CliError::Raw(
            format!("error: pathspec '{bad}' did not match any file(s) known to git"),
            1,
        ));
    }
    let mut code = 0;
    if a.staged {
        stage_from_tree(ws, index, &unmerged, stage_source, head_root, &spec).await?;
    }
    if worktree {
        let mut paths = worktree_paths(ws, index, wt_source, &spec).await?;
        if !a.staged && source.is_none() {
            for raw in &unmerged {
                let path = RepoPath::new(raw.clone()).map_err(fatal)?;
                if spec.spec.matches(&path) {
                    eprintln!("error: path '{}' is unmerged", String::from_utf8_lossy(raw));
                    paths.remove(&path);
                    code = 1;
                }
            }
        }
        if !paths.is_empty() {
            ext::op_sink().snapshot_wc();
            let paths: Vec<RepoPath> = paths.into_iter().collect();
            sync_worktree(ctx, wt_source, &paths, false).await?;
        }
    }
    Ok(ExitCode::from(code))
}
