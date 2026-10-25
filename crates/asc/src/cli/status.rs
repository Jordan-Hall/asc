//! `status` (SPEC §9.5, §11.2): the long, short, porcelain v1/v2 and `--json` forms. The
//! staged/unstaged/untracked computation is `asc_wc::status::compute`; this file adds what git
//! prints around it: renames, ahead/behind, the in-progress banners, advice and the quoting and
//! relative-path rules of each format. The synthetic index's stat cache is refreshed only when
//! optional locks are allowed (`GIT_OPTIONAL_LOCKS`, `--no-optional-locks`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use asc_core::id::{ChangesetId, GitOid, RefTarget, TreeId};
use asc_core::object::{Entry, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_engine::diff::rename::{self, RenameOptions};
use asc_engine::diff::{Status as TreeStatus, TreeChange, mode as kind_mode};
use asc_engine::graph::{EngineSource, Graph, LogQuery};
use asc_engine::tree::Pathspec;
use asc_git::index::{IndexFile, PlainStat, Stage, StatSource};
use asc_git::projection::Head;
use asc_state::state::stage::{self, StageRow, StageSet};
use asc_store::cachedir::CacheDir;
use asc_store::refs::Refs;
use asc_store::storeset::StoreSet;
use asc_wc::SafeRoot;
use asc_wc::ignore::{IgnoreSources, IgnoreStack, shadow_warning};
use asc_wc::plain::{Sparse, Wc};
use asc_wc::redirect::Redirections;
use asc_wc::status::{ChangeKind, NoEol, Untracked, compute};
use prost_reflect::{DescriptorPool, DynamicMessage, Value};

use super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::json::JsonLines;
use super::rebase_todo::RebaseInfo;
use crate::msg::Msg;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "status", disable_help_flag = true, disable_version_flag = true)]
pub struct Cli {
    #[arg(short = 's', long)]
    pub short: bool,
    #[arg(short = 'b', long)]
    pub branch: bool,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "v1")]
    pub porcelain: Option<String>,
    #[arg(long)]
    pub long: bool,
    #[arg(short = 'z')]
    pub z: bool,
    #[arg(short = 'u', long = "untracked-files", num_args = 0..=1, require_equals = true,
        default_missing_value = "all")]
    pub untracked: Option<String>,
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "traditional")]
    pub ignored: Option<String>,
    #[arg(long = "ahead-behind")]
    pub ahead_behind: bool,
    #[arg(long = "no-ahead-behind")]
    pub no_ahead_behind: bool,
    #[arg(long)]
    pub renames: bool,
    #[arg(long = "no-renames")]
    pub no_renames: bool,
    /// asc only: `WsStatusResponse` as one JSON line.
    #[arg(long)]
    pub json: bool,
    /// Limits the report to these paths (relative to the current directory).
    #[arg(num_args = 0..)]
    pub paths: Vec<OsString>,
}

/// The parsed command line. git takes `-u<mode>` with the mode attached to the flag, which clap
/// cannot express for an optional value, so `try_parse_from` rewrites it first.
#[derive(Debug)]
pub struct Args(pub Cli);

impl clap::CommandFactory for Args {
    fn command() -> clap::Command {
        Cli::command()
    }

    fn command_for_update() -> clap::Command {
        Cli::command_for_update()
    }
}

impl clap::FromArgMatches for Args {
    fn from_arg_matches(m: &clap::ArgMatches) -> Result<Self, clap::Error> {
        Cli::from_arg_matches(m).map(Self)
    }

    fn update_from_arg_matches(&mut self, m: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(m)
    }
}

impl clap::Parser for Args {
    fn try_parse_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let argv: Vec<OsString> = itr.into_iter().map(Into::into).collect();
        Cli::try_parse_from(expand_untracked(argv)).map(Self)
    }
}

/// Rewrites `-u`, `-uno` and clusters such as `-suall` to `--untracked-files=<mode>`.
fn expand_untracked(argv: Vec<OsString>) -> Vec<OsString> {
    let mut out = Vec::with_capacity(argv.len());
    let mut ended = false;
    for (i, arg) in argv.into_iter().enumerate() {
        let text = arg.to_str().map(str::to_owned);
        let cluster = match &text {
            Some(t) if i > 0 && !ended && t.starts_with('-') && !t.starts_with("--") => {
                Some(t.clone())
            }
            _ => None,
        };
        if text.as_deref() == Some("--") {
            ended = true;
        }
        match cluster.and_then(|c| c[1..].find('u').map(|at| (c.clone(), at + 1))) {
            Some((c, at)) => {
                if at > 1 {
                    out.push(OsString::from(&c[..at]));
                }
                let mode = &c[at + 1..];
                let mode = if mode.is_empty() { "all" } else { mode };
                out.push(OsString::from(format!("--untracked-files={mode}")));
            }
            None => out.push(arg),
        }
    }
    out
}

/// The letters of `-u<mode>` clusters (`-uno`, `-unormal`, `-uall`) are served with `-u`.
pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "status",
    flags: &[
        "-s",
        "--short",
        "-b",
        "--branch",
        "--porcelain",
        "--long",
        "-z",
        "-u",
        "--untracked-files",
        "--ignored",
        "--ahead-behind",
        "--no-ahead-behind",
        "--renames",
        "--no-renames",
        "--json",
        "-n",
        "-o",
        "-r",
        "-m",
        "-a",
        "-l",
    ],
    placeholders: &[],
}];

// ------------------------------------------------------------------ options

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Long,
    Short,
    V1,
    V2,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UntrackedMode {
    No,
    Normal,
    All,
}

struct Opts {
    format: Format,
    branch: bool,
    z: bool,
    untracked: UntrackedMode,
    ignored: bool,
    ahead_behind: bool,
    renames: bool,
    hints: bool,
    u_hint: bool,
    relative: bool,
    quote_high: bool,
    optional_locks: bool,
    /// The text goes into the commit message template (git's `commit_template`).
    template: bool,
    /// The program name hints print (`git` or `asc`).
    prog: &'static str,
    /// The current directory below the work tree root, `""` or `a/b/`.
    prefix: Vec<u8>,
    spec: Option<Pathspec>,
}

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

fn untracked_mode(text: &str) -> Result<UntrackedMode> {
    match text {
        "no" => Ok(UntrackedMode::No),
        "normal" => Ok(UntrackedMode::Normal),
        "all" => Ok(UntrackedMode::All),
        other => Err(fatal(format!("Invalid untracked files mode '{other}'"))),
    }
}

/// The directory of `cwd` below `root` as a `/`-terminated byte string.
fn cwd_prefix(root: &Path, cwd: &Path) -> Vec<u8> {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_owned()
    };
    let (r, c) = (norm(root), norm(cwd));
    let (r_cmp, c_cmp) = if cfg!(windows) {
        (r.to_ascii_lowercase(), c.to_ascii_lowercase())
    } else {
        (r.clone(), c.clone())
    };
    match c_cmp.strip_prefix(&r_cmp) {
        Some("") | None => Vec::new(),
        Some(rest) if rest.starts_with('/') => {
            let mut v = c.as_bytes()[r.len() + 1..].to_vec();
            v.push(b'/');
            v
        }
        Some(_) => Vec::new(),
    }
}

/// Pathspec items relative to the current directory, made relative to the work tree root.
fn pathspec(prefix: &[u8], items: &[OsString]) -> Result<Option<Pathspec>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for item in items {
        let text = item.to_string_lossy().replace('\\', "/");
        if text.starts_with(':') {
            out.push(text.into_bytes());
            continue;
        }
        let mut parts: Vec<&[u8]> = prefix
            .split(|&b| b == b'/')
            .filter(|p| !p.is_empty())
            .collect();
        for comp in text.split('/') {
            match comp {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return Err(fatal(format!("{text}: '{text}' is outside repository")));
                    }
                }
                c => parts.push(c.as_bytes()),
            }
        }
        if !parts.is_empty() {
            out.push(parts.join(&b'/'));
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    Pathspec::parse(out)
        .map(Some)
        .map_err(|e| fatal(e.to_string()))
}

fn options(ctx: &Ctx, ws: &WsCtx, a: &Cli) -> Result<Opts> {
    let cfg = &ctx.config;
    let version = |v: &str| match v {
        "v1" | "1" => Ok(Format::V1),
        "v2" | "2" => Ok(Format::V2),
        other => Err(fatal(format!("unsupported porcelain version '{other}'"))),
    };
    let porcelain = a.porcelain.as_deref().map(version).transpose()?;
    let configured_short = cfg.bool("status.short").unwrap_or(false);
    let mut format = if a.json {
        Format::Json
    } else if let Some(p) = porcelain {
        p
    } else if a.long {
        Format::Long
    } else if a.short || (configured_short && !a.long) {
        Format::Short
    } else {
        Format::Long
    };
    if a.z && format == Format::Long {
        format = Format::V1; // git: -z without a format means porcelain v1
    }
    let machine = matches!(format, Format::V1 | Format::V2 | Format::Json);
    let untracked = match a.untracked.as_deref() {
        Some(m) => untracked_mode(m)?,
        None => match cfg.get("status.showUntrackedFiles") {
            Some(m) => untracked_mode(&m)?,
            None => UntrackedMode::Normal,
        },
    };
    let ignored = match a.ignored.as_deref() {
        None | Some("no") => false,
        Some("traditional") => true,
        Some("matching") => {
            return Err(CliError::Msg(Msg::AscOptionUnsupported {
                flag: "--ignored=matching".into(),
            }));
        }
        Some(other) => return Err(fatal(format!("Invalid ignored mode '{other}'"))),
    };
    if a.json && ignored {
        return Err(CliError::Usage(
            "error: --json does not take --ignored".into(),
        ));
    }
    let renames = if a.no_renames {
        false
    } else if a.renames {
        true
    } else {
        let value = cfg
            .get("status.renames")
            .or_else(|| cfg.get("diff.renames"));
        value.is_none_or(|v| {
            !matches!(
                v.to_ascii_lowercase().as_str(),
                "false" | "no" | "off" | "0"
            )
        })
    };
    let prefix = cwd_prefix(ws.projection.toplevel(), &ctx.cwd);
    let spec = pathspec(&prefix, &a.paths)?;
    Ok(Opts {
        format,
        branch: a.branch || (cfg.bool("status.branch").unwrap_or(false) && !machine),
        z: a.z,
        untracked,
        ignored,
        ahead_behind: !a.no_ahead_behind
            && (a.ahead_behind || cfg.get("status.aheadBehind").is_none_or(|v| v != "false")),
        renames,
        hints: cfg.bool("advice.statusHints").unwrap_or(true),
        u_hint: cfg.bool("advice.statusUoption").unwrap_or(true),
        relative: !matches!(format, Format::V1 | Format::Json)
            && cfg.bool("status.relativePaths").unwrap_or(true),
        quote_high: cfg.bool("core.quotePath").unwrap_or(true),
        optional_locks: ctx.env.optional_locks,
        template: false,
        prog: ctx.prog.name(),
        prefix,
        spec,
    })
}

// -------------------------------------------------------------------- model

/// One side of a path: its git mode and blob id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Side {
    mode: u32,
    oid: GitOid,
}

fn zero() -> GitOid {
    GitOid::from_bytes([0; 20])
}

fn mode_of(side: Option<Side>) -> u32 {
    side.map_or(0, |s| s.mode)
}

fn oid_of(side: Option<Side>) -> GitOid {
    side.map_or_else(zero, |s| s.oid)
}

fn side_of(entry: &Entry) -> Side {
    Side {
        mode: kind_mode(entry.kind),
        oid: entry.git,
    }
}

/// One reported path: an ordinary change, a rename or an unmerged path.
struct Row {
    path: Vec<u8>,
    orig: Option<(Vec<u8>, u8)>,
    x: u8,
    y: u8,
    head: Option<Side>,
    index: Option<Side>,
    work: u32,
    /// Stages 1 to 3 of an unmerged path.
    stages: Option<[Option<Side>; 3]>,
}

impl Row {
    fn new(path: Vec<u8>) -> Self {
        Self {
            path,
            orig: None,
            x: b'.',
            y: b'.',
            head: None,
            index: None,
            work: 0,
            stages: None,
        }
    }
}

/// How HEAD is attached.
enum HeadKind {
    Branch(String),
    Detached,
}

/// The state of the branch against its upstream.
enum Track {
    Gone,
    Counts {
        ahead: usize,
        behind: usize,
    },
    /// `--no-ahead-behind` and the tips differ.
    Different,
}

struct Upstream {
    name: String,
    track: Track,
}

struct Branch {
    head: HeadKind,
    oid: Option<GitOid>,
    upstream: Option<Upstream>,
}

/// An operation in progress (git's `wt_status_state`).
enum State {
    Merge,
    CherryPick(String),
    Revert(String),
    Rebase {
        branch: Option<String>,
        onto: Option<String>,
        info: Option<Box<RebaseInfo>>,
    },
    Bisect(Option<String>),
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::CherryPick(_) => "cherry-pick",
            Self::Revert(_) => "revert",
            Self::Rebase { .. } => "rebase",
            Self::Bisect(_) => "bisect",
        }
    }
}

struct Report {
    branch: Branch,
    rows: Vec<Row>,
    untracked: Vec<Vec<u8>>,
    ignored: Vec<Vec<u8>>,
    state: Option<State>,
    initial: bool,
}

// ----------------------------------------------------------------- gathering

fn ignore_sources(ctx: &Ctx, ws: &WsCtx) -> IgnoreSources {
    let home = std::env::home_dir();
    let xdg = |sub: &str| {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        base.map(|b| b.join(sub))
    };
    let root = ws.projection.toplevel();
    IgnoreSources {
        asc_excludes: ctx
            .config
            .path("asc.excludesFile")
            .or_else(|| xdg("asc/ignore"))
            .unwrap_or_else(|| root.join(".asc/no-such-global-ignore")),
        core_excludes: ctx
            .config
            .path("core.excludesFile")
            .or_else(|| xdg("git/ignore")),
        local_exclude: root.join(".asc/info/exclude"),
        common_exclude: ws.projection.common_dir().join("info/exclude"),
    }
}

fn wc_err(e: impl std::fmt::Display) -> CliError {
    fatal(e.to_string())
}

fn repo_path(bytes: &[u8]) -> Result<RepoPath> {
    RepoPath::new(bytes.to_vec()).map_err(wc_err)
}

fn selected(spec: &Option<Pathspec>, path: &[u8], dir: bool) -> bool {
    let Some(spec) = spec else { return true };
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    let Ok(p) = RepoPath::new(trimmed.to_vec()) else {
        return false;
    };
    spec.matches(&p) || (dir && spec.may_contain_dir(&p))
}

/// The mode of the work tree entry at `path`, 0 when absent.
fn work_mode(gate: &SafeRoot, path: &RepoPath, index: Option<Side>, symlinks: bool) -> u32 {
    // Behind a symlinked or reparse-point ancestor the path is outside the work tree: absent.
    let Some(abs) = gate.resolve(path) else {
        return 0;
    };
    let Ok(meta) = std::fs::symlink_metadata(&abs) else {
        return 0;
    };
    if meta.is_dir() {
        return index.filter(|s| s.mode == 0o160000).map_or(0, |s| s.mode);
    }
    if meta.file_type().is_symlink() {
        return 0o120000;
    }
    let index_mode = mode_of(index);
    if index_mode == 0o120000 && !symlinks {
        return 0o120000;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        }
    }
    #[cfg(not(unix))]
    {
        if index_mode == 0o100755 {
            0o100755
        } else {
            0o100644
        }
    }
}

struct Gather<'a> {
    ctx: &'a Ctx,
    ws: &'a WsCtx,
    o: &'a Opts,
    head_root: TreeId,
    rows: HashMap<Vec<u8>, Vec<StageRow>>,
    symlinks: bool,
}

impl Gather<'_> {
    async fn head_entry(&self, path: &RepoPath) -> Result<Option<Entry>> {
        Ok(self.ws.engine.lookup(self.head_root, path).await?)
    }

    /// The staged state of `path`: its row, else HEAD's entry.
    async fn index_side(&self, path: &RepoPath) -> Result<Option<Side>> {
        let staged = self
            .rows
            .get(path.as_bytes())
            .and_then(|r| r.iter().find(|r| r.stage == 0));
        match staged {
            Some(r) => Ok(r.git.filter(|_| r.file.is_some()).map(|git| Side {
                mode: kind_mode(r.kind),
                oid: git,
            })),
            None => Ok(self.head_entry(path).await?.as_ref().map(side_of)),
        }
    }
}

fn change_letter(kind: ChangeKind) -> u8 {
    kind.letter() as u8
}

fn tree_status(kind: ChangeKind) -> TreeStatus {
    match kind {
        ChangeKind::Added => TreeStatus::Added,
        ChangeKind::Modified => TreeStatus::Modified,
        ChangeKind::Deleted => TreeStatus::Deleted,
        ChangeKind::TypeChanged => TreeStatus::TypeChanged,
    }
}

fn row_entry(path: &RepoPath, r: &StageRow) -> Option<Entry> {
    Some(Entry {
        name: path.file_name(),
        kind: r.kind,
        id: *r.file?.as_bytes(),
        size: r.size,
        git: r.git?,
    })
}

/// The mask of conflict stages of an unmerged path to git's two-letter code.
fn unmerged_xy(stages: &[Option<Side>; 3]) -> [u8; 2] {
    match (
        stages[0].is_some(),
        stages[1].is_some(),
        stages[2].is_some(),
    ) {
        (true, true, true) => *b"UU",
        (false, true, true) => *b"AA",
        (true, true, false) => *b"UD",
        (true, false, true) => *b"DU",
        (false, true, false) => *b"AU",
        (false, false, true) => *b"UA",
        (true, false, false) | (false, false, false) => *b"DD",
    }
}

async fn gather(ctx: &Ctx, ws: &WsCtx, o: &Opts) -> Result<Report> {
    let root = ws.projection.toplevel().to_owned();
    let gate = SafeRoot::new(&root);
    let index_path = ws.projection.git_dir().join("index");
    let redirections = Redirections::load(&root).map_err(wc_err)?;
    let sparse = ws.workspace.ws.sparse.as_deref().map(Sparse::parse);
    let sources = ignore_sources(ctx, ws);
    let store = StoreSet {
        store: CacheDir::open(ws.home.store(), ws.home.tmp())?,
        cache: CacheDir::open(ws.home.cache(), ws.home.tmp())?,
    };
    let symlinks = ctx.config.bool("core.symlinks").unwrap_or(!cfg!(windows));
    let wc = Wc {
        engine: &ws.engine,
        store: &store,
        root: &root,
        index_path: &index_path,
        symlinks,
        sparse: sparse.as_ref(),
        redirections: &redirections,
        ignore: &sources,
        eol: &NoEol,
    };
    let status = compute(&wc, &ws.db, &ws.workspace).await.map_err(wc_err)?;

    let base = ws.workspace.ws.base_cs;
    let head_root = match base {
        Some(cs) => ws.engine.get_changeset(cs).await?.root,
        None => CliEngine::empty_tree_id(),
    };
    let mut rows: HashMap<Vec<u8>, Vec<StageRow>> = HashMap::new();
    for r in stage::overlay(&ws.db, ws.workspace.ws.id, StageSet::Default).await? {
        rows.entry(r.path.clone()).or_default().push(r);
    }
    let g = Gather {
        ctx,
        ws,
        o,
        head_root,
        rows,
        symlinks,
    };

    let mut out: BTreeMap<Vec<u8>, Row> = BTreeMap::new();

    // Staged: HEAD against the stage rows, with rename detection.
    let mut changes = Vec::new();
    for c in &status.staged {
        if !selected(&o.spec, c.path.as_bytes(), false) {
            continue;
        }
        let old = g.head_entry(&c.path).await?;
        let new = g
            .rows
            .get(c.path.as_bytes())
            .and_then(|r| r.iter().find(|r| r.stage == 0))
            .and_then(|r| row_entry(&c.path, r));
        changes.push(TreeChange {
            path: c.path.clone(),
            old_path: c.path.clone(),
            old,
            new,
            status: tree_status(c.kind),
        });
    }
    if o.renames {
        changes = rename::detect(changes, &ws.engine, &RenameOptions::default())
            .await
            .map_err(wc_err)?;
    }
    for c in changes {
        let mut row = Row::new(c.path.as_bytes().to_vec());
        row.x = match c.status {
            TreeStatus::Renamed(score) => {
                row.orig = Some((c.old_path.as_bytes().to_vec(), score));
                b'R'
            }
            s => s.letter() as u8,
        };
        row.head = c.old.as_ref().map(side_of);
        row.index = c.new.as_ref().map(side_of);
        out.insert(row.path.clone(), row);
    }

    // Unstaged: the stage state against the work tree.
    for c in &status.unstaged {
        if !selected(&o.spec, c.path.as_bytes(), false) {
            continue;
        }
        let key = c.path.as_bytes().to_vec();
        let row = match out.get_mut(&key) {
            Some(r) => r,
            None => {
                let mut r = Row::new(key.clone());
                r.head = g.head_entry(&c.path).await?.as_ref().map(side_of);
                r.index = g.index_side(&c.path).await?;
                out.entry(key).or_insert(r)
            }
        };
        row.y = change_letter(c.kind);
        row.work = work_mode(&gate, &c.path, row.index, symlinks);
    }

    // Unmerged paths.
    for path in &status.unmerged {
        if !selected(&o.spec, path.as_bytes(), false) {
            continue;
        }
        let mut stages = [None; 3];
        for r in g.rows.get(path.as_bytes()).into_iter().flatten() {
            if (1..=3).contains(&r.stage) && r.file.is_some() {
                stages[r.stage as usize - 1] = r.git.map(|git| Side {
                    mode: kind_mode(r.kind),
                    oid: git,
                });
            }
        }
        let [x, y] = unmerged_xy(&stages);
        let mut row = Row::new(path.as_bytes().to_vec());
        (row.x, row.y) = (x, y);
        row.work = work_mode(&gate, path, stages[1].or(stages[0]), symlinks);
        row.stages = Some(stages);
        out.insert(row.path.clone(), row);
    }

    // Changed rows still need their work tree mode for the staged-only ones in v2.
    for row in out.values_mut() {
        if row.stages.is_none() && row.y == b'.' {
            row.work = row.index.map_or(0, |s| s.mode);
        }
    }

    // Untracked and ignored paths.
    let mut untracked = Vec::new();
    if o.untracked != UntrackedMode::No {
        for u in &status.untracked {
            let mut p = u.path.as_bytes().to_vec();
            if u.dir {
                p.push(b'/');
            }
            if !selected(&o.spec, &p, u.dir) {
                continue;
            }
            if u.dir && o.untracked == UntrackedMode::All {
                expand_dir(&root, &sources, u, &mut untracked).map_err(wc_err)?;
            } else {
                untracked.push(p);
            }
        }
        untracked.retain(|p| selected(&o.spec, p, p.ends_with(b"/")));
    }
    let mut ignored = Vec::new();
    if o.ignored {
        let tracked = tracked_sets(&g, &index_path)?;
        let mut walk = IgnoredWalk {
            root: &root,
            redirections: &redirections,
            stack: IgnoreStack::new(&root, &sources),
            tracked_files: &tracked.files,
            tracked_dirs: &tracked.dirs,
            out: Vec::new(),
        };
        walk.dir(None, &root).map_err(wc_err)?;
        ignored = walk.out;
        ignored.retain(|p| selected(&o.spec, p, p.ends_with(b"/")));
        ignored.sort();
    }

    for dir in &status.shadowed {
        eprintln!("{}", shadow_warning(dir));
    }

    if o.optional_locks {
        let skip: HashSet<Vec<u8>> = status
            .unstaged
            .iter()
            .map(|c| c.path.as_bytes().to_vec())
            .chain(status.unmerged.iter().map(|p| p.as_bytes().to_vec()))
            .chain(g.rows.keys().cloned())
            .collect();
        // A stat cache that cannot be refreshed is not an error (git ignores it too).
        let _ = refresh_stat_cache(&index_path, &root, &skip);
    }

    let branch = branch_info(&g).await?;
    let state = in_progress(ws.projection.git_dir());
    Ok(Report {
        initial: base.is_none() && branch.oid.is_none(),
        branch,
        rows: out.into_values().collect(),
        untracked,
        ignored,
        state,
    })
}

/// Paths of the tracked files and the directories above them.
struct Tracked {
    files: HashSet<Vec<u8>>,
    dirs: HashSet<Vec<u8>>,
}

/// The synthetic index plus the stage rows.
fn tracked_sets(g: &Gather<'_>, index_path: &Path) -> Result<Tracked> {
    let index = IndexFile::read_or_empty(index_path).map_err(wc_err)?;
    let mut files: HashSet<Vec<u8>> = index
        .entries
        .iter()
        .map(|e| e.path.as_bytes().to_vec())
        .collect();
    files.extend(g.rows.keys().cloned());
    let mut dirs = HashSet::new();
    for f in &files {
        let mut cur = f.as_slice();
        while let Some(at) = cur.iter().rposition(|&b| b == b'/') {
            cur = &cur[..at];
            if !dirs.insert(cur.to_vec()) {
                break;
            }
        }
    }
    Ok(Tracked { files, dirs })
}

fn join_path(rel: Option<&RepoPath>, name: &[u8]) -> Vec<u8> {
    match rel {
        Some(r) => [r.as_bytes(), b"/", name].concat(),
        None => name.to_vec(),
    }
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

/// `-uall`: the files below an untracked directory (a nested workspace stays one entry).
pub(crate) fn expand_dir(
    root: &Path,
    sources: &IgnoreSources,
    u: &Untracked,
    out: &mut Vec<Vec<u8>>,
) -> std::io::Result<()> {
    let mut stack = IgnoreStack::new(root, sources);
    stack.push_dir(root)?;
    let mut abs = root.to_owned();
    let mut pushed = 1;
    for comp in u.path.components() {
        abs.push(String::from_utf8_lossy(comp).as_ref());
        if abs != asc_wc::status::untracked_abs(root, u) {
            stack.push_dir(&abs)?;
            pushed += 1;
        }
    }
    let target = asc_wc::status::untracked_abs(root, u);
    fn walk(
        stack: &mut IgnoreStack,
        rel: &RepoPath,
        abs: &Path,
        out: &mut Vec<Vec<u8>>,
    ) -> std::io::Result<()> {
        stack.push_dir(abs)?;
        // The walk lists real directories only: `file_type` does not follow a link.
        let mut names: Vec<_> = std::fs::read_dir(abs)? // lint:allow-worktree-fs
            .collect::<std::io::Result<_>>()?;
        names.sort_by_key(|e| name_bytes(&e.file_name()));
        for entry in names {
            let name = name_bytes(&entry.file_name());
            if name == b".git" {
                continue;
            }
            let child_abs = entry.path();
            let is_dir = entry.file_type()?.is_dir();
            if stack.is_ignored(&child_abs, is_dir) {
                continue;
            }
            let Ok(child) = RepoPath::new(join_path(Some(rel), &name)) else {
                continue;
            };
            if !is_dir {
                out.push(child.as_bytes().to_vec());
            } else if child_abs.join(".git").exists() {
                out.push([child.as_bytes(), b"/"].concat());
            } else {
                walk(stack, &child, &child_abs, out)?;
            }
        }
        stack.pop_dir();
        Ok(())
    }
    walk(&mut stack, &u.path, &target, out)?;
    for _ in 0..pushed {
        stack.pop_dir();
    }
    Ok(())
}

/// `--ignored`: ignored paths that are not tracked; an ignored directory is one `dir/` entry
/// unless it holds tracked files.
struct IgnoredWalk<'a> {
    root: &'a Path,
    redirections: &'a Redirections,
    stack: IgnoreStack,
    tracked_files: &'a HashSet<Vec<u8>>,
    tracked_dirs: &'a HashSet<Vec<u8>>,
    out: Vec<Vec<u8>>,
}

impl IgnoredWalk<'_> {
    fn dir(&mut self, rel: Option<&RepoPath>, abs: &Path) -> std::io::Result<()> {
        self.stack.push_dir(abs)?;
        let result = self.entries(rel, abs);
        self.stack.pop_dir();
        result
    }

    fn entries(&mut self, rel: Option<&RepoPath>, abs: &Path) -> std::io::Result<()> {
        // Real directories only, as above: `file_type` does not follow a link.
        let listing = std::fs::read_dir(abs)?; // lint:allow-worktree-fs
        for entry in listing {
            let entry = entry?;
            let name = name_bytes(&entry.file_name());
            if name == b".git" || self.redirections.is_redirected_name(&name) {
                continue;
            }
            let Ok(child) = RepoPath::new(join_path(rel, &name)) else {
                continue;
            };
            let child_abs = entry.path();
            let is_dir = entry.file_type()?.is_dir();
            let key = child.as_bytes();
            if is_dir {
                if self.tracked_files.contains(key) || child_abs.join(".git").exists() {
                    continue;
                }
                let ignored = self.stack.is_ignored(&child_abs, true);
                if ignored && !self.tracked_dirs.contains(key) {
                    self.out.push([key, b"/"].concat());
                } else {
                    self.dir(Some(&child), &child_abs)?;
                }
            } else if !self.tracked_files.contains(key) && self.stack.is_ignored(&child_abs, false)
            {
                self.out.push(key.to_vec());
            }
        }
        let _ = self.root;
        Ok(())
    }
}

/// Refreshes the stat data of index entries whose file is unchanged (verified by `compute`) and
/// old enough not to be racily clean. Writes the index only when an entry changed.
fn refresh_stat_cache(index_path: &Path, root: &Path, skip: &HashSet<Vec<u8>>) -> Result<()> {
    let mut index = IndexFile::read_or_empty(index_path).map_err(wc_err)?;
    let stat = PlainStat {
        root: root.to_owned(),
    };
    let cutoff = SystemTime::now() - Duration::from_secs(2);
    let mut newest: Option<SystemTime> = None;
    for e in &mut index.entries {
        if e.flags.stage != Stage::Normal
            || e.flags.skip_worktree
            || e.flags.intent_to_add
            || e.mode == asc_git::index::FileMode::Gitlink
            || skip.contains(e.path.as_bytes())
        {
            continue;
        }
        let probe = Entry {
            name: e.path.file_name(),
            kind: Kind::File,
            id: [0; 32],
            size: 0,
            git: e.oid,
        };
        let fresh = stat.stat(&e.path, &probe);
        let old_enough = fresh.mtime.to_system_time() < cutoff && fresh.mtime.secs != 0;
        let same = fresh.mtime == e.mtime && fresh.size == e.size && fresh.ino == e.ino;
        if same || !old_enough {
            continue;
        }
        (e.ctime, e.mtime, e.dev, e.ino, e.uid, e.gid, e.size) = (
            fresh.ctime,
            fresh.mtime,
            fresh.dev,
            fresh.ino,
            fresh.uid,
            fresh.gid,
            fresh.size,
        );
        let t = fresh.mtime.to_system_time();
        newest = Some(newest.map_or(t, |n| n.max(t)));
    }
    let Some(newest) = newest else { return Ok(()) };
    let floor = newest + Duration::from_nanos(1);
    index.mtime = Some(index.mtime.map_or(floor, |m| m.max(floor)));
    asc_git::index::write(index_path, &index).map_err(wc_err)
}

// -------------------------------------------------------------------- branch

/// Maps `name` through a `[+]src:dst` refspec with at most one `*`.
fn map_refspec(spec: &str, name: &str) -> Option<String> {
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (src, dst) = spec.split_once(':')?;
    match (src.split_once('*'), dst.split_once('*')) {
        (Some((sp, ss)), Some((dp, ds))) => {
            let mid = name.strip_prefix(sp)?.strip_suffix(ss)?;
            Some(format!("{dp}{mid}{ds}"))
        }
        (None, None) if src == name => Some(dst.to_owned()),
        _ => None,
    }
}

/// The upstream of `branch`: its display name and the ref that holds it.
fn upstream_ref(ctx: &Ctx, branch: &str) -> Option<(String, String)> {
    let remote = ctx.config.get(&format!("branch.{branch}.remote"))?;
    let merge = ctx.config.get(&format!("branch.{branch}.merge"))?;
    let tracking = if remote == "." {
        merge.clone()
    } else {
        ctx.config
            .get_all(&format!("remote.{remote}.fetch"))
            .iter()
            .find_map(|spec| map_refspec(spec, &merge))?
    };
    let display = tracking
        .strip_prefix("refs/remotes/")
        .or_else(|| tracking.strip_prefix("refs/heads/"))
        .unwrap_or(&tracking)
        .to_owned();
    Some((display, tracking))
}

async fn ref_commit(g: &Gather<'_>, name: &str) -> Result<Option<ChangesetId>> {
    let Ok(name) = RefName::new(name.as_bytes().to_vec()) else {
        return Ok(None);
    };
    let entry = g.ws.db.refs().get(g.ws.repo.key, &name).await?;
    Ok(entry.and_then(|e| match e.target {
        RefTarget::Changeset(c) => Some(c),
        RefTarget::Tag(_) => None,
    }))
}

async fn branch_info(g: &Gather<'_>) -> Result<Branch> {
    let ws = g.ws;
    let head = ws.projection.read_head()?;
    let commit = ws.head_commit().await?;
    let oid = match commit {
        Some(cs) => Some(
            ws.engine
                .git_commit_of(cs)
                .await
                .map_err(|e| fatal(e.to_string()))?,
        ),
        None => None,
    };
    let (kind, upstream) = match head {
        Head::Detached(_) => (HeadKind::Detached, None),
        Head::Branch(name) => {
            let up = match upstream_ref(g.ctx, &name) {
                None => None,
                Some((display, tracking)) => {
                    let track = match (ref_commit(g, &tracking).await?, commit) {
                        (None, _) => Track::Gone,
                        (Some(theirs), Some(ours)) => {
                            if !g.o.ahead_behind {
                                if theirs == ours {
                                    Track::Counts {
                                        ahead: 0,
                                        behind: 0,
                                    }
                                } else {
                                    Track::Different
                                }
                            } else {
                                let graph = Graph::new(EngineSource::new(&ws.engine));
                                let count = |inc, exc| LogQuery {
                                    include: vec![inc],
                                    exclude: vec![exc],
                                    ..LogQuery::default()
                                };
                                let ahead = graph.count(count(ours, theirs)).await;
                                let behind = graph.count(count(theirs, ours)).await;
                                Track::Counts {
                                    ahead: ahead.map_err(|e| fatal(e.to_string()))?,
                                    behind: behind.map_err(|e| fatal(e.to_string()))?,
                                }
                            }
                        }
                        // An unborn branch has no commits to compare.
                        (Some(_), None) => Track::Counts {
                            ahead: 0,
                            behind: 0,
                        },
                    };
                    Some(Upstream {
                        name: display,
                        track,
                    })
                }
            };
            (HeadKind::Branch(name), up)
        }
    };
    Ok(Branch {
        head: kind,
        oid,
        upstream,
    })
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

fn short_oid(text: &str) -> String {
    text.chars().take(7).collect()
}

fn in_progress(git_dir: &Path) -> Option<State> {
    if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
        let dir = if git_dir.join("rebase-merge").is_dir() {
            "rebase-merge"
        } else {
            "rebase-apply"
        };
        let branch = read_trim(git_dir.join(dir).join("head-name"))
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(&b).to_owned());
        let onto = read_trim(git_dir.join(dir).join("onto")).map(|o| short_oid(&o));
        // A rebase on the sequencer lists what is done and what is left.
        let info = super::rebase_todo::read_info(git_dir, 7)
            .filter(|i| i.interactive)
            .map(Box::new);
        return Some(State::Rebase { branch, onto, info });
    }
    if git_dir.join("MERGE_HEAD").exists() {
        return Some(State::Merge);
    }
    if let Some(h) = read_trim(git_dir.join("CHERRY_PICK_HEAD")) {
        return Some(State::CherryPick(short_oid(&h)));
    }
    if let Some(h) = read_trim(git_dir.join("REVERT_HEAD")) {
        return Some(State::Revert(short_oid(&h)));
    }
    if git_dir.join("BISECT_LOG").exists() {
        return Some(State::Bisect(read_trim(git_dir.join("BISECT_START"))));
    }
    None
}

// ---------------------------------------------------------------- formatting

fn quote(path: &[u8], quote_space: bool, quote_high: bool) -> Vec<u8> {
    asc_engine::render::quote_path(path, quote_space, quote_high)
}

fn relativize(path: &[u8], prefix: &[u8]) -> Vec<u8> {
    asc_engine::render::relativize(path, prefix)
}

struct Printer<'a> {
    o: &'a Opts,
    out: Vec<u8>,
}

impl Printer<'_> {
    fn text(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    /// Hint text names the program as invoked (SPEC §11.6).
    fn named(&self, s: &str) -> String {
        s.replace("\"git ", &format!("\"{} ", self.o.prog))
    }

    fn hint(&mut self, s: &str) {
        let named = self.named(s);
        self.line(&named);
    }

    fn line(&mut self, s: &str) {
        self.text(s);
        self.out.push(b'\n');
    }

    /// A path as the current format shows it.
    fn path(&self, p: &[u8], quote_space: bool) -> Vec<u8> {
        let shown = if self.o.relative {
            relativize(p, &self.o.prefix)
        } else {
            p.to_vec()
        };
        if self.o.z {
            shown
        } else {
            quote(&shown, quote_space, self.o.quote_high)
        }
    }

    fn term(&mut self) {
        self.out.push(if self.o.z { 0 } else { b'\n' });
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn tracking_text(up: &Upstream, hints: bool) -> String {
    let name = &up.name;
    let mut text = String::new();
    let mut hint = |s: &str| {
        if hints {
            text.push_str(&format!("  ({s})\n"));
        }
    };
    let head = match up.track {
        Track::Gone => {
            hint("use \"git branch --unset-upstream\" to fixup");
            format!("Your branch is based on '{name}', but the upstream is gone.\n")
        }
        Track::Different => {
            hint("use \"git status --ahead-behind\" for details");
            format!("Your branch and '{name}' refer to different commits.\n")
        }
        Track::Counts {
            ahead: 0,
            behind: 0,
        } => format!("Your branch is up to date with '{name}'.\n"),
        Track::Counts { ahead, behind: 0 } => {
            hint("use \"git push\" to publish your local commits");
            format!(
                "Your branch is ahead of '{name}' by {}.\n",
                plural(ahead, "commit", "commits")
            )
        }
        Track::Counts { ahead: 0, behind } => {
            hint("use \"git pull\" to update your local branch");
            format!(
                "Your branch is behind '{name}' by {}, and can be fast-forwarded.\n",
                plural(behind, "commit", "commits")
            )
        }
        Track::Counts { ahead, behind } => {
            hint("use \"git pull\" if you want to integrate the remote branch with yours");
            format!(
                "Your branch and '{name}' have diverged,\nand have {ahead} and {behind} different commits each, respectively.\n"
            )
        }
    };
    head + &text
}

fn state_text(s: &State, unmerged: bool, hints: bool) -> String {
    let mut out = String::new();
    let mut hint = |t: &str| {
        if hints {
            out.push_str(&format!("  ({t})\n"));
        }
    };
    let head = match s {
        State::Merge if unmerged => {
            hint("fix conflicts and run \"git commit\"");
            hint("use \"git merge --abort\" to abort the merge");
            "You have unmerged paths.\n".to_owned()
        }
        State::Merge => {
            hint("use \"git commit\" to conclude merge");
            "All conflicts fixed but you are still merging.\n".to_owned()
        }
        State::CherryPick(commit) | State::Revert(commit) => {
            let (verb, noun) = match s {
                State::CherryPick(_) => ("cherry-pick", "cherry-picking"),
                _ => ("revert", "reverting"),
            };
            if unmerged {
                hint(&format!("fix conflicts and run \"git {verb} --continue\""));
            } else {
                hint(&format!(
                    "all conflicts fixed: run \"git {verb} --continue\""
                ));
            }
            hint(&format!("use \"git {verb} --skip\" to skip this patch"));
            let abort = if verb == "revert" {
                "revert"
            } else {
                "cherry-pick"
            };
            hint(&format!(
                "use \"git {verb} --abort\" to cancel the {abort} operation"
            ));
            format!("You are currently {noun} commit {commit}.\n")
        }
        State::Rebase {
            info: Some(info), ..
        } => info.block(unmerged, hints, "git"),
        State::Rebase { branch, onto, .. } => {
            if unmerged {
                hint("fix conflicts and then run \"git rebase --continue\"");
                hint("use \"git rebase --skip\" to skip this patch");
                hint("use \"git rebase --abort\" to check out the original branch");
            } else {
                hint("all conflicts fixed: run \"git rebase --continue\"");
            }
            match (branch, onto) {
                (Some(b), Some(o)) => {
                    format!("You are currently rebasing branch '{b}' on '{o}'.\n")
                }
                _ => "You are currently rebasing.\n".to_owned(),
            }
        }
        State::Bisect(from) => {
            hint("use \"git bisect reset\" to get back to the original branch");
            match from {
                Some(b) => format!("You are currently bisecting, started from branch '{b}'.\n"),
                None => "You are currently bisecting.\n".to_owned(),
            }
        }
    };
    head + &out
}

fn long_label(side: u8) -> &'static str {
    match side {
        b'A' => "new file:",
        b'M' => "modified:",
        b'D' => "deleted:",
        b'T' => "typechange:",
        b'R' => "renamed:",
        _ => "unknown:",
    }
}

fn unmerged_label(xy: [u8; 2]) -> &'static str {
    match &xy {
        b"DD" => "both deleted:",
        b"AU" => "added by us:",
        b"UD" => "deleted by them:",
        b"UA" => "added by them:",
        b"DU" => "deleted by us:",
        b"AA" => "both added:",
        _ => "both modified:",
    }
}

fn print_long(p: &mut Printer<'_>, r: &Report) {
    let o = p.o;
    match &r.branch.head {
        HeadKind::Branch(name) => p.line(&format!("On branch {name}")),
        HeadKind::Detached => match &r.state {
            Some(State::Rebase {
                info: Some(info), ..
            }) => p.line(&info.head_line()),
            _ => {
                let at = r
                    .branch
                    .oid
                    .map(|o| short_oid(&o.to_string()))
                    .unwrap_or_default();
                p.line(&format!("HEAD detached at {at}"));
            }
        },
    }
    if let Some(up) = &r.branch.upstream {
        let text = p.named(&tracking_text(up, o.hints));
        p.text(&text);
        p.line("");
    }
    let unmerged_any = r.rows.iter().any(|x| x.stages.is_some());
    if let Some(state) = &r.state {
        let text = p.named(&state_text(state, unmerged_any, o.hints));
        p.text(&text);
        p.line("");
    }
    if r.initial {
        p.line("");
        p.line(if o.template {
            "Initial commit"
        } else {
            "No commits yet"
        });
        p.line("");
    }

    let staged: Vec<&Row> = r
        .rows
        .iter()
        .filter(|x| x.stages.is_none() && x.x != b'.')
        .collect();
    let unmerged: Vec<&Row> = r.rows.iter().filter(|x| x.stages.is_some()).collect();
    let unstaged: Vec<&Row> = r
        .rows
        .iter()
        .filter(|x| x.stages.is_none() && x.y != b'.')
        .collect();

    if !staged.is_empty() {
        p.line("Changes to be committed:");
        if o.hints
            && r.state
                .as_ref()
                .is_none_or(|s| s.name() != "merge" && s.name() != "cherry-pick")
        {
            if r.initial {
                p.hint("  (use \"git rm --cached <file>...\" to unstage)");
            } else {
                p.hint("  (use \"git restore --staged <file>...\" to unstage)");
            }
        }
        for row in &staged {
            let label = long_label(row.x);
            let mut name = p.path(&row.path, false);
            if let Some((orig, _)) = &row.orig {
                let mut full = p.path(orig, false);
                full.extend_from_slice(b" -> ");
                full.append(&mut name);
                name = full;
            }
            p.text(&format!("\t{label:<12}"));
            p.out.extend_from_slice(&name);
            p.out.push(b'\n');
        }
        p.line("");
    }
    if !unmerged.is_empty() {
        p.line("Unmerged paths:");
        if o.hints
            && r.state
                .as_ref()
                .is_none_or(|s| s.name() != "merge" && s.name() != "cherry-pick")
        {
            if r.initial {
                p.hint("  (use \"git rm --cached <file>...\" to unstage)");
            } else {
                p.hint("  (use \"git restore --staged <file>...\" to unstage)");
            }
        }
        if o.hints {
            let removes = unmerged
                .iter()
                .any(|x| matches!(&[x.x, x.y], b"DD" | b"UD" | b"DU"));
            if removes {
                p.hint("  (use \"git add/rm <file>...\" as appropriate to mark resolution)");
            } else {
                p.hint("  (use \"git add <file>...\" to mark resolution)");
            }
        }
        for row in &unmerged {
            let label = unmerged_label([row.x, row.y]);
            p.text(&format!("\t{label:<17}"));
            let name = p.path(&row.path, false);
            p.out.extend_from_slice(&name);
            p.out.push(b'\n');
        }
        p.line("");
    }
    if !unstaged.is_empty() {
        p.line("Changes not staged for commit:");
        if o.hints {
            let deleted = unstaged.iter().any(|x| x.y == b'D');
            if deleted {
                p.hint("  (use \"git add/rm <file>...\" to update what will be committed)");
            } else {
                p.hint("  (use \"git add <file>...\" to update what will be committed)");
            }
            p.hint("  (use \"git restore <file>...\" to discard changes in working directory)");
        }
        for row in &unstaged {
            let label = long_label(row.y);
            p.text(&format!("\t{label:<12}"));
            let name = p.path(&row.path, false);
            p.out.extend_from_slice(&name);
            p.out.push(b'\n');
        }
        p.line("");
    }
    let listed = |p: &mut Printer<'_>, title: &str, hint: &str, items: &[Vec<u8>]| {
        if items.is_empty() {
            return;
        }
        p.line(&format!("{title}:"));
        if p.o.hints {
            p.hint(&format!(
                "  (use \"{hint}\" to include in what will be committed)"
            ));
        }
        for item in items {
            p.text("\t");
            let name = p.path(item, false);
            p.out.extend_from_slice(&name);
            p.out.push(b'\n');
        }
        p.line("");
    };
    if o.untracked != UntrackedMode::No {
        listed(p, "Untracked files", "git add <file>...", &r.untracked);
    }
    if o.ignored {
        listed(p, "Ignored files", "git add -f <file>...", &r.ignored);
    }
    let committable = !staged.is_empty();
    if o.template {
        return; // the template has no summary line (git's `nowarn`)
    }
    if o.untracked == UntrackedMode::No && committable {
        let hint = if o.u_hint {
            " (use -u option to show untracked files)"
        } else {
            ""
        };
        p.line(&format!("Untracked files not listed{hint}"));
    }
    if !committable {
        let dirty = !unstaged.is_empty() || !unmerged.is_empty();
        let hint = |s: &str| {
            if o.hints {
                p.named(&format!(" ({s})"))
            } else {
                String::new()
            }
        };
        if dirty {
            p.line(&format!(
                "no changes added to commit{}",
                hint("use \"git add\" and/or \"git commit -a\"")
            ));
        } else if !r.untracked.is_empty() && o.untracked != UntrackedMode::No {
            p.line(&format!(
                "nothing added to commit but untracked files present{}",
                hint("use \"git add\" to track")
            ));
        } else if r.initial {
            p.line(&format!(
                "nothing to commit{}",
                hint("create/copy files and use \"git add\" to track")
            ));
        } else if o.untracked == UntrackedMode::No {
            p.line(&format!(
                "nothing to commit{}",
                hint("use -u to show untracked files")
            ));
        } else {
            p.line("nothing to commit, working tree clean");
        }
    }
}

fn branch_line(b: &Branch, initial: bool) -> String {
    let mut line = match (&b.head, initial) {
        (HeadKind::Detached, _) => "HEAD (no branch)".to_owned(),
        (HeadKind::Branch(n), true) => format!("No commits yet on {n}"),
        (HeadKind::Branch(n), false) => n.clone(),
    };
    if let Some(up) = &b.upstream {
        line.push_str(&format!("...{}", up.name));
        match up.track {
            Track::Gone => line.push_str(" [gone]"),
            Track::Different => line.push_str(" [different]"),
            Track::Counts {
                ahead: 0,
                behind: 0,
            } => {}
            Track::Counts { ahead, behind: 0 } => line.push_str(&format!(" [ahead {ahead}]")),
            Track::Counts { ahead: 0, behind } => line.push_str(&format!(" [behind {behind}]")),
            Track::Counts { ahead, behind } => {
                line.push_str(&format!(" [ahead {ahead}, behind {behind}]"));
            }
        }
    }
    line
}

fn print_short(p: &mut Printer<'_>, r: &Report) {
    if p.o.branch {
        p.text(&format!("## {}", branch_line(&r.branch, r.initial)));
        p.term();
    }
    for row in &r.rows {
        // Short format shows an unchanged side as a space.
        let xy = [row.x, row.y].map(|c| if c == b'.' { b' ' } else { c });
        p.out.extend_from_slice(&xy);
        p.out.push(b' ');
        let path = p.path(&row.path, true);
        match (&row.orig, p.o.z) {
            (Some((orig, _)), true) => {
                p.out.extend_from_slice(&path);
                p.out.push(0);
                let o = p.path(orig, true);
                p.out.extend_from_slice(&o);
                p.out.push(0);
            }
            (Some((orig, _)), false) => {
                let o = p.path(orig, true);
                p.out.extend_from_slice(&o);
                p.out.extend_from_slice(b" -> ");
                p.out.extend_from_slice(&path);
                p.out.push(b'\n');
            }
            (None, _) => {
                p.out.extend_from_slice(&path);
                p.term();
            }
        }
    }
    for (mark, items) in [("??", &r.untracked), ("!!", &r.ignored)] {
        for item in items {
            p.text(mark);
            p.out.push(b' ');
            let path = p.path(item, true);
            p.out.extend_from_slice(&path);
            p.term();
        }
    }
}

fn print_v2(p: &mut Printer<'_>, r: &Report) {
    use asc_engine::render::status as rs;
    let side = |s: Option<Side>| {
        s.map(|s| rs::Side {
            mode: s.mode,
            oid: s.oid,
        })
    };
    let branch = p.o.branch.then(|| rs::Branch {
        head: match &r.branch.head {
            HeadKind::Branch(n) => rs::Head::Branch(n.clone()),
            HeadKind::Detached => rs::Head::Detached,
        },
        oid: r.branch.oid,
        upstream: r.branch.upstream.as_ref().map(|u| rs::Upstream {
            name: u.name.clone(),
            track: match u.track {
                Track::Gone => rs::Track::Gone,
                Track::Different => rs::Track::Different,
                Track::Counts { ahead, behind } => rs::Track::Counts { ahead, behind },
            },
        }),
    });
    let rows: Vec<rs::Row> = r
        .rows
        .iter()
        .map(|row| rs::Row {
            path: row.path.clone(),
            orig: row.orig.clone(),
            x: row.x,
            y: row.y,
            head: side(row.head),
            index: side(row.index),
            work: row.work,
            stages: row.stages.map(|st| [side(st[0]), side(st[1]), side(st[2])]),
        })
        .collect();
    // git prints the changed entries, then the unmerged ones.
    let mut rows = rows;
    rows.sort_by_key(|row| row.stages.is_some());
    let opts = rs::Opts {
        z: p.o.z,
        relative: p.o.relative,
        prefix: p.o.prefix.clone(),
        quote_high: p.o.quote_high,
    };
    p.out.extend_from_slice(&rs::porcelain_v2(
        branch.as_ref(),
        &rows,
        &r.untracked,
        &r.ignored,
        &opts,
    ));
}

// ---------------------------------------------------------------------- json

fn json_line(r: &Report) -> Result<Vec<u8>> {
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).map_err(wc_err)?;
    let desc = |n: &str| {
        pool.get_message_by_name(n)
            .ok_or_else(|| fatal(format!("asc: missing message {n}")))
    };
    let mut resp = DynamicMessage::new(desc("asc.v1.WsStatusResponse")?);
    let b = &r.branch;
    if let HeadKind::Branch(n) = &b.head {
        resp.set_field_by_name("head_ref", Value::String(format!("refs/heads/{n}")));
    }
    if let Some(o) = b.oid {
        resp.set_field_by_name("head", Value::Bytes(o.as_bytes().to_vec().into()));
    }
    if let Some(up) = &b.upstream {
        resp.set_field_by_name("upstream", Value::String(up.name.clone()));
        if let Track::Counts { ahead, behind } = up.track {
            resp.set_field_by_name("ahead", Value::U32(ahead as u32));
            resp.set_field_by_name("behind", Value::U32(behind as u32));
        }
    }
    let file_desc = desc("asc.v1.WsFileStatus")?;
    let bytes = |v: &[u8]| Value::Bytes(v.to_vec().into());
    let mut files = Vec::new();
    for row in &r.rows {
        let mut f = DynamicMessage::new(file_desc.clone());
        f.set_field_by_name("path", bytes(&row.path));
        f.set_field_by_name(
            "xy",
            Value::String(format!("{}{}", row.x as char, row.y as char)),
        );
        if let Some((orig, _)) = &row.orig {
            f.set_field_by_name("orig_path", bytes(orig));
        }
        f.set_field_by_name("mode_head", Value::U32(mode_of(row.head)));
        f.set_field_by_name("mode_index", Value::U32(mode_of(row.index)));
        f.set_field_by_name("mode_worktree", Value::U32(row.work));
        if let Some(s) = row.head {
            f.set_field_by_name("git_head", bytes(s.oid.as_bytes()));
        }
        if let Some(s) = row.index {
            f.set_field_by_name("git_index", bytes(s.oid.as_bytes()));
        }
        files.push(Value::Message(f));
    }
    for (xy, items) in [("??", &r.untracked)] {
        for item in items {
            let mut f = DynamicMessage::new(file_desc.clone());
            f.set_field_by_name("path", bytes(item));
            f.set_field_by_name("xy", Value::String(xy.into()));
            files.push(Value::Message(f));
        }
    }
    resp.set_field_by_name("files", Value::List(files));
    if let Some(s) = &r.state {
        resp.set_field_by_name("in_progress", Value::String(s.name().into()));
    }
    let mut lines = JsonLines::new(Vec::new());
    lines.emit(&resp).map_err(wc_err)?;
    lines.finish().map_err(CliError::Io)
}

// ----------------------------------------------------------------------- run

/// The long report as `git commit` shows it. With `template`, it is the text of the commit
/// message template: no hints, no summary line, "Initial commit", and every line behind the
/// comment character `comment`. Otherwise it is what `git status` prints for "nothing to commit".
pub(crate) async fn long_text(ctx: &Ctx, ws: &WsCtx, template: Option<u8>) -> Result<Vec<u8>> {
    let cli =
        <Cli as clap::Parser>::try_parse_from(["status"]).map_err(|e| fatal(e.to_string()))?;
    let mut o = options(ctx, ws, &cli)?;
    o.format = Format::Long;
    o.z = false;
    if template.is_some() {
        o.hints = false;
        o.template = true;
    }
    let report = gather(ctx, ws, &o).await?;
    let mut p = Printer {
        o: &o,
        out: Vec::new(),
    };
    print_long(&mut p, &report);
    Ok(match template {
        Some(c) => comment_lines(&p.out, c),
        None => p.out,
    })
}

/// git's `status_printf` with `display_comment_prefix`: an empty line becomes the bare comment
/// character, a tab-indented line follows it directly, any other line gets a space.
fn comment_lines(text: &[u8], c: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + text.len() / 8);
    for line in text.split_inclusive(|&b| b == b'\n') {
        out.push(c);
        if !matches!(line.first(), Some(b'\n' | b'\t')) {
            out.push(b' ');
        }
        out.extend_from_slice(line);
    }
    out
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let a = args.0;
    let ws = ctx.ws()?;
    let o = options(ctx, ws, &a)?;
    let report = gather(ctx, ws, &o).await?;
    let mut p = Printer {
        o: &o,
        out: Vec::new(),
    };
    match o.format {
        Format::Long => print_long(&mut p, &report),
        Format::Short | Format::V1 => print_short(&mut p, &report),
        Format::V2 => print_v2(&mut p, &report),
        Format::Json => p.out = json_line(&report)?,
    }
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    // A closed pipe is the reader's choice, not a failure of status.
    let _ = lock.write_all(&p.out).and_then(|()| lock.flush());
    // B31: the live-conflict notice goes to stderr, so no stdout format changes.
    if matches!(o.format, Format::Long | Format::Short | Format::V1) {
        if let Some(n) = super::live::notice(&ws.home, &ws.workspace.ws.id.to_string()) {
            eprintln!("{n}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_git() {
        assert_eq!(quote(b"plain", true, true), b"plain");
        assert_eq!(quote(b"a b", true, true), b"\"a b\"");
        assert_eq!(quote(b"a b", false, true), b"a b");
        assert_eq!(quote("é".as_bytes(), false, true), b"\"\\303\\251\"");
        assert_eq!(quote("é".as_bytes(), false, false), "é".as_bytes());
        assert_eq!(quote(b"t\tq\"b\\", false, true), b"\"t\\tq\\\"b\\\\\"");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relativize(b"sub/x", b"sub/"), b"x");
        assert_eq!(relativize(b"a.txt", b"sub/"), b"../a.txt");
        assert_eq!(relativize(b"sub/deep/x", b"sub/other/"), b"../deep/x");
        assert_eq!(relativize(b"x", b""), b"x");
        assert_eq!(relativize(b"sub/", b"sub/"), b"./");
        assert_eq!(relativize(b"sub/new/", b"sub/"), b"new/");
    }

    #[test]
    fn refspec_mapping() {
        let m = |s, n| map_refspec(s, n);
        assert_eq!(
            m("+refs/heads/*:refs/remotes/origin/*", "refs/heads/trunk").as_deref(),
            Some("refs/remotes/origin/trunk")
        );
        assert_eq!(
            m("refs/heads/x:refs/remotes/o/y", "refs/heads/x").as_deref(),
            Some("refs/remotes/o/y")
        );
        assert_eq!(m("refs/tags/*:refs/tags/*", "refs/heads/x"), None);
    }

    #[test]
    fn unmerged_codes() {
        let s = Some(Side {
            mode: 0o100644,
            oid: zero(),
        });
        assert_eq!(unmerged_xy(&[s, s, s]), *b"UU");
        assert_eq!(unmerged_xy(&[None, s, s]), *b"AA");
        assert_eq!(unmerged_xy(&[s, s, None]), *b"UD");
        assert_eq!(unmerged_xy(&[s, None, s]), *b"DU");
        assert_eq!(unmerged_xy(&[None, s, None]), *b"AU");
        assert_eq!(unmerged_xy(&[None, None, s]), *b"UA");
    }

    #[test]
    fn prefix_of_cwd() {
        assert_eq!(cwd_prefix(Path::new("/r"), Path::new("/r")), b"");
        assert_eq!(cwd_prefix(Path::new("/r"), Path::new("/r/a/b")), b"a/b/");
        assert_eq!(cwd_prefix(Path::new("/r"), Path::new("/rr")), b"");
    }
}
