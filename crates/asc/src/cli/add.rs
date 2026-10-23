//! `add` (SPEC §9.5): ingests work-tree content into `store/` with the §2.4.1 normalisation,
//! computes the git blob SHA-1, and writes the `stage` and `git_map` rows. The stage set
//! follows `GIT_INDEX_FILE` (`target`); `rm` and `mv` share its pathspecs and stage edits.

pub mod attrs;
pub mod convert;
mod scan;
pub mod spec;
pub mod target;

use std::ffi::OsString;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bytes::Bytes;
use asc_core::codec::Enveloped;
use asc_core::content::{Body, Ingested, ingest};
use asc_core::object::Kind;
use asc_engine::derive::GitMapRow;
use asc_state::state::git_map;
use asc_state::state::stage::Leaf;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_wc::SafeRoot;

use self::attrs::Attrs;
use self::scan::{Op, Opts};
use self::spec::Spec;
use self::target::{Edit, Target};
use super::add_patch;
use super::common::{CliError, Ctx, Result, WsCtx};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "add", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Stage removals and changes of tracked paths only.
    #[arg(short = 'u', long = "update")]
    pub update: bool,
    /// Stage new, changed and removed paths.
    #[arg(short = 'A', long = "all")]
    pub all: bool,
    /// Record only that new paths will be added later.
    #[arg(short = 'N', long = "intent-to-add")]
    pub intent: bool,
    /// Add ignored paths too.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
    /// Choose hunks interactively (`add -p`).
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,
    /// Context lines of `-p` hunks.
    #[arg(short = 'U', long = "unified")]
    pub unified: Option<usize>,
    #[arg(long = "inter-hunk-context")]
    pub inter_hunk_context: Option<usize>,
    pub pathspec: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "add",
    flags: &[
        "-u",
        "--update",
        "-A",
        "--all",
        "-N",
        "--intent-to-add",
        "-f",
        "--force",
        "-n",
        "--dry-run",
        "-v",
        "--verbose",
        "-p",
        "--patch",
        "-U",
        "--unified",
        "--inter-hunk-context",
    ],
    placeholders: &[],
}];

/// Whether git would print the hints of advice key `advice.<key>`.
pub fn advice(ctx: &Ctx, key: &str) -> bool {
    ctx.config.bool(&format!("advice.{key}")) != Some(false)
}

/// The directory the command runs in, below the work tree root: `""` at the root.
pub fn cwd_prefix(ctx: &Ctx, ws: &WsCtx) -> String {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    let rel = canon(&ctx.cwd)
        .strip_prefix(canon(ws.projection.toplevel()))
        .map(Path::to_owned);
    rel.map_or_else(
        |_| String::new(),
        |r| {
            r.to_string_lossy()
                .replace('\\', "/")
                .trim_matches('/')
                .to_owned()
        },
    )
}

/// The attribute lookup of the workspace (SPEC §2.4.1).
pub fn workspace_attrs(ctx: &Ctx, ws: &WsCtx) -> Result<Attrs> {
    let global = ctx.config.path("core.attributesFile").or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|h| h.join(".config")))
            .map(|c| c.join("git").join("attributes"))
    });
    Attrs::load(ws.projection.toplevel(), ws.projection.common_dir(), global)
        .map_err(|e| CliError::Fatal(format!("cannot read attributes: {e}")))
}

/// Prints git's hint lines for `advice.<key>`, unless the user turned them off.
pub fn print_hints(ctx: &Ctx, key: &str, lines: &[&str]) {
    if !advice(ctx, key) {
        return;
    }
    for line in lines {
        eprintln!("hint: {line}");
    }
    eprintln!("hint: Disable this message with \"git config set advice.{key} false\"");
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if a.all && a.update {
        return Err(CliError::Fatal(
            "options '-A' and '-u' cannot be used together".into(),
        ));
    }
    if a.patch {
        return add_patch::run(ctx, &a).await;
    }
    if a.pathspec.is_empty() && !a.all && !a.update {
        eprintln!("Nothing specified, nothing added.");
        print_hints(
            ctx,
            "addEmptyPathspec",
            &["Maybe you wanted to say 'git add .'?"],
        );
        return Ok(ExitCode::SUCCESS);
    }
    let prefix = cwd_prefix(ctx, ws);
    let spec = Spec::parse(&a.pathspec, &prefix, ws.projection.toplevel())?;
    let mut target = Target::open(ctx).await?;
    let mut attrs = workspace_attrs(ctx, ws)?;
    let opts = Opts {
        update: a.update,
        intent: a.intent,
        force: a.force,
    };
    let mut plan = scan::plan(ctx, &target, &spec, opts, &mut attrs).await?;
    plan.warn();

    if a.dry_run || a.verbose {
        for op in &plan.ops {
            match op {
                Op::Add { path, .. } => println!("add '{path}'"),
                Op::Remove(path) => println!("remove '{path}'"),
            }
        }
    }
    let mut code = ExitCode::SUCCESS;
    if !plan.ignored.is_empty() {
        eprintln!("The following paths are ignored by one of your .gitignore files:");
        for path in &plan.ignored {
            eprintln!("{path}");
        }
        print_hints(
            ctx,
            "addIgnoredFile",
            &["Use -f if you really want to add them."],
        );
        code = ExitCode::from(1);
    }
    if a.dry_run || plan.ops.is_empty() {
        return Ok(code);
    }
    execute(ctx, ws, &mut target, plan.ops).await?;
    Ok(code)
}

/// Ingests the planned paths and applies the staging in one step.
async fn execute(ctx: &Ctx, ws: &WsCtx, target: &mut Target, ops: Vec<Op>) -> Result<()> {
    let gate = SafeRoot::new(ws.projection.toplevel());
    let tmp = ws.home.tmp();
    let filemode = ctx.config.bool("core.filemode").unwrap_or(cfg!(unix));
    let core_symlinks = ctx.config.bool("core.symlinks").unwrap_or(cfg!(unix));
    let (mut edits, mut mapped) = (Vec::new(), Vec::new());
    for op in ops {
        match op {
            Op::Remove(path) => edits.push(Edit::Drop(path)),
            Op::Add {
                path,
                attrs,
                ita,
                prev,
            } => {
                let fail = |e: &dyn std::fmt::Display| {
                    eprintln!("error: open(\"{path}\"): {e}");
                    eprintln!("error: unable to index file '{path}'");
                    CliError::Fatal("adding files failed".into())
                };
                // The plan read this path already, but a link may have replaced a parent since.
                let abs = gate
                    .resolve(&path)
                    .ok_or_else(|| fail(&"beyond a symbolic link or not a valid name here"))?;
                let meta = std::fs::symlink_metadata(&abs).map_err(|e| fail(&e))?;
                let symlink = meta.file_type().is_symlink();
                let got: Ingested = if ita {
                    ingest(Cursor::new(Vec::new()))
                } else if symlink {
                    ingest(Cursor::new(
                        convert::link_target(&abs).map_err(|e| fail(&e))?,
                    ))
                } else {
                    convert::ingest_file(&abs, attrs.eol, &tmp)
                }
                .map_err(|e| fail(&e))?;
                let exec = convert::exec_bit(&meta).filter(|_| filemode);
                let kind = staged_kind(symlink, exec, prev, core_symlinks);
                store(ctx, ws, &got).await?;
                mapped.push(GitMapRow {
                    git: got.git,
                    kind: asc_core::git::ObjectKind::Blob,
                    asc: *got.file.as_bytes(),
                });
                let leaf = Leaf {
                    kind,
                    file: got.file,
                    size: got.size,
                    git: got.git,
                };
                edits.push(Edit::Put { path, leaf, ita });
            }
        }
    }
    git_map::insert_many(&ws.db, ws.repo.key, &mapped).await?;
    target.apply(ctx, edits).await
}

/// The kind a work tree entry is staged as: `exec` is its exec bit where the file system has one,
/// `prev` the kind it is staged as now (which decides where the work tree cannot say).
pub(crate) fn staged_kind(
    symlink: bool,
    exec: Option<bool>,
    prev: Option<Kind>,
    core_symlinks: bool,
) -> Kind {
    match (symlink, exec, prev) {
        (true, ..) => Kind::Symlink,
        (false, _, Some(Kind::Symlink)) if !core_symlinks => Kind::Symlink,
        (false, Some(x), _) => {
            if x {
                Kind::Exec
            } else {
                Kind::File
            }
        }
        (false, None, Some(Kind::Exec)) => Kind::Exec,
        (false, None, _) => Kind::File,
    }
}

/// Writes the file's objects to the object stack: `store/` takes new objects (SPEC §3.5).
pub(crate) async fn store(_ctx: &Ctx, ws: &WsCtx, got: &Ingested) -> Result<()> {
    match &got.body {
        Body::Inline(bytes) => {
            ws.objects
                .put(&ObjKey::File(got.file), encode_stored(bytes))
                .await?;
        }
        Body::Chunked { list, chunks } => {
            for (id, data) in chunks {
                ws.objects
                    .put(&ObjKey::Chunk(*id), encode_stored(data))
                    .await?;
            }
            let envelope = Bytes::from(list.to_envelope());
            ws.objects
                .put(&ObjKey::ChunkList(got.file), encode_stored(&envelope))
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use asc_git::projection::Projection;
    use asc_state::state::repos::{self, NewRepo, RemoteKind};
    use asc_state::state::workspaces::{self, Mode, NewWorkspace};
    use asc_state::{AscHome, RepoId, StateDb, WorkspaceId};

    use super::*;
    use crate::cli::common::Gate;
    use crate::cli::safe_dir::{SafeDirectory, owned_by_current_user};
    use crate::dispatch::Program;
    use crate::env::GitEnv;
    use crate::ext::{self, SinkResult, SyntheticIndexSink};

    struct Log(Arc<Mutex<Vec<Vec<String>>>>);

    #[async_trait]
    impl SyntheticIndexSink for Log {
        async fn on_stage_changed(
            &self,
            _: WorkspaceId,
            paths: &[asc_core::path::RepoPath],
        ) -> SinkResult {
            self.0
                .lock()
                .unwrap()
                .push(paths.iter().map(ToString::to_string).collect());
            Ok(())
        }
    }

    /// A local-only repository with one plain workspace at `root`, as `asc init` makes it.
    async fn workspace(tmp: &Path) -> (AscHome, PathBuf) {
        let home = AscHome::open(tmp.join("asc-home")).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let (repo_id, ws) = (RepoId::new(), WorkspaceId::new());
        let repo = repos::insert(
            &db,
            &NewRepo {
                id: repo_id,
                name: "t".into(),
                ns: vec![7; 16],
                remote_url: None,
                remote_kind: RemoteKind::LocalOnly,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        let root = tmp.join("work");
        std::fs::create_dir_all(&root).unwrap();
        Projection::create(&home, repo_id, ws, &root).unwrap();
        let path = root.display().to_string();
        workspaces::insert(
            &db,
            &NewWorkspace {
                id: ws,
                repo,
                path: path.clone(),
                mount: path,
                mode: Mode::Plain,
                head: "refs/heads/master".into(),
                base_cs: None,
                sparse: None,
                ephemeral_until: None,
            },
        )
        .await
        .unwrap();
        (home, root)
    }

    #[tokio::test]
    async fn every_change_to_set_zero_reaches_the_synthetic_index_sink() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        assert!(
            ext::set_index_sink(Box::new(Log(calls.clone()))).is_ok(),
            "first registration"
        );
        let tmp = tempfile::tempdir().unwrap();
        let (home, root) = workspace(tmp.path()).await;
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("b.txt"), b"b").unwrap();
        let safe = SafeDirectory::default();
        let home = || Ok(home.clone());
        let gate = Gate {
            safe: &safe,
            home: &home,
            owned: &owned_by_current_user,
        };
        let env = GitEnv::from_vars(&|_| None);
        let ctx = Ctx::open_with(Program::Asc, env, root.clone(), &gate)
            .await
            .unwrap();
        let args = |paths: &[&str]| Args {
            update: false,
            all: false,
            intent: false,
            force: false,
            dry_run: false,
            verbose: false,
            patch: false,
            unified: None,
            inter_hunk_context: None,
            pathspec: paths.iter().map(OsString::from).collect(),
        };
        run(&ctx, args(&["a.txt", "b.txt"])).await.unwrap();
        std::fs::remove_file(root.join("a.txt")).unwrap();
        run(&ctx, args(&["a.txt"])).await.unwrap();
        run(
            &ctx,
            Args {
                dry_run: true,
                ..args(&["b.txt"])
            },
        )
        .await
        .unwrap();
        let log = calls.lock().unwrap().clone();
        assert_eq!(
            log,
            [vec!["a.txt", "b.txt"], vec!["a.txt"]],
            "a dry run changes nothing"
        );
    }
}
