//! `submodule status | set-pin | update` (SPEC §10.6, §15.5): submodules are composed read-only
//! from the nested repository, never cloned. `status` lists the pins, `set-pin` stages a new
//! `Change::Gitlink` and re-projects a plain work tree, and `update` / `init` do nothing.

use std::ffi::OsString;
use std::io::Read;
use std::process::ExitCode;
use std::sync::Arc;

use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, FileId, GitOid, RefTarget, TreeId};
use asc_core::object::Kind;
use asc_core::path::{RefName, RepoPath};
use asc_engine::Engine;
use asc_state::state::git_map;
use asc_state::state::repos::{self, Repo};
use asc_state::state::stage::Leaf;
use asc_state::state::workspaces::Mode;
use asc_store::refs::Refs;
use asc_wc::SafeRoot;
use asc_wc::compose_plain::{
    Limits, MAX_GITMODULES_BYTES, Replace, Submodules, clear, looks_composed, project, restrict,
    restricted_line,
};

use super::add::cwd_prefix;
use super::add::spec::{Spec, repo_path};
use super::add::target::{Edit, Staged, Target};
use super::common::{CliError, Ctx, Result, WsCtx};
use super::refs_plumbing::args::parse_oid;
use crate::msg::Msg;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "submodule", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// `[status|set-pin|update|init] [<args>…]`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "submodule",
    flags: &["-q", "--quiet", "--init", "--recursive", "--cached"],
    placeholders: &[],
}];

const USAGE_SET_PIN: &str = "usage: git submodule set-pin <path> <rev>";

/// What a gitlink composes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The nested tree, projected read-only.
    Composed { repo: String, tree: TreeId },
    /// No access to the nested repository: an empty read-only directory.
    Restricted { repo: String },
    /// No nested repository is configured, or it has no changeset for the commit here.
    Unavailable { repo: Option<String> },
}

/// One line of `status`.
pub fn status_line(path: &RepoPath, pin: &GitOid, outcome: &Outcome) -> String {
    match outcome {
        Outcome::Composed { repo, .. } => format!(" {pin} {path} ({repo})"),
        Outcome::Restricted { repo } => restricted_line(path, repo),
        Outcome::Unavailable { repo: Some(repo) } => {
            format!("-{pin} {path} ({repo}: commit not available)")
        }
        Outcome::Unavailable { repo: None } => format!("-{pin} {path}"),
    }
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// Every staged gitlink and its pin, by path.
async fn pins(ws: &WsCtx, target: &Target, only: &Spec) -> Result<Vec<(RepoPath, GitOid)>> {
    let mut out = Vec::new();
    target
        .tracked(ws, only, |path, t| {
            if let Staged::Leaf { leaf, .. } = &t.staged
                && leaf.kind == Kind::Gitlink
            {
                out.push((path.clone(), leaf.git));
            }
            Ok(())
        })
        .await?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// `.gitmodules` of the work tree, bounded; a link or a missing file reads as absent.
fn read_gitmodules(gate: &SafeRoot) -> Option<String> {
    let path = RepoPath::new(".gitmodules").ok()?;
    let abs = gate.resolve(&path)?;
    let meta = std::fs::symlink_metadata(&abs).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(&abs)
        .ok()?
        .take(MAX_GITMODULES_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    buf.truncate(MAX_GITMODULES_BYTES);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn submodules(ctx: &Ctx, ws: &WsCtx, paths: &[RepoPath]) -> Submodules {
    let gate = SafeRoot::new(ws.projection.toplevel());
    let gitmodules = read_gitmodules(&gate);
    Submodules::resolve(&ws.repo.repo.name, gitmodules.as_deref(), paths, |p| {
        ctx.config.get(&format!("asc.submodule.{p}.repo"))
    })
}

/// The nested repository registered under `name`; none when absent or ambiguous.
async fn nested_repo(ws: &WsCtx, name: &str) -> Result<Option<Repo>> {
    let mut found = repos::by_name(&ws.db, name).await.map_err(fatal)?;
    Ok(if found.len() == 1 { found.pop() } else { None })
}

/// Whether the caller may read `repo`. Repositories in `state.db` were admitted by the user
/// (clone or init); a server-side denial arrives with the nested fetch and projects as
/// [`Outcome::Restricted`] (SPEC §15.5).
fn may_read(_ws: &WsCtx, _repo: &Repo) -> bool {
    true
}

async fn outcome(ws: &WsCtx, subs: &Submodules, path: &RepoPath, pin: &GitOid) -> Result<Outcome> {
    let Some(name) = subs.repo_for(path) else {
        return Ok(Outcome::Unavailable { repo: None });
    };
    let repo = name.to_owned();
    let Some(nested) = nested_repo(ws, name).await? else {
        return Ok(Outcome::Unavailable { repo: Some(repo) });
    };
    if !may_read(ws, &nested) {
        return Ok(Outcome::Restricted { repo });
    }
    let Some(asc) = git_map::git_to_asc(&ws.db, nested.key, *pin, ObjectKind::Commit)
        .await
        .map_err(fatal)?
    else {
        return Ok(Outcome::Unavailable { repo: Some(repo) });
    };
    let engine = Engine::new(ws.objects.clone(), Arc::new(ws.db.refs()), nested.key);
    let cs = engine
        .get_changeset(ChangesetId::from_bytes(asc))
        .await
        .map_err(fatal)?;
    Ok(Outcome::Composed { repo, tree: cs.root })
}

async fn status(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let prefix = cwd_prefix(ctx, ws);
    let only = Spec::parse(rest, &prefix, ws.projection.toplevel())?;
    let target = Target::open(ctx).await?;
    let pins = pins(ws, &target, &only).await?;
    let paths: Vec<RepoPath> = pins.iter().map(|(p, _)| p.clone()).collect();
    let subs = submodules(ctx, ws, &paths);
    for (path, pin) in &pins {
        let o = outcome(ws, &subs, path, pin).await?;
        println!("{}", status_line(path, pin, &o));
    }
    Ok(ExitCode::SUCCESS)
}

/// The commit `rev` names in the nested repository, which must know it when it is present.
async fn resolve_rev(ws: &WsCtx, nested: Option<&Repo>, rev: &str) -> Result<GitOid> {
    if let Some(oid) = parse_oid(rev) {
        if let Some(n) = nested
            && git_map::git_to_asc(&ws.db, n.key, oid, ObjectKind::Commit)
                .await
                .map_err(fatal)?
                .is_none()
        {
            return Err(fatal(format!("{rev}: not a commit of {}", n.repo.name)));
        }
        return Ok(oid);
    }
    let Some(n) = nested else {
        return Err(fatal(format!(
            "{rev}: the nested repository is not available here; give a full commit id"
        )));
    };
    let refs = ws.db.refs();
    for name in [rev.to_owned(), format!("refs/heads/{rev}"), format!("refs/tags/{rev}")] {
        let Ok(name) = RefName::new(name.into_bytes()) else { continue };
        let Some(entry) = Refs::get(&refs, n.key, &name).await.map_err(fatal)? else { continue };
        let RefTarget::Changeset(cs) = entry.target else { continue };
        let rows = git_map::asc_to_git(&ws.db, n.key, cs.as_bytes()).await.map_err(fatal)?;
        if let Some((oid, _)) = rows.into_iter().find(|(_, k)| *k == ObjectKind::Commit) {
            return Ok(oid);
        }
    }
    Err(fatal(format!("{rev}: not a branch, tag or commit of {}", n.repo.name)))
}

async fn set_pin(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let [path_arg, rev] = rest else {
        return Err(CliError::Usage(USAGE_SET_PIN.into()));
    };
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel();
    let bytes = repo_path(path_arg, &cwd_prefix(ctx, ws), root)?;
    let path = RepoPath::new(bytes).map_err(fatal)?;
    let rev = rev.to_str().ok_or_else(|| CliError::Usage(USAGE_SET_PIN.into()))?;

    let mut target = Target::open(ctx).await?;
    let is_link = matches!(
        target.staged(ws, &path).await?,
        Staged::Leaf { leaf, .. } if leaf.kind == Kind::Gitlink
    );
    if !is_link {
        return Err(fatal(format!("no submodule mapping found for '{path}'")));
    }
    let subs = submodules(ctx, ws, std::slice::from_ref(&path));
    let nested = match subs.repo_for(&path) {
        Some(name) => nested_repo(ws, name).await?,
        None => None,
    };
    let pin = resolve_rev(ws, nested.as_ref(), rev).await?;
    let leaf = Leaf { kind: Kind::Gitlink, file: FileId::from_bytes([0; 32]), size: 0, git: pin };
    target.apply(ctx, vec![Edit::Put { path: path.clone(), leaf, ita: false }]).await?;
    println!("{pin} {path}");

    if target.is_default() && ws.workspace.ws.mode == Mode::Plain {
        reproject(ctx, ws, &subs, &path, &pin).await;
    }
    Ok(ExitCode::SUCCESS)
}

/// Re-projects the composed directory after a pin change. A failure is a warning: the pin is
/// already staged, and a later switch projects it again.
async fn reproject(ctx: &Ctx, ws: &WsCtx, subs: &Submodules, path: &RepoPath, pin: &GitOid) {
    let gate = SafeRoot::new(ws.projection.toplevel());
    let done = async {
        let replace = if looks_composed(&gate, path) { Replace::Composed } else { Replace::EmptyOnly };
        match outcome(ws, subs, path, pin).await.map_err(|e| e.to_string())? {
            Outcome::Composed { tree, .. } => {
                let symlinks = ctx.config.bool("core.symlinks").unwrap_or(!cfg!(windows));
                project(&*ws.objects, &gate, path, tree, symlinks, replace, &Limits::default())
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
            Outcome::Restricted { .. } => restrict(&gate, path, replace).map_err(|e| e.to_string()),
            Outcome::Unavailable { .. } => {
                // The old composed content no longer matches the pin; leave a plain placeholder.
                if replace == Replace::Composed {
                    clear(&gate, path).map_err(|e| e.to_string())?;
                    restrict(&gate, path, Replace::EmptyOnly).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
        }
    }
    .await;
    if let Err(e) = done {
        eprintln!("warning: {path}: the composed directory was not updated: {e}");
    }
}

fn update_notice() -> ExitCode {
    eprintln!(
        "asc: submodules are composed read-only from their repositories; nothing to update (use `asc submodule set-pin <path> <rev>` to move a pin)"
    );
    ExitCode::SUCCESS
}

fn is_flag(arg: &OsString) -> bool {
    let a = arg.to_string_lossy();
    a.starts_with('-') && a != "--"
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let Some(at) = a.rest.iter().position(|x| !is_flag(x)) else {
        return status(ctx, &[]).await;
    };
    let tail = &a.rest[at + 1..];
    match a.rest[at].to_string_lossy().as_ref() {
        "status" => {
            let paths: Vec<OsString> = tail.iter().filter(|x| !is_flag(x)).cloned().collect();
            status(ctx, &paths).await
        }
        "set-pin" => set_pin(ctx, tail).await,
        "update" | "init" => Ok(update_notice()),
        other => Err(CliError::Msg(Msg::AscNotInWorkspace { cmd: format!("submodule {other}") })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn status_lines() {
        let pin = GitOid::from_bytes([0xab; 20]);
        let hex = "ab".repeat(20);
        let tree = TreeId::from_bytes([1; 32]);
        let composed = Outcome::Composed { repo: "asc/soma".into(), tree };
        assert_eq!(status_line(&rp("sdk"), &pin, &composed), format!(" {hex} sdk (asc/soma)"));
        let restricted = Outcome::Restricted { repo: "asc/vault".into() };
        assert_eq!(
            status_line(&rp("vault"), &pin, &restricted),
            "vault: restricted (no access to asc/vault)"
        );
        assert_eq!(
            status_line(&rp("x"), &pin, &Outcome::Unavailable { repo: None }),
            format!("-{hex} x")
        );
        assert_eq!(
            status_line(&rp("x"), &pin, &Outcome::Unavailable { repo: Some("A/b".into()) }),
            format!("-{hex} x (A/b: commit not available)")
        );
    }
}
