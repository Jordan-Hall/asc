//! `commit` (SPEC §9.7): stage rows to a changeset, in git's hook order. Step 0 snapshots set 0;
//! `-a` and path lists stage first; `pre-commit` runs with `GIT_INDEX_FILE` naming the stage set
//! in use; the stage is re-read after it; the message hooks run; the build reads no content; a
//! failure before the ref moves restores set 0 from the snapshot, which is dropped in every case.

pub(crate) mod ident;
pub(super) mod message;
pub(crate) mod stagebuild;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use asc_core::change::RewriteOp;
use asc_core::git::{ObjectKind, export_commit, tree_oid};
use asc_core::id::{ChangesetId, GitOid, RefTarget, TreeId};
use asc_core::object::Change;
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_engine::derive::{Gen, index_commit_git};
use asc_engine::diff::format::write_summary;
use asc_engine::diff::{DiffCtx, DiffOptions, NoAttributes, RenameOptions, write_shortstat};
use asc_engine::history::HistoryApi;
use asc_git::projection::Head;
use asc_state::state::stage::{self, SetId, StageSet};
use asc_state::state::tables::{cs_gen, ws_ops};
use asc_state::state::{git_map, stage_sets};
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate};

use self::message::Cleanup;
use super::common::{CliEngine, CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::format::date::Mode as DateMode;
use super::seqstate::{self, Sequence};
use super::status;
use crate::ext::{self, OpId};
use crate::hooks_runner::{Gate, Hook, HookEnv, MsgSource, RewriteKind, Runner};
use crate::native::NativeSpec;

/// `git commit` arguments.
#[derive(clap::Parser, Debug)]
#[command(name = "commit", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(short = 'm', long = "message")]
    message: Vec<String>,
    #[arg(short = 'F', long = "file")]
    file: Option<PathBuf>,
    #[arg(short = 'a', long = "all")]
    all: bool,
    #[arg(long)]
    amend: bool,
    #[arg(long)]
    fixup: Option<String>,
    #[arg(long)]
    squash: Option<String>,
    #[arg(short = 'n', long = "no-verify")]
    no_verify: bool,
    #[arg(long = "allow-empty")]
    allow_empty: bool,
    #[arg(long = "allow-empty-message")]
    allow_empty_message: bool,
    #[arg(short = 'o', long)]
    only: bool,
    #[arg(short = 'i', long)]
    include: bool,
    #[arg(long)]
    author: Option<String>,
    #[arg(long)]
    date: Option<String>,
    #[arg(short = 'e', long)]
    edit: bool,
    #[arg(long = "no-edit")]
    no_edit: bool,
    #[arg(short = 'v', long)]
    verbose: bool,
    #[arg(short = 'S', long = "gpg-sign", num_args = 0..=1, require_equals = true)]
    gpg_sign: Option<Option<String>>,
    #[arg(long = "no-gpg-sign")]
    no_gpg_sign: bool,
    #[arg(short = 'q', long)]
    quiet: bool,
    #[arg(long)]
    cleanup: Option<String>,
    #[arg(num_args = 0.., trailing_var_arg = false)]
    paths: Vec<OsString>,
}

/// The flags this command serves.
pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "commit",
    flags: &[
        "-m",
        "--message",
        "-F",
        "--file",
        "-a",
        "--all",
        "--amend",
        "--fixup",
        "--squash",
        "-n",
        "--no-verify",
        "--allow-empty",
        "--allow-empty-message",
        "-o",
        "--only",
        "-i",
        "--include",
        "--author",
        "--date",
        "-e",
        "--edit",
        "--no-edit",
        "-v",
        "--verbose",
        "-S",
        "--gpg-sign",
        "--no-gpg-sign",
        "-q",
        "--quiet",
        "--cleanup",
    ],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// How the flow ended when it did not fail.
enum Flow {
    Committed,
    /// Stopped by a hook, the editor or an empty message, with git's exit code.
    Abort(u8),
}

/// What step 0 and step 1 created, so the cleanup can undo it.
struct Session {
    snapshot: SetId,
    temp: Option<(SetId, PathBuf)>,
}

/// Which paths the commit takes from the work tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Everything staged in set 0.
    Staged,
    /// `<paths>` / `--only`: the named paths from the work tree, nothing else.
    Only,
    /// `-i`: the staged changes plus the named paths.
    Include,
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let ws_id = ws.workspace.ws.id;
    let runner = Runner::for_ctx(ctx)?;
    let before = ws.head_commit().await?;
    let snapshot = stage::snapshot(&ws.db, ws_id).await?;
    let mut session = Session {
        snapshot,
        temp: None,
    };
    let result = flow(ctx, ws, &args, &runner, &mut session).await;
    let moved = ws.head_commit().await.map(|h| h != before).unwrap_or(false);
    if !matches!(result, Ok(Flow::Committed)) && !moved {
        // Step 0: the commit stopped before step 6, so set 0 goes back to what it was.
        let _ = stage::restore(&ws.db, ws_id, session.snapshot).await;
    }
    let _ = stage_sets::drop_set(&ws.db, ws_id, session.snapshot).await;
    if let Some((set, file)) = session.temp.take() {
        let _ = stage_sets::drop_set(&ws.db, ws_id, set).await;
        let _ = std::fs::remove_file(file);
    }
    Ok(match result? {
        Flow::Committed => ExitCode::SUCCESS,
        Flow::Abort(code) => ExitCode::from(code),
    })
}

/// The commits named by `MERGE_HEAD`, oldest line first.
async fn merge_parents(ws: &WsCtx, gitdir: &Path) -> Result<Vec<ChangesetId>> {
    let Ok(text) = std::fs::read_to_string(gitdir.join("MERGE_HEAD")) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let oid: GitOid = line
            .trim()
            .parse()
            .map_err(|_| fatal("asc: MERGE_HEAD holds a bad object id"))?;
        let asc = git_map::git_to_asc(&ws.db, ws.repo.key, oid, ObjectKind::Commit)
            .await?
            .ok_or_else(|| fatal(format!("asc: MERGE_HEAD names an unknown commit {oid}")))?;
        out.push(ChangesetId::from_bytes(asc));
    }
    Ok(out)
}

/// The initial message and where it came from.
struct Initial {
    text: Vec<u8>,
    source: Option<MsgSource>,
    /// Whether an editor opens without `-e`.
    edit_by_default: bool,
}

async fn initial_message(
    ctx: &Ctx,
    ws: &WsCtx,
    args: &Args,
    gitdir: &Path,
    amended: Option<(ChangesetId, &[u8])>,
    merging: bool,
    head_name: Option<String>,
) -> Result<Initial> {
    let m = args.message.join("\n\n").into_bytes();
    if !args.message.is_empty() && args.file.is_some() {
        return Err(fatal("options '-m' and '-F' cannot be used together"));
    }
    let target_subject = |spec: &str| {
        // `HEAD` is not a stored ref: name the branch (or the detached commit) instead.
        let spec = match (spec.strip_prefix("HEAD"), &head_name) {
            (Some(rest), Some(name)) => format!("{name}{rest}"),
            _ => spec.to_owned(),
        };
        async move {
            let found = HistoryApi::resolve(&ws.engine, std::slice::from_ref(&spec))
                .await
                .map_err(|e| fatal(e.to_string()))?;
            let Some(Some(id)) = found.first().copied() else {
                return Err(fatal(format!("could not lookup commit '{spec}'")));
            };
            let cs = ws.engine.get_changeset(id).await?;
            Ok::<_, CliError>(message::subject(&cs.message))
        }
    };
    if let Some(spec) = &args.fixup {
        let mut text = format!("fixup! {}\n", target_subject(spec).await?).into_bytes();
        if !m.is_empty() {
            text.extend_from_slice(b"\n");
            text.extend_from_slice(&m);
        }
        return Ok(Initial {
            text,
            source: Some(MsgSource::Message),
            edit_by_default: false,
        });
    }
    if let Some(spec) = &args.squash {
        let mut text = format!("squash! {}\n", target_subject(spec).await?).into_bytes();
        if !m.is_empty() {
            text.extend_from_slice(b"\n");
            text.extend_from_slice(&m);
        }
        return Ok(Initial {
            text,
            source: Some(MsgSource::Message),
            edit_by_default: m.is_empty(),
        });
    }
    if !args.message.is_empty() {
        // git completes the last line of `-m` text, so COMMIT_EDITMSG ends in a newline.
        let mut m = m;
        if !m.is_empty() && !m.ends_with(b"\n") {
            m.push(b'\n');
        }
        return Ok(Initial {
            text: m,
            source: Some(MsgSource::Message),
            edit_by_default: false,
        });
    }
    if let Some(file) = &args.file {
        let read = if file == Path::new("-") {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf).map(|_| buf)
        } else {
            std::fs::read(file)
        };
        let text = read.map_err(|e| {
            fatal(format!(
                "could not read log file '{}': {}",
                file.display(),
                io_reason(&e)
            ))
        })?;
        return Ok(Initial {
            text,
            source: Some(MsgSource::Message),
            edit_by_default: false,
        });
    }
    if let Some((id, message)) = amended {
        let sha = ws
            .engine
            .git_commit_of(id)
            .await
            .map_err(|e| fatal(e.to_string()))?;
        return Ok(Initial {
            text: message.to_vec(),
            source: Some(MsgSource::Commit(sha)),
            edit_by_default: true,
        });
    }
    if merging {
        let text =
            std::fs::read(gitdir.join("MERGE_MSG")).unwrap_or_else(|_| b"Merge commit\n".to_vec());
        return Ok(Initial {
            text,
            source: Some(MsgSource::Merge),
            edit_by_default: true,
        });
    }
    if let Ok(text) = std::fs::read(gitdir.join("SQUASH_MSG")) {
        return Ok(Initial {
            text,
            source: Some(MsgSource::Squash),
            edit_by_default: true,
        });
    }
    if let Some(template) = ctx.config.path("commit.template") {
        let text = std::fs::read(&template).map_err(|e| {
            fatal(format!(
                "could not read '{}': {}",
                template.display(),
                io_reason(&e)
            ))
        })?;
        return Ok(Initial {
            text,
            source: Some(MsgSource::Template),
            edit_by_default: true,
        });
    }
    Ok(Initial {
        text: Vec::new(),
        source: None,
        edit_by_default: true,
    })
}

/// Step 1 for `<paths>` / `--only` / `-i`: registers the temporary set and renders its file.
async fn temp_set(
    ws: &WsCtx,
    scope: Scope,
    spec_args: &[String],
    prefix: &str,
    head_root: TreeId,
    index_file: &Path,
) -> Result<SetId> {
    let ws_id = ws.workspace.ws.id;
    let key = index_file.display().to_string();
    if let Some(old) = stage_sets::lookup_by_path(&ws.db, &key).await? {
        stage_sets::drop_set(&ws.db, old.ws, old.set).await?;
    }
    let staged = stage::overlay(&ws.db, ws_id, StageSet::Default).await?;
    let spec = stagebuild::pathspec(prefix, spec_args)?;
    let matched = stagebuild::tracked_matches(ws, head_root, &staged, spec.as_ref()).await?;
    if matched.is_empty() {
        let first = spec_args.first().cloned().unwrap_or_default();
        return Err(CliError::Raw(
            format!("error: pathspec '{first}' did not match any file(s) known to git"),
            1,
        ));
    }
    let mut rows = if scope == Scope::Include {
        staged.clone()
    } else {
        Vec::new()
    };
    for (path, kind) in matched {
        let row = stagebuild::row_of_path(ws, &path, kind).await?;
        rows.retain(|r| r.path != row.path);
        rows.push(row);
    }
    let set = stage_sets::register(&ws.db, ws_id, &key).await?;
    stage::upsert(&ws.db, ws_id, StageSet::Temp(set), &rows).await?;
    let changes = stagebuild::rows_to_changes(ws, head_root, &rows).await?;
    let root = ws.engine.apply(head_root, &changes).await?;
    stagebuild::write_index_file(ws, root, index_file).await?;
    Ok(set)
}

async fn flow(
    ctx: &Ctx,
    ws: &WsCtx,
    a: &Args,
    runner: &Runner,
    session: &mut Session,
) -> Result<Flow> {
    let ws_id = ws.workspace.ws.id;
    let engine: &CliEngine = &ws.engine;
    let gitdir = ws.projection.git_dir().to_owned();
    let head = ws.head_commit().await?;
    let head_ref = ws.projection.read_head()?;
    let head_cs = match head {
        Some(h) => Some(engine.get_changeset(h).await?),
        None => None,
    };
    let head_root = head_cs
        .as_ref()
        .map_or_else(CliEngine::empty_tree_id, |c| c.root);
    let merge = merge_parents(ws, &gitdir).await?;
    // Hooks see git ids, so every commit a ref update names has its git map row first.
    let sink = git_map::RepoGitMap {
        db: ws.db.clone(),
        repo: ws.repo.key,
    };
    for &c in head.iter().chain(merge.iter()) {
        index_commit_git(engine, c, &sink)
            .await
            .map_err(|e| fatal(e.to_string()))?;
    }

    let paths: Vec<String> = a
        .paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if a.only && a.include && a.all {
        return Err(fatal(
            "options '-i/--include', '-o/--only', and '-a/--all' cannot be used together",
        ));
    }
    if a.all && !paths.is_empty() {
        return Err(fatal(format!(
            "paths '{} ...' with -a does not make sense",
            paths[0]
        )));
    }
    if paths.is_empty() && (a.only || a.include) {
        return Err(fatal("No paths with --include/--only does not make sense."));
    }
    if a.amend && head.is_none() {
        return Err(fatal("You have nothing to amend."));
    }
    if a.amend && !merge.is_empty() {
        return Err(fatal("You are in the middle of a merge -- cannot amend."));
    }
    if !paths.is_empty() && !merge.is_empty() {
        return Err(fatal("cannot do a partial commit during a merge."));
    }
    let scope = match (paths.is_empty(), a.include) {
        (true, _) => Scope::Staged,
        (false, true) => Scope::Include,
        (false, false) => Scope::Only,
    };
    let cleanup_mode = match a
        .cleanup
        .as_deref()
        .or(ctx.config.get("commit.cleanup").as_deref())
    {
        None => Cleanup::Default,
        Some(text) => {
            Cleanup::parse(text).ok_or_else(|| fatal(format!("Invalid cleanup mode {text}")))?
        }
    };

    let mut set0 = StageSet::Default;
    let mut index_file = gitdir.join("index");
    if let Some(foreign) = ctx.env.foreign_index(&gitdir) {
        let info = stage_sets::lookup_by_path(&ws.db, &foreign.display().to_string())
            .await?
            .filter(|i| i.ws == ws_id)
            .ok_or_else(|| fatal("asc: GIT_INDEX_FILE names an index asc does not know; stage with 'git add' first"))?;
        set0 = StageSet::Temp(info.set);
        index_file = foreign.to_owned();
    }

    let env_base = HookEnv::of(ctx, "commit");
    let prefix = env_base.prefix.clone();

    // Step 1: `-a` stages the stat-dirty tracked files; a path list builds the temporary set.
    if a.all {
        let rows = stagebuild::stat_dirty_rows(ws, &ctx.config).await?;
        stage::upsert(&ws.db, ws_id, set0, &rows).await?;
    }
    let use_set = if scope == Scope::Staged {
        set0
    } else {
        let file = gitdir.join("asc-commit-index");
        let set = temp_set(ws, scope, &paths, &prefix, head_root, &file).await?;
        session.temp = Some((set, file.clone()));
        index_file = file;
        StageSet::Temp(set)
    };

    // The message plan decides whether hooks see GIT_EDITOR=:.
    let head_name = match &head_ref {
        Head::Branch(b) => Some(format!("refs/heads/{b}")),
        Head::Detached(_) => head.map(|h| h.to_string()),
    };
    let amended = head
        .zip(head_cs.as_ref())
        .filter(|_| a.amend)
        .map(|(id, c)| (id, c.message.as_slice()));
    let initial =
        initial_message(ctx, ws, a, &gitdir, amended, !merge.is_empty(), head_name).await?;
    let use_editor = !a.no_edit && (a.edit || initial.edit_by_default);
    let mut hook_env = env_base.with_index(index_file.clone());
    if !use_editor {
        hook_env = hook_env.without_editor();
    }

    // Step 2: pre-commit.
    if !a.no_verify
        && let Gate::Reject(_) = runner.gate(Hook::PreCommit, &hook_env).await?
    {
        return Ok(Flow::Abort(1));
    }
    // Step 3: the stage set as the hook left it.
    let rows = stage::overlay(&ws.db, ws_id, use_set).await?;
    // The root and the changes, from the rows as re-read; no file content is read.
    let parents: Vec<ChangesetId> = match (&head_cs, a.amend) {
        (Some(c), true) => c.parents.clone(),
        _ => head.into_iter().chain(merge.iter().copied()).collect(),
    };
    let changes = stagebuild::rows_to_changes(ws, head_root, &rows).await?;
    let root = engine.apply(head_root, &changes).await?;

    // Step 4: COMMIT_EDITMSG, prepare-commit-msg, the editor, commit-msg.
    let comment = message::comment_char(ctx);
    let edit_file = gitdir.join("COMMIT_EDITMSG");
    let branch = match &head_ref {
        Head::Branch(b) => Some(b.as_str()),
        Head::Detached(_) => None,
    };
    let idents = ident::resolve(
        ctx,
        a.author.as_deref(),
        a.date.as_deref(),
        head_cs.as_ref().filter(|_| a.amend).map(|c| &c.author),
    )?;
    let mut buf = initial.text.clone();
    if use_editor {
        if !buf.is_empty() && !buf.ends_with(b"\n") {
            buf.push(b'\n');
        }
        if !merge.is_empty() {
            let c = comment as char;
            buf.extend_from_slice(
                format!(
                    "{c}\n{c} It looks like you may be committing a merge.\n{c} If this is not correct, please run\n{c}\tgit update-ref -d MERGE_HEAD\n{c} and try again.\n\n"
                )
                .as_bytes(),
            );
        }
        buf.push(b'\n');
        let diff = if a.verbose {
            Some(staged_patch(ws, head_root, &rows).await?)
        } else {
            None
        };
        let status = status::long_text(ctx, ws, Some(comment)).await?;
        let idents_shown = template_idents(comment, &idents, author_date_is_interesting(a));
        buf.extend_from_slice(&message::template(
            comment,
            &idents_shown,
            &status,
            diff.as_deref(),
        ));
    }
    std::fs::write(&edit_file, &buf)?;
    // git stops here, before prepare-commit-msg and the editor, when nothing would be committed.
    if !a.amend && !a.allow_empty && merge.is_empty() && root == head_root {
        let report = nothing_report(ctx, ws, use_set, &rows).await?;
        std::io::stdout().write_all(&report)?;
        if seqstate::exists(&gitdir, "CHERRY_PICK_HEAD") {
            let (t, sequenced) = (super::restore::tool(ctx), Sequence::exists(&gitdir));
            eprint!(
                "The previous cherry-pick is now empty, possibly due to conflict resolution.\n\
                 If you wish to commit it anyway, use:\n\n    {t} commit --allow-empty\n\n{}",
                if sequenced {
                    format!(
                        "and then use:\n\n    {t} cherry-pick --continue\n\n\
                         to resume cherry-picking the remaining commits.\n\
                         If you wish to skip this commit, use:\n\n    {t} cherry-pick --skip\n\n"
                    )
                } else {
                    format!("Otherwise, please use '{t} cherry-pick --skip'\n")
                }
            );
        }
        return Ok(Flow::Abort(1));
    }
    let prepare = Hook::PrepareCommitMsg {
        file: edit_file.clone(),
        source: initial.source.clone(),
    };
    if let Gate::Reject(_) = runner.gate(prepare, &hook_env).await? {
        return Ok(Flow::Abort(1));
    }
    if use_editor && !message::edit(ctx, &edit_file).await? {
        return Err(fatal(
            "Please supply the message using either -m or -F option.",
        ));
    }
    if !a.no_verify
        && let Gate::Reject(_) = runner
            .gate(
                Hook::CommitMsg {
                    file: edit_file.clone(),
                },
                &hook_env,
            )
            .await?
    {
        return Ok(Flow::Abort(1));
    }
    let raw = std::fs::read(&edit_file)?;
    let text = message::cleanup(&raw, cleanup_mode, use_editor, comment);
    if text.is_empty() && !a.allow_empty_message {
        eprintln!("Aborting commit due to empty commit message.");
        return Ok(Flow::Abort(1));
    }

    // Step 5: the first parent's root and the pending copies.
    let first_parent_root = match parents.first() {
        Some(&p) => engine.get_changeset(p).await?.root,
        None => CliEngine::empty_tree_id(),
    };
    let copies = pending_copies(ws, first_parent_root, root).await?;

    // Step 6: sign, write, derive, move the branch, clear the stage.
    let mut new = NewChangeset {
        parents: parents.clone(),
        author: idents.author.clone(),
        committer: idents.committer.clone(),
        message: text.clone(),
        extra: Vec::new(),
        root,
        copies,
    };
    let policy = PathPolicy::Native {
        windows_safe: ws.repo.repo.windows_safe,
    };
    let sign_key = match &a.gpg_sign {
        Some(key) => Some(key.clone()),
        None if !a.no_gpg_sign && ctx.config.bool("commit.gpgsign") == Some(true) => Some(None),
        None => None,
    };
    if let Some(key) = sign_key {
        let unsigned = engine.create_changeset(new.clone(), policy).await?;
        let cs = engine.get_changeset(unsigned).await?;
        let root_git = tree_oid(engine.get_tree(root).await?.as_ref());
        let mut parent_gits = Vec::new();
        for &p in &parents {
            parent_gits.push(
                engine
                    .git_commit_of(p)
                    .await
                    .map_err(|e| fatal(e.to_string()))?,
            );
        }
        let payload =
            export_commit(&cs, root_git, &parent_gits).map_err(|e| fatal(e.to_string()))?;
        let sig = sign(
            ctx,
            &payload,
            key.as_deref(),
            &ident::display(&idents.committer),
        )
        .await?;
        new.extra.push((b"git.gpgsig".to_vec(), sig));
    }
    let id = engine.create_changeset(new, policy).await?;
    let gen_no = engine
        .get_derived::<Gen>(id)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    engine.warm(id).await.map_err(|e| fatal(e.to_string()))?;
    cs_gen::insert_many(
        &ws.db,
        ws.repo.key,
        &[cs_gen::CsGen {
            cs: id,
            gen_no: u64::from(gen_no.generation),
            parents: parents.clone(),
        }],
    )
    .await?;
    stagebuild::index_spine(ws, root, &changes).await?;
    index_commit_git(engine, id, &sink)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    let new_git = engine
        .git_commit_of(id)
        .await
        .map_err(|e| fatal(e.to_string()))?;

    let (name, expect) = match &head_ref {
        Head::Branch(b) => (
            RefName::new(format!("refs/heads/{b}").into_bytes())
                .map_err(|e| fatal(e.to_string()))?,
            head.map_or(Expect::Absent, |h| Expect::Value(RefTarget::Changeset(h))),
        ),
        Head::Detached(_) => (
            RefName::new_onelevel(b"HEAD".to_vec()).map_err(|e| fatal(e.to_string()))?,
            head.map_or(Expect::Absent, |h| Expect::Value(RefTarget::Changeset(h))),
        ),
    };
    let staging = if scope == Scope::Only {
        let committed: BTreeSet<Vec<u8>> = rows.iter().map(|r| r.path.clone()).collect();
        let mut rest = stage::overlay(&ws.db, ws_id, StageSet::Default).await?;
        rest.retain(|r| !committed.contains(&r.path));
        StagingAfterMove::Set(rest)
    } else {
        StagingAfterMove::Set(Vec::new())
    };
    let subject = message::subject(&text);
    let kind = if a.amend {
        "commit (amend)"
    } else if parents.is_empty() {
        "commit (initial)"
    } else if !merge.is_empty() {
        "commit (merge)"
    } else {
        "commit"
    };
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name,
            expect,
            new: Some(RefTarget::Changeset(id)),
            expires_at: None,
        }],
        actor: Actor {
            id: String::from_utf8_lossy(idents.committer.email()).into_owned(),
            via: "cli".into(),
        },
        reason: String::new(),
        op_id: uuid::Uuid::now_v7(),
    };
    let op = OpId(txn.op_id);
    ctx.update_local_refs(txn, &format!("{kind}: {subject}"), staging)
        .await?;

    // The commit exists: state files, the synthetic index, the rewrite record, then the
    // post-commit hooks.
    let preds: Vec<ChangesetId> = if a.amend {
        head.into_iter().collect()
    } else {
        Vec::new()
    };
    ext::rewrite_sink().record(
        id,
        &preds,
        if a.amend {
            RewriteOp::Amend
        } else {
            RewriteOp::New
        },
        op,
    );
    ext::index_sink()
        .on_commit(ws_id, id)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    for file in [
        "MERGE_HEAD",
        "MERGE_MSG",
        "MERGE_MODE",
        "SQUASH_MSG",
        "AUTO_MERGE",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ] {
        let _ = std::fs::remove_file(gitdir.join(file));
    }
    ws_ops::clear(&ws.db, ws_id).await?;

    let _ = runner.run(Hook::PostCommit, &hook_env).await?;
    if a.amend
        && let Some(old) = head
    {
        let old_git = engine
            .git_commit_of(old)
            .await
            .map_err(|e| fatal(e.to_string()))?;
        let hook = Hook::PostRewrite {
            kind: RewriteKind::Amend,
            pairs: vec![(old_git, new_git)],
        };
        let _ = runner.run(hook, &hook_env).await?;
    }
    if !a.quiet {
        let summary = Summary {
            label: branch.map_or_else(|| "detached HEAD".to_owned(), str::to_owned),
            root_commit: head.is_none(),
            oid: new_git,
            subject,
            author_differs: ident::display(&idents.author) != ident::display(&idents.committer),
            author: ident::display(&idents.author),
            date: author_date_is_interesting(a).then(|| default_date(&idents.author)),
            merge: parents.len() > 1,
        };
        print_summary(ws, &summary, first_parent_root, root).await?;
    }
    Ok(Flow::Committed)
}

/// The "nothing to commit" report. A partial commit (`<paths>`, `--only`, `--include`) reports
/// against its temporary index, so staged work outside the named paths is not shown as staged.
async fn nothing_report(
    ctx: &Ctx,
    ws: &WsCtx,
    set: StageSet,
    rows: &[stage::StageRow],
) -> Result<Vec<u8>> {
    if set == StageSet::Default {
        return status::long_text(ctx, ws, None).await;
    }
    let id = ws.workspace.ws.id;
    let kept = stage::overlay(&ws.db, id, StageSet::Default).await?;
    stage::replace(&ws.db, id, StageSet::Default, rows).await?;
    let report = status::long_text(ctx, ws, None).await;
    stage::replace(&ws.db, id, StageSet::Default, &kept).await?;
    report
}

/// git's `author_date_is_interesting`: the author date came from a commit being amended or from
/// `--date`, so the template and the summary show it.
fn author_date_is_interesting(a: &Args) -> bool {
    a.amend || a.date.is_some()
}

/// git's default date format for an identity's time.
fn default_date(sig: &Signature) -> String {
    DateMode::default().render(sig.time(), sig.tz_min(), sig.time())
}

/// The template lines that name the author when they differ from the committer: `Author:` and
/// `Date:`, followed by a blank comment line; empty when there is nothing to show.
fn template_idents(comment: u8, idents: &ident::Idents, date: bool) -> Vec<u8> {
    let c = comment as char;
    let mut out = String::new();
    if ident::display(&idents.author) != ident::display(&idents.committer) {
        out.push_str(&format!(
            "{c} Author:    {}\n",
            ident::display(&idents.author)
        ));
    }
    if date {
        out.push_str(&format!(
            "{c} Date:      {}\n",
            default_date(&idents.author)
        ));
    }
    if !out.is_empty() {
        out.push_str(&format!("{c}\n"));
    }
    out.into_bytes()
}

/// The `asc mv` renames whose destination is written by this commit and whose source is in the
/// first parent.
async fn pending_copies(
    ws: &WsCtx,
    first_parent_root: TreeId,
    root: TreeId,
) -> Result<Vec<(RepoPath, RepoPath)>> {
    let renames = ws_ops::pending_renames(&ws.db, ws.workspace.ws.id).await?;
    if renames.is_empty() {
        return Ok(Vec::new());
    }
    let written: BTreeSet<RepoPath> = ws
        .engine
        .leafdiff(first_parent_root, root, None)
        .await?
        .into_iter()
        .filter(|(_, c)| matches!(c, Change::Write { .. }))
        .map(|(p, _)| p)
        .collect();
    let mut out = Vec::new();
    for (from, to) in renames {
        let (Ok(from), Ok(to)) = (RepoPath::new(from), RepoPath::new(to)) else {
            continue;
        };
        if written.contains(&to) && ws.engine.lookup(first_parent_root, &from).await?.is_some() {
            out.push((to, from));
        }
    }
    Ok(out)
}

/// The staged patch shown below the scissors line by `-v`.
async fn staged_patch(ws: &WsCtx, head_root: TreeId, rows: &[stage::StageRow]) -> Result<Vec<u8>> {
    let changes = stagebuild::rows_to_changes(ws, head_root, rows).await?;
    let root = ws.engine.apply(head_root, &changes).await?;
    let opts = DiffOptions::default();
    let tree_changes = ws
        .engine
        .diff_trees(head_root, root, None, &opts)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    let dctx = DiffCtx {
        opts: &opts,
        content: &ws.engine,
        attrs: &NoAttributes,
    };
    let mut out = Vec::new();
    asc_engine::diff::write_patch(&mut out, &dctx, &tree_changes)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    Ok(out)
}

/// Signs `payload` with git's signing program (`gpg.program`, default `gpg`).
async fn sign(ctx: &Ctx, payload: &[u8], key: Option<&str>, committer: &str) -> Result<Vec<u8>> {
    let program = ctx
        .config
        .get("gpg.program")
        .unwrap_or_else(|| "gpg".to_owned());
    let key = key
        .map(str::to_owned)
        .or_else(|| ctx.config.get("user.signingkey"))
        .unwrap_or_else(|| committer.to_owned());
    let mut cmd = ctx.child(ctx.shell())?;
    cmd.args(["-c", "exec \"$@\"", "sh"])
        .arg(&program)
        .args(["--status-fd=2", "-bsau"])
        .arg(&key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let payload = payload.to_vec();
    let out = tokio::task::spawn_blocking(move || -> std::io::Result<std::process::Output> {
        let mut child = cmd.spawn()?;
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(&payload);
        }
        child.wait_with_output()
    })
    .await
    .map_err(|e| fatal(e.to_string()))??;
    let mut sig = out.stdout;
    while sig.last() == Some(&b'\n') {
        sig.pop();
    }
    if !out.status.success() || sig.is_empty() {
        return Err(CliError::Raw(
            "error: gpg failed to sign the data\nfatal: failed to write commit object".to_owned(),
            128,
        ));
    }
    Ok(sig)
}

/// What the post-commit summary prints.
struct Summary {
    label: String,
    root_commit: bool,
    oid: GitOid,
    subject: String,
    author_differs: bool,
    author: String,
    /// The author date, when it is not simply now (amend, `--date`).
    date: Option<String>,
    /// A merge commit: git prints no diffstat for it.
    merge: bool,
}

/// `[branch abc1234] subject`, the author if it differs, the shortstat and the mode lines.
async fn print_summary(ws: &WsCtx, s: &Summary, parent_root: TreeId, root: TreeId) -> Result<()> {
    let mut out = Vec::new();
    let marker = if s.root_commit { " (root-commit)" } else { "" };
    let short = &s.oid.to_string()[..7];
    writeln!(out, "[{}{marker} {short}] {}", s.label, s.subject)?;
    if s.author_differs {
        writeln!(out, " Author: {}", s.author)?;
    }
    if let Some(date) = &s.date {
        writeln!(out, " Date: {date}")?;
    }
    if s.merge {
        std::io::stdout().write_all(&out)?;
        return Ok(());
    }
    // git hardcodes rename detection (limit 100) for the summary, whatever `diff.renames` says.
    let opts = DiffOptions {
        renames: Some(RenameOptions {
            limit: 100,
            ..RenameOptions::default()
        }),
        ..DiffOptions::default()
    };
    let changes = ws
        .engine
        .diff_trees(parent_root, root, None, &opts)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    let dctx = DiffCtx {
        opts: &opts,
        content: &ws.engine,
        attrs: &NoAttributes,
    };
    write_shortstat(&mut out, &dctx, &changes)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    write_summary(&mut out, &changes, &opts).map_err(|e| fatal(e.to_string()))?;
    std::io::stdout().write_all(&out)?;
    Ok(())
}

/// git's wording of an I/O failure.
fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_owned(),
        _ => e.to_string(),
    }
}
