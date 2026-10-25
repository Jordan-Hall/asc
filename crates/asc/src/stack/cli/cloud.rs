//! `asc cloud status|sync|join|list|release|purge` (SPEC §21.3, B16): the asc-only face of the
//! commit cloud.
//!
//! * `status` reads `state.db` only (cheap, no network): the mode, the workspace name, the upload
//!   queue (pending, held by the guard, failed, too large) and the last sync.
//! * `sync` and `release` run the upload pipeline and the head sync in-process and report every
//!   failure (SPEC §21.3.3 "No daemon"; nothing is hidden behind a background retry).
//! * `join <name>` points this machine at another cloud workspace of the same repository. The
//!   stored sync base belongs to the old workspace, so it is cleared: the first sync after a join
//!   is a union, never a deletion of heads the new workspace does not know.
//! * `list [--all] [--user <login>]` reads `Cloud.ListWorkspaces` and `Cloud.GetWorkspace`; the
//!   server enforces the §21.2.5 matrix. [`workspaces`] is also what `asc sl --cloud --user` shows.
//! * `purge <change>` deletes drafts on the server (`Cloud.Purge`, §21.3.7). It needs `--yes`,
//!   hides the local heads so the sync cannot upload them again, and says that copies already
//!   fetched cannot be recalled.
//!
//! Labels, change text and server messages are untrusted: they are printed through [`clean`].
//! Libraries never prompt: the only prompt is the credential one of `server::open_for`.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use bytes::Bytes;
use asc_core::change::{ChangeId, ChangePrefix};
use asc_core::id::{ChangesetId, HexPrefix};
use asc_engine::branchless::guard::{DenyList, Guard};
use asc_engine::branchless::upload::{DEFAULT_MAX_SIZE, Outcome, UploadOptions, UploadQueue as _};
use asc_engine::change::ChangeRows as _;
use asc_engine::sync::changes::ChangesExt;
use asc_rpc::client::CallError;
use asc_rpc::cloud as wire_cloud;
use asc_rpc::server::{MAX_MESSAGE, wire};
use asc_rpc::services::cloud_client::CloudClient;
use asc_rpc::services::repo_client::RepoClient;
use asc_state::StateDb;
use asc_state::changes::StateChanges;
use asc_state::drafts::{self, Drafts};
use asc_store::refs::RepoKey;
use asc_wc::ignore::IgnoreStack;
use serde::Serialize;
use tonic::codec::CompressionEncoding;

use super::sl::{clean, fatal};
use crate::branchless::mode::{CloudSetting, KEY_CLOUD_WORKSPACE, RepoPolicy, Settings};
use crate::cli::common::{CliError, Ctx, Result, WsCtx};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::daemon::cloud::{
    Clock, CloudSync, Facts, Gate, RpcCloudStore, StateRows, SystemClock, gate, workspace_name,
};
use crate::daemon::scheduler::parse_size;
use crate::native::NativeSpec;
use crate::remote::server::{self, ServerRemote};
use crate::remote::{StateBoundaries, StoreDemote, options, store_set};
use crate::stack::json::{self, VERSION};

/// Most queue rows read.
const MAX_QUEUE: usize = 100_000;
/// Most draft head rows read.
const MAX_HEADS: usize = 100_000;
/// Most queue rows `status` lists one by one.
const MAX_LISTED: usize = 50;
/// Most uploads one `sync` runs.
const MAX_DRAIN: usize = 10_000;
/// Most revisions one `release` or `purge` takes.
const MAX_REVS: usize = 1024;
/// Most workspaces and pages of heads one `list` reads.
const MAX_WORKSPACES: usize = 64;
const MAX_PAGES: usize = 16;
/// Most versions of one change a purge expands to.
const MAX_VERSIONS: usize = 256;
/// Most characters of a server message shown.
const MAX_MSG: usize = 300;
/// Default `asc.cloud.maxSyncAge` (SPEC §21.3 table): 14 days.
const DEFAULT_MAX_AGE_S: i64 = 14 * 86_400;
/// The reason `UploadQueue::set_state` stores for a held entry (B14).
const HELD_PREFIX: &str = "held by the pre-upload guard";
const FEATURE_CLOUD: &str = "cloud.v1";

#[derive(clap::Parser, Debug)]
#[command(
    name = "cloud",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum Cmd {
    /// The upload queue and the last sync, from `state.db`.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Upload the queue and sync the heads now, in this process.
    Sync {
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Use another cloud workspace of this repository.
    Join {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// The heads of a cloud workspace.
    List {
        /// Include heads older than `asc.cloud.maxSyncAge`.
        #[arg(long)]
        all: bool,
        /// Another user's workspaces (needs repo write).
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Upload held drafts anyway.
    Release {
        #[arg(required = true, num_args = 1..)]
        revs: Vec<String>,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete drafts on the server (secrets).
    Purge {
        #[arg(required = true, num_args = 1..)]
        revs: Vec<String>,
        /// Do it; without it the command only says what it would purge.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "cloud",
    flags: &["--json", "--all", "--user", "--remote", "--yes"],
    placeholders: &[],
}];

super::verb!(CLOUD, "cloud");

fn now() -> i64 {
    SystemClock.now()
}

fn db_err(e: impl std::fmt::Display) -> CliError {
    fatal(clean(&e.to_string()))
}

fn short(text: &str) -> String {
    clean(text).chars().take(MAX_MSG).collect()
}

fn call_err(what: &str, e: impl Into<CallError>) -> CliError {
    let e: CallError = e.into();
    CliError::Fatal(format!("asc: cloud {what}: {}", short(&e.to_string())))
}

fn settings(ctx: &Ctx) -> Result<Settings> {
    Settings::resolve(&ctx.config, &ctx.env, &|k| std::env::var(k).ok()).map_err(CliError::Fatal)
}

fn emit<T: Serialize>(ctx: &Ctx, as_json: bool, value: &T, text: String) -> Result<()> {
    let mut out = Output::open(ctx, "cloud", PagerWant::Never)?;
    if as_json {
        out.write_all(json::line(value).map_err(fatal)?.as_bytes())?;
    } else {
        out.write_all(text.as_bytes())?;
    }
    out.finish()?;
    Ok(())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Cmd::Status { json } => status(ctx, json).await,
        Cmd::Sync { remote, json } => sync(ctx, remote.as_deref(), json).await,
        Cmd::Join { name, json } => join(ctx, &name, json).await,
        Cmd::List { all, user, remote, json } => {
            list(ctx, all, user.as_deref(), remote.as_deref(), json).await
        }
        Cmd::Release { revs, remote, json } => {
            release(ctx, &revs, remote.as_deref(), json).await
        }
        Cmd::Purge { revs, yes, remote, json } => {
            purge(ctx, &revs, yes, remote.as_deref(), json).await
        }
    }
}

// ------------------------------------------------------------------------------ local rows

/// `cloud_queue.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Q {
    Pending,
    Held,
    Failed,
    TooLarge,
}

struct QRow {
    cs: ChangesetId,
    attempts: i64,
    state: Q,
    error: Option<String>,
}

/// The state a stored row stands for: B14 stores a held entry as `failed` with a fixed reason.
fn q_state(state: i64, error: Option<&str>) -> Q {
    match state {
        0 => Q::Pending,
        2 => Q::TooLarge,
        _ if error.is_some_and(|e| e.starts_with(HELD_PREFIX)) => Q::Held,
        _ => Q::Failed,
    }
}

async fn queue_rows(db: &StateDb, repo: RepoKey) -> Result<Vec<QRow>> {
    use sqlx::Row as _;
    let rows = sqlx::query(
        "SELECT cs, attempts, state, last_error FROM cloud_queue WHERE repo_id = ? \
         ORDER BY enq, cs LIMIT ?",
    )
    .bind(repo.0)
    .bind(MAX_QUEUE as i64)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let cs: Vec<u8> = r.try_get("cs").map_err(db_err)?;
        let cs: [u8; 32] = cs.try_into().map_err(|_| fatal("a corrupt cloud_queue row"))?;
        let state: i64 = r.try_get("state").map_err(db_err)?;
        let error: Option<String> = r.try_get("last_error").map_err(db_err)?;
        out.push(QRow {
            cs: ChangesetId::from_bytes(cs),
            attempts: r.try_get("attempts").map_err(db_err)?,
            state: q_state(state, error.as_deref()),
            error,
        });
    }
    Ok(out)
}

/// Puts every retryable row back in the queue now (SPEC §21.3.3: a permanent error is retried
/// "on `asc cloud sync`"). A held row is retried too: the guard holds it again if it still must
/// be. A `too_large` row stays (`push` uploads it, `release` overrides it).
async fn requeue(db: &StateDb, repo: RepoKey) -> Result<()> {
    sqlx::query("UPDATE cloud_queue SET state = 0, next_at = 0 WHERE repo_id = ? AND state IN (0, 1)")
        .bind(repo.0)
        .execute(db.pool())
        .await
        .map_err(db_err)?;
    Ok(())
}

// ------------------------------------------------------------------------------ status

#[derive(Serialize)]
struct QueueOut {
    pending: usize,
    held: usize,
    failed: usize,
    too_large: usize,
    total: usize,
    max: usize,
    full: bool,
}

#[derive(Serialize)]
struct EntryOut {
    cs: String,
    state: Q,
    attempts: i64,
    error: Option<String>,
}

#[derive(Serialize)]
struct StatusOut {
    version: u32,
    /// `off`, `synced`, `pending` or `attention` (something is held or failed).
    state: &'static str,
    summary: String,
    cloud: &'static str,
    branchless: bool,
    workspace: String,
    synced_version: Option<i64>,
    last_sync: Option<i64>,
    queue: QueueOut,
    entries: Vec<EntryOut>,
    visible_heads: usize,
    hidden_heads: usize,
}

fn cloud_word(c: CloudSetting) -> &'static str {
    match c {
        CloudSetting::Auto => "auto",
        CloudSetting::On => "on",
        CloudSetting::Off => "off",
    }
}

/// The headline of `status` (the VS Code status item reads it): `off`, `synced`, `N pending`.
fn headline(off: bool, q: &QueueOut) -> (&'static str, String) {
    if off {
        return ("off", "off".into());
    }
    if q.held + q.failed + q.too_large > 0 {
        let n = q.held + q.failed + q.too_large;
        return ("attention", format!("{n} need attention"));
    }
    if q.pending > 0 {
        return ("pending", format!("{} pending", q.pending));
    }
    ("synced", "synced".into())
}

async fn status(ctx: &Ctx, as_json: bool) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let s = settings(ctx)?;
    let d = Drafts::new(ws.repo.key);
    let rows = queue_rows(&ws.db, ws.repo.key).await?;
    let count = |q: Q| rows.iter().filter(|r| r.state == q).count();
    let max = ctx
        .config
        .get("asc.cloud.queueMax")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(drafts::QUEUE_MAX, |n| n.clamp(1, drafts::QUEUE_MAX));
    let queue = QueueOut {
        pending: count(Q::Pending),
        held: count(Q::Held),
        failed: count(Q::Failed),
        too_large: count(Q::TooLarge),
        total: rows.len(),
        max,
        full: rows.len() >= max,
    };
    let state = d.cloud_state(&ws.db).await.map_err(db_err)?;
    let visible = d.visible(&ws.db, MAX_HEADS).await.map_err(db_err)?.len();
    let hidden = d.hidden(&ws.db, MAX_HEADS).await.map_err(db_err)?.len();
    let off = s.cloud == CloudSetting::Off || !s.branchless;
    let (word, summary) = headline(off, &queue);
    let entries: Vec<EntryOut> = rows
        .iter()
        .filter(|r| r.state != Q::Pending)
        .take(MAX_LISTED)
        .map(|r| EntryOut {
            cs: r.cs.to_string(),
            state: r.state,
            attempts: r.attempts,
            error: r.error.as_deref().map(short),
        })
        .collect();
    let out = StatusOut {
        version: VERSION,
        state: word,
        summary,
        cloud: cloud_word(s.cloud),
        branchless: s.branchless,
        workspace: workspace_name(&s.workspace),
        synced_version: state.as_ref().map(|c| c.version),
        last_sync: state.as_ref().map(|c| c.last_sync),
        queue,
        entries,
        visible_heads: visible,
        hidden_heads: hidden,
    };
    let text = render_status(&out);
    emit(ctx, as_json, &out, text)?;
    Ok(ExitCode::SUCCESS)
}

fn render_status(s: &StatusOut) -> String {
    let mut t = format!("cloud: {}\n", s.summary);
    t += &format!("mode: asc.cloud={} asc.branchless={}\n", s.cloud, s.branchless);
    t += &format!("workspace: {}\n", s.workspace);
    match (s.synced_version, s.last_sync) {
        (Some(v), Some(at)) => t += &format!("last sync: v{v} at {at}\n"),
        _ => t += "last sync: never\n",
    }
    let q = &s.queue;
    t += &format!(
        "queue: {} pending, {} held, {} failed, {} too large ({} of {})\n",
        q.pending, q.held, q.failed, q.too_large, q.total, q.max
    );
    if q.full {
        t += "queue full: new heads stay local until it drains\n";
    }
    for e in &s.entries {
        let why = e.error.as_deref().unwrap_or("");
        t += &format!("  {:?} {} (attempts {}) {}\n", e.state, &e.cs[..e.cs.len().min(12)], e.attempts, why);
    }
    t += &format!("heads: {} visible, {} hidden\n", s.visible_heads, s.hidden_heads);
    t
}

// ------------------------------------------------------------------------------ the server

/// A connected asc server and what it advertises.
struct Link {
    server: Arc<ServerRemote>,
    features: Vec<String>,
}

impl Link {
    async fn open(ctx: &Ctx, remote: Option<&str>) -> Result<Self> {
        let server = server::open_for(ctx, remote).await?;
        let version = server
            .client()
            .server_version()
            .await
            .map_err(|e| call_err("server version", e))?;
        Ok(Self { server, features: version.features.clone() })
    }

    fn wire(&self) -> server::Wire {
        self.server.session().wire.clone()
    }

    fn require_cloud(&self) -> Result<()> {
        if self.features.iter().any(|f| f == FEATURE_CLOUD) {
            Ok(())
        } else {
            Err(CliError::Fatal("asc: this server has no commit cloud (cloud.v1)".into()))
        }
    }

    fn repo_ref(&self) -> wire::RepoRef {
        wire::RepoRef { name: self.wire().repo.clone(), id_mode: asc_rpc::IdMode::Asc.to_wire() }
    }

    fn cloud(&self) -> CloudClient<tonic::transport::Channel> {
        CloudClient::new(self.wire().channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(CompressionEncoding::Zstd)
    }

    /// The §21.3.3 gate, asked the way the daemon asks it: cloud off, a server without cloud, a
    /// repo with cloud off and a public repo all stop an upload.
    async fn check_upload(&self, s: &Settings) -> Result<()> {
        if s.cloud == CloudSetting::Off || !s.branchless {
            return Err(CliError::Fatal(
                "asc: cloud is off (asc.cloud=off or asc.branchless=false)".into(),
            ));
        }
        self.require_cloud()?;
        let w = self.wire();
        let conf = RepoClient::new(w.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(CompressionEncoding::Zstd)
            .repo_config(w.request(
                wire::RepoConfigRequest { repo: Some(self.repo_ref()), rev: String::new() },
                None,
            ))
            .await
            .map_err(|s| call_err("repo config", CallError::from(s)))?
            .into_inner();
        let facts = Facts {
            settings: s.clone(),
            policy: RepoPolicy {
                branchless: conf.branchless,
                cloud: conf.cloud,
                // Anything but private or internal counts as public (as in the daemon).
                public: !matches!(conf.visibility.as_str(), "private" | "internal"),
            },
            cloud_v1: true,
        };
        match gate(&facts) {
            Gate::On => Ok(()),
            Gate::Off => Err(CliError::Fatal(
                "asc: cloud is off for this repository (the server's branchless or cloud setting)"
                    .into(),
            )),
            Gate::Refused(why) => Err(CliError::Fatal(short(&why))),
        }
    }
}

// ------------------------------------------------------------------------------ upload

enum Job {
    /// Every due queue entry.
    Drain,
    /// These entries, past the guard and the size bound.
    Release(Vec<ChangesetId>),
}

#[derive(Default, Serialize)]
struct UploadOut {
    uploaded: Vec<String>,
    held: Vec<String>,
    too_large: Vec<String>,
    synced_version: Option<u64>,
}

fn ignored_fn(ctx: &Ctx, ws: &WsCtx) -> impl Fn(&asc_core::path::RepoPath) -> bool + use<> {
    let sources = crate::cli::check_ignore::ignore_sources(ctx, ws);
    let root = ws.projection.toplevel().to_path_buf();
    move |p: &asc_core::path::RepoPath| {
        let rel = String::from_utf8_lossy(p.as_bytes()).into_owned();
        let abs = root.join(&rel);
        let mut stack = IgnoreStack::new(&root, &sources);
        let mut dir = root.clone();
        let _ = stack.push_dir(&dir);
        if let Some(parent) = std::path::Path::new(&rel).parent() {
            for part in parent.components() {
                dir.push(part);
                let _ = stack.push_dir(&dir);
            }
        }
        stack.is_ignored(&abs, false)
    }
}

async fn upload(ctx: &Ctx, link: &Link, s: &Settings, job: Job) -> Result<UploadOut> {
    let ws = ctx.ws()?;
    let rows = StateRows::new(ws.db.clone(), ws.repo.key);
    let name = workspace_name(&s.workspace);
    let syncer = CloudSync::new(
        rows.clone(),
        RpcCloudStore::new(link.wire(), name.clone()),
        name,
        Arc::new(SystemClock),
    );
    let patterns = ctx.config.get_all("asc.cloud.denyPaths");
    let deny = if patterns.is_empty() {
        DenyList::default_list()
    } else {
        DenyList::new(patterns.iter().map(String::as_str))
    }
    .map_err(db_err)?;
    let guard = Guard::new(deny).map_err(db_err)?;
    let ignored = ignored_fn(ctx, ws);
    let boundaries = StateBoundaries::new(ws);
    let mut sync = options(ctx, &boundaries);
    let demote = StoreDemote(store_set(ctx, ws)?);
    sync.demote = Some(&demote);
    let ext = ChangesExt::new(StateChanges::new(ws.db.clone(), ws.repo.key), &link.features);
    sync.ext = &ext;
    let release = matches!(job, Job::Release(_));
    let opts = UploadOptions {
        guard: &guard,
        ignored: &ignored,
        max_size: ctx
            .config
            .get("asc.cloud.maxChangesetSize")
            .and_then(|v| parse_size(&v))
            .unwrap_or(DEFAULT_MAX_SIZE),
        skip_guard: release,
        ignore_size: release,
        sync,
        heads: &syncer,
    };
    let mut out = UploadOut::default();
    let mut tally = |cs: ChangesetId, outcome: &Outcome| match outcome {
        Outcome::Uploaded(_) => out.uploaded.push(cs.to_string()),
        Outcome::Held(_) => out.held.push(cs.to_string()),
        Outcome::TooLarge { .. } => out.too_large.push(cs.to_string()),
    };
    match &job {
        Job::Drain => {
            requeue(&ws.db, ws.repo.key).await?;
            for _ in 0..MAX_DRAIN {
                match ws
                    .engine
                    .drain_one(&*link.server, &rows, now(), &opts)
                    .await
                    .map_err(|e| call_err("upload", CallError::Protocol(e.to_string())))?
                {
                    Some((cs, outcome)) => tally(cs, &outcome),
                    None => break,
                }
            }
        }
        Job::Release(list) => {
            for &cs in list {
                let outcome = ws
                    .engine
                    .upload_draft(&*link.server, cs, &opts)
                    .await
                    .map_err(|e| call_err("release", CallError::Protocol(e.to_string())))?;
                if matches!(outcome, Outcome::Uploaded(_)) {
                    rows.delete(cs).await.map_err(db_err)?;
                }
                tally(cs, &outcome);
            }
        }
    }
    out.synced_version = Some(syncer.run().await.map_err(|e| {
        CliError::Fatal(format!("asc: cloud sync: {}", short(&e)))
    })?);
    Ok(out)
}

fn render_upload(o: &UploadOut) -> String {
    let id = |c: &String| c.chars().take(12).collect::<String>();
    let mut t = format!("uploaded {}\n", o.uploaded.len());
    for c in &o.held {
        t += &format!("held {}: asc cloud release {} uploads it anyway\n", id(c), id(c));
    }
    for c in &o.too_large {
        t += &format!("too large {}: asc push uploads it, or asc cloud release {}\n", id(c), id(c));
    }
    if let Some(v) = o.synced_version {
        t += &format!("synced to workspace version {v}\n");
    }
    t
}

async fn sync(ctx: &Ctx, remote: Option<&str>, as_json: bool) -> Result<ExitCode> {
    let s = settings(ctx)?;
    let link = Link::open(ctx, remote).await?;
    link.check_upload(&s).await?;
    let out = upload(ctx, &link, &s, Job::Drain).await?;
    let text = render_upload(&out);
    emit(ctx, as_json, &out, text)?;
    // A held entry is a result to act on, not a failure of the sync.
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------ resolution

/// A changeset and, when known, its change.
type Known = (ChangesetId, Option<ChangeId>);

/// The changesets `word` names among `pool`: a hex prefix of a changeset id, or a `k`-`z` prefix
/// of a change id. More than one change is ambiguous; a word that names nothing is an error.
fn pick(word: &str, pool: &[Known]) -> std::result::Result<Vec<Known>, String> {
    let shown = clean(word);
    let hits: Vec<Known> = if let Ok(p) = HexPrefix::parse(word) {
        pool.iter().filter(|(cs, _)| p.matches(cs.as_bytes())).copied().collect()
    } else if let Ok(p) = ChangePrefix::parse(word) {
        pool.iter().filter(|(_, ch)| ch.as_ref().is_some_and(|c| p.matches(c))).copied().collect()
    } else {
        return Err(format!(
            "asc: '{shown}' is not a change prefix (k-z, 4 or more) or a changeset prefix (hex, 4 or more)"
        ));
    };
    let mut distinct: Vec<Known> = Vec::new();
    for h in hits {
        if !distinct.iter().any(|d| d.0 == h.0) {
            distinct.push(h);
        }
    }
    let changes: std::collections::BTreeSet<_> = distinct.iter().filter_map(|d| d.1).collect();
    if distinct.len() > 1 && (changes.len() > 1 || HexPrefix::parse(word).is_ok()) {
        return Err(format!("asc: '{shown}' is ambiguous"));
    }
    if distinct.is_empty() {
        return Err(format!("asc: '{shown}' names no queued or draft head"));
    }
    Ok(distinct)
}

/// Every changeset the local draft heads and the upload queue know, with its change when a head
/// row has it.
async fn pool(ws: &WsCtx, queue: &[QRow]) -> Result<Vec<Known>> {
    let d = Drafts::new(ws.repo.key);
    let mut out: Vec<Known> = Vec::new();
    let heads = d.visible(&ws.db, MAX_HEADS).await.map_err(db_err)?;
    let hidden = d.hidden(&ws.db, MAX_HEADS).await.map_err(db_err)?;
    for h in heads.into_iter().chain(hidden) {
        out.push((h.cs, Some(h.change)));
    }
    for q in queue {
        if !out.iter().any(|k| k.0 == q.cs) {
            out.push((q.cs, None));
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ release

#[derive(Serialize)]
struct ReleaseOut {
    version: u32,
    #[serde(flatten)]
    upload: UploadOut,
}

async fn release(ctx: &Ctx, revs: &[String], remote: Option<&str>, as_json: bool) -> Result<ExitCode> {
    if revs.len() > MAX_REVS {
        return Err(CliError::Usage(format!("asc: more than {MAX_REVS} revisions")));
    }
    let ws = ctx.ws()?;
    let s = settings(ctx)?;
    let queue = queue_rows(&ws.db, ws.repo.key).await?;
    let pool = pool(ws, &queue).await?;
    let mut targets: Vec<ChangesetId> = Vec::new();
    for w in revs {
        for (cs, _) in pick(w, &pool).map_err(CliError::Fatal)? {
            // Only an entry that waits can be released.
            let waits = queue.iter().any(|q| q.cs == cs && q.state != Q::Pending);
            if !waits {
                return Err(CliError::Fatal(format!(
                    "asc: {} is not held, failed or too large in the upload queue",
                    clean(w)
                )));
            }
            if !targets.contains(&cs) {
                targets.push(cs);
            }
        }
    }
    let link = Link::open(ctx, remote).await?;
    link.check_upload(&s).await?;
    let upload = upload(ctx, &link, &s, Job::Release(targets)).await?;
    let text = render_upload(&upload);
    emit(ctx, as_json, &ReleaseOut { version: VERSION, upload }, text)?;
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------ join

#[derive(Serialize)]
struct JoinOut {
    version: u32,
    workspace: String,
    previous: String,
}

/// `[A-Za-z0-9._-]{1,64}` (SPEC §21.3 table). Refused rather than rewritten: a typo must not
/// silently join a different workspace.
fn valid_workspace(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

async fn join(ctx: &Ctx, name: &str, as_json: bool) -> Result<ExitCode> {
    if !valid_workspace(name) {
        return Err(CliError::Usage(format!(
            "asc: bad workspace name '{}': use 1 to 64 of A-Z a-z 0-9 . _ -",
            clean(name)
        )));
    }
    let ws = ctx.ws()?;
    let before = workspace_name(&settings(ctx)?.workspace);
    let rest: Vec<OsString> = ["--local", KEY_CLOUD_WORKSPACE, name].map(OsString::from).to_vec();
    crate::cli::config::run(ctx, crate::cli::config::Args { rest }).await?;
    let d = Drafts::new(ws.repo.key);
    if before != name {
        // The base and the version belong to the old workspace; reading them against the new one
        // would turn its missing heads into deletions.
        d.set_base(&ws.db, &[]).await.map_err(db_err)?;
        d.set_cloud_state(
            &ws.db,
            &drafts::CloudState { ws_name: name.to_owned(), version: 0, last_sync: 0 },
        )
        .await
        .map_err(db_err)?;
    }
    let out = JoinOut { version: VERSION, workspace: name.to_owned(), previous: before };
    let text = format!(
        "joined workspace {name}; the next sync (the daemon's, or asc cloud sync) merges its heads\n"
    );
    emit(ctx, as_json, &out, text)?;
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------ list

#[derive(Serialize)]
pub(crate) struct HeadOut {
    pub cs: String,
    pub change: Option<String>,
    /// Untrusted display text.
    pub labels: Vec<String>,
    pub added: i64,
    pub hidden_at: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct WorkspaceOut {
    pub owner: String,
    pub name: String,
    pub version: u64,
    pub heads: Vec<HeadOut>,
    pub omitted: u32,
}

#[derive(Serialize)]
struct ListOut {
    version: u32,
    workspaces: Vec<WorkspaceOut>,
}

fn max_age(ctx: &Ctx) -> i64 {
    ctx.config.get("asc.cloud.maxSyncAge").and_then(|v| parse_age(&v)).unwrap_or(DEFAULT_MAX_AGE_S)
}

/// `14d`, `36h`, `90m`, `30s`, `2w` or bare seconds.
fn parse_age(text: &str) -> Option<i64> {
    let t = text.trim();
    let (digits, unit) = match t.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => t.split_at(i),
        None => (t, "s"),
    };
    let n: i64 = digits.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// The cloud workspaces of `user` (`None`: the caller) with their heads. Heads older than
/// `asc.cloud.maxSyncAge` are left to `all`. This is `asc cloud list` and `asc sl --cloud --user`.
pub(crate) async fn workspaces(
    ctx: &Ctx,
    remote: Option<&str>,
    user: Option<&str>,
    all: bool,
) -> Result<Vec<WorkspaceOut>> {
    let link = Link::open(ctx, remote).await?;
    link.require_cloud()?;
    let owner = user.unwrap_or_default().to_owned();
    let w = link.wire();
    let listed = link
        .cloud()
        .list_workspaces(w.request(
            wire::ListWorkspacesRequest {
                repo: Some(link.repo_ref()),
                owner: owner.clone(),
                limit: MAX_WORKSPACES as u32,
                page_token: String::new(),
            },
            None,
        ))
        .await
        .map_err(|s| call_err("list", CallError::from(s)))?
        .into_inner();
    let age = if all { 0 } else { max_age(ctx) };
    let mut out = Vec::new();
    for info in listed.workspaces.iter().take(MAX_WORKSPACES) {
        let mut ws = WorkspaceOut {
            owner: short(&info.owner),
            name: short(&info.name),
            version: info.version,
            heads: Vec::new(),
            omitted: 0,
        };
        let mut token = String::new();
        for _ in 0..MAX_PAGES {
            let got = link
                .cloud()
                .get_workspace(w.request(
                    wire::GetWorkspaceRequest {
                        repo: Some(link.repo_ref()),
                        owner: info.owner.clone(),
                        name: info.name.clone(),
                        since_version: 0,
                        max_age_s: age,
                        include_hidden: all,
                        page_token: token.clone(),
                    },
                    None,
                ))
                .await
                .map_err(|s| call_err("workspace", CallError::from(s)))?
                .into_inner();
            let doc = wire_cloud::Workspace::from_response(&got, asc_rpc::IdMode::Asc)
                .map_err(|e| call_err("workspace", CallError::Protocol(e.to_string())))?;
            ws.version = doc.version;
            ws.omitted = ws.omitted.saturating_add(doc.omitted);
            for h in &doc.heads {
                let asc_rpc::ModeId::Asc(cs) = h.cs else { continue };
                ws.heads.push(HeadOut {
                    cs: cs.to_string(),
                    change: h.change.map(|c| c.to_string()),
                    labels: h.labels.iter().map(|l| clean(l.as_str())).collect(),
                    added: h.added,
                    hidden_at: (h.hidden_at != 0).then_some(h.hidden_at),
                });
            }
            if doc.next_page_token.is_empty() {
                break;
            }
            token = doc.next_page_token;
        }
        out.push(ws);
    }
    Ok(out)
}

pub(crate) fn render_workspaces(list: &[WorkspaceOut]) -> String {
    if list.is_empty() {
        return "no cloud workspaces\n".into();
    }
    let mut t = String::new();
    for w in list {
        t += &format!("workspace {} ({}) v{}, {} heads", w.name, w.owner, w.version, w.heads.len());
        if w.omitted > 0 {
            t += &format!(", {} older left out (--all)", w.omitted);
        }
        t.push('\n');
        for h in &w.heads {
            let cs: String = h.cs.chars().take(12).collect();
            let change = h.change.as_deref().map_or(String::from("-"), |c| c.chars().take(12).collect());
            t += &format!("  {change} {cs} added {}", h.added);
            if let Some(at) = h.hidden_at {
                t += &format!(" hidden {at}");
            }
            if !h.labels.is_empty() {
                t += &format!(" [{}]", h.labels.join(" "));
            }
            t.push('\n');
        }
    }
    t
}

async fn list(
    ctx: &Ctx,
    all: bool,
    user: Option<&str>,
    remote: Option<&str>,
    as_json: bool,
) -> Result<ExitCode> {
    let workspaces = workspaces(ctx, remote, user, all).await?;
    let text = render_workspaces(&workspaces);
    emit(ctx, as_json, &ListOut { version: VERSION, workspaces }, text)?;
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------ purge

#[derive(Serialize)]
struct PurgeOut {
    version: u32,
    dry_run: bool,
    targets: Vec<String>,
    purged: u32,
    refused: Vec<String>,
    unacked_tiers: Vec<String>,
}

async fn purge(
    ctx: &Ctx,
    revs: &[String],
    yes: bool,
    remote: Option<&str>,
    as_json: bool,
) -> Result<ExitCode> {
    if revs.len() > MAX_REVS {
        return Err(CliError::Usage(format!("asc: more than {MAX_REVS} revisions")));
    }
    let ws = ctx.ws()?;
    let queue = queue_rows(&ws.db, ws.repo.key).await?;
    let known = pool(ws, &queue).await?;
    let rows = StateChanges::new(ws.db.clone(), ws.repo.key);
    let mut targets: Vec<ChangesetId> = Vec::new();
    for w in revs {
        for (cs, change) in pick(w, &known).map_err(CliError::Fatal)? {
            if !targets.contains(&cs) {
                targets.push(cs);
            }
            // A change word names the change: every version of it goes, not only the head.
            if ChangePrefix::parse(w).is_ok()
                && let Some(ch) = change
            {
                for row in rows.by_change(ch, MAX_VERSIONS).await.map_err(db_err)? {
                    if !targets.contains(&row.cs) {
                        targets.push(row.cs);
                    }
                }
            }
        }
    }
    let ids: Vec<String> = targets.iter().map(ToString::to_string).collect();
    if !yes {
        let out = PurgeOut {
            version: VERSION,
            dry_run: true,
            targets: ids.clone(),
            purged: 0,
            refused: Vec::new(),
            unacked_tiers: Vec::new(),
        };
        let mut text = format!("would purge {} changesets from the server:\n", ids.len());
        for i in &ids {
            text += &format!("  {}\n", &i[..i.len().min(12)]);
        }
        text += "nothing was changed; run again with --yes to purge\n";
        emit(ctx, as_json, &out, text)?;
        return Ok(ExitCode::from(1));
    }
    let link = Link::open(ctx, remote).await?;
    link.require_cloud()?;
    let w = link.wire();
    let req = wire::PurgeRequest {
        repo: Some(link.repo_ref()),
        cs: targets.iter().map(|&c| Bytes::copy_from_slice(&asc_rpc::ModeId::Asc(c).to_wire())).collect(),
    };
    let got = link
        .cloud()
        .purge(w.request(req, None))
        .await
        .map_err(|s| call_err("purge", CallError::from(s)))?
        .into_inner();
    // The heads stay out of the next sync: the daemon would put them back on the server.
    let d = Drafts::new(ws.repo.key);
    let live: Vec<ChangesetId> = d
        .visible(&ws.db, MAX_HEADS)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|h| h.cs)
        .filter(|c| targets.contains(c))
        .collect();
    if !live.is_empty() {
        let mut tx = ws.db.begin().await.map_err(db_err)?;
        d.hide(&mut tx, &live, None, now(), drafts::QUEUE_MAX).await.map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
    }
    let refused: Vec<String> = got
        .refused
        .iter()
        .filter_map(|b| match asc_rpc::ModeId::<ChangesetId>::from_wire_in(b, asc_rpc::IdMode::Asc) {
            Ok(asc_rpc::ModeId::Asc(c)) => Some(c.to_string()),
            _ => None,
        })
        .collect();
    let out = PurgeOut {
        version: VERSION,
        dry_run: false,
        targets: ids,
        purged: got.purged,
        refused,
        unacked_tiers: got.unacked_tiers.iter().map(|t| short(t)).collect(),
    };
    let mut text = format!("purged {} changesets on the server\n", out.purged);
    for r in &out.refused {
        text += &format!("refused {} (not yours to purge, or already submitted)\n", &r[..r.len().min(12)]);
    }
    if !out.unacked_tiers.is_empty() {
        text += &format!(
            "cache tiers that have not yet evicted them: {}\n",
            out.unacked_tiers.join(", ")
        );
    }
    text += "copies already fetched to other machines cannot be recalled; rotate any secret\n";
    let refused_any = !out.refused.is_empty();
    emit(ctx, as_json, &out, text)?;
    Ok(if refused_any { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    fn ch(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }

    #[test]
    fn a_held_row_is_told_from_a_failed_one() {
        assert_eq!(q_state(0, None), Q::Pending);
        assert_eq!(q_state(2, Some("x")), Q::TooLarge);
        assert_eq!(q_state(1, Some("held by the pre-upload guard (asc cloud release uploads it)")), Q::Held);
        assert_eq!(q_state(1, Some("PERMISSION_DENIED")), Q::Failed);
        assert_eq!(q_state(1, None), Q::Failed);
    }

    #[test]
    fn the_headline_reads_the_queue() {
        let q = |p, h, f, t| QueueOut {
            pending: p,
            held: h,
            failed: f,
            too_large: t,
            total: p + h + f + t,
            max: 10,
            full: false,
        };
        assert_eq!(headline(false, &q(0, 0, 0, 0)).1, "synced");
        assert_eq!(headline(false, &q(3, 0, 0, 0)).1, "3 pending");
        assert_eq!(headline(false, &q(3, 1, 0, 0)).0, "attention");
        assert_eq!(headline(true, &q(3, 1, 0, 0)).1, "off");
    }

    #[test]
    fn words_pick_by_hex_or_change_prefix() {
        let pool = vec![(cs(0xab), Some(ch(0x12))), (cs(0xcd), Some(ch(0x34))), (cs(0xcc), None)];
        assert_eq!(pick("abab", &pool).unwrap(), vec![pool[0]]);
        // Two changesets share "c": hex prefix "cdcd" is exact, "cc" is too short.
        assert_eq!(pick("cdcd", &pool).unwrap(), vec![pool[1]]);
        assert!(pick("cc", &pool).is_err());
        assert!(pick("ffff", &pool).is_err());
        assert!(pick("not a word", &pool).is_err());
        // 0x12 bytes spell "lmlm…" in the k-z alphabet.
        let word = ch(0x12).to_string();
        assert_eq!(pick(&word[..6], &pool).unwrap(), vec![pool[0]]);
    }

    #[test]
    fn an_ambiguous_word_is_refused() {
        let pool = vec![(cs(0xab), None), (ChangesetId::from_bytes([0xab; 32]), None)];
        // The same changeset twice is one hit.
        assert_eq!(pick("abab", &pool).unwrap().len(), 1);
        let mut other = [0xab; 32];
        other[31] = 0;
        let pool = vec![(cs(0xab), None), (ChangesetId::from_bytes(other), None)];
        assert!(pick("abab", &pool).unwrap_err().contains("ambiguous"));
    }

    #[test]
    fn workspace_names_are_refused_not_rewritten() {
        assert!(valid_workspace("laptop-2.local_x"));
        assert!(!valid_workspace(""));
        assert!(!valid_workspace("a b"));
        assert!(!valid_workspace("a/b"));
        assert!(!valid_workspace(&"x".repeat(65)));
        assert!(!valid_workspace("name\u{1b}[31m"));
    }

    #[test]
    fn ages_parse() {
        assert_eq!(parse_age("14d"), Some(14 * 86_400));
        assert_eq!(parse_age("2w"), Some(14 * 86_400));
        assert_eq!(parse_age("90"), Some(90));
        assert_eq!(parse_age("x"), None);
        assert_eq!(parse_age("5y"), None);
    }

    #[test]
    fn untrusted_labels_are_printed_clean() {
        let w = WorkspaceOut {
            owner: "me".into(),
            name: "default".into(),
            version: 3,
            omitted: 0,
            heads: vec![HeadOut {
                cs: "ab".repeat(32),
                change: None,
                labels: vec![clean("evil\u{1b}[2Jlabel")],
                added: 5,
                hidden_at: None,
            }],
        };
        let t = render_workspaces(&[w]);
        assert!(!t.contains('\u{1b}'), "{t:?}");
        assert!(t.contains("evil?[2Jlabel"));
    }
}
