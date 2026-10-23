//! `asc init [dir] [-b <branch>]` (SPEC §6.5, §9.3, §2.4.2): a local-only repository, a plain
//! workspace with the synthetic `.git`, and an unborn branch.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_core::path::RefName;
use asc_git::projection::{Head, Projection, ProjectionError};
use asc_state::state::repos::{self, NewRepo, RemoteKind};
use asc_state::state::workspaces::{self, NewWorkspace};
use asc_state::{AscHome, RepoId, StateDb, WorkspaceId};

use super::common::{CliError, Ctx, Result};
use crate::dispatch::Program;
use crate::msg::Msg;
use crate::native::NativeSpec;

/// The first line of the `.ascignore` that `asc init` writes (SPEC §2.4.2).
pub const ASCIGNORE_HEADER: &str = "# asc: replaces .gitignore in this directory";

/// The default branch when neither `-b` nor `init.defaultBranch` names one (SPEC §6.5).
pub const DEFAULT_BRANCH: &str = "master";

#[derive(clap::Parser, Debug)]
#[command(name = "init", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The initial branch name.
    #[arg(short = 'b', long = "initial-branch")]
    pub branch: Option<String>,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    /// A bucket URL to keep objects in (SPEC §6.2); the first remote, `origin`.
    #[arg(long = "store")]
    pub store: Option<String>,
    /// Who holds the bucket's refs: `local` (default) or `shared` (SPEC §3.4).
    #[arg(long = "refs")]
    pub refs: Option<String>,
    /// The directory to initialise; the current directory by default.
    pub dir: Option<PathBuf>,
}

/// `init` serves its own flags only; `--bare` is not in the table.
pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "init",
    flags: &["-b", "--initial-branch", "-q", "--quiet", "--store", "--refs"],
    placeholders: &[],
}];

/// `-b`, else `init.defaultBranch`, else `master`; validated as `refs/heads/<name>`.
pub fn initial_branch(flag: Option<&str>, configured: Option<String>) -> Result<String> {
    let name = flag
        .map(str::to_owned)
        .or(configured)
        .unwrap_or_else(|| DEFAULT_BRANCH.to_owned());
    let valid = name != "HEAD"
        && !name.starts_with('-')
        && RefName::new(format!("refs/heads/{name}").into_bytes()).is_ok();
    if !valid {
        return Err(CliError::Msg(Msg::InvalidInitialBranch { name }));
    }
    Ok(name)
}

fn display(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Writes `.ascignore` when the directory has neither it nor a `.gitignore`. `git init` through
/// the shim (argv[0] = `git`) is never given asc-only additions.
fn write_ascignore(root: &Path, prog: Program) -> std::io::Result<()> {
    if prog == Program::Git {
        return Ok(());
    }
    // The root is the directory being initialised: no tracked path exists yet to lie behind a
    // link. A link named like either file counts as present, and `create_new` never follows one.
    let (ascignore, gitignore) = (root.join(".ascignore"), root.join(".gitignore")); // lint:allow-worktree-fs
    if std::fs::symlink_metadata(&ascignore).is_ok() || std::fs::symlink_metadata(&gitignore).is_ok() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(ascignore)?;
    std::io::Write::write_all(&mut file, format!("{ASCIGNORE_HEADER}\n").as_bytes())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let root = match &a.dir {
        Some(d) => ctx.cwd.join(d),
        None => ctx.cwd.clone(),
    };
    std::fs::create_dir_all(&root).map_err(|e| {
        CliError::Fatal(format!("cannot mkdir {}: {e}", display(&root)))
    })?;
    let root = std::path::absolute(&root)?;
    let quiet = a.quiet;
    let say = |m: Msg| {
        if !quiet {
            println!("{}", m.text(ctx.prog));
        }
    };

    // A directory that already is a workspace root is re-initialised in place.
    if root.join(".git").is_file()
        && let Ok(existing) = Projection::open(&root)
        && existing.toplevel() == root
    {
        write_ascignore(&root, ctx.prog)?;
        say(Msg::Reinitialized {
            path: format!("{}/", display(&root.join(".git"))),
        });
        return Ok(ExitCode::SUCCESS);
    }

    let branch = initial_branch(a.branch.as_deref(), ctx.config.get("init.defaultBranch"))?;
    let (home, db) = match ctx.ws_opt() {
        Some(ws) => (ws.home.clone(), ws.db.clone()),
        None => {
            let home = AscHome::locate()?;
            let db = StateDb::open(&home).await?;
            (home, db)
        }
    };
    let store = match (&a.store, &a.refs) {
        (None, None) => None,
        (None, Some(_)) => return Err(CliError::Fatal("asc: --refs needs --store".into())),
        (Some(typed), refs) => {
            let mode = match refs.as_deref() {
                None | Some("local") => crate::remote::url::RefsMode::Local,
                Some("shared") => crate::remote::url::RefsMode::Shared,
                Some(other) => return Err(CliError::Fatal(format!("asc: --refs takes local or shared, not '{other}'"))),
            };
            let spec = crate::remote::url::parse(typed, mode).map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
            let crate::remote::url::RemoteSpec::Bucket { url, .. } = &spec else {
                return Err(CliError::Fatal("asc: --store needs a bucket URL (s3:// or file://)".into()));
            };
            let stored = super::clone::stored_form(typed, &spec);
            // Reach the bucket (and probe a shared one) before anything is created.
            crate::remote::open_url(&db, "origin".into(), stored.clone(), true, mode, crate::remote::Intent::Write).await?;
            Some((url.clone(), stored, mode))
        }
    };
    let (repo_id, ws_id) = (RepoId::new(), WorkspaceId::new());
    let projection = Projection::create(&home, repo_id, ws_id, &root).map_err(|e| match e {
        ProjectionError::RealGitDir(p) => CliError::Fatal(format!(
            "asc: {} is a real git directory; asc cannot initialise over it",
            display(&p)
        )),
        other => CliError::Projection(other),
    })?;
    projection.write_head(&Head::Branch(branch.clone()))?;
    let name = root
        .file_name()
        .map_or_else(|| "repo".to_owned(), |n| n.to_string_lossy().into_owned());
    let key = repos::insert(
        &db,
        &NewRepo {
            id: repo_id,
            name,
            ns: uuid::Uuid::new_v4().as_bytes().to_vec(),
            remote_url: store.as_ref().map(|(url, _, _)| url.clone()),
            remote_kind: if store.is_some() { RemoteKind::Bucket } else { RemoteKind::LocalOnly },
            default_branch: branch.clone(),
            windows_safe: true,
        },
    )
    .await?;
    let path = root.display().to_string();
    workspaces::insert(
        &db,
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
    write_ascignore(&root, ctx.prog)?;
    if let Some((_, stored, mode)) = &store {
        let fresh = super::clone::open_at(ctx, root.clone()).await?;
        super::clone::write_remote_config(&fresh, "origin", stored, *mode).await?;
    }
    say(Msg::InitializedEmpty {
        path: format!("{}/", display(&root.join(".git"))),
    });
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_precedence_and_validation() {
        assert_eq!(initial_branch(None, None).unwrap(), "master");
        assert_eq!(initial_branch(None, Some("trunk".into())).unwrap(), "trunk");
        assert_eq!(initial_branch(Some("dev"), Some("trunk".into())).unwrap(), "dev");
        for bad in ["", "a..b", "HEAD", "-x", "a b", "x.lock"] {
            assert!(
                matches!(
                    initial_branch(Some(bad), None),
                    Err(CliError::Msg(Msg::InvalidInitialBranch { .. }))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn ascignore_only_without_gitignore_and_never_for_git() {
        let t = tempfile::tempdir().unwrap();
        write_ascignore(t.path(), Program::Git).unwrap();
        assert!(!t.path().join(".ascignore").exists());
        write_ascignore(t.path(), Program::Asc).unwrap();
        let text = std::fs::read_to_string(t.path().join(".ascignore")).unwrap();
        assert_eq!(text.lines().next(), Some(ASCIGNORE_HEADER));
        let u = tempfile::tempdir().unwrap();
        std::fs::write(u.path().join(".gitignore"), "x\n").unwrap();
        write_ascignore(u.path(), Program::Asc).unwrap();
        assert!(!u.path().join(".ascignore").exists());
    }
}
