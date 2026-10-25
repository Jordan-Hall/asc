//! `checkout` (SPEC §11.2): switching branches and detaching HEAD through the same core as
//! `switch`, creating branches (`-b`/`-B`/`--orphan`), the DWIM of a remote-tracking branch, and
//! `checkout [<tree-ish>] [--] <paths>` which updates the index and the working tree at the paths.
//!
//! Which of the forms a command line is follows git's `parse_branchname_arg`: with `--` the words
//! before it name at most one tree-ish; without it the first word is a revision when it resolves
//! and a path otherwise, and a lone word that is neither may name a remote-tracking branch.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::git::tree_oid;
use asc_core::id::FileId;
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::RepoPath;
use futures::TryStreamExt;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::restore::{
    Spec, index_tree, root_of, stage_from_tree, sync_worktree, tool, worktree_paths,
};
use super::switch::{
    Cmd, Dest, Move, Opts, Rev, RevKind, abbrev_len, bad_branch_name, create_tracking, fatal,
    guess_remote, parse, perform, report_tracking, resolve_rev, valid_branch_name,
};
use crate::ext;
use crate::hooks_runner::{Hook, HookEnv, Outcome, Runner};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "checkout",
    disable_help_flag = true,
    disable_version_flag = true
)]
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
    pub(crate) fn argv(&self) -> Vec<OsString> {
        let mut all = self.rest.clone();
        if !self.after.is_empty() {
            all.push(OsString::from("--"));
            all.extend(self.after.iter().cloned());
        }
        all
    }
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "checkout",
    flags: &[
        "-b",
        "-B",
        "-d",
        "--detach",
        "-f",
        "--force",
        "-q",
        "--quiet",
        "-t",
        "--track",
        "--no-track",
        "--guess",
        "--no-guess",
        "--orphan",
        "--ignore-other-worktrees",
        "--progress",
        "--no-progress",
    ],
    placeholders: &[],
}];

const USAGE: &str = "usage: git checkout [<options>] <branch>\n   or: git checkout [<options>] [<branch>] -- <file>...\n\n    -b <branch>           create and checkout a new branch\n    -B <branch>           create/reset and checkout a branch\n    --orphan <new-branch> new unparented branch\n    -q, --quiet           suppress progress reporting\n    -d, --detach          detach HEAD at named commit\n    -t, --track[=(direct|inherit)]\n                          set branch tracking configuration\n    -f, --force           force checkout (throw away local modifications)\n    --guess               second guess 'git checkout <no-such-branch>'\n    --ignore-other-worktrees\n                          do not check if another worktree is holding the given ref";

/// The `tree` change that makes an entry.
fn change_of(e: &Entry) -> Result<Change> {
    Ok(match e.kind {
        Kind::Gitlink => Change::Gitlink { commit: e.git },
        kind => {
            Change::write(kind, FileId::from_bytes(e.id), e.size, e.git, None).map_err(fatal)?
        }
    })
}

/// `post-checkout <HEAD> <HEAD> 0`, whose exit status is the command's.
async fn post_checkout_files(ctx: &Ctx, ws: &WsCtx) -> Result<ExitCode> {
    let head = match ws.head_commit().await? {
        Some(c) => ws.engine.git_commit_of(c).await.map_err(fatal)?,
        None => crate::hooks_runner::null_oid(),
    };
    let runner = Runner::for_ctx(ctx)?;
    let hook = Hook::PostCheckout {
        old: head,
        new: head,
        branch: false,
    };
    match runner
        .run(hook, &HookEnv::of(ctx, "checkout"))
        .await
        .map_err(fatal)?
    {
        Outcome::Exit(code) if code != 0 => Ok(ExitCode::from(u8::try_from(code).unwrap_or(1))),
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// `checkout [<source>] [--] <paths>`: the working tree and the index at the paths take the
/// source's entries (the index's, without a source).
async fn checkout_paths(
    ctx: &Ctx,
    ws: &WsCtx,
    source: Option<Rev>,
    paths: &[String],
    quiet: bool,
    count: bool,
) -> Result<ExitCode> {
    let items: Vec<OsString> = paths.iter().map(OsString::from).collect();
    let head = ws.head_commit().await?;
    let head_root = root_of(ws, head).await?;
    let (index, unmerged) = index_tree(ws, head_root).await?;
    let spec = Spec::new(ctx, &items)?;
    let unmatched = |bad: String| {
        CliError::Raw(
            format!("error: pathspec '{bad}' did not match any file(s) known to git"),
            1,
        )
    };
    let mut code = 0u8;
    let (wanted, shown) = match source {
        Some(rev) => {
            let changeset = ws.engine.get_changeset(rev.commit).await?;
            let src_root = changeset.root;
            if let Some(bad) = spec.first_unmatched(ws, &[src_root]).await? {
                return Err(unmatched(bad));
            }
            // Only what the source has under the pathspec replaces the index's entries.
            let mut changes = Vec::new();
            let mut walk = ws.engine.walk(src_root, Some(&spec.spec));
            while let Some((path, entry)) = walk.try_next().await? {
                if entry.kind != Kind::Dir {
                    changes.push((path, change_of(&entry)?));
                }
            }
            drop(walk);
            let wanted = ws.engine.apply(index, &changes).await?;
            let tree = ws.engine.get_tree(src_root).await?;
            let abbrev = tree_oid(tree.as_ref()).to_string()[..abbrev_len(ctx)].to_owned();
            stage_from_tree(ws, index, &unmerged, wanted, head_root, &spec).await?;
            (wanted, format!("from {abbrev}"))
        }
        None => {
            if let Some(bad) = spec.first_unmatched(ws, &[index]).await? {
                return Err(unmatched(bad));
            }
            (index, "from the index".to_owned())
        }
    };
    let mut files: BTreeSet<RepoPath> = worktree_paths(ws, index, wanted, &spec).await?;
    if wanted == index {
        for raw in &unmerged {
            let path = RepoPath::new(raw.clone()).map_err(fatal)?;
            if spec.spec.matches(&path) {
                eprintln!("error: path '{}' is unmerged", String::from_utf8_lossy(raw));
                files.remove(&path);
                code = 1;
            }
        }
    }
    let written = files.len();
    if !files.is_empty() {
        ext::op_sink().snapshot_wc();
        let files: Vec<RepoPath> = files.into_iter().collect();
        sync_worktree(ctx, wanted, &files, false).await?;
    }
    if count && !quiet {
        let noun = if written == 1 { "path" } else { "paths" };
        eprintln!("Updated {written} {noun} {shown}");
    }
    let hook = post_checkout_files(ctx, ws).await?;
    Ok(if code != 0 {
        ExitCode::from(code)
    } else {
        hook
    })
}

/// Where HEAD goes for `rev`: its branch when it is one (unless `--detach`), else a detached HEAD.
fn dest_of(rev: &Rev, detach: bool) -> Dest {
    match &rev.kind {
        RevKind::Branch(name) if !detach => Dest::Existing {
            branch: name.clone(),
            tip: Some(rev.commit),
        },
        _ => Dest::Detach { commit: rev.commit },
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let p = parse(&a.argv(), Cmd::Checkout, USAGE)?;
    let o: Opts = p.opts.clone();
    let guess = o
        .guess
        .or_else(|| ctx.config.bool("checkout.guess"))
        .unwrap_or(true);
    let named = [
        o.create.as_ref(),
        o.force_create.as_ref(),
        o.orphan.as_ref(),
    ];
    if named.iter().filter(|n| n.is_some()).count() > 1 {
        return Err(CliError::Fatal(
            "options '-b', '-B' and '--orphan' cannot be used together".into(),
        ));
    }
    let new_branch = named.into_iter().flatten().next().cloned();
    if o.detach && new_branch.is_some() {
        return Err(CliError::Fatal(
            "options '--detach' and '-b' cannot be used together".into(),
        ));
    }

    // A new branch: `-b`, `-B` or `--orphan`, with at most a start point.
    if let Some(name) = new_branch {
        if p.after.as_ref().is_some_and(|a| !a.is_empty()) || p.words.len() > 1 {
            return Err(CliError::Fatal(format!(
                "Cannot update paths and switch to branch '{name}' at the same time."
            )));
        }
        if !valid_branch_name(&name) {
            return Err(bad_branch_name(ctx, &name));
        }
        if o.orphan.is_some() {
            if !p.words.is_empty() {
                return Err(CliError::Fatal(
                    "asc: 'checkout --orphan' with a start point is not supported".into(),
                ));
            }
            let mv = Move {
                dest: Dest::Orphan {
                    branch: name.clone(),
                },
                opts: o,
                typed: name,
                keep_tree: true,
            };
            return perform(ctx, mv).await;
        }
        let force = o.force_create.is_some();
        let start = match p.words.first() {
            Some(w) => Some(resolve_rev(ctx, ws, w).await?.ok_or_else(|| {
                CliError::Fatal(format!(
                    "'{w}' is not a commit and a branch '{name}' cannot be created from it"
                ))
            })?),
            None => ws.head_commit().await?.map(|c| Rev {
                label: "HEAD".into(),
                commit: c,
                kind: RevKind::Other,
            }),
        };
        let mv = Move {
            dest: Dest::Create {
                branch: name.clone(),
                start,
                force,
            },
            opts: o,
            typed: name,
            keep_tree: false,
        };
        return perform(ctx, mv).await;
    }

    // Words around `--`.
    if let Some(after) = &p.after {
        if p.words.len() > 1 {
            return Err(CliError::Fatal(format!(
                "only one reference expected, {} given.",
                p.words.len()
            )));
        }
        let source = match p.words.first() {
            Some(w) => Some(
                resolve_rev(ctx, ws, w)
                    .await?
                    .ok_or_else(|| CliError::Fatal(format!("invalid reference: {w}")))?,
            ),
            None => None,
        };
        if after.is_empty() {
            return match source {
                Some(rev) => switch_rev(ctx, rev, p.words[0].clone(), o).await,
                None => Ok(ExitCode::SUCCESS),
            };
        }
        return checkout_paths(ctx, ws, source, after, o.quiet, false).await;
    }

    // `--detach [<commit>]`.
    if o.detach {
        let word = p.words.first().map_or("HEAD", String::as_str);
        if p.words.len() > 1 {
            return Err(CliError::Fatal(format!(
                "only one reference expected, {} given.",
                p.words.len()
            )));
        }
        // A word that names no commit is taken for a path, which `--detach` refuses.
        let rev = resolve_rev(ctx, ws, word).await?.ok_or_else(|| {
            CliError::Fatal(format!(
                "{} checkout: --detach does not take a path argument '{word}'",
                tool(ctx)
            ))
        })?;
        let mv = Move {
            dest: Dest::Detach { commit: rev.commit },
            opts: o,
            typed: word.to_owned(),
            keep_tree: false,
        };
        return perform(ctx, mv).await;
    }

    // No words: report where the branch stands.
    let Some(first) = p.words.first() else {
        if let asc_git::projection::Head::Branch(b) = ws.projection.read_head()? {
            report_tracking(ctx, ws, &b).await?;
        }
        return Ok(ExitCode::SUCCESS);
    };
    // `checkout HEAD` stays where it is.
    if p.words.len() == 1
        && matches!(first.as_str(), "HEAD" | "@")
        && matches!(
            ws.projection.read_head()?,
            asc_git::projection::Head::Branch(_)
        )
    {
        return Ok(ExitCode::SUCCESS);
    }
    match resolve_rev(ctx, ws, first).await? {
        Some(rev) if p.words.len() == 1 => switch_rev(ctx, rev, first.clone(), o).await,
        Some(rev) => checkout_paths(ctx, ws, Some(rev), &p.words[1..], o.quiet, true).await,
        None => {
            if p.words.len() == 1
                && guess
                && valid_branch_name(first)
                && let Some(remote) = guess_remote(ctx, ws, first).await?
            {
                return create_tracking(ctx, first, remote, o).await;
            }
            checkout_paths(ctx, ws, None, &p.words, o.quiet, true).await
        }
    }
}

/// Switches to what `rev` names: its branch, or a detached HEAD at its commit.
async fn switch_rev(ctx: &Ctx, rev: Rev, typed: String, o: Opts) -> Result<ExitCode> {
    let mv = Move {
        dest: dest_of(&rev, o.detach),
        opts: o,
        typed,
        keep_tree: false,
    };
    perform(ctx, mv).await
}
