//! Git hooks run by native commands (SPEC §9.4.1). [`Hook`] lists every hook asc runs with its
//! argv and stdin; [`Runner`] finds the hooks directory, spawns the script through
//! [`crate::child::command`] (with Git for Windows' `usr\bin\sh.exe` on Windows) and returns the
//! exit code. [`RefTxnRunner`] is the `reference-transaction` hook behind
//! `Ctx::update_local_refs`, so no command that moves a local ref has to run it itself.
#![allow(dead_code)] // The later units that run hooks use the rest of the table.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};

use asc_core::git::ObjectKind;
use asc_core::id::{GitOid, RefTarget};
use asc_git::realgit::RealGit;
use asc_state::state::git_map;
use asc_state::{AscHome, StateDb};
use asc_store::refs::RepoKey;
use async_trait::async_trait;

use crate::child;
use crate::cli::common::{self, CliError, Ctx};
use crate::cli::config::{ConfigSnapshot, LocalConfig};
use crate::cli::safe_dir::owned_by_current_user;
use crate::env::GitEnv;
use crate::ext::{HookRejected, RefChange, RefTxnHook, RefTxnView};

/// A hook failed to start.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("cannot run hook '{name}': {reason}")]
    Spawn { name: &'static str, reason: String },
    #[error("hook '{0}' did not finish")]
    Join(&'static str),
}

impl From<HookError> for CliError {
    fn from(e: HookError) -> Self {
        CliError::Fatal(e.to_string())
    }
}

/// The phase argument of `reference-transaction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefTxnState {
    Prepared,
    Committed,
    Aborted,
}

impl RefTxnState {
    fn word(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

/// The second argument of `prepare-commit-msg`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MsgSource {
    Message,
    Template,
    Merge,
    Squash,
    /// `commit <sha>`: the message was taken from that commit (`--amend`, `-c`, `-C`).
    Commit(GitOid),
}

/// Why `post-rewrite` runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteKind {
    Amend,
    Rebase,
}

/// One line of `pre-push` stdin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushLine {
    pub local_ref: String,
    pub local: GitOid,
    pub remote_ref: String,
    pub remote: GitOid,
}

/// One line of `reference-transaction` stdin, with git SHA-1s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefLine {
    pub old: GitOid,
    pub new: GitOid,
    pub name: String,
}

/// Every hook SPEC §9.4.1 lists that asc runs, with the data of its argv and stdin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hook {
    PreCommit,
    PrepareCommitMsg {
        file: PathBuf,
        source: Option<MsgSource>,
    },
    CommitMsg {
        file: PathBuf,
    },
    PostCommit,
    PostRewrite {
        kind: RewriteKind,
        pairs: Vec<(GitOid, GitOid)>,
    },
    PreMergeCommit,
    PostMerge {
        squash: bool,
    },
    PreRebase {
        upstream: String,
        branch: Option<String>,
    },
    PostCheckout {
        old: GitOid,
        new: GitOid,
        branch: bool,
    },
    PrePush {
        remote: String,
        url: String,
        lines: Vec<PushLine>,
    },
    ApplypatchMsg {
        file: PathBuf,
    },
    PreApplypatch,
    PostApplypatch,
    ReferenceTransaction {
        state: RefTxnState,
        lines: Vec<RefLine>,
    },
}

/// The null object id git prints for an absent ref value.
pub fn null_oid() -> GitOid {
    GitOid::from_bytes([0; 20])
}

impl Hook {
    /// The file name in the hooks directory.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PreCommit => "pre-commit",
            Self::PrepareCommitMsg { .. } => "prepare-commit-msg",
            Self::CommitMsg { .. } => "commit-msg",
            Self::PostCommit => "post-commit",
            Self::PostRewrite { .. } => "post-rewrite",
            Self::PreMergeCommit => "pre-merge-commit",
            Self::PostMerge { .. } => "post-merge",
            Self::PreRebase { .. } => "pre-rebase",
            Self::PostCheckout { .. } => "post-checkout",
            Self::PrePush { .. } => "pre-push",
            Self::ApplypatchMsg { .. } => "applypatch-msg",
            Self::PreApplypatch => "pre-applypatch",
            Self::PostApplypatch => "post-applypatch",
            Self::ReferenceTransaction { .. } => "reference-transaction",
        }
    }

    /// Whether a non-zero exit stops the command. `reference-transaction` stops it only in the
    /// `prepared` phase.
    pub fn can_abort(&self) -> bool {
        match self {
            Self::PreCommit
            | Self::PrepareCommitMsg { .. }
            | Self::CommitMsg { .. }
            | Self::PreMergeCommit
            | Self::PreRebase { .. }
            | Self::PrePush { .. }
            | Self::ApplypatchMsg { .. }
            | Self::PreApplypatch => true,
            Self::ReferenceTransaction { state, .. } => *state == RefTxnState::Prepared,
            _ => false,
        }
    }

    /// Whether `--no-verify` skips it: exactly the hooks git skips.
    pub fn skipped_by_no_verify(&self) -> bool {
        matches!(
            self,
            Self::PreCommit | Self::CommitMsg { .. } | Self::PrePush { .. } | Self::PreMergeCommit
        )
    }

    /// The arguments after the hook path.
    pub fn args(&self) -> Vec<OsString> {
        let os = |s: &str| OsString::from(s);
        match self {
            Self::PreCommit
            | Self::PostCommit
            | Self::PreMergeCommit
            | Self::PreApplypatch
            | Self::PostApplypatch => Vec::new(),
            Self::PrepareCommitMsg { file, source } => {
                let mut v = vec![file.clone().into_os_string()];
                match source {
                    None => {}
                    Some(MsgSource::Message) => v.push(os("message")),
                    Some(MsgSource::Template) => v.push(os("template")),
                    Some(MsgSource::Merge) => v.push(os("merge")),
                    Some(MsgSource::Squash) => v.push(os("squash")),
                    Some(MsgSource::Commit(sha)) => {
                        v.push(os("commit"));
                        v.push(OsString::from(sha.to_string()));
                    }
                }
                v
            }
            Self::CommitMsg { file } | Self::ApplypatchMsg { file } => {
                vec![file.clone().into_os_string()]
            }
            Self::PostRewrite { kind, .. } => vec![os(match kind {
                RewriteKind::Amend => "amend",
                RewriteKind::Rebase => "rebase",
            })],
            Self::PostMerge { squash } => vec![os(if *squash { "1" } else { "0" })],
            Self::PreRebase { upstream, branch } => {
                let mut v = vec![os(upstream)];
                v.extend(branch.iter().map(|b| os(b)));
                v
            }
            Self::PostCheckout { old, new, branch } => vec![
                OsString::from(old.to_string()),
                OsString::from(new.to_string()),
                os(if *branch { "1" } else { "0" }),
            ],
            Self::PrePush { remote, url, .. } => vec![os(remote), os(url)],
            Self::ReferenceTransaction { state, .. } => vec![os(state.word())],
        }
    }

    /// What the hook reads on stdin.
    pub fn stdin(&self) -> Vec<u8> {
        let mut out = String::new();
        match self {
            Self::PostRewrite { pairs, .. } => {
                for (old, new) in pairs {
                    out.push_str(&format!("{old} {new}\n"));
                }
            }
            Self::PrePush { lines, .. } => {
                for l in lines {
                    out.push_str(&format!(
                        "{} {} {} {}\n",
                        l.local_ref, l.local, l.remote_ref, l.remote
                    ));
                }
            }
            Self::ReferenceTransaction { lines, .. } => {
                for l in lines {
                    out.push_str(&format!("{} {} {}\n", l.old, l.new, l.name));
                }
            }
            _ => {}
        }
        out.into_bytes()
    }
}

/// The environment git sets for a hook (SPEC §9.4.1).
#[derive(Clone, Debug, Default)]
pub struct HookEnv {
    /// `GIT_INDEX_FILE`: `<gitdir>/index` (stage set 0) or `<gitdir>/asc-commit-index`.
    pub index_file: Option<PathBuf>,
    /// `GIT_EDITOR=:` for commit hooks when no editor runs.
    pub editor_null: bool,
    /// `GIT_PREFIX`: the current directory below the workspace root, with a trailing `/`.
    pub prefix: String,
    /// `GIT_REFLOG_ACTION`.
    pub reflog_action: Option<String>,
}

impl HookEnv {
    /// The environment for a hook run by `action` in `ctx`'s current directory.
    pub fn of(ctx: &Ctx, action: &str) -> Self {
        let prefix = ctx
            .ws_opt()
            .and_then(|w| ctx.cwd.strip_prefix(w.projection.toplevel()).ok())
            .map(|rel| {
                let text = rel.to_string_lossy().replace('\\', "/");
                if text.is_empty() {
                    text
                } else {
                    format!("{text}/")
                }
            })
            .unwrap_or_default();
        Self {
            index_file: None,
            editor_null: false,
            prefix,
            reflog_action: Some(
                ctx.env
                    .reflog_action
                    .clone()
                    .unwrap_or_else(|| action.to_owned()),
            ),
        }
    }

    pub fn with_index(mut self, index: PathBuf) -> Self {
        self.index_file = Some(index);
        self
    }

    pub fn without_editor(mut self) -> Self {
        self.editor_null = true;
        self
    }
}

/// What running a hook came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// There is no such hook, or it is not executable.
    Absent,
    Exit(i32),
}

/// Whether a hook that can abort did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Pass,
    /// The hook exited with this non-zero code.
    Reject(i32),
}

/// Runs hooks of one workspace.
#[derive(Clone, Debug)]
pub struct Runner {
    workdir: PathBuf,
    hooks_dir: PathBuf,
    shell: PathBuf,
}

/// What a bound runner needs beyond the runner: the state for git ids.
#[derive(Clone)]
struct Site {
    runner: Runner,
    db: StateDb,
}

static BOUND: OnceLock<Site> = OnceLock::new();

impl Runner {
    /// A runner for the workspace of `ctx`. The first call also binds it for
    /// [`RefTxnRunner`], so the hook sees the invocation's `-c` values and `PATH`.
    pub fn for_ctx(ctx: &Ctx) -> Result<Self, CliError> {
        let ws = ctx.ws()?;
        let runner = Self {
            workdir: ws.projection.toplevel().to_owned(),
            hooks_dir: hooks_dir_of(
                &ctx.config,
                ws.projection.toplevel(),
                ws.projection.common_dir(),
            ),
            shell: ctx.shell(),
        };
        let _ = BOUND.set(Site {
            runner: runner.clone(),
            db: ws.db.clone(),
        });
        Ok(runner)
    }

    /// The hooks directory (`core.hooksPath` against the workspace root, else `<common>/hooks`).
    pub fn hooks_dir(&self) -> &Path {
        &self.hooks_dir
    }

    fn script(&self, name: &str) -> Option<PathBuf> {
        let path = self.hooks_dir.join(name);
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                eprintln!(
                    "hint: The '{name}' hook was ignored because it's not set as executable."
                );
                return None;
            }
        }
        Some(path)
    }

    /// Whether the hook exists and would run.
    pub fn exists(&self, hook: &Hook) -> bool {
        self.script(hook.name()).is_some()
    }

    fn run_sync(&self, hook: &Hook, env: &HookEnv) -> Result<Outcome, HookError> {
        let name = hook.name();
        let Some(path) = self.script(name) else {
            return Ok(Outcome::Absent);
        };
        let fail = |reason: String| HookError::Spawn { name, reason };
        let mut cmd = if cfg!(windows) {
            let mut c = child::command(&self.shell).map_err(|m| fail(format!("{m:?}")))?;
            c.arg(&path);
            c
        } else {
            child::command(&path).map_err(|m| fail(format!("{m:?}")))?
        };
        cmd.args(hook.args()).current_dir(&self.workdir);
        if let Some(index) = &env.index_file {
            cmd.env("GIT_INDEX_FILE", index);
        }
        if env.editor_null {
            cmd.env("GIT_EDITOR", ":");
        }
        cmd.env("GIT_PREFIX", &env.prefix);
        if let Some(action) = &env.reflog_action {
            cmd.env("GIT_REFLOG_ACTION", action);
        }
        let stdin = hook.stdin();
        cmd.stdin(if stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        });
        // Hooks print to the user's stderr, as git runs them.
        cmd.stdout(Stdio::from(std::io::stderr()));
        let mut child = cmd.spawn().map_err(|e| fail(e.to_string()))?;
        if let Some(mut pipe) = child.stdin.take() {
            // A hook that does not read its stdin closes the pipe; that is not an error.
            let _ = pipe.write_all(&stdin);
        }
        let status = child.wait().map_err(|e| fail(e.to_string()))?;
        Ok(Outcome::Exit(status.code().unwrap_or(128 + 15)))
    }

    /// Runs `hook` and reports how it ended.
    pub async fn run(&self, hook: Hook, env: &HookEnv) -> Result<Outcome, HookError> {
        let name = hook.name();
        let (this, env) = (self.clone(), env.clone());
        tokio::task::spawn_blocking(move || this.run_sync(&hook, &env))
            .await
            .map_err(|_| HookError::Join(name))?
    }

    /// Runs `hook`; a non-zero exit of a hook that can abort is [`Gate::Reject`].
    pub async fn gate(&self, hook: Hook, env: &HookEnv) -> Result<Gate, HookError> {
        let aborts = hook.can_abort();
        Ok(match self.run(hook, env).await? {
            Outcome::Exit(code) if code != 0 && aborts => Gate::Reject(code),
            _ => Gate::Pass,
        })
    }
}

/// The git SHA-1 a ref value has in the git map, or the null id when it has none.
async fn git_id(db: &StateDb, repo: RepoKey, target: Option<RefTarget>) -> GitOid {
    let (asc, want) = match target {
        Some(RefTarget::Changeset(c)) => (*c.as_bytes(), ObjectKind::Commit),
        Some(RefTarget::Tag(t)) => (*t.as_bytes(), ObjectKind::Tag),
        None => return null_oid(),
    };
    git_map::asc_to_git(db, repo, &asc)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().find(|(_, k)| *k == want).map(|(o, _)| o))
        .unwrap_or_else(null_oid)
}

async fn ref_lines(db: &StateDb, tx: &RefTxnView) -> Vec<RefLine> {
    let mut out = Vec::with_capacity(tx.changes.len());
    for RefChange { name, old, new } in &tx.changes {
        out.push(RefLine {
            old: git_id(db, tx.repo, *old).await,
            new: git_id(db, tx.repo, *new).await,
            name: String::from_utf8_lossy(name.as_bytes()).into_owned(),
        });
    }
    out
}

/// The `reference-transaction` hook for `Ctx::update_local_refs` (registered in `main`).
#[derive(Default)]
pub struct RefTxnRunner {
    discovered: tokio::sync::OnceCell<Option<Site>>,
}

impl RefTxnRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// The runner a command bound, else one found from the process environment.
    async fn site(&self) -> Option<&Site> {
        if let Some(site) = BOUND.get() {
            return Some(site);
        }
        self.discovered.get_or_init(discover).await.as_ref()
    }

    async fn fire(&self, tx: &RefTxnView, state: RefTxnState) -> Result<(), HookRejected> {
        let Some(site) = self.site().await else {
            return Ok(());
        };
        let probe = Hook::ReferenceTransaction {
            state,
            lines: Vec::new(),
        };
        if !site.runner.exists(&probe) {
            return Ok(());
        }
        let hook = Hook::ReferenceTransaction {
            state,
            lines: ref_lines(&site.db, tx).await,
        };
        let env = HookEnv {
            reflog_action: std::env::var("GIT_REFLOG_ACTION").ok(),
            ..HookEnv::default()
        };
        match site.runner.gate(hook, &env).await {
            Ok(Gate::Reject(code)) => Err(HookRejected(format!(
                "reference-transaction hook exited with {code}"
            ))),
            Ok(Gate::Pass) => Ok(()),
            Err(e) => Err(HookRejected(e.to_string())),
        }
    }
}

/// Finds the workspace from the process environment. The ownership gate is not `Send`, so the
/// lookup runs to completion on a blocking thread.
async fn discover() -> Option<Site> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(find_site()))
        .await
        .ok()?
}

async fn find_site() -> Option<Site> {
    let env = GitEnv::from_process();
    let cwd = std::env::current_dir().ok()?;
    let get = |k: &str| std::env::var_os(k);
    let safe = crate::cli::config::safe_directories(&env, &get).ok()?;
    let home_of = || AscHome::locate().map_err(CliError::from);
    let gate = common::Gate {
        safe: &safe,
        home: &home_of,
        owned: &owned_by_current_user,
    };
    // The home is the one the gate admitted against, not one derived from the gitdir's path.
    let admitted = common::discover(&env, &cwd, &gate).await.ok()??;
    let local = LocalConfig {
        common_dir: admitted.projection.common_dir(),
        toplevel: admitted.projection.toplevel(),
    };
    let config =
        ConfigSnapshot::load_with(&env, &get, Some(&local), &owned_by_current_user).ok()?;
    let db = StateDb::open(&admitted.home).await.ok()?;
    Some(Site {
        runner: runner_for(&admitted, &config),
        db,
    })
}

/// The hooks directory: `core.hooksPath` from the trust-filtered `config` (against the
/// workspace root, as git runs hooks), else `<common>/hooks`. A config file the current user
/// does not own cannot set `core.hooksPath`; reading the raw file would let it name hooks.
fn hooks_dir_of(config: &ConfigSnapshot, toplevel: &Path, common_dir: &Path) -> PathBuf {
    match config.path("core.hooksPath") {
        Some(path) => toplevel.join(path),
        None => common_dir.join("hooks"),
    }
}

/// Real git from `asc.realGit` of the trust-filtered `config`, for the hook shell.
fn resolve_real_git(config: &ConfigSnapshot, home: &AscHome) -> Option<RealGit> {
    RealGit::resolve(config.path("asc.realGit"), Some(home.root())).ok()
}

/// The runner of an admitted workspace, configured only through the filtered `config`.
fn runner_for(admitted: &common::Admitted, config: &ConfigSnapshot) -> Runner {
    let real = resolve_real_git(config, &admitted.home);
    Runner {
        workdir: admitted.projection.toplevel().to_owned(),
        hooks_dir: hooks_dir_of(
            config,
            admitted.projection.toplevel(),
            admitted.projection.common_dir(),
        ),
        shell: child::shell(real.as_ref()),
    }
}

#[async_trait]
impl RefTxnHook for RefTxnRunner {
    async fn prepared(&self, tx: &RefTxnView) -> Result<(), HookRejected> {
        self.fire(tx, RefTxnState::Prepared).await
    }

    async fn committed(&self, tx: &RefTxnView) {
        let _ = self.fire(tx, RefTxnState::Committed).await;
    }

    async fn aborted(&self, tx: &RefTxnView) {
        let _ = self.fire(tx, RefTxnState::Aborted).await;
    }
}

/// Shared handle type for code that keeps a runner across awaits.
pub type SharedRunner = Arc<Runner>;

#[cfg(test)]
mod tests {
    use asc_git::projection::Projection;
    use asc_state::{RepoId, WorkspaceId};

    use super::*;

    /// A workspace in a fresh home whose common config holds `extra`, loaded the way
    /// `find_site` loads it, with `owned` as the ownership probe.
    fn runner_with_config(
        extra: &str,
        owned: &dyn Fn(&Path) -> std::io::Result<bool>,
    ) -> (tempfile::TempDir, Runner, ConfigSnapshot) {
        let dir = tempfile::tempdir().unwrap();
        let home = AscHome::open(dir.path().join("home")).unwrap();
        let root = dir.path().join("work");
        let projection =
            Projection::create(&home, RepoId::new(), WorkspaceId::new(), &root).unwrap();
        let path = projection.common_dir().join("config");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(extra);
        std::fs::write(&path, text).unwrap();
        let global = dir.path().join("gitconfig");
        std::fs::write(&global, "").unwrap();
        let get = move |k: &str| match k {
            "GIT_CONFIG_NOSYSTEM" => Some("1".into()),
            "GIT_CONFIG_GLOBAL" => Some(global.clone().into()),
            _ => None,
        };
        let local = LocalConfig {
            common_dir: projection.common_dir(),
            toplevel: projection.toplevel(),
        };
        let config =
            ConfigSnapshot::load_with(&GitEnv::from_vars(&get), &get, Some(&local), owned).unwrap();
        let admitted = common::Admitted { projection, home };
        let runner = runner_for(&admitted, &config);
        assert_eq!(runner.workdir, admitted.projection.toplevel());
        (dir, runner, config)
    }

    #[test]
    fn a_foreign_owned_local_config_cannot_redirect_hooks_or_real_git() {
        let dir = tempfile::tempdir().unwrap();
        let evil_git = dir.path().join("evil-git");
        std::fs::write(&evil_git, "").unwrap();
        let git_path = evil_git.display().to_string().replace('\\', "/");
        let extra = format!("[core]\n\thooksPath = evil-hooks\n[asc]\n\trealGit = {git_path}\n");
        let (tmp, runner, foreign) = runner_with_config(&extra, &|_| Ok(false));
        let common = std::fs::read_dir(tmp.path().join("home").join("repos"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join(".git");
        assert_eq!(
            runner.hooks_dir(),
            common.join("hooks"),
            "core.hooksPath from a foreign file is ignored"
        );
        let home = AscHome::open(tmp.path().join("home")).unwrap();
        let used = resolve_real_git(&foreign, &home).map(|r| r.path().to_owned());
        assert_ne!(
            used.as_deref(),
            Some(evil_git.as_path()),
            "asc.realGit from a foreign file is ignored"
        );
        let config = ConfigSnapshot::from_text(&extra);
        let trusted = resolve_real_git(&config, &home).expect("a config the user owns is honoured");
        assert_eq!(
            trusted.path(),
            evil_git,
            "control: the key works when the config is trusted"
        );
        let (_tmp, owned, _) = runner_with_config(&extra, &|_| Ok(true));
        assert_eq!(
            owned.hooks_dir(),
            owned.workdir.join("evil-hooks"),
            "control: an owned config may set it"
        );
    }

    fn oid(n: u8) -> GitOid {
        GitOid::from_bytes([n; 20])
    }

    #[test]
    fn argv_follows_git() {
        let h = Hook::PrepareCommitMsg {
            file: PathBuf::from("m"),
            source: Some(MsgSource::Commit(oid(1))),
        };
        let args: Vec<_> = h
            .args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["m", "commit", &oid(1).to_string()]);
        let h = Hook::PostCheckout {
            old: oid(1),
            new: oid(2),
            branch: true,
        };
        assert_eq!(h.args().len(), 3);
        assert_eq!(
            Hook::PostMerge { squash: true }.args(),
            [OsString::from("1")]
        );
    }

    #[test]
    fn stdin_lines() {
        let h = Hook::PostRewrite {
            kind: RewriteKind::Amend,
            pairs: vec![(oid(1), oid(2))],
        };
        assert_eq!(h.stdin(), format!("{} {}\n", oid(1), oid(2)).into_bytes());
        let h = Hook::ReferenceTransaction {
            state: RefTxnState::Prepared,
            lines: vec![RefLine {
                old: null_oid(),
                new: oid(3),
                name: "refs/heads/x".into(),
            }],
        };
        assert!(
            String::from_utf8(h.stdin())
                .unwrap()
                .ends_with(" refs/heads/x\n")
        );
    }

    #[test]
    fn no_verify_skips_exactly_git_hooks() {
        assert!(Hook::PreCommit.skipped_by_no_verify());
        assert!(Hook::PreMergeCommit.skipped_by_no_verify());
        assert!(!Hook::PostCommit.skipped_by_no_verify());
        assert!(
            !Hook::PreRebase {
                upstream: "u".into(),
                branch: None
            }
            .skipped_by_no_verify()
        );
    }

    #[test]
    fn only_the_prepared_phase_aborts() {
        let phase = |state| Hook::ReferenceTransaction {
            state,
            lines: Vec::new(),
        };
        assert!(phase(RefTxnState::Prepared).can_abort());
        assert!(!phase(RefTxnState::Committed).can_abort());
        assert!(!Hook::PostCommit.can_abort());
    }
}
