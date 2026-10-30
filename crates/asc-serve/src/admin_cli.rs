//! `asc serve admin import|verify-import|verify|outbox drain|set-mode` (PLAN O03, SPEC §14.3).
//!
//! The operator's side of the ASC cutover. The `asc` binary parses the flags and prints
//! [`Report::text`]; the logic lives here so it is tested with the server. Every command talks
//! to the database and the store named by `asc-serve.toml`, never to a running server, so a
//! cutover step can run while the server is stopped or frozen.
//!
//! * `import` is the only way git history reaches a server (SPEC §8.4): it runs the mirror
//!   engine's fetch and import over the repository's configured source, creating the
//!   repository in mirror mode with `--init`.
//! * `verify-import` re-derives the git commit of every commit in the mirror copy and compares
//!   it with git. Refs the importer skipped (SPEC §14.1 step 6) are listed but are not
//!   mismatches. The report ends with `mismatches: <n>`; a non-zero count fails the command.
//! * `verify --all` runs the `RepoAdmin.verify` closure and git-map check on every repository.
//! * `outbox drain --wait` blocks until the running server has delivered every queued effect.
//! * `set-mode` flips `repos.mode`, `git_reads` and `bridge`, and refuses the flips that
//!   lose data: to `mirror` while the outbox holds undelivered rows, and to `primary` before a
//!   clean import.
//!
//! Nothing here prompts, and nothing takes a secret on the command line: mirror sources come
//! from the config, and `--source` carries a URL without credentials.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use asc_core::id::{ChangesetId, GitOid};
use asc_git::realgit::{RealGit, RealGitError};
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::server::{AdminHandler, CallCtx, RepoName, wire};
use sqlx::Row;
use tokio::io::AsyncWriteExt;

use crate::admin::AdminService;
use crate::config::ServeConfig;
use crate::migrate;
use crate::mirror::{Mirror, MirrorConfig, MirrorError, MirrorTarget};
use crate::registry::{NewRepo, Registry, RegistryError, RepoMode};
use crate::update::PipelineHooks;

/// Connections the CLI opens.
const CLI_DB_CONNECTIONS: u32 = 4;
/// `git_map.kind` of a commit row (SPEC §3.3).
const KIND_COMMIT: i64 = 3;
/// How many mismatching commits a report names; the rest are counted.
const MAX_NAMED: usize = 20;
/// Most refs and most commits one `verify-import` reads from git.
const MAX_REFS: usize = 1_000_000;
const MAX_COMMITS: usize = 20_000_000;
/// Longest a git process may run.
const GIT_TIMEOUT: Duration = Duration::from_secs(1800);
/// The default wait of `outbox drain --wait`.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How often `outbox drain --wait` looks at the outbox.
pub const DEFAULT_DRAIN_POLL: Duration = Duration::from_secs(1);

/// What the admin commands can fail with. A failed check is a [`Report`] with `ok = false`,
/// not an error: errors mean the command could not run.
#[derive(Debug, thiserror::Error)]
pub enum AdminCliError {
    #[error("{0}")]
    Invalid(String),
    #[error("repository {0} is not known (use --init with `import` to create it)")]
    NotFound(String),
    #[error("mirror mode is unavailable: set `cache.disk_dir` and install git")]
    NoMirror,
    #[error("no mirror copy of {0} exists; run `asc serve admin import --repo {0}` first")]
    NoCopy(String),
    #[error("git: {0}")]
    Git(String),
    #[error(transparent)]
    Mirror(#[from] MirrorError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Migrate(#[from] crate::migrate::MigrateError),
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    Status(String),
    #[error(transparent)]
    Pack(#[from] crate::pack_service::PackError),
}

impl From<RealGitError> for AdminCliError {
    fn from(e: RealGitError) -> Self {
        Self::Git(e.to_string())
    }
}

/// The result of a command: text for stdout and whether the process exits 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub text: String,
    pub ok: bool,
}

impl Report {
    fn new(text: String, ok: bool) -> Self {
        Self { text, ok }
    }
}

/// The `set-mode` flags.
#[derive(Debug, Clone, Default)]
pub struct SetMode {
    pub repo: String,
    /// `mirror` or `primary`.
    pub mode: Option<String>,
    /// `git` or `asc`.
    pub git_reads: Option<String>,
    /// `off`, `lazy` or `eager`.
    pub bridge: Option<String>,
    /// Skips the import check of a flip to `primary`. The outbox check has no override.
    pub force: bool,
}

/// The server pieces the commands use.
pub struct AdminCli {
    registry: Arc<Registry>,
    mirror: Option<Arc<Mirror>>,
    mirror_dir: Option<PathBuf>,
    git: Option<RealGit>,
    service: Arc<AdminService>,
    packs: Arc<crate::pack_service::PackService>,
}

/// What `pack` does.
#[derive(Debug, Clone)]
pub enum PackCmd {
    /// One maintenance pass; `force` packs below `min_loose` too.
    Run { force: bool },
    /// Loose small objects, packs, bytes and dead bytes.
    Status,
    /// Checks every live pack against its index and every value against its key.
    Verify,
    /// Rebuilds the index from the pack files.
    Reindex,
}

/// Opens the database and the store of `cfg`. The schema must already be migrated.
pub async fn open(cfg: &ServeConfig) -> Result<AdminCli, AdminCliError> {
    let (Some(db), Some(store)) = (&cfg.db, &cfg.store) else {
        return Err(AdminCliError::Invalid(
            "`db` and `store` are required".into(),
        ));
    };
    let pool = migrate::connect(db, CLI_DB_CONNECTIONS).await?;
    migrate::ensure_current(&pool).await?;
    let registry = Arc::new(Registry::new(pool, store)?);
    registry.set_pack_config(
        cfg.pack
            .to_config()
            .map_err(|e| AdminCliError::Invalid(e.to_string()))?,
    );
    let git = RealGit::resolve(None, None).ok();
    let mirror_cfg = MirrorConfig::from_serve(cfg)?;
    let mirror_dir = mirror_cfg.as_ref().map(|c| c.dir.clone());
    let mirror = match (mirror_cfg, git.clone()) {
        (Some(mc), Some(g)) => Some(Arc::new(Mirror::new(
            registry.clone(),
            &PipelineHooks::default(),
            g,
            mc,
        ))),
        _ => None,
    };
    Ok(AdminCli::new(registry, mirror, mirror_dir, git))
}

fn name_of(repo: &str) -> Result<RepoName, AdminCliError> {
    RepoName::parse(repo).map_err(|e| AdminCliError::Invalid(format!("repository {repo:?}: {e}")))
}

impl AdminCli {
    pub fn new(
        registry: Arc<Registry>,
        mirror: Option<Arc<Mirror>>,
        mirror_dir: Option<PathBuf>,
        git: Option<RealGit>,
    ) -> Self {
        let service = AdminService::new(registry.clone(), mirror.clone());
        let packs = Arc::new(crate::pack_service::PackService::new(
            registry.clone(),
            Arc::new(PipelineHooks::default()),
        ));
        Self {
            registry,
            mirror,
            mirror_dir,
            git,
            service,
            packs,
        }
    }

    // --------------------------------------------------------------------------------- pack

    /// `pack run|status|verify|reindex`, for `--repo <r>` (repeatable) or `--all`.
    pub async fn pack(
        &self,
        cmd: &PackCmd,
        repos: &[String],
        all: bool,
    ) -> Result<Report, AdminCliError> {
        if all != repos.is_empty() {
            return Err(AdminCliError::Invalid(
                "name the repositories with --repo, or use --all, not both".into(),
            ));
        }
        let names: Vec<String> = if all {
            sqlx::query_scalar("SELECT name FROM repos ORDER BY id")
                .fetch_all(self.registry.pool())
                .await?
        } else {
            repos.to_vec()
        };
        let now = std::time::SystemTime::now();
        let mut text = String::new();
        let mut ok = true;
        for n in names {
            let name = name_of(&n)?;
            match cmd {
                PackCmd::Run { force } => {
                    let r = self.packs.run_repo(&name, now, *force).await?;
                    let _ = writeln!(
                        text,
                        "{n}: found {} loose, packed {} into {} pack(s), deleted {} loose; \
                         merged {} pack(s), compacted {}, emptied {}; removed {} retired file(s) \
                         and {} orphan(s); {} damaged{}",
                        r.loose_found,
                        r.packed,
                        r.packs_written,
                        r.loose_deleted,
                        r.merged,
                        r.compacted,
                        r.emptied,
                        r.deleted,
                        r.orphans,
                        r.damaged,
                        if r.below_threshold {
                            " (below [pack] min_loose: left alone)"
                        } else {
                            ""
                        }
                    );
                    ok &= r.damaged == 0;
                }
                PackCmd::Status => {
                    let s = self.packs.status(&name).await?;
                    let _ = writeln!(
                        text,
                        "{n}: {} loose small object(s) ({} bytes); {} live pack(s) holding {} object(s) in {} bytes, {} dead; {} retired",
                        s.loose_small,
                        s.loose_small_bytes,
                        s.live_packs,
                        s.packed_objects,
                        s.pack_bytes,
                        s.dead_bytes,
                        s.retired_packs
                    );
                }
                PackCmd::Verify => {
                    let v = self.packs.verify(&name).await?;
                    let _ = writeln!(
                        text,
                        "{n}: {} pack(s), {} object(s), {} problem(s)",
                        v.packs,
                        v.objects,
                        v.problems.len()
                    );
                    for p in v.problems.iter().take(MAX_NAMED) {
                        let _ = writeln!(text, "  {p}");
                    }
                    ok &= v.problems.is_empty();
                }
                PackCmd::Reindex => {
                    let r = self.packs.reindex(&name).await?;
                    let _ = writeln!(
                        text,
                        "{n}: rebuilt the index from {} pack(s), {} object(s); {} pack file(s) left out",
                        r.packs,
                        r.objects,
                        r.skipped.len()
                    );
                    for s in r.skipped.iter().take(MAX_NAMED) {
                        let _ = writeln!(text, "  {s}");
                    }
                    ok &= r.skipped.is_empty();
                }
            }
        }
        Ok(Report::new(text, ok))
    }

    // ------------------------------------------------------------------------------ import

    /// `import --repo <r> [--init] [--source <url>]`.
    pub async fn import(
        &self,
        repo: &str,
        init: bool,
        source: Option<&str>,
    ) -> Result<Report, AdminCliError> {
        let name = name_of(repo)?;
        let mirror = self.mirror.as_ref().ok_or(AdminCliError::NoMirror)?;
        match self.registry.open(&name).await {
            Ok(_) => {}
            Err(RegistryError::NotFound(_)) if init => {
                self.registry
                    .create(
                        &name,
                        &NewRepo {
                            mode: RepoMode::Mirror,
                            ..NewRepo::default()
                        },
                    )
                    .await?;
            }
            Err(RegistryError::NotFound(_)) => return Err(AdminCliError::NotFound(repo.into())),
            Err(e) => return Err(e.into()),
        }
        if let Some(url) = source {
            // The operator typed this; a URL with credentials in it is refused.
            if url.contains('@') && url.contains("://") && !url.contains("ssh://") {
                return Err(AdminCliError::Invalid(
                    "--source must not carry credentials; the config supplies them".into(),
                ));
            }
            mirror.sources().set_configured(
                &name,
                MirrorTarget {
                    url: url.to_owned(),
                    token: None,
                },
            )?;
        }
        let done = mirror.sync(&name).await?;
        let mut text = format!(
            "imported {repo}: {} commits, {} refs set, {} refs deleted\n",
            done.commits, done.refs_set, done.refs_deleted
        );
        for line in &done.skipped {
            let _ = writeln!(text, "{line}");
        }
        Ok(Report::new(text, true))
    }

    // ------------------------------------------------------------------------ verify-import

    /// `verify-import --repo <r>...`: every git commit's export equals git.
    pub async fn verify_import(&self, repos: &[String]) -> Result<Report, AdminCliError> {
        if repos.is_empty() {
            return Err(AdminCliError::Invalid(
                "name at least one --repo (or use --all)".into(),
            ));
        }
        let mut text = String::new();
        let mut total = 0usize;
        for repo in repos {
            let (body, bad) = self.verify_one_import(repo).await?;
            text.push_str(&body);
            total += bad;
        }
        let _ = writeln!(text, "mismatches: {total}");
        Ok(Report::new(text, total == 0))
    }

    async fn verify_one_import(&self, repo: &str) -> Result<(String, usize), AdminCliError> {
        let name = name_of(repo)?;
        let entry = match self.registry.open(&name).await {
            Ok(e) => e,
            Err(RegistryError::NotFound(_)) => return Err(AdminCliError::NotFound(repo.into())),
            Err(e) => return Err(e.into()),
        };
        let dir = self
            .mirror_dir
            .as_ref()
            .map(|d| d.join(format!("{}.git", entry.key.0)))
            .filter(|d| d.join("HEAD").is_file())
            .ok_or_else(|| AdminCliError::NoCopy(repo.into()))?;
        let git = self.git.as_ref().ok_or(AdminCliError::NoMirror)?;
        let refs = git_out(
            git,
            &dir,
            &[
                "for-each-ref",
                "--format=%(objectname) %(objecttype) %(*objecttype) %(refname)",
            ],
            &[],
        )
        .await?;
        let (tips, skipped) = classify_refs(&refs)?;
        let commits = if tips.is_empty() {
            Vec::new()
        } else {
            let mut stdin = String::new();
            for t in &tips {
                let _ = writeln!(stdin, "{t}");
            }
            let out = git_out(git, &dir, &["rev-list", "--stdin"], stdin.as_bytes()).await?;
            parse_oids(&out)?
        };
        let ns = entry.ns.as_bytes().to_vec();
        let (mut bad, mut named) = (0usize, Vec::new());
        let mut note = |line: String| {
            bad += 1;
            if named.len() < MAX_NAMED {
                named.push(line);
            }
        };
        for oid in &commits {
            let mapped: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT asc FROM git_map WHERE ns = $1 AND kind = $2 AND git = $3",
            )
            .bind(ns.clone())
            .bind(KIND_COMMIT)
            .bind(oid.as_bytes().to_vec())
            .fetch_optional(self.registry.pool())
            .await?;
            let Some(mapped) = mapped else {
                note(format!("{oid}: not imported"));
                continue;
            };
            let Ok(arr) = <[u8; 32]>::try_from(mapped.as_slice()) else {
                note(format!("{oid}: the git map row is not a changeset id"));
                continue;
            };
            let cs = ChangesetId::from_bytes(arr);
            match entry.backend.engine().git_commit_of(cs).await {
                Ok(derived) if derived == *oid => {}
                Ok(derived) => note(format!("{oid}: exports as {derived}")),
                Err(e) => note(format!("{oid}: cannot be exported: {e}")),
            }
        }
        let mut text = format!(
            "{repo}: {} commits, {} skipped refs, {bad} mismatches\n",
            commits.len(),
            skipped.len()
        );
        for s in &skipped {
            let _ = writeln!(text, "  skipped (expected): {s}");
        }
        for n in &named {
            let _ = writeln!(text, "  mismatch: {n}");
        }
        if bad > named.len() {
            let _ = writeln!(text, "  ... and {} more", bad - named.len());
        }
        Ok((text, bad))
    }

    // ------------------------------------------------------------------------------ verify

    /// `verify --repo <r>...` or `verify --all`: the closure and git-map check.
    pub async fn verify(&self, repos: &[String], all: bool) -> Result<Report, AdminCliError> {
        if all != repos.is_empty() {
            return Err(AdminCliError::Invalid(
                "name the repositories with --repo, or use --all, not both or neither".into(),
            ));
        }
        let names: Vec<String> = if all {
            sqlx::query_scalar("SELECT name FROM repos ORDER BY name")
                .fetch_all(self.registry.pool())
                .await?
        } else {
            repos.to_vec()
        };
        let mut text = String::new();
        let mut ok = true;
        for repo in &names {
            let name = name_of(repo)?;
            let ctx = CallCtx {
                repo: name.clone(),
                id_mode: IdMode::Asc,
                principal: Principal {
                    id: "admin-cli".into(),
                },
                op_id: None,
                client: None,
                admin: Default::default(),
            };
            let req = wire::RepoAdminRequest {
                repo: Some(wire::RepoRef {
                    name: repo.clone(),
                    id_mode: IdMode::Asc as i32,
                }),
                op: Some(wire::repo_admin_request::Op::Verify(true)),
            };
            let report = match self.service.repo_admin(&ctx, req).await {
                Ok(r) => r.report,
                Err(s) => {
                    ok = false;
                    format!("FAILED: {}", s.message())
                }
            };
            if !report.starts_with("ok:") {
                ok = false;
            }
            let _ = writeln!(text, "{repo}: {report}");
        }
        let _ = writeln!(
            text,
            "verify: {} repositories, {}",
            names.len(),
            if ok { "ok" } else { "FAILED" }
        );
        Ok(Report::new(text, ok))
    }

    // ------------------------------------------------------------------------------ outbox

    async fn pending(&self, repo_id: Option<i64>) -> Result<i64, sqlx::Error> {
        let sql = match repo_id {
            Some(_) => {
                "SELECT CAST(COUNT(*) AS BIGINT) FROM outbox WHERE done_at IS NULL AND repo_id = $1"
            }
            None => "SELECT CAST(COUNT(*) AS BIGINT) FROM outbox WHERE done_at IS NULL AND $1 = 0",
        };
        sqlx::query_scalar(sql)
            .bind(repo_id.unwrap_or(0))
            .fetch_one(self.registry.pool())
            .await
    }

    async fn repo_id(&self, repo: &str) -> Result<i64, AdminCliError> {
        let name = name_of(repo)?;
        match self.registry.open(&name).await {
            Ok(e) => Ok(e.key.0),
            Err(RegistryError::NotFound(_)) => Err(AdminCliError::NotFound(repo.into())),
            Err(e) => Err(e.into()),
        }
    }

    /// `outbox drain [--wait] [--repo <r>]`: the running server delivers the rows; this waits
    /// for it to finish. Without `wait` it reports once.
    pub async fn outbox_drain(
        &self,
        repo: Option<&str>,
        wait: bool,
        timeout: Duration,
        poll: Duration,
    ) -> Result<Report, AdminCliError> {
        let id = match repo {
            Some(r) => Some(self.repo_id(r).await?),
            None => None,
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let left = match id {
                Some(i) => self.pending(Some(i)).await?,
                None => self.pending_all().await?,
            };
            if left == 0 {
                return Ok(Report::new("outbox: drained (0 pending)\n".into(), true));
            }
            if !wait || tokio::time::Instant::now() >= deadline {
                let mut text = format!("outbox: {left} pending\n");
                if let Some(err) = self.last_error(id).await? {
                    let _ = writeln!(text, "last error: {err}");
                }
                if wait {
                    text.push_str("timed out waiting for the server to deliver\n");
                }
                return Ok(Report::new(text, false));
            }
            tokio::time::sleep(poll).await;
        }
    }

    async fn pending_all(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM outbox WHERE done_at IS NULL")
            .fetch_one(self.registry.pool())
            .await
    }

    async fn last_error(&self, repo_id: Option<i64>) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT last_error FROM outbox WHERE done_at IS NULL AND last_error IS NOT NULL \
             AND ($1 < 0 OR repo_id = $1) ORDER BY id LIMIT 1",
        )
        .bind(repo_id.unwrap_or(-1))
        .fetch_optional(self.registry.pool())
        .await?;
        match row {
            Some(r) => Ok(r
                .try_get::<Option<String>, _>("last_error")?
                .map(|e| e.chars().take(300).collect::<String>())),
            None => Ok(None),
        }
    }

    // ---------------------------------------------------------------------------- set-mode

    /// `set-mode --repo <r> [--mode m] [--git-reads g] [--bridge b]`.
    pub async fn set_mode(&self, req: &SetMode) -> Result<Report, AdminCliError> {
        let one_of = |what: &str, v: &Option<String>, ok: &[&str]| -> Result<(), AdminCliError> {
            match v {
                Some(s) if !ok.contains(&s.as_str()) => Err(AdminCliError::Invalid(format!(
                    "{what} must be one of {}",
                    ok.join(", ")
                ))),
                _ => Ok(()),
            }
        };
        one_of("--mode", &req.mode, &["mirror", "primary"])?;
        one_of("--git-reads", &req.git_reads, &["git", "asc"])?;
        one_of("--bridge", &req.bridge, &["off", "lazy", "eager"])?;
        if req.mode.is_none() && req.git_reads.is_none() && req.bridge.is_none() {
            return Err(AdminCliError::Invalid(
                "give at least one of --mode, --git-reads, --bridge".into(),
            ));
        }
        let name = name_of(&req.repo)?;
        let id = self.repo_id(&req.repo).await?;
        let current: String = sqlx::query_scalar("SELECT mode FROM repos WHERE id = $1")
            .bind(id)
            .fetch_one(self.registry.pool())
            .await?;
        match req.mode.as_deref() {
            Some("mirror") if current != "mirror" => {
                // Back to mirror: the bridge must be current, so every effect is delivered.
                let left = self.pending(Some(id)).await?;
                if left > 0 {
                    return Ok(Report::new(
                        format!(
                            "refused: {left} outbox rows are undelivered; run \
                             `asc serve admin outbox drain --wait --repo {}` first\n",
                            req.repo
                        ),
                        false,
                    ));
                }
            }
            Some("primary") if current != "primary" && !req.force => {
                let row = sqlx::query(
                    "SELECT CAST(last_import AS BIGINT) AS at, last_error \
                     FROM mirror_state WHERE repo_id = $1",
                )
                .bind(id)
                .fetch_optional(self.registry.pool())
                .await?;
                let clean = match row {
                    Some(r) => {
                        r.try_get::<i64, _>("at")? > 0
                            && r.try_get::<Option<String>, _>("last_error")?.is_none()
                    }
                    None => false,
                };
                if !clean {
                    return Ok(Report::new(
                        format!(
                            "refused: {} has no clean import; run the final \
                             `asc serve admin import` and `verify-import` first\n",
                            req.repo
                        ),
                        false,
                    ));
                }
            }
            _ => {}
        }
        let mut tx = self.registry.pool().begin().await?;
        let mut said = String::new();
        if let Some(v) = &req.mode {
            sqlx::query("UPDATE repos SET mode = $1 WHERE id = $2")
                .bind(v.clone())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            let _ = writeln!(said, "{}: mode = {v}", req.repo);
        }
        if let Some(v) = &req.git_reads {
            // `forgejo_reads` is the pre-rename column; keep it in step until a release drops it.
            sqlx::query("UPDATE repos SET git_reads = $1, forgejo_reads = $1 WHERE id = $2")
                .bind(v.clone())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            let _ = writeln!(said, "{}: git_reads = {v}", req.repo);
        }
        if let Some(v) = &req.bridge {
            sqlx::query("UPDATE repos SET bridge = $1 WHERE id = $2")
                .bind(v.clone())
                .bind(id)
                .execute(&mut *tx)
                .await?;
            let _ = writeln!(said, "{}: bridge = {v}", req.repo);
        }
        tx.commit().await?;
        self.registry.forget(&name);
        Ok(Report::new(said, true))
    }
}

// ------------------------------------------------------------------------------ git helpers

async fn git_out(
    git: &RealGit,
    dir: &Path,
    args: &[&str],
    stdin: &[u8],
) -> Result<Vec<u8>, AdminCliError> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut cmd = tokio::process::Command::from(git.command()?);
    cmd.arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| AdminCliError::Git(e.to_string()))?;
    let mut pipe = child.stdin.take();
    let input = stdin.to_vec();
    let feed = tokio::spawn(async move {
        if let Some(p) = pipe.as_mut() {
            let _ = p.write_all(&input).await;
        }
        drop(pipe);
    });
    let out = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| AdminCliError::Git("timed out".into()))?
        .map_err(|e| AdminCliError::Git(e.to_string()))?;
    let _ = feed.await;
    if !out.status.success() {
        let err: String = String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(300)
            .collect();
        return Err(AdminCliError::Git(format!("git {}: {err}", args[0])));
    }
    Ok(out.stdout)
}

/// Splits `for-each-ref` output into the tips whose commits must verify and the refs the
/// importer cannot represent: a ref to a tree or blob, or a tag whose target is not a commit.
fn classify_refs(out: &[u8]) -> Result<(Vec<GitOid>, Vec<String>), AdminCliError> {
    let text = std::str::from_utf8(out).map_err(|_| AdminCliError::Git("non-UTF-8 refs".into()))?;
    let (mut tips, mut skipped) = (Vec::new(), Vec::new());
    for (i, line) in text.lines().enumerate() {
        if i >= MAX_REFS {
            return Err(AdminCliError::Git("too many refs".into()));
        }
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        let (oid, kind, peeled, refname) = match f.as_slice() {
            [o, k, r] => (*o, *k, "", *r),
            [o, k, p, r] => (*o, *k, *p, *r),
            _ => return Err(AdminCliError::Git("unexpected for-each-ref output".into())),
        };
        let parsed: GitOid = oid
            .parse()
            .map_err(|_| AdminCliError::Git("unexpected object id".into()))?;
        match (kind, peeled) {
            ("commit", _) | ("tag", "commit") => tips.push(parsed),
            ("tag", p) if !p.is_empty() => {
                skipped.push(format!(
                    "{refname} (points to a {})",
                    if p == "tag" { "tag" } else { p }
                ));
            }
            (k, _) => skipped.push(format!("{refname} (points to a {k})")),
        }
    }
    tips.sort();
    tips.dedup();
    Ok((tips, skipped))
}

fn parse_oids(out: &[u8]) -> Result<Vec<GitOid>, AdminCliError> {
    let text =
        std::str::from_utf8(out).map_err(|_| AdminCliError::Git("non-UTF-8 output".into()))?;
    let mut v = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        if v.len() >= MAX_COMMITS {
            return Err(AdminCliError::Git("too many commits".into()));
        }
        v.push(
            line.parse()
                .map_err(|_| AdminCliError::Git("unexpected commit id".into()))?,
        );
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::testutil::registry;

    fn git(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
        use std::io::Write as _;
        let real = RealGit::resolve(None, None).expect("git is installed");
        let mut cmd = real.command().unwrap();
        cmd.arg("--git-dir")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn push(up: &Path, parent: Option<&str>, file: &str, content: &str, refname: &str) -> String {
        let blob = git(up, &["hash-object", "-w", "--stdin"], content.as_bytes());
        let tree = git(
            up,
            &["mktree"],
            format!("100644 blob {blob}\t{file}\n").as_bytes(),
        );
        let mut args = vec!["commit-tree", tree.as_str(), "-m", content];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        let commit = git(up, &args, b"");
        git(up, &["update-ref", refname, &commit], b"");
        commit
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        up: PathBuf,
        cli: AdminCli,
    }

    async fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let up = tmp.path().join("up.git");
        let real = RealGit::resolve(None, None).expect("git is installed");
        assert!(
            real.command()
                .unwrap()
                .args(["init", "--bare", "--quiet", "--initial-branch=master"])
                .arg(&up)
                .status()
                .unwrap()
                .success()
        );
        let registry = registry().await;
        let dir = tmp.path().join("mirrors");
        let mc = MirrorConfig {
            dir: dir.clone(),
            forgejo_url: None,
            allowed_hosts: Vec::new(),
            interval: Duration::from_secs(60),
            fetch_timeout: Duration::from_secs(120),
        };
        let mirror = Arc::new(Mirror::new(
            registry.clone(),
            &PipelineHooks::default(),
            real.clone(),
            mc,
        ));
        let cli = AdminCli::new(registry, Some(mirror), Some(dir), Some(real));
        Fx { _tmp: tmp, up, cli }
    }

    impl Fx {
        fn src(&self) -> String {
            self.up.to_str().unwrap().to_owned()
        }

        async fn imported(&self) -> Report {
            self.cli
                .import("o/m", true, Some(&self.src()))
                .await
                .unwrap()
        }
    }

    fn history(up: &Path) -> String {
        let a = push(up, None, "a", "one", "refs/heads/master");
        let b = push(up, Some(&a), "a", "two", "refs/heads/master");
        // A tag of a tree cannot be represented (SPEC §14.1 step 6).
        let tree = git(up, &["rev-parse", &format!("{b}^{{tree}}")], b"");
        git(up, &["update-ref", "refs/tags/tree-tag", &tree], b"");
        b
    }

    #[tokio::test]
    async fn import_then_verify_import_reports_zero_mismatches_and_excludes_skipped_refs() {
        let f = fx().await;
        history(&f.up);
        let r = f.imported().await;
        assert!(r.ok && r.text.contains("2 commits"), "{}", r.text);
        let v = f.cli.verify_import(&["o/m".into()]).await.unwrap();
        assert!(v.ok, "{}", v.text);
        assert!(v.text.contains("mismatches: 0"), "{}", v.text);
        assert!(
            v.text
                .contains("skipped (expected): refs/tags/tree-tag (points to a tree)"),
            "{}",
            v.text
        );
    }

    #[tokio::test]
    async fn import_without_init_refuses_an_unknown_repository() {
        let f = fx().await;
        let e = f
            .cli
            .import("o/none", false, Some(&f.src()))
            .await
            .unwrap_err();
        assert!(matches!(e, AdminCliError::NotFound(_)), "{e}");
    }

    #[tokio::test]
    async fn import_refuses_a_source_with_credentials() {
        let f = fx().await;
        let e = f
            .cli
            .import("o/m", true, Some("https://user:pw@host/x.git"))
            .await
            .unwrap_err();
        assert!(matches!(e, AdminCliError::Invalid(_)), "{e}");
    }

    #[tokio::test]
    async fn verify_import_fails_on_a_commit_that_is_not_imported() {
        let f = fx().await;
        let tip = history(&f.up);
        f.imported().await;
        // A fetch that the importer has not seen: the mirror copy gains a commit.
        let dir = f.cli.mirror_dir.clone().unwrap().join("1.git");
        let _ = push(&dir, Some(&tip), "a", "three", "refs/heads/extra");
        let v = f.cli.verify_import(&["o/m".into()]).await.unwrap();
        assert!(!v.ok, "{}", v.text);
        assert!(v.text.contains("not imported"), "{}", v.text);
        assert!(v.text.contains("mismatches: 1"), "{}", v.text);
    }

    #[tokio::test]
    async fn verify_import_fails_when_the_git_map_points_at_another_changeset() {
        let f = fx().await;
        history(&f.up);
        f.imported().await;
        // Point one commit's map row at the other commit's changeset.
        let rows = sqlx::query("SELECT git, asc FROM git_map WHERE kind = 3 ORDER BY git")
            .fetch_all(f.cli.registry.pool())
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        let other: Vec<u8> = rows[1].try_get("asc").unwrap();
        let victim: Vec<u8> = rows[0].try_get("git").unwrap();
        sqlx::query("UPDATE git_map SET asc = $1 WHERE kind = 3 AND git = $2")
            .bind(other)
            .bind(victim)
            .execute(f.cli.registry.pool())
            .await
            .unwrap();
        let v = f.cli.verify_import(&["o/m".into()]).await.unwrap();
        assert!(!v.ok, "{}", v.text);
        assert!(v.text.contains("exports as"), "{}", v.text);
    }

    #[tokio::test]
    async fn verify_import_needs_a_mirror_copy() {
        let f = fx().await;
        f.cli
            .registry
            .create(&name_of("o/m").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let e = f.cli.verify_import(&["o/m".into()]).await.unwrap_err();
        assert!(matches!(e, AdminCliError::NoCopy(_)), "{e}");
    }

    #[tokio::test]
    async fn verify_all_checks_every_repository() {
        let f = fx().await;
        history(&f.up);
        f.imported().await;
        let ok = f.cli.verify(&[], true).await.unwrap();
        assert!(ok.ok && ok.text.contains("o/m: ok:"), "{}", ok.text);
        // Lose the git map: the closure is intact but nothing round-trips.
        sqlx::query("DELETE FROM git_map WHERE kind = 3")
            .execute(f.cli.registry.pool())
            .await
            .unwrap();
        let bad = f.cli.verify(&[], true).await.unwrap();
        assert!(!bad.ok && bad.text.contains("FAILED"), "{}", bad.text);
    }

    #[tokio::test]
    async fn verify_needs_all_or_repos_but_not_both() {
        let f = fx().await;
        assert!(f.cli.verify(&[], false).await.is_err());
        assert!(f.cli.verify(&["o/m".into()], true).await.is_err());
    }

    async fn queue(f: &Fx, repo_id: i64) {
        sqlx::query("INSERT INTO outbox (repo_id, op_id, effects, next_at) VALUES ($1, $2, $3, 0)")
            .bind(repo_id)
            .bind(vec![1u8; 16])
            .bind(b"[]".to_vec())
            .execute(f.cli.registry.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn outbox_drain_waits_until_the_server_delivers() {
        let f = fx().await;
        let e = f
            .cli
            .registry
            .create(&name_of("o/m").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        queue(&f, e.key.0).await;
        let quick = Duration::from_millis(20);
        let r = f
            .cli
            .outbox_drain(Some("o/m"), false, quick, quick)
            .await
            .unwrap();
        assert!(!r.ok && r.text.contains("1 pending"), "{}", r.text);
        let r = f
            .cli
            .outbox_drain(Some("o/m"), true, quick, quick)
            .await
            .unwrap();
        assert!(!r.ok && r.text.contains("timed out"), "{}", r.text);
        let pool = f.cli.registry.pool().clone();
        let later = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            sqlx::query("UPDATE outbox SET done_at = 1")
                .execute(&pool)
                .await
                .unwrap();
        });
        let r = f
            .cli
            .outbox_drain(None, true, Duration::from_secs(10), quick)
            .await
            .unwrap();
        later.await.unwrap();
        assert!(r.ok && r.text.contains("drained"), "{}", r.text);
    }

    fn mode(repo: &str, m: &str) -> SetMode {
        SetMode {
            repo: repo.into(),
            mode: Some(m.into()),
            ..SetMode::default()
        }
    }

    async fn column(f: &Fx, col: &str) -> String {
        let sql = match col {
            "mode" => "SELECT mode FROM repos WHERE name = 'o/m'",
            "bridge" => "SELECT bridge FROM repos WHERE name = 'o/m'",
            _ => "SELECT COALESCE(git_reads, forgejo_reads) FROM repos WHERE name = 'o/m'",
        };
        sqlx::query_scalar(sql)
            .fetch_one(f.cli.registry.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn set_mode_refuses_mirror_while_the_outbox_is_undrained() {
        let f = fx().await;
        let e = f
            .cli
            .registry
            .create(&name_of("o/m").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        queue(&f, e.key.0).await;
        let r = f.cli.set_mode(&mode("o/m", "mirror")).await.unwrap();
        assert!(!r.ok && r.text.contains("undelivered"), "{}", r.text);
        assert_eq!(column(&f, "mode").await, "primary");
        sqlx::query("UPDATE outbox SET done_at = 1")
            .execute(f.cli.registry.pool())
            .await
            .unwrap();
        let r = f.cli.set_mode(&mode("o/m", "mirror")).await.unwrap();
        assert!(r.ok, "{}", r.text);
        assert_eq!(column(&f, "mode").await, "mirror");
    }

    #[tokio::test]
    async fn set_mode_refuses_primary_before_a_clean_import() {
        let f = fx().await;
        history(&f.up);
        f.cli
            .registry
            .create(
                &name_of("o/m").unwrap(),
                &NewRepo {
                    mode: RepoMode::Mirror,
                    ..NewRepo::default()
                },
            )
            .await
            .unwrap();
        let r = f.cli.set_mode(&mode("o/m", "primary")).await.unwrap();
        assert!(!r.ok && r.text.contains("no clean import"), "{}", r.text);
        assert_eq!(column(&f, "mode").await, "mirror");
        f.imported().await;
        let r = f.cli.set_mode(&mode("o/m", "primary")).await.unwrap();
        assert!(r.ok, "{}", r.text);
        assert_eq!(column(&f, "mode").await, "primary");
    }

    #[tokio::test]
    async fn set_mode_sets_bridge_and_reads_and_rejects_unknown_values() {
        let f = fx().await;
        f.cli
            .registry
            .create(&name_of("o/m").unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let r = f
            .cli
            .set_mode(&SetMode {
                repo: "o/m".into(),
                bridge: Some("eager".into()),
                git_reads: Some("asc".into()),
                ..SetMode::default()
            })
            .await
            .unwrap();
        assert!(r.ok, "{}", r.text);
        assert_eq!(column(&f, "bridge").await, "eager");
        assert_eq!(column(&f, "git_reads").await, "asc");
        for bad in [
            SetMode {
                repo: "o/m".into(),
                bridge: Some("x'; DROP TABLE repos; --".into()),
                ..SetMode::default()
            },
            SetMode {
                repo: "o/m".into(),
                mode: Some("dual".into()),
                ..SetMode::default()
            },
            SetMode {
                repo: "o/m".into(),
                ..SetMode::default()
            },
        ] {
            assert!(matches!(
                f.cli.set_mode(&bad).await,
                Err(AdminCliError::Invalid(_))
            ));
        }
        assert_eq!(column(&f, "bridge").await, "eager");
    }
}
