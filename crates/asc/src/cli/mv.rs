//! `mv`: renames tracked files or directories in the work tree and the staged tree, and records
//! each rename in `ws_ops` for the commit's `copy_from` (SPEC §9.7 step 5).

use std::ffi::OsString;
use std::fs;
use std::process::ExitCode;

use asc_core::path::RepoPath;
use asc_state::state::ws_ops::{self, Renames};
use asc_wc::SafeRoot;

use super::add::spec::{Spec, repo_path};
use super::add::target::{Edit, Staged, Target};
use super::add::cwd_prefix;
use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "mv", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Overwrite an existing destination.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    /// Skip sources that cannot be moved instead of failing.
    #[arg(short = 'k')]
    pub skip_errors: bool,
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
    /// `<source>… <destination>`.
    #[arg(required = true, num_args = 2..)]
    pub paths: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "mv",
    flags: &["-f", "--force", "-n", "--dry-run", "-k", "-v", "--verbose"],
    placeholders: &[],
}];

fn text(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() { name.to_vec() } else { [dir, b"/", name].concat() }
}

fn base_name(path: &[u8]) -> &[u8] {
    path.iter().rposition(|&c| c == b'/').map_or(path, |i| &path[i + 1..])
}

/// Records `from` -> `to`, folding a chain of renames into one.
fn record(renames: &mut Renames, from: &[u8], to: &[u8]) {
    renames.retain(|(_, t)| t != to);
    match renames.iter().position(|(_, t)| t == from) {
        Some(i) if renames[i].0 == to => {
            renames.remove(i);
        }
        Some(i) => renames[i].1 = to.to_vec(),
        None => renames.push((from.to_vec(), to.to_vec())),
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel();
    let prefix = cwd_prefix(ctx, ws);
    let gate = SafeRoot::new(root);
    let (dest_arg, source_args) = a.paths.split_last().ok_or_else(|| CliError::Usage("usage: git mv [-v] [-f] [-n] [-k] <source> <destination>".into()))?;
    let dest = repo_path(dest_arg, &prefix, root)?;
    let dest_is_dir = path_of(&gate, &dest).is_some_and(|p| fs::metadata(p).is_ok_and(|m| m.is_dir()));
    if source_args.len() > 1 && !dest_is_dir {
        return Err(CliError::Fatal(format!("destination '{}' is not a directory", text(&dest))));
    }
    let target = Target::open(ctx).await?;
    let mut edits = Vec::new();
    let mut moves: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut files: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for arg in source_args {
        let src = repo_path(arg, &prefix, root)?;
        let dst = if dest_is_dir { join(&dest, base_name(&src)) } else { dest.clone() };
        let fail = |what: &str| {
            CliError::Fatal(format!("{what}, source={}, destination={}", text(&src), text(&dst)))
        };
        if a.dry_run {
            println!("Checking rename of '{}' to '{}'", text(&src), text(&dst));
        }
        let checked = check(ctx, &gate, &target, &src, &dst, a.force, &fail).await;
        let tracked = match checked {
            Ok(t) => t,
            Err(_) if a.skip_errors => continue,
            Err(e) => return Err(e),
        };
        for (from, to) in tracked {
            files.push((from, to));
        }
        moves.push((src, dst));
    }
    if a.verbose || a.dry_run {
        for (src, dst) in &moves {
            println!("Renaming {} to {}", text(src), text(dst));
        }
    }
    if a.dry_run {
        return Ok(ExitCode::SUCCESS);
    }
    for (src, dst) in &moves {
        let renamed = match (path_of(&gate, src), path_of(&gate, dst)) {
            (Some(from), Some(to)) => fs::rename(from, to),
            // Behind a link, or a name this system cannot hold: outside the work tree.
            _ => Err(std::io::ErrorKind::PermissionDenied.into()),
        };
        renamed.map_err(|e| {
            // git prints the C library's text, not Windows' own.
            let why = match e.kind() {
                std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
                std::io::ErrorKind::PermissionDenied => "Permission denied".to_owned(),
                _ => e.to_string(),
            };
            CliError::Fatal(format!("renaming '{}' failed: {why}", text(src)))
        })?;
    }
    let mut renames = ws_ops::pending_renames(&ws.db, ws.workspace.ws.id).await?;
    for (from, to) in &files {
        let (Ok(old), Ok(new)) = (RepoPath::new(from.clone()), RepoPath::new(to.clone())) else {
            continue;
        };
        let Staged::Leaf { leaf, ita } = target.staged(ws, &old).await? else { continue };
        edits.push(Edit::Drop(old));
        edits.push(Edit::Put { path: new, leaf, ita });
        record(&mut renames, from, to);
    }
    ws_ops::set_pending_renames(&ws.db, ws.workspace.ws.id, &renames).await?;
    let mut target = target;
    target.apply(ctx, edits).await?;
    Ok(ExitCode::SUCCESS)
}

/// The file system path of `repo`, or `None` when it is invalid, beyond a link or unrepresentable.
fn path_of(gate: &SafeRoot, repo: &[u8]) -> Option<std::path::PathBuf> {
    gate.resolve(&RepoPath::new(repo.to_vec()).ok()?)
}

/// Checks one rename; the tracked `(old, new)` file paths it moves.
async fn check(
    ctx: &Ctx,
    gate: &SafeRoot,
    target: &Target,
    src: &[u8],
    dst: &[u8],
    force: bool,
    fail: &dyn Fn(&str) -> CliError,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel();
    let meta = path_of(gate, src)
        .and_then(|p| fs::symlink_metadata(p).ok())
        .ok_or_else(|| fail("bad source"))?;
    if src.is_empty() {
        return Err(fail("bad source"));
    }
    let mut tracked = Vec::new();
    if meta.is_dir() {
        if dst.len() > src.len() && dst.starts_with(src) && dst[src.len()] == b'/' {
            return Err(fail("can not move directory into itself"));
        }
        let spec = Spec::parse(
            &[OsString::from(format!(":(top,literal){}", text(src)))],
            "",
            root,
        )?;
        target
            .tracked(ws, &spec, |path, t| {
                if matches!(t.staged, Staged::Leaf { .. }) {
                    let rest = &path.as_bytes()[src.len()..];
                    tracked.push((path.as_bytes().to_vec(), [dst, rest].concat()));
                }
                Ok(())
            })
            .await?;
        if tracked.is_empty() {
            return Err(fail("source directory is empty"));
        }
    } else {
        let path = RepoPath::new(src.to_vec()).map_err(|_| fail("bad source"))?;
        match target.staged(ws, &path).await? {
            Staged::Leaf { .. } => tracked.push((src.to_vec(), dst.to_vec())),
            Staged::Unmerged => return Err(fail("conflicted")),
            Staged::Absent => return Err(fail("not under version control")),
        }
    }
    let dst_abs = path_of(gate, dst).ok_or_else(|| fail("destination is beyond a symbolic link"))?;
    if fs::symlink_metadata(dst_abs).is_ok() && !force {
        return Err(fail("destination exists"));
    }
    Ok(tracked)
}
