//! `worktree add|list|remove|prune|lock|unlock|move` and `worktree convert --plain` (SPEC §9.3).
//!
//! A worktree is a `workspaces` row, a synthetic gitdir `wc/<id>/git/`, a `.git` gitfile at its
//! root and materialised files (plain mode: no daemon and no VFS code). A requested path inside
//! another workspace's root is mounted at `$ASC_HOME/wt/<id>` and reached through a directory
//! symlink or junction at the requested path; `workspaces.path` is the requested path and
//! `workspaces.mount` the mount root. Output formats and errors follow git 2.55.
//!
//! Every path is untrusted: nothing follows a link below the enclosing workspace, removal never
//! follows a link, and a directory is deleted only when its `.git` names the workspace.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_git::projection::{Head, Projection};
use asc_state::WorkspaceId;
use asc_state::state::workspaces::{self, Mode, NewWorkspace, Workspace};
use asc_store::refs::{Expect, RefTxn, RefUpdate};
use asc_wc::worktree as wt;

use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::restore::{actor, root_of, sync_worktree};
use super::switch::{
    Rev, RevKind, abbrev_len, bad_branch_name, branch_tip, commit_line, ensure_mapped, held_anywhere, ref_name,
    resolve_rev, valid_branch_name,
};
use crate::ext;
use crate::hooks_runner::{Hook, HookEnv, Outcome, Runner, null_oid};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "worktree", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Every word before `--`, options included: the command reads its own flags.
    #[arg(allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
    /// The words after `--`.
    #[arg(last = true)]
    pub after: Vec<OsString>,
}

impl Args {
    /// The words as typed, `--` restored (clap consumes it).
    fn argv(&self) -> Vec<OsString> {
        let mut all = self.rest.clone();
        if !self.after.is_empty() {
            all.push(OsString::from("--"));
            all.extend(self.after.iter().cloned());
        }
        all
    }
}

/// The flags of every subcommand; `--plain` and `--mode`, `--ephemeral` and `--ttl` are asc's.
pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "worktree",
    flags: &[
        "-f", "--force", "--detach", "--checkout", "--lock", "--reason", "-b", "-B", "-q", "--quiet",
        "--ephemeral", "--ttl", "--mode", "-n", "--dry-run", "-v", "--verbose", "--porcelain", "-z",
        "--plain",
    ],
    placeholders: &[],
}];

const USAGE: &str = "usage: git worktree add [-f] [--detach] [--checkout] [--lock [--reason <string>]]\n                        [(-b | -B) <new-branch>] <path> [<commit-ish>]\n   or: git worktree list [-v | --porcelain [-z]]\n   or: git worktree lock [--reason <string>] <worktree>\n   or: git worktree move <worktree> <new-path>\n   or: git worktree prune [-n] [-v]\n   or: git worktree remove [-f] <worktree>\n   or: git worktree unlock <worktree>";

// ---------------------------------------------------------------------------------- arguments

/// One option a subcommand reads.
struct Spec {
    short: Option<char>,
    long: &'static str,
    value: bool,
}

const fn flag(short: Option<char>, long: &'static str) -> Spec {
    Spec { short, long, value: false }
}

const fn valued(short: Option<char>, long: &'static str) -> Spec {
    Spec { short, long, value: true }
}

/// The options and words of one subcommand.
#[derive(Default)]
struct Scanned {
    flags: Vec<(&'static str, Option<String>)>,
    words: Vec<OsString>,
}

impl Scanned {
    fn count(&self, long: &str) -> usize {
        self.flags.iter().filter(|(l, _)| *l == long).count()
    }

    fn has(&self, long: &str) -> bool {
        self.count(long) > 0
    }

    fn value(&self, long: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(l, _)| *l == long).and_then(|(_, v)| v.as_deref())
    }
}

fn unknown(what: String) -> CliError {
    CliError::Usage(format!("error: {what}\n{USAGE}"))
}

fn scan(argv: &[OsString], specs: &[Spec]) -> Result<Scanned> {
    let mut out = Scanned::default();
    let mut ended = false;
    let mut at = 0;
    let utf8 = |a: &OsString, opt: &str| {
        a.to_str().map(str::to_owned).ok_or_else(|| unknown(format!("option `{opt}' needs a UTF-8 value")))
    };
    while at < argv.len() {
        let arg = &argv[at];
        at += 1;
        let Some(text) = arg.to_str().filter(|t| !ended && t.len() > 1 && t.starts_with('-')) else {
            out.words.push(arg.clone());
            continue;
        };
        if text == "--" {
            ended = true;
            continue;
        }
        if let Some(long) = text.strip_prefix("--") {
            let (name, inline) = long.split_once('=').map_or((long, None), |(n, v)| (n, Some(v.to_owned())));
            let Some(spec) = specs.iter().find(|s| s.long.trim_start_matches("--") == name) else {
                return Err(unknown(format!("unknown option `{name}'")));
            };
            let value = match (spec.value, inline) {
                (true, Some(v)) => Some(v),
                (true, None) => {
                    let next = argv.get(at).ok_or_else(|| unknown(format!("option `{name}' requires a value")))?;
                    at += 1;
                    Some(utf8(next, name)?)
                }
                (false, None) => None,
                (false, Some(_)) => return Err(unknown(format!("option `{name}' takes no value"))),
            };
            out.flags.push((spec.long, value));
            continue;
        }
        let body = &text[1..];
        for (i, c) in body.char_indices() {
            let Some(spec) = specs.iter().find(|s| s.short == Some(c)) else {
                return Err(unknown(format!("unknown switch `{c}'")));
            };
            if !spec.value {
                out.flags.push((spec.long, None));
                continue;
            }
            let rest = &body[i + c.len_utf8()..];
            let value = if rest.is_empty() {
                let next = argv.get(at).ok_or_else(|| unknown(format!("switch `{c}' requires a value")))?;
                at += 1;
                utf8(next, &c.to_string())?
            } else {
                rest.to_owned()
            };
            out.flags.push((spec.long, Some(value)));
            break;
        }
    }
    Ok(out)
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

fn usage_error() -> CliError {
    CliError::Usage(USAGE.to_owned())
}

// ----------------------------------------------------------------------------------- helpers

fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

fn text_of(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| fatal(format!("'{}' is not valid UTF-8", path.display())))
}

fn name_of(path: &str) -> String {
    Path::new(path).file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// The repository's worktrees: the main one (registered first) and then the others by name.
async fn rows(ws: &WsCtx) -> Result<Vec<Workspace>> {
    let mut all = workspaces::list(&ws.db, Some(ws.repo.key)).await?;
    if all.len() > 1 {
        let main = all.remove(0);
        all.sort_by(|a, b| name_of(&a.ws.path).cmp(&name_of(&b.ws.path)).then_with(|| a.ws.path.cmp(&b.ws.path)));
        all.insert(0, main);
    }
    Ok(all)
}

/// The worktree `typed` names: a path (against the current directory), or a unique name.
fn locate<'a>(ctx: &Ctx, all: &'a [Workspace], typed: &OsString) -> Result<&'a Workspace> {
    let not_one = || fatal(format!("'{}' is not a working tree", typed.to_string_lossy()));
    let abs = wt::absolutize(&ctx.cwd, Path::new(typed));
    let key = wt::path_key(&abs);
    if let Some(row) = all
        .iter()
        .find(|r| wt::path_key(Path::new(&r.ws.path)) == key || wt::path_key(Path::new(&r.ws.mount)) == key)
    {
        return Ok(row);
    }
    let plain_name = typed.to_str().is_some_and(|t| !t.is_empty() && !t.contains(['/', '\\']));
    if plain_name {
        let mut hits = all.iter().filter(|r| name_of(&r.ws.path) == typed.to_string_lossy());
        if let (Some(one), None) = (hits.next(), hits.next()) {
            return Ok(one);
        }
    }
    Err(not_one())
}

fn is_main(all: &[Workspace], row: &Workspace) -> bool {
    all.first().is_some_and(|m| m.ws.id == row.ws.id)
}

/// A context for the workspace at `dir`, ignoring `GIT_DIR` and its kin in the environment.
async fn open_at(ctx: &Ctx, dir: PathBuf) -> Result<Ctx> {
    let mut env = ctx.env.clone();
    env.git_dir = None;
    env.work_tree = None;
    env.common_dir = None;
    env.index_file = None;
    Ctx::open(ctx.prog, env, dir).await
}

/// Whether the worktree at `dir` has modified or untracked files (ignored files do not count).
/// The check is the `status` command itself, run in the worktree, so it sees exactly what
/// `asc status` shows there.
fn is_dirty(dir: &Path) -> Result<bool> {
    let exe = std::env::current_exe()?;
    let out = std::process::Command::new(exe)
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(fatal(format!(
            "cannot read the status of '{}': {}",
            slashed(&dir.display().to_string()),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(!out.stdout.is_empty())
}

fn lock_text(reason: &str, what: &str, ff: &str) -> String {
    if reason.is_empty() {
        format!("cannot {what} a locked working tree;\nuse '{what} -f -f' to override or unlock first")
    } else {
        format!("cannot {what} a locked working tree, lock reason: {reason}\nuse '{what} -f -f' to override or unlock first{ff}")
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ----------------------------------------------------------------------------------------- add

/// What `worktree add` is asked for.
pub(crate) struct AddOpts {
    /// The path as typed, for messages.
    pub typed: String,
    /// The absolute, lexically normalised path.
    pub path: PathBuf,
    /// `-b` / `-B`: the new branch and whether it may reset an existing one.
    pub branch: Option<(String, bool)>,
    pub detach: bool,
    pub force: bool,
    /// `--lock`: the reason (possibly empty).
    pub lock: Option<String>,
    pub quiet: bool,
    pub start: Option<String>,
    /// `--ephemeral --ttl`: seconds until the worktree may be removed.
    pub ephemeral_secs: Option<u64>,
    /// `--mode virtual`.
    pub virtual_mode: bool,
}

/// What `worktree add` made.
pub(crate) struct Added {
    pub path: PathBuf,
    /// The exit status of a failing `post-checkout` hook, else 0.
    pub hook_exit: u8,
}

enum Dest {
    New { name: String, force: bool, start: Rev },
    Existing { name: String, tip: ChangesetId },
    Detach { commit: ChangesetId },
}

async fn need_rev(ctx: &Ctx, ws: &WsCtx, word: &str) -> Result<Rev> {
    resolve_rev(ctx, ws, word).await?.ok_or_else(|| fatal(format!("invalid reference: {word}")))
}

async fn destination(ctx: &Ctx, ws: &WsCtx, o: &AddOpts) -> Result<Dest> {
    if let Some((name, force)) = &o.branch {
        if !valid_branch_name(name) {
            return Err(bad_branch_name(ctx, name));
        }
        let start = need_rev(ctx, ws, o.start.as_deref().unwrap_or("HEAD")).await?;
        return Ok(Dest::New { name: name.clone(), force: *force, start });
    }
    if o.detach {
        let rev = need_rev(ctx, ws, o.start.as_deref().unwrap_or("HEAD")).await?;
        return Ok(Dest::Detach { commit: rev.commit });
    }
    if let Some(word) = o.start.as_deref() {
        let rev = need_rev(ctx, ws, word).await?;
        return Ok(match rev.kind {
            RevKind::Branch(name) => Dest::Existing { name, tip: rev.commit },
            _ => Dest::Detach { commit: rev.commit },
        });
    }
    // `worktree add <path>`: a branch named after the directory, made from HEAD when missing.
    let name = o.path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
    if !valid_branch_name(&name) {
        return Err(bad_branch_name(ctx, &name));
    }
    Ok(match branch_tip(ws, &name).await? {
        Some(tip) => Dest::Existing { name, tip },
        None => Dest::New { name, force: false, start: need_rev(ctx, ws, "HEAD").await? },
    })
}

/// What `add` created, for undoing a failed attempt.
#[derive(Default)]
struct Made {
    id: Option<WorkspaceId>,
    /// The mount root was created by this call.
    created_mount: bool,
    /// The mount holds files and a gitfile of this call.
    populated: bool,
    link: bool,
}

/// Undoes what a failed `add` made: the link, the mount (only what this call made), the gitdir
/// and the row. Errors here are not reported; the original error is.
async fn undo(ws: &WsCtx, made: &Made, path: &Path, mount: &Path, nested: bool) {
    if made.link && nested {
        let _ = wt::remove_tree(path);
    }
    if made.populated {
        if made.created_mount {
            let _ = wt::remove_tree(mount);
        } else {
            let _ = wt::remove_tree(&mount.join(".git"));
        }
    } else if made.created_mount {
        let _ = std::fs::remove_dir(mount);
    }
    if let Some(id) = made.id {
        let _ = wt::remove_tree(&ws.home.workspace_dir(id));
        let _ = workspaces::remove(&ws.db, id).await;
    }
}

/// `worktree add`: see the module docs. The caller has parsed and validated the options.
pub(crate) async fn add(ctx: &Ctx, o: AddOpts) -> Result<Added> {
    let ws = ctx.ws()?;
    // Bind the hook runner to this workspace before anything else can bind another.
    let _ = Runner::for_ctx(ctx)?;
    if o.virtual_mode {
        return Err(fatal("asc: virtual worktrees need the daemon, which is not available yet; use --mode plain"));
    }
    let dest = destination(ctx, ws, &o).await?;
    let (commit, branch_name): (ChangesetId, Option<String>) = match &dest {
        Dest::New { name, start, .. } => (start.commit, Some(name.clone())),
        Dest::Existing { name, tip } => (*tip, Some(name.clone())),
        Dest::Detach { commit } => (*commit, None),
    };
    let git = ensure_mapped(ws, commit).await?;
    let existing_tip = match &dest {
        Dest::New { name, .. } => branch_tip(ws, name).await?,
        _ => None,
    };

    if let Dest::New { name, force: true, .. } = &dest
        && existing_tip.is_some()
        && !o.force
        && let Some(at) = held_anywhere(ws, name).await?
    {
        return Err(fatal(format!("'{name}' is already used by worktree at '{at}'")));
    }

    if !o.quiet {
        let abbrev = |oid: &GitOid| oid.to_string()[..abbrev_len(ctx)].to_owned();
        match &dest {
            Dest::New { name, force: true, .. } if existing_tip.is_some() => {
                let was = ensure_mapped(ws, existing_tip.unwrap_or(commit)).await?;
                eprintln!("Preparing worktree (resetting branch '{name}'; was at {})", abbrev(&was));
            }
            Dest::New { name, .. } => eprintln!("Preparing worktree (new branch '{name}')"),
            Dest::Existing { name, .. } => eprintln!("Preparing worktree (checking out '{name}')"),
            Dest::Detach { .. } => eprintln!("Preparing worktree (detached HEAD {})", abbrev(&git)),
        }
    }

    // Where it goes.
    let abs = o.path.clone();
    let home = ws.home.root();
    if wt::path_key(&abs) == wt::path_key(home) || wt::is_inside(&abs, home) {
        return Err(fatal(format!("'{}' is inside the asc home", o.typed)));
    }
    let all = workspaces::list(&ws.db, None).await?;
    let key = wt::path_key(&abs);
    if all.iter().any(|r| wt::path_key(Path::new(&r.ws.path)) == key || wt::path_key(Path::new(&r.ws.mount)) == key) {
        return Err(fatal(if std::fs::symlink_metadata(&abs).is_ok() {
            format!("'{}' already exists", o.typed)
        } else {
            format!(
                "'{}' is a missing but already registered worktree;\nuse 'prune' or 'remove' to clear",
                o.typed
            )
        }));
    }
    let occupancy = wt::occupancy(&abs)?;
    if occupancy == wt::Occupancy::Occupied {
        return Err(fatal(format!("'{}' already exists", o.typed)));
    }
    match &dest {
        Dest::New { name, force: false, .. } if existing_tip.is_some() => {
            return Err(CliError::Raw(format!("fatal: a branch named '{name}' already exists"), 255));
        }
        Dest::Existing { name, .. } if !o.force => {
            if let Some(at) = held_anywhere(ws, name).await? {
                return Err(fatal(format!("'{name}' is already used by worktree at '{at}'")));
            }
        }
        _ => {}
    }
    let enclosing = all
        .iter()
        .filter(|r| wt::is_inside(&abs, Path::new(&r.ws.path)))
        .max_by_key(|r| r.ws.path.len())
        .map(|r| PathBuf::from(&r.ws.path));

    let id = WorkspaceId::new();
    let nested = enclosing.is_some();
    let mount = if nested { ws.home.worktree_mount(id) } else { abs.clone() };
    let mut made = Made::default();
    let built = materialise(ctx, ws, &o, &dest, commit, git, branch_name.as_deref(), &abs, &mount, enclosing.as_deref(), id, occupancy, &mut made).await;
    let new_ctx = match built {
        Ok(c) => c,
        Err(e) => {
            undo(ws, &made, &abs, &mount, nested).await;
            return Err(e);
        }
    };

    if !o.quiet {
        println!("HEAD is now at {}", commit_line(ctx, ws, commit).await?);
    }
    let runner = Runner::for_ctx(&new_ctx)?;
    let hook = Hook::PostCheckout { old: null_oid(), new: git, branch: true };
    let hook_exit = match runner.run(hook, &HookEnv::of(&new_ctx, "worktree add")).await.map_err(fatal)? {
        Outcome::Exit(code) if code != 0 => u8::try_from(code).unwrap_or(1),
        _ => 0,
    };
    Ok(Added { path: abs, hook_exit })
}

#[allow(clippy::too_many_arguments)]
async fn materialise(
    ctx: &Ctx,
    ws: &WsCtx,
    o: &AddOpts,
    dest: &Dest,
    commit: ChangesetId,
    git: GitOid,
    branch: Option<&str>,
    abs: &Path,
    mount: &Path,
    enclosing: Option<&Path>,
    id: WorkspaceId,
    occupancy: wt::Occupancy,
    made: &mut Made,
) -> Result<Ctx> {
    match enclosing {
        Some(boundary) => {
            let parent = abs.parent().ok_or_else(|| fatal("path has no parent"))?;
            wt::make_dirs_below(boundary, parent)?;
            if occupancy == wt::Occupancy::EmptyDir {
                std::fs::remove_dir(abs)?;
            }
            std::fs::create_dir_all(mount)?;
            made.created_mount = true;
        }
        None => {
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            made.created_mount = occupancy == wt::Occupancy::Missing;
        }
    }
    made.populated = true;
    made.id = Some(id);
    let projection = Projection::create(&ws.home, ws.repo.repo.id, id, mount)?;
    projection.write_head(&match branch {
        Some(name) => Head::Branch(name.to_owned()),
        None => Head::Detached(git),
    })?;
    workspaces::insert(
        &ws.db,
        &NewWorkspace {
            id,
            repo: ws.repo.key,
            path: text_of(abs)?.to_owned(),
            mount: text_of(mount)?.to_owned(),
            mode: Mode::Plain,
            head: branch.map_or_else(String::new, |b| format!("refs/heads/{b}")),
            base_cs: Some(commit),
            sparse: None,
            ephemeral_until: o
                .ephemeral_secs
                .map(|s| now().saturating_add(i64::try_from(s).unwrap_or(i64::MAX))),
        },
    )
    .await?;

    // Files.
    let fresh = open_at(ctx, mount.to_owned()).await?;
    let fws = fresh.ws()?;
    if fws.workspace.ws.id != id {
        return Err(fatal("asc: the new worktree did not open as itself"));
    }
    let root = root_of(fws, Some(commit)).await?;
    let empty = super::common::CliEngine::empty_tree_id();
    let paths: Vec<_> = fws.engine.leafdiff(empty, root, None).await?.into_iter().map(|(p, _)| p).collect();
    sync_worktree(&fresh, root, &paths, true).await?;

    // The branch.
    if let Dest::New { name, force, start } = dest {
        let full = format!("refs/heads/{name}");
        let old = super::switch::ref_target(ws, &full).await?;
        let expect = match (old, force) {
            (Some(t), true) => Expect::Value(t),
            _ => Expect::Absent,
        };
        let update = RefUpdate {
            name: ref_name(&full)?,
            expect,
            new: Some(RefTarget::Changeset(start.commit)),
            expires_at: None,
        };
        let txn = RefTxn { updates: vec![update], actor: actor(), reason: String::new(), op_id: ext::OpId::new().0 };
        let reason = if old.is_some() {
            format!("branch: Reset to {}", start.label)
        } else {
            format!("branch: Created from {}", start.label)
        };
        ctx.update_local_refs(txn, &reason, StagingAfterMove::Keep).await?;
    }

    // The path a nested worktree is reached by.
    if enclosing.is_some() {
        wt::make_dir_link(abs, mount).map_err(|e| {
            fatal(format!("cannot link '{}' to its mount: {e}", o.typed))
        })?;
        made.link = true;
    }
    if let Some(reason) = &o.lock {
        workspaces::set_locked(&ws.db, id, Some(reason)).await?;
    }
    ext::index_sink().on_head_moved(id, None, Some(commit)).await.map_err(fatal)?;
    Ok(fresh)
}

async fn add_command(ctx: &Ctx, argv: &[OsString]) -> Result<ExitCode> {
    let sc = scan(
        argv,
        &[
            flag(Some('f'), "--force"),
            flag(None, "--detach"),
            flag(None, "--checkout"),
            flag(None, "--lock"),
            valued(None, "--reason"),
            valued(Some('b'), "-b"),
            valued(Some('B'), "-B"),
            flag(Some('q'), "--quiet"),
            flag(None, "--ephemeral"),
            valued(None, "--ttl"),
            valued(None, "--mode"),
        ],
    )?;
    let ws = ctx.ws()?;
    let _ = ws;
    if sc.has("-b") && sc.has("-B") {
        return Err(fatal("options '-b' and '-B' cannot be used together"));
    }
    if sc.has("--detach") && (sc.has("-b") || sc.has("-B")) {
        let which = if sc.has("-b") { "-b" } else { "-B" };
        return Err(fatal(format!("options '--detach' and '{which}' cannot be used together")));
    }
    if sc.has("--reason") && !sc.has("--lock") {
        return Err(fatal("the option '--reason' requires '--lock'"));
    }
    if sc.has("--ttl") && !sc.has("--ephemeral") {
        return Err(fatal("the option '--ttl' requires '--ephemeral'"));
    }
    let ephemeral_secs = if sc.has("--ephemeral") {
        Some(match sc.value("--ttl") {
            Some(t) => wt::parse_ttl(t).ok_or_else(|| fatal(format!("invalid --ttl '{t}'; use a number and s, m, h, d or w")))?,
            None => wt::DEFAULT_TTL_SECS,
        })
    } else {
        None
    };
    let virtual_mode = match sc.value("--mode") {
        None | Some("plain") => false,
        Some("virtual") => true,
        Some(other) => return Err(fatal(format!("invalid --mode '{other}'; use virtual or plain"))),
    };
    let (typed, start) = match sc.words.as_slice() {
        [p] => (p, None),
        [p, s] => (p, Some(s.to_str().ok_or_else(|| fatal("the commit-ish is not valid UTF-8"))?.to_owned())),
        _ => return Err(usage_error()),
    };
    let branch = match (sc.value("-b"), sc.value("-B")) {
        (Some(b), _) => Some((b.to_owned(), false)),
        (_, Some(b)) => Some((b.to_owned(), true)),
        _ => None,
    };
    let done = add(
        ctx,
        AddOpts {
            typed: typed.to_string_lossy().into_owned(),
            path: wt::absolutize(&ctx.cwd, Path::new(typed)),
            branch,
            detach: sc.has("--detach"),
            force: sc.has("--force"),
            lock: sc.has("--lock").then(|| sc.value("--reason").unwrap_or("").to_owned()),
            quiet: sc.has("--quiet"),
            start,
            ephemeral_secs,
            virtual_mode,
        },
    )
    .await?;
    Ok(ExitCode::from(done.hook_exit))
}

// ---------------------------------------------------------------------------------------- list

struct Line {
    path: String,
    oid: GitOid,
    branch: Option<String>,
    locked: Option<String>,
    prunable: Option<&'static str>,
}

const GONE: &str = "gitdir file points to non-existent location";

/// Why a worktree could be pruned: its directory is gone. The main worktree and a locked one
/// never are.
fn prunable(all: &[Workspace], row: &Workspace) -> Option<&'static str> {
    if is_main(all, row) || row.locked.is_some() {
        return None;
    }
    let path = Path::new(&row.ws.path);
    let gone = matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        || (row.ws.mount != row.ws.path && !Path::new(&row.ws.mount).exists())
        || !Path::new(&row.ws.mount).join(".git").is_file();
    gone.then_some(GONE)
}

async fn describe(ws: &WsCtx, all: &[Workspace], row: &Workspace) -> Result<Line> {
    let branch = row.ws.head.strip_prefix("refs/heads/").map(str::to_owned);
    let commit = match &branch {
        Some(b) => branch_tip(ws, b).await?,
        None => row.ws.base_cs,
    };
    let oid = match commit {
        Some(c) => ws.engine.git_commit_of(c).await.map_err(fatal)?,
        None => null_oid(),
    };
    Ok(Line {
        path: slashed(&row.ws.path),
        oid,
        branch,
        locked: row.locked.clone(),
        prunable: prunable(all, row),
    })
}

async fn list_command(ctx: &Ctx, argv: &[OsString]) -> Result<ExitCode> {
    let sc = scan(argv, &[flag(Some('v'), "--verbose"), flag(None, "--porcelain"), flag(Some('z'), "--z")])?;
    if !sc.words.is_empty() {
        return Err(usage_error());
    }
    if sc.has("--z") && !sc.has("--porcelain") {
        return Err(fatal("the option '-z' requires '--porcelain'"));
    }
    if sc.has("--verbose") && sc.has("--porcelain") {
        return Err(fatal("options '--verbose' and '--porcelain' cannot be used together"));
    }
    let ws = ctx.ws()?;
    let all = rows(ws).await?;
    let mut lines = Vec::new();
    for row in &all {
        lines.push(describe(ws, &all, row).await?);
    }
    let mut out = String::new();
    if sc.has("--porcelain") {
        let nul = sc.has("--z");
        let end = if nul { '\0' } else { '\n' };
        for l in &lines {
            out.push_str(&format!("worktree {}{end}HEAD {}{end}", l.path, l.oid));
            match &l.branch {
                Some(b) => out.push_str(&format!("branch refs/heads/{b}{end}")),
                None => out.push_str(&format!("detached{end}")),
            }
            if let Some(reason) = &l.locked {
                if reason.is_empty() {
                    out.push_str(&format!("locked{end}"));
                } else {
                    out.push_str(&format!("locked {reason}{end}"));
                }
            }
            if let Some(why) = l.prunable {
                out.push_str(&format!("prunable {why}{end}"));
            }
            out.push(end);
        }
    } else {
        let verbose = sc.has("--verbose");
        let width = lines.iter().map(|l| l.path.chars().count()).max().unwrap_or(0);
        for l in &lines {
            let short = &l.oid.to_string()[..abbrev_len(ctx)];
            let head = l.branch.as_ref().map_or_else(|| "(detached HEAD)".to_owned(), |b| format!("[{b}]"));
            out.push_str(&format!("{:<width$} {short} {head}", l.path));
            match &l.locked {
                Some(reason) if verbose && !reason.is_empty() => out.push_str(&format!("\n\tlocked: {reason}")),
                Some(_) => out.push_str(" locked"),
                None => {}
            }
            match l.prunable {
                Some(why) if verbose => out.push_str(&format!("\n\tprunable: {why}")),
                Some(_) => out.push_str(" prunable"),
                None => {}
            }
            out.push('\n');
        }
    }
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    stdout.flush()?;
    Ok(ExitCode::SUCCESS)
}

// -------------------------------------------------------------------------------------- remove

/// Deletes the worktree's files, link, gitdir and row. The row goes last, so a failure leaves
/// something `remove` can be run on again.
async fn delete(ws: &WsCtx, row: &Workspace) -> Result<()> {
    let id = row.ws.id;
    let path = PathBuf::from(&row.ws.path);
    let mount = PathBuf::from(&row.ws.mount);
    let nested = wt::path_key(&path) != wt::path_key(&mount);
    // Never delete from a directory that is not this worktree's: stale state must not turn
    // into the loss of an unrelated directory.
    let ours = |dir: &Path| {
        std::fs::symlink_metadata(dir.join(".git"))
            .ok()
            .filter(|m| m.is_file())
            .and_then(|_| std::fs::read_to_string(dir.join(".git")).ok())
            .is_some_and(|t| t.contains(&id.to_string()))
    };
    if nested {
        if mount != ws.home.worktree_mount(id) {
            return Err(fatal(format!("asc: the mount of '{}' is not where asc makes mounts", slashed(&row.ws.path))));
        }
        if std::fs::symlink_metadata(&path).is_ok() {
            if !wt::is_link_at(&path) {
                return Err(fatal(format!(
                    "'{}' is not the link asc made for this worktree; remove it yourself",
                    slashed(&row.ws.path)
                )));
            }
            wt::remove_tree(&path)?;
        }
        if mount.exists() {
            if !ours(&mount) {
                return Err(fatal(format!("asc: '{}' is not the mount of this worktree", slashed(&row.ws.mount))));
            }
            wt::remove_tree(&mount)?;
        }
    } else if std::fs::symlink_metadata(&path).is_ok() {
        if !ours(&path) {
            return Err(fatal(format!(
                "'{}' is not a worktree of this repository (its .git does not name it); remove it yourself",
                slashed(&row.ws.path)
            )));
        }
        wt::remove_tree(&path)?;
    }
    wt::remove_tree(&ws.home.workspace_dir(id))?;
    workspaces::remove(&ws.db, id).await?;
    Ok(())
}

/// Moves the process out of a directory about to be deleted (Windows will not delete the
/// current directory).
fn leave(row: &Workspace, ws: &WsCtx) {
    if let Ok(cwd) = std::env::current_dir() {
        let (p, m) = (Path::new(&row.ws.path), Path::new(&row.ws.mount));
        let inside = |d: &Path| wt::path_key(&cwd) == wt::path_key(d) || wt::is_inside(&cwd, d);
        if inside(p) || inside(m) {
            let _ = std::env::set_current_dir(ws.home.root());
        }
    }
}

/// `worktree remove`: `force` is the count of `-f`.
pub(crate) async fn remove(ctx: &Ctx, typed: &OsString, force: usize) -> Result<()> {
    let ws = ctx.ws()?;
    let all = rows(ws).await?;
    let row = locate(ctx, &all, typed)?;
    let shown = typed.to_string_lossy();
    if is_main(&all, row) {
        return Err(fatal(format!("'{shown}' is a main working tree")));
    }
    if let Some(reason) = &row.locked
        && force < 2
    {
        return Err(fatal(lock_text(reason, "remove", "")));
    }
    let mount = Path::new(&row.ws.mount);
    if force == 0 {
        if !Path::new(&row.ws.path).exists() {
            return Err(fatal(format!(
                "validation failed, cannot remove working tree: '{}' does not exist",
                slashed(&row.ws.path)
            )));
        }
        if is_dirty(mount)? {
            return Err(fatal(format!("'{shown}' contains modified or untracked files, use --force to delete it")));
        }
    }
    leave(row, ws);
    delete(ws, row).await
}

async fn remove_command(ctx: &Ctx, argv: &[OsString]) -> Result<ExitCode> {
    let sc = scan(argv, &[flag(Some('f'), "--force")])?;
    let [typed] = sc.words.as_slice() else { return Err(usage_error()) };
    remove(ctx, typed, sc.count("--force")).await?;
    Ok(ExitCode::SUCCESS)
}

// --------------------------------------------------------------------------------------- prune

async fn prune_command(ctx: &Ctx, argv: &[OsString]) -> Result<ExitCode> {
    let sc = scan(argv, &[flag(Some('n'), "--dry-run"), flag(Some('v'), "--verbose")])?;
    if !sc.words.is_empty() {
        return Err(usage_error());
    }
    let (dry, verbose) = (sc.has("--dry-run"), sc.has("--verbose"));
    let ws = ctx.ws()?;
    let all = rows(ws).await?;
    for row in &all {
        let why = match prunable(&all, row) {
            Some(why) => why,
            None => {
                // An expired ephemeral worktree goes when it is clean; a dirty one is kept.
                let expired = row.ws.ephemeral_until.is_some_and(|t| t < now());
                if !expired || is_main(&all, row) || row.locked.is_some() || is_dirty(Path::new(&row.ws.mount))? {
                    continue;
                }
                "ephemeral worktree expired"
            }
        };
        if dry || verbose {
            eprintln!("Removing worktrees/{}: {why}", name_of(&row.ws.path));
        }
        if !dry {
            leave(row, ws);
            delete(ws, row).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------- lock, unlock

async fn lock_command(ctx: &Ctx, argv: &[OsString], lock: bool) -> Result<ExitCode> {
    let specs = [valued(None, "--reason")];
    let sc = scan(argv, if lock { &specs } else { &[] })?;
    let [typed] = sc.words.as_slice() else { return Err(usage_error()) };
    let ws = ctx.ws()?;
    let all = rows(ws).await?;
    let row = locate(ctx, &all, typed)?;
    if is_main(&all, row) {
        return Err(fatal("The main working tree cannot be locked or unlocked"));
    }
    let shown = typed.to_string_lossy();
    match (&row.locked, lock) {
        (Some(reason), true) if reason.is_empty() => return Err(fatal(format!("'{shown}' is already locked"))),
        (Some(reason), true) => return Err(fatal(format!("'{shown}' is already locked, reason: {reason}"))),
        (None, false) => return Err(fatal(format!("'{shown}' is not locked"))),
        _ => {}
    }
    let reason = lock.then(|| sc.value("--reason").unwrap_or(""));
    workspaces::set_locked(&ws.db, row.ws.id, reason).await?;
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------------------- move

async fn move_command(ctx: &Ctx, argv: &[OsString]) -> Result<ExitCode> {
    let sc = scan(argv, &[flag(Some('f'), "--force")])?;
    let [typed, to] = sc.words.as_slice() else { return Err(usage_error()) };
    let ws = ctx.ws()?;
    let all = rows(ws).await?;
    let row = locate(ctx, &all, typed)?;
    let shown = typed.to_string_lossy();
    if is_main(&all, row) {
        return Err(fatal(format!("'{shown}' is a main working tree")));
    }
    if let Some(reason) = &row.locked
        && sc.count("--force") < 2
    {
        return Err(fatal(lock_text(reason, "move", "")));
    }
    let mut dst = wt::absolutize(&ctx.cwd, Path::new(to));
    if std::fs::symlink_metadata(&dst).is_ok_and(|m| m.is_dir()) {
        dst.push(name_of(&row.ws.path));
    }
    let src = PathBuf::from(&row.ws.path);
    let key = wt::path_key(&dst);
    if wt::path_key(&dst) == wt::path_key(ws.home.root()) || wt::is_inside(&dst, ws.home.root()) {
        return Err(fatal(format!("'{}' is inside the asc home", to.to_string_lossy())));
    }
    if wt::is_inside(&dst, &src) || wt::is_inside(&dst, Path::new(&row.ws.mount)) {
        return Err(fatal(format!("cannot move '{shown}' into itself")));
    }
    if std::fs::symlink_metadata(&dst).is_ok()
        || all.iter().any(|r| wt::path_key(Path::new(&r.ws.path)) == key || wt::path_key(Path::new(&r.ws.mount)) == key)
    {
        return Err(fatal(format!("'{}' already exists", to.to_string_lossy())));
    }
    let enclosing = all
        .iter()
        .filter(|r| r.ws.id != row.ws.id && wt::is_inside(&dst, Path::new(&r.ws.path)))
        .max_by_key(|r| r.ws.path.len())
        .map(|r| PathBuf::from(&r.ws.path));
    let old_mount = PathBuf::from(&row.ws.mount);
    let old_nested = wt::path_key(&src) != wt::path_key(&old_mount);
    let new_mount = if enclosing.is_some() { ws.home.worktree_mount(row.ws.id) } else { dst.clone() };
    leave(row, ws);

    // The files first, then the row; the file moves are undone if the row cannot follow.
    if let Some(parent) = dst.parent() {
        match &enclosing {
            Some(boundary) => wt::make_dirs_below(boundary, parent)?,
            None => std::fs::create_dir_all(parent)?,
        }
    }
    let renamed = |from: &Path, to: &Path| std::fs::rename(from, to).map_err(|e| fatal(format!("cannot move '{}': {e}", from.display())));
    match (old_nested, enclosing.is_some()) {
        (false, false) => renamed(&old_mount, &new_mount)?,
        (false, true) => {
            renamed(&old_mount, &new_mount)?;
            if let Err(e) = wt::make_dir_link(&dst, &new_mount) {
                let _ = std::fs::rename(&new_mount, &old_mount);
                return Err(fatal(format!("cannot link '{}' to its mount: {e}", to.to_string_lossy())));
            }
        }
        (true, true) => {
            wt::remove_tree(&src)?;
            if let Err(e) = wt::make_dir_link(&dst, &new_mount) {
                let _ = wt::make_dir_link(&src, &old_mount);
                return Err(fatal(format!("cannot link '{}' to its mount: {e}", to.to_string_lossy())));
            }
        }
        (true, false) => {
            wt::remove_tree(&src)?;
            if let Err(e) = std::fs::rename(&old_mount, &new_mount) {
                let _ = wt::make_dir_link(&src, &old_mount);
                return Err(fatal(format!("cannot move '{shown}': {e}")));
            }
        }
    }
    if let Err(e) = workspaces::set_location(&ws.db, row.ws.id, text_of(&dst)?, text_of(&new_mount)?).await {
        // Put the files back where the row says they are.
        if enclosing.is_some() {
            let _ = wt::remove_tree(&dst);
        }
        if new_mount != old_mount {
            let _ = std::fs::rename(&new_mount, &old_mount);
        }
        if old_nested {
            let _ = wt::make_dir_link(&src, &old_mount);
        }
        return Err(e.into());
    }
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------------ convert

async fn convert_command(ctx: &Ctx, argv: &[OsString]) -> Result<ExitCode> {
    let sc = scan(argv, &[flag(None, "--plain")])?;
    let [typed] = sc.words.as_slice() else { return Err(usage_error()) };
    if !sc.has("--plain") {
        return Err(fatal("worktree convert needs --plain"));
    }
    let ws = ctx.ws()?;
    let all = rows(ws).await?;
    let row = locate(ctx, &all, typed)?;
    ext::workspace_factory().convert_plain(row).await.map_err(fatal)?;
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------------ command

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let argv = a.argv();
    let Some(sub) = argv.first().and_then(|s| s.to_str()) else {
        return Err(CliError::Usage(format!("error: need a subcommand\n{USAGE}")));
    };
    let rest = &argv[1..];
    ctx.ws()?;
    match sub {
        "add" => add_command(ctx, rest).await,
        "list" => list_command(ctx, rest).await,
        "remove" => remove_command(ctx, rest).await,
        "prune" => prune_command(ctx, rest).await,
        "lock" => lock_command(ctx, rest, true).await,
        "unlock" => lock_command(ctx, rest, false).await,
        "move" => move_command(ctx, rest).await,
        "convert" => convert_command(ctx, rest).await,
        other => Err(CliError::Usage(format!("error: unknown subcommand: `{other}'\n{USAGE}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    const ADD: [Spec; 4] = [flag(Some('f'), "--force"), valued(Some('b'), "-b"), flag(Some('q'), "--quiet"), valued(None, "--ttl")];

    #[test]
    fn options_words_clusters_and_values() {
        let sc = scan(&os(&["-fq", "-b", "topic", "../w", "master", "--ttl=7d"]), &ADD).unwrap();
        assert!(sc.has("--force") && sc.has("--quiet"));
        assert_eq!(sc.value("-b"), Some("topic"));
        assert_eq!(sc.value("--ttl"), Some("7d"));
        assert_eq!(sc.words, os(&["../w", "master"]));
        let attached = scan(&os(&["-btopic", "p"]), &ADD).unwrap();
        assert_eq!(attached.value("-b"), Some("topic"));
        let ended = scan(&os(&["--", "-b"]), &ADD).unwrap();
        assert_eq!(ended.words, os(&["-b"]));
        assert_eq!(scan(&os(&["-"]), &ADD).unwrap().words, os(&["-"]));
    }

    #[test]
    fn unknown_and_valueless_options_are_usage_errors() {
        for bad in [&["--nope"][..], &["-x"], &["--ttl"], &["-b"], &["--force=1"]] {
            assert!(matches!(scan(&os(bad), &ADD), Err(CliError::Usage(_))), "{bad:?}");
        }
    }

    #[test]
    fn lock_messages_match_git() {
        assert_eq!(
            lock_text("why", "remove", ""),
            "cannot remove a locked working tree, lock reason: why\nuse 'remove -f -f' to override or unlock first"
        );
        assert_eq!(lock_text("", "move", ""), "cannot move a locked working tree;\nuse 'move -f -f' to override or unlock first");
    }

    #[test]
    fn names_come_from_the_last_component() {
        assert_eq!(name_of("D:\\a\\b\\wt"), "wt");
        assert_eq!(name_of("/a/b/c"), "c");
        assert_eq!(slashed("D:\\a\\b"), "D:/a/b");
    }
}
