//! `clone <url> [<dir>]` for bucket URLs (SPEC §6.3, §9.3, plain mode): register the repository,
//! fetch the branch tip, its root tree and the history metadata (O(window + root tree)), then
//! every other branch's tip, create the workspace and check the files out.
//!
//! The remote is written to git config the way `remote add` does, plus the `^refs/heads/agent/*`
//! negative refspec (SPEC §6.2), so a plain `fetch` never downloads agent branches.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_core::id::RefTarget;
use asc_core::path::RepoPath;
use asc_git::projection::{Head, Projection};
use asc_state::state::repos::{self, NewRepo, RemoteKind};
use asc_state::state::workspaces::{self, NewWorkspace};
use asc_state::{AscHome, RepoId, StateDb, WorkspaceId};
use asc_store::refs::{Expect, RefTxn, RefUpdate};
use asc_wc::worktree as wt;

use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::fetch::{Opts as FetchOpts, fatal, fetch_one, sync_error};
use super::restore::{actor, root_of, sync_worktree};
use super::switch::{ensure_mapped, ref_name, set_config};
use crate::ext;
use crate::hooks_runner::{Hook, HookEnv, Outcome, Runner, null_oid};
use crate::native::NativeSpec;
use crate::remote::url::{self, RefsMode, RemoteSpec, default_dir_name};
use crate::remote::{self, Intent, StateBoundaries};

#[derive(clap::Parser, Debug)]
#[command(name = "clone", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "clone",
    flags: &[
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "-b",
        "--branch",
        "-o",
        "--origin",
        "-n",
        "--no-checkout",
        "--mode",
        "--depth",
        "--filter",
        "--single-branch",
        "--no-single-branch",
        "--seed",
        "--progress",
        "--no-progress",
        "--no-tags",
        "--refs",
        "--reference",
        "--end-of-options",
    ],
    placeholders: &[],
}];

const USAGE: &str = "usage: git clone [<options>] [--] <repo> [<dir>]";

/// The most `.gitattributes` files the attribute warning reads, and the most bytes of each.
const ATTR_FILES: usize = 64;
const ATTR_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default)]
struct Opts {
    quiet: bool,
    branch: Option<String>,
    origin: Option<String>,
    no_checkout: bool,
    refs: Option<RefsMode>,
    /// `--reference <git-dir>`: git-remote mode fetches from it first (SPEC §6.3).
    reference: Vec<String>,
    words: Vec<String>,
}

fn parse(argv: &[OsString]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = argv.iter().map(|a| a.to_string_lossy().into_owned());
    let mut ended = false;
    let need = |v: Option<String>| v.ok_or_else(|| CliError::Usage(USAGE.to_owned()));
    while let Some(a) = it.next() {
        if ended || !a.starts_with('-') || a == "-" {
            o.words.push(a);
            continue;
        }
        match a.as_str() {
            "--" | "--end-of-options" => ended = true,
            "-q" | "--quiet" => o.quiet = true,
            "-v" | "--verbose" | "--single-branch" | "--no-single-branch" | "--progress"
            | "--no-progress" | "--no-tags" => {}
            "-b" | "--branch" => o.branch = Some(need(it.next())?),
            "-o" | "--origin" => o.origin = Some(need(it.next())?),
            "-n" | "--no-checkout" => o.no_checkout = true,
            "--mode" => match need(it.next())?.as_str() {
                "plain" => {}
                "virtual" => {
                    return Err(fatal(
                        "virtual clones need the daemon, which is not available yet; use --mode plain",
                    ));
                }
                other => return Err(CliError::Fatal(format!("unknown clone mode '{other}'"))),
            },
            "--reference" => o.reference.push(need(it.next())?),
            "--depth" | "--filter" | "--seed" => {
                need(it.next())?;
            }
            "--refs" => {
                o.refs = Some(match need(it.next())?.as_str() {
                    "local" => RefsMode::Local,
                    "shared" => RefsMode::Shared,
                    _ => return Err(CliError::Usage(USAGE.to_owned())),
                });
            }
            w if w.starts_with("--branch=") => o.branch = Some(w["--branch=".len()..].to_owned()),
            w if w.starts_with("--origin=") => o.origin = Some(w["--origin=".len()..].to_owned()),
            w if w.starts_with("--mode=") => match &w["--mode=".len()..] {
                "plain" => {}
                _ => return Err(fatal("only --mode plain is available")),
            },
            w if w.starts_with("--reference=") => {
                o.reference.push(w["--reference=".len()..].to_owned());
            }
            w if w.starts_with("--depth=")
                || w.starts_with("--filter=")
                || w.starts_with("--seed=") => {}
            _ => return Err(CliError::Usage(USAGE.to_owned())),
        }
    }
    Ok(o)
}

/// A context for the workspace at `dir`, ignoring `GIT_DIR` and its kin in the environment.
pub(crate) async fn open_at(ctx: &Ctx, dir: PathBuf) -> Result<Ctx> {
    let mut env = ctx.env.clone();
    env.git_dir = None;
    env.work_tree = None;
    env.common_dir = None;
    env.index_file = None;
    Ctx::open(ctx.prog, env, dir).await
}

/// Writes `remote.<name>.*` for a bucket remote (what `clone` and `init --store` leave behind).
pub(crate) async fn write_remote_config(
    ctx: &Ctx,
    name: &str,
    stored_url: &str,
    refs: RefsMode,
) -> Result<()> {
    set_config(ctx, &format!("remote.{name}.url"), stored_url).await?;
    set_config(
        ctx,
        &format!("remote.{name}.fetch"),
        &format!("+refs/heads/*:refs/remotes/{name}/*"),
    )
    .await?;
    let rest = vec![
        OsString::from("--local"),
        OsString::from("--add"),
        OsString::from(format!("remote.{name}.fetch")),
        OsString::from("^refs/heads/agent/*"),
    ];
    super::config::run(ctx, super::config::Args { rest }).await?;
    set_config(
        ctx,
        &format!("remote.{name}.ascRefs"),
        if refs == RefsMode::Shared {
            "shared"
        } else {
            "local"
        },
    )
    .await
}

/// `remote.<name>.*` of a server remote: the URL, the fetch refspec and the agent-ref exclusion
/// that bucket remotes get (the server lists user refs; a clone does not fetch them).
pub(crate) async fn write_server_remote_config(
    ctx: &Ctx,
    name: &str,
    stored_url: &str,
) -> Result<()> {
    write_git_remote_config(ctx, name, stored_url).await?;
    let rest = vec![
        OsString::from("--local"),
        OsString::from("--add"),
        OsString::from(format!("remote.{name}.fetch")),
        OsString::from("^refs/heads/agent/*"),
    ];
    super::config::run(ctx, super::config::Args { rest }).await?;
    Ok(())
}

/// `remote.<name>.*` of a git remote: git's own `clone` leaves exactly these two keys.
pub(crate) async fn write_git_remote_config(ctx: &Ctx, name: &str, stored_url: &str) -> Result<()> {
    set_config(ctx, &format!("remote.{name}.url"), stored_url).await?;
    set_config(
        ctx,
        &format!("remote.{name}.fetch"),
        &format!("+refs/heads/*:refs/remotes/{name}/*"),
    )
    .await
}

/// The bucket URL as stored: the user's text, or the file URL a plain directory names.
pub(crate) fn stored_form(typed: &str, spec: &RemoteSpec) -> String {
    match spec {
        RemoteSpec::Bucket { url, .. } if !typed.contains("://") => url.clone(),
        _ => typed.to_owned(),
    }
}

fn slashed(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// The branch to clone: `-b`, else `master`, `init.defaultBranch`, or the first branch.
fn choose_branch(ctx: &Ctx, wanted: Option<&str>, branches: &[String]) -> Result<Option<String>> {
    if let Some(b) = wanted {
        return if branches.iter().any(|x| x == b) {
            Ok(Some(b.to_owned()))
        } else {
            Err(CliError::Fatal(format!(
                "Remote branch {b} not found in upstream origin"
            )))
        };
    }
    let preferred = ctx.config.get("init.defaultBranch");
    for p in [Some("master".to_owned()), preferred] {
        if let Some(p) = p
            && branches.contains(&p)
        {
            return Ok(Some(p));
        }
    }
    Ok(branches.first().cloned())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    let [typed_url, rest @ ..] = o.words.as_slice() else {
        return Err(CliError::Usage(USAGE.to_owned()));
    };
    if rest.len() > 1 {
        return Err(CliError::Usage(USAGE.to_owned()));
    }
    let spec = url::parse(typed_url, o.refs.unwrap_or(RefsMode::Local)).map_err(fatal)?;
    remote::server::check_transport(typed_url)?;
    // An asc server is found by discovery (SPEC §6.2); a host without one is a git remote.
    let mut git_mode = matches!(spec, RemoteSpec::Git { .. });
    let mut server_mode = matches!(spec, RemoteSpec::Server { .. });
    if matches!(spec, RemoteSpec::Discover { .. }) {
        let probe = StateDb::open(&AscHome::locate()?).await?;
        match remote::server::resolve(ctx.config.file(), &probe, None, typed_url).await? {
            remote::server::Resolution::Server(_) => server_mode = true,
            remote::server::Resolution::Git => git_mode = true,
        }
    }
    let refs = match &spec {
        RemoteSpec::Bucket { refs, .. } => *refs,
        // Git-remote mode (SPEC §6.3) and server remotes keep no local-refs claim.
        RemoteSpec::Git { .. } | RemoteSpec::Discover { .. } | RemoteSpec::Server { .. } => {
            RefsMode::Local
        }
    };
    let origin = o.origin.clone().unwrap_or_else(|| "origin".to_owned());
    if !super::remote::valid_name(&origin) {
        return Err(CliError::Fatal(format!(
            "'{origin}' is not a valid remote name"
        )));
    }
    let stored = if git_mode {
        remote::git::absolute_url(&ctx.cwd, typed_url)
    } else {
        stored_form(typed_url, &spec)
    };

    let dir_name = rest
        .first()
        .cloned()
        .unwrap_or_else(|| default_dir_name(typed_url));
    let root = std::path::absolute(ctx.cwd.join(&dir_name))?;
    let occupancy = wt::occupancy(&root)?;
    if occupancy == wt::Occupancy::Occupied {
        return Err(CliError::Fatal(format!(
            "destination path '{dir_name}' already exists and is not an empty directory."
        )));
    }
    if !o.quiet {
        eprintln!("Cloning into '{dir_name}'...");
    }

    // Reach the bucket before anything is registered or created.
    let home = AscHome::locate()?;
    let db = StateDb::open(&home).await?;
    let (branches, remote_head) = if server_mode {
        let connected = remote::server::connect(
            ctx.config.file(),
            &db,
            ctx.real_git().ok().cloned(),
            Some(&origin),
            &stored,
            crate::credential::terminal_prompt(),
        )
        .await?
        .ok_or_else(|| {
            CliError::Fatal(format!(
                "repository '{}' does not exist",
                super::url_display::anonymize(typed_url)
            ))
        })?;
        let probe = remote::server::opened(
            origin.clone(),
            stored.clone(),
            true,
            spec.clone(),
            connected,
        );
        let remote_refs = probe.remote.list_refs().await.map_err(sync_error)?;
        let branches: Vec<String> = remote_refs
            .iter()
            .filter_map(|r| {
                String::from_utf8_lossy(r.name.as_bytes())
                    .strip_prefix("refs/heads/")
                    .map(str::to_owned)
            })
            .collect();
        drop(probe);
        (branches, None)
    } else if git_mode {
        let advertised = remote::git::ls_remote(ctx, &stored).await?;
        let heads: Vec<String> = advertised
            .refs
            .iter()
            .filter_map(|(n, _)| n.strip_prefix("refs/heads/").map(str::to_owned))
            .collect();
        let head = advertised
            .head
            .as_deref()
            .and_then(|h| h.strip_prefix("refs/heads/"))
            .map(str::to_owned);
        (heads, head)
    } else {
        let probe = remote::open_url(
            &db,
            origin.clone(),
            stored.clone(),
            true,
            refs,
            Intent::Read,
        )
        .await
        .map_err(|e| match e {
            CliError::Fatal(m) if m.contains("does not appear to be an asc bucket") => {
                CliError::Fatal(format!(
                    "repository '{}' does not exist",
                    super::url_display::anonymize(typed_url)
                ))
            }
            other => other,
        })?;
        let remote_refs = probe.remote.list_refs().await.map_err(sync_error)?;
        let branches: Vec<String> = remote_refs
            .iter()
            .filter_map(|r| {
                String::from_utf8_lossy(r.name.as_bytes())
                    .strip_prefix("refs/heads/")
                    .map(str::to_owned)
            })
            .collect();
        drop(probe);
        (branches, None)
    };
    // git clone follows the remote's HEAD; a bucket has none.
    let picked = match (&o.branch, &remote_head) {
        (None, Some(h)) if branches.contains(h) => Some(h.clone()),
        _ => choose_branch(ctx, o.branch.as_deref(), &branches)?,
    };
    let empty = picked.is_none();
    let branch = picked.unwrap_or_else(|| {
        ctx.config
            .get("init.defaultBranch")
            .unwrap_or_else(|| super::init::DEFAULT_BRANCH.to_owned())
    });
    let branch_ok = branch_ok(&branch);
    if !branch_ok {
        return Err(CliError::Fatal(format!(
            "asc: '{branch}' is not a valid branch name"
        )));
    }

    let created_dir = occupancy == wt::Occupancy::Missing;
    if created_dir {
        std::fs::create_dir_all(&root)?;
    }
    let (repo_id, ws_id) = (RepoId::new(), WorkspaceId::new());
    let built = build(
        ctx,
        &home,
        &db,
        &root,
        repo_id,
        ws_id,
        &stored,
        &origin,
        refs,
        &branch,
        &o,
        empty,
        typed_url,
        git_mode,
        server_mode,
    )
    .await;
    match built {
        Ok(code) => Ok(code),
        Err(e) => {
            // Only what this command made: the workspace's state and a directory it created.
            let _ = wt::remove_tree(&home.workspace_dir(ws_id));
            let _ = workspaces::remove(&db, ws_id).await;
            if created_dir {
                let _ = wt::remove_tree(&root);
            } else if std::fs::read_to_string(root.join(".git"))
                .is_ok_and(|t| t.contains(&ws_id.to_string()))
            {
                let _ = std::fs::remove_file(root.join(".git"));
            }
            Err(e)
        }
    }
}

fn branch_ok(branch: &str) -> bool {
    branch != "HEAD"
        && !branch.starts_with('-')
        && ref_name(&format!("refs/heads/{branch}")).is_ok()
}

#[allow(clippy::too_many_arguments)]
async fn build(
    ctx: &Ctx,
    home: &AscHome,
    db: &StateDb,
    root: &Path,
    repo_id: RepoId,
    ws_id: WorkspaceId,
    stored: &str,
    origin: &str,
    refs: RefsMode,
    branch: &str,
    o: &Opts,
    empty: bool,
    typed_url: &str,
    git_mode: bool,
    server_mode: bool,
) -> Result<ExitCode> {
    let projection = Projection::create(home, repo_id, ws_id, root)?;
    projection.write_head(&Head::Branch(branch.to_owned()))?;
    let bucket_url = match url::parse(stored, refs) {
        Ok(RemoteSpec::Bucket { url, .. }) if !git_mode => url,
        _ => stored.to_owned(),
    };
    let key = repos::insert(
        db,
        &NewRepo {
            id: repo_id,
            name: default_dir_name(typed_url),
            ns: uuid::Uuid::new_v4().as_bytes().to_vec(),
            remote_url: Some(bucket_url),
            remote_kind: if git_mode {
                RemoteKind::GitRemote
            } else if server_mode {
                RemoteKind::Server
            } else {
                RemoteKind::Bucket
            },
            default_branch: branch.to_owned(),
            windows_safe: true,
        },
    )
    .await?;
    let path = root.display().to_string();
    workspaces::insert(
        db,
        &NewWorkspace {
            id: ws_id,
            repo: key,
            path: path.clone(),
            mount: path,
            mode: ctx.workspace_mode(),
            head: format!("refs/heads/{branch}"),
            base_cs: None,
            sparse: None,
            ephemeral_until: None,
        },
    )
    .await?;

    // The config comes first: every later step reads the remote from it.
    let setup = open_at(ctx, root.to_owned()).await?;
    if git_mode {
        write_git_remote_config(&setup, origin, stored).await?;
    } else if server_mode {
        write_server_remote_config(&setup, origin, stored).await?;
    } else {
        write_remote_config(&setup, origin, stored, refs).await?;
    }
    if !empty {
        set_config(&setup, &format!("branch.{branch}.remote"), origin).await?;
        set_config(
            &setup,
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
        )
        .await?;
    }
    drop(setup);
    let fresh = open_at(ctx, root.to_owned()).await?;
    let ws = fresh.ws()?;
    if ws.workspace.ws.id != ws_id {
        return Err(CliError::Fatal(
            "asc: the new workspace did not open as itself".into(),
        ));
    }

    if empty {
        eprintln!("warning: You appear to have cloned an empty repository.");
        return Ok(ExitCode::SUCCESS);
    }

    // `--reference`: objects of an existing checkout first, so the fetch moves only the rest.
    if git_mode && !o.reference.is_empty() {
        remote::git::seed_from_references(&fresh, ws, &o.reference).await?;
    }

    // The branch tip, its root tree and the history within the window.
    let opened = remote::open(&fresh, ws, origin, Intent::Read).await?;
    let boundaries = StateBoundaries::new(ws);
    let opts = remote::options(&fresh, &boundaries);
    let report = ws
        .engine
        .clone_minimal(&*opened.remote, branch, &opts)
        .await
        .map_err(sync_error)?;
    let Some(head) = report.head else {
        return Err(CliError::Fatal(format!(
            "Remote branch {branch} not found in upstream {origin}"
        )));
    };
    let tracking = report.local_updates.updates;
    // Every other branch's tip (not the agent branches), as git clone's fetch does.
    super::remote::apply_refs(
        &fresh,
        tracking,
        &format!("clone: from {}", super::url_display::anonymize(typed_url)),
    )
    .await?;
    let quiet = FetchOpts {
        quiet: true,
        ..FetchOpts::default()
    };
    drop(fetch_one(&fresh, ws, origin, &quiet, &[]).await?);
    if git_mode && !o.reference.is_empty() {
        remote::git::drop_reference_refs(&fresh, ws).await?;
    }

    // The local branch and the checkout.
    let update = RefUpdate {
        name: ref_name(&format!("refs/heads/{branch}"))?,
        expect: Expect::Absent,
        new: Some(RefTarget::Changeset(head)),
        expires_at: None,
    };
    let txn = RefTxn {
        updates: vec![update],
        actor: actor(),
        reason: String::new(),
        op_id: ext::OpId::new().0,
    };
    fresh
        .update_local_refs(
            txn,
            &format!("clone: from {}", super::url_display::anonymize(typed_url)),
            // The new branch starts with an index equal to its tip (nothing staged).
            StagingAfterMove::Set(Vec::new()),
        )
        .await?;
    let git = ensure_mapped(ws, head).await?;
    let mut paths: Vec<RepoPath> = Vec::new();
    if !o.no_checkout {
        let tree = root_of(ws, Some(head)).await?;
        // A work tree is written from the local tiers: bring in what this machine lacks.
        if !git_mode {
            remote::hydrate(&fresh, ws, &*opened.remote, tree).await?;
        }
        let empty_tree = super::common::CliEngine::empty_tree_id();
        paths = ws
            .engine
            .leafdiff(empty_tree, tree, None)
            .await?
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        sync_worktree(&fresh, tree, &paths, true).await?;
    }
    drop(opened);
    ext::index_sink()
        .on_head_moved(ws_id, None, Some(head))
        .await
        .map_err(fatal)?;

    attribute_warning(&fresh, ws, &paths);

    let mut code = 0u8;
    if !o.no_checkout {
        let runner = Runner::for_ctx(&fresh)?;
        let hook = Hook::PostCheckout {
            old: null_oid(),
            new: git,
            branch: true,
        };
        if let Outcome::Exit(c) = runner
            .run(hook, &HookEnv::of(&fresh, "clone"))
            .await
            .map_err(fatal)?
            && c != 0
        {
            code = u8::try_from(c).unwrap_or(1);
        }
    }
    Ok(ExitCode::from(code))
}

/// SPEC §2.4.1: one `warning: asc: …` per attribute that asc does not apply at checkout or does
/// not support, and the `autocrlf` rule. Reads at most [`ATTR_FILES`] files of [`ATTR_BYTES`].
fn attribute_warning(ctx: &Ctx, ws: &WsCtx, paths: &[RepoPath]) {
    use std::collections::BTreeMap;
    use std::io::Read;
    let root = ws.projection.toplevel().to_owned();
    // attribute -> (file, patterns)
    let mut found: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    let mut read = 0usize;
    for p in paths {
        let text = String::from_utf8_lossy(p.as_bytes()).into_owned();
        let name = text.rsplit('/').next().unwrap_or("");
        if name != ".gitattributes" && name != ".ascattributes" {
            continue;
        }
        if read >= ATTR_FILES {
            break;
        }
        read += 1;
        let full = root.join(&text); // lint:allow-worktree-fs
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(file) = std::fs::File::open(&full) else {
            continue;
        }; // lint:allow-worktree-fs
        let mut body = String::new();
        if file.take(ATTR_BYTES).read_to_string(&mut body).is_err() {
            continue;
        }
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(pattern) = parts.next() else {
                continue;
            };
            for attr in parts {
                let key = attr
                    .trim_start_matches(['-', '!'])
                    .split('=')
                    .next()
                    .unwrap_or("");
                let flagged = match key {
                    "eol" | "crlf" | "ident" | "working-tree-encoding" => Some(key.to_owned()),
                    "text" if attr.contains('=') || attr == "text" => Some("text".to_owned()),
                    "filter" => {
                        let value = attr.split_once('=').map_or("", |(_, v)| v);
                        (value != "lfs").then(|| "filter".to_owned())
                    }
                    _ => None,
                };
                if let Some(k) = flagged {
                    let entry = found.entry(k).or_insert_with(|| (text.clone(), Vec::new()));
                    if entry.1.len() < 3 && !entry.1.iter().any(|x| x == pattern) {
                        entry.1.push(pattern.to_owned());
                    }
                }
            }
        }
    }
    for (attr, (file, patterns)) in &found {
        let what = match attr.as_str() {
            "ident" | "working-tree-encoding" | "filter" => "is not supported",
            _ => "is not applied at checkout",
        };
        eprintln!(
            "warning: asc: attribute '{attr}' in {file} {what} (affects {})",
            patterns.join(", ")
        );
    }
    if !found.is_empty() && ctx.config.bool("core.autocrlf") == Some(true) {
        eprintln!(
            "warning: asc: core.autocrlf=true is ignored in asc workspaces; files hold the committed bytes"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse() {
        let argv: Vec<OsString> = ["-b", "dev", "--origin", "up", "file:///x", "dst"]
            .iter()
            .map(Into::into)
            .collect();
        let o = parse(&argv).unwrap();
        assert_eq!(o.branch.as_deref(), Some("dev"));
        assert_eq!(o.origin.as_deref(), Some("up"));
        assert_eq!(o.words, ["file:///x", "dst"]);
        assert!(parse(&[OsString::from("--mode=virtual")]).is_err());
        assert!(parse(&[OsString::from("--bogus")]).is_err());
    }

    #[test]
    fn branch_names_are_checked() {
        assert!(branch_ok("master"));
        assert!(branch_ok("feat/x"));
        for bad in ["HEAD", "-x", "a..b", "x.lock", ""] {
            assert!(!branch_ok(bad), "{bad:?}");
        }
        assert_eq!(slashed(Path::new("a\\b")), "a/b");
    }
}
