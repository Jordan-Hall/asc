//! `rm`: removes tracked paths from the staged tree and, unless `--cached`, from the work tree.
//! Without `-f` it refuses a file whose removal would lose content, with git's checks and text.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::process::ExitCode;

use asc_core::path::RepoPath;
use asc_wc::SafeRoot;
use asc_wc::status::Eol;

use super::add::attrs::Attrs;
use super::add::spec::Spec;
use super::add::target::{Edit, Staged, Target, Tracked};
use super::add::{self, convert, cwd_prefix, workspace_attrs};
use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "rm", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Allow recursive removal of directories.
    #[arg(short = 'r')]
    pub recursive: bool,
    /// Remove even when the file has changes git would warn about.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// Unstage only; keep the work tree file.
    #[arg(long = "cached")]
    pub cached: bool,
    #[arg(long = "ignore-unmatch")]
    pub ignore_unmatch: bool,
    pub pathspec: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "rm",
    flags: &[
        "-r", "-f", "--force", "-n", "--dry-run", "-q", "--quiet", "--cached", "--ignore-unmatch",
    ],
    placeholders: &[],
}];

/// The files `rm` refuses, grouped as git groups them.
#[derive(Default)]
struct Refused {
    staged: Vec<RepoPath>,
    cached: Vec<RepoPath>,
    local: Vec<RepoPath>,
}

impl Refused {
    fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.cached.is_empty() && self.local.is_empty()
    }

    fn print(&self, ctx: &Ctx) {
        let show = |files: &[RepoPath], one: &str, many: &str, hint: &str| {
            if files.is_empty() {
                return;
            }
            let mut text = String::from(if files.len() == 1 { one } else { many });
            for f in files {
                text.push_str(&format!("\n    {f}"));
            }
            if add::advice(ctx, "rmHints") {
                text.push_str(hint);
            }
            eprintln!("error: {text}");
        };
        let keep = "\n(use --cached to keep the file, or -f to force removal)";
        show(
            &self.staged,
            "the following file has staged content different from both the\nfile and the HEAD:",
            "the following files have staged content different from both the\nfiles and the HEAD:",
            "\n(use -f to force removal)",
        );
        show(
            &self.cached,
            "the following file has changes staged in the index:",
            "the following files have changes staged in the index:",
            keep,
        );
        show(
            &self.local,
            "the following file has local modifications:",
            "the following files have local modifications:",
            keep,
        );
    }
}

/// Whether the work tree file at `path` differs from what is staged.
fn locally_modified(gate: &SafeRoot, attrs: &mut Attrs, path: &RepoPath, t: &Tracked) -> io::Result<bool> {
    let Staged::Leaf { leaf, .. } = &t.staged else { return Ok(false) };
    let Some(abs) = gate.resolve(path) else { return Ok(false) };
    let Ok(meta) = fs::symlink_metadata(&abs) else { return Ok(false) };
    if meta.is_dir() {
        return Ok(false);
    }
    let git = if meta.file_type().is_symlink() {
        convert::bytes_oid(&convert::link_target(&abs)?)?
    } else {
        convert::blob_oid(&abs, attrs.lookup(path).map_or(Eol::Keep, |a| a.eol))?
    };
    Ok(git != leaf.git)
}

/// Removes the file and then the directories it leaves empty, as git does. A path under a linked
/// ancestor is outside the work tree: nothing is deleted there, and the cleanup stops at it.
fn remove_from_work_tree(gate: &SafeRoot, path: &RepoPath) -> io::Result<()> {
    if gate.remove_file(path)? {
        gate.remove_empty_parents(path);
    }
    Ok(())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if a.pathspec.is_empty() {
        return Err(CliError::Fatal("No pathspec was given. Which files should I remove?".into()));
    }
    let spec = Spec::parse(&a.pathspec, &cwd_prefix(ctx, ws), ws.projection.toplevel())?;
    let mut target = Target::open(ctx).await?;
    let gate = SafeRoot::new(ws.projection.toplevel());
    let mut seen = vec![false; spec.includes().len()];
    let mut hits: Vec<(RepoPath, Tracked)> = Vec::new();
    target
        .tracked(ws, &spec, |path, t| {
            spec.mark(path.as_bytes(), &mut seen);
            hits.push((path.clone(), t));
            Ok(())
        })
        .await?;
    if !a.ignore_unmatch
        && let Some((item, _)) = spec.includes().iter().zip(&seen).find(|(_, hit)| !**hit)
    {
        return Err(CliError::Fatal(format!("pathspec '{}' did not match any files", item.raw())));
    }
    if !a.recursive {
        let below = |path: &RepoPath| {
            let key = path.as_bytes();
            spec.includes().iter().find(|i| i.literal().is_some_and(|l| key != l && i.matches(key)))
        };
        if let Some(dir) = hits.iter().find_map(|(p, _)| below(p)) {
            return Err(CliError::Fatal(format!("not removing '{}' recursively without -r", dir.raw())));
        }
    }

    if !a.force {
        let mut attrs = workspace_attrs(ctx, ws)?;
        let mut refused = Refused::default();
        for (path, t) in &hits {
            let Staged::Leaf { leaf, ita: false } = &t.staged else { continue };
            let local = locally_modified(&gate, &mut attrs, path, t)
                .map_err(|e| CliError::Fatal(format!("{path}: {e}")))?;
            let staged = t.head.as_ref().is_none_or(|h| h.git != leaf.git || h.kind != leaf.kind);
            match (local, staged) {
                (true, true) => refused.staged.push(path.clone()),
                (false, true) if !a.cached => refused.cached.push(path.clone()),
                (true, false) if !a.cached => refused.local.push(path.clone()),
                _ => {}
            }
        }
        if !refused.is_empty() {
            refused.print(ctx);
            return Ok(ExitCode::from(1));
        }
    }

    if !a.quiet {
        for (path, _) in &hits {
            println!("rm '{path}'");
        }
    }
    if a.dry_run {
        return Ok(ExitCode::SUCCESS);
    }
    if !a.cached {
        for (path, _) in &hits {
            remove_from_work_tree(&gate, path)
                .map_err(|e| CliError::Fatal(format!("git rm: '{path}': {e}")))?;
        }
    }
    target.apply(ctx, hits.into_iter().map(|(p, _)| Edit::Drop(p)).collect()).await?;
    Ok(ExitCode::SUCCESS)
}
