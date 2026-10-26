//! The commit-cloud uploader and sync tasks of the daemon (SPEC §21.3.1 to §21.3.4; B14).
//!
//! Two scheduler tasks share one [`CloudJob`]: `cloud_upload` drains the `cloud_queue` of every
//! repository every few seconds, and `cloud_sync` runs the Commit Cloud head sync every five
//! minutes. The upload pipeline is the engine's (B13) and the head merge is the engine's (B11);
//! this module owns what only the daemon knows:
//!
//! * the gate: nothing is sent unless the repo is an asc server repo with `cloud.v1`, branchless
//!   and `cloud` on, not public, and `asc.cloud` is not `off` ([`gate`]);
//! * the no-daemon rule: with `ASC_NO_DAEMON` no task is registered ([`extra_jobs`]);
//! * the retry policy of §21.3.3: transient errors back off with full jitter from 1 s to 5 min
//!   without end, permanent ones park the entry as `failed`, and an unreachable server pauses
//!   the repo while a cheap probe backs off the same way ([`classify`], [`CloudJob`]);
//! * the glue to `state.db` ([`StateRows`]: B04's `DraftRows` and B13's `UploadQueue`) and to
//!   `cloud.proto` ([`RpcCloudStore`]: B11's `CloudStore`).
//!
//! Every loop is bounded: [`MAX_REPOS`] per run, [`MAX_UPLOADS_PER_RUN`] entries per repo and
//! run, [`MAX_PAGES`] workspace pages. Errors stored in `state.db` or logged are server text cut
//! to a fixed length; a credential is never part of one.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use asc_engine::branchless::sync::{
    self as engine_sync, CloudDoc, CloudHead, CloudStore, CloudUpdate, LocalState, StoreError,
    SyncOptions as HeadSyncOptions,
};
use asc_engine::branchless::upload::{
    self, HeadSync, QueueEntry, QueueState as EngineQueueState, UploadQueue,
};
use asc_engine::branchless::{DraftHead, DraftRows};
use asc_rpc::client::CallError;
use asc_rpc::cloud as wire_cloud;
use asc_rpc::errors::RpcError;
use asc_rpc::retry::RetryPolicy;
use asc_rpc::server::wire;
use asc_state::drafts::{self, CloudState, Drafts};
use asc_state::state::StateDb;
use asc_state::{AscHome, state::workspaces};
use asc_store::refs::RepoKey;
use async_trait::async_trait;
use tonic::codec::CompressionEncoding;
use uuid::Uuid;

use super::scheduler::{Job, Spec};
use crate::branchless::mode::{self, RepoPolicy, Settings as ModeSettings};
use crate::remote::server::Wire;

/// The feature a server advertises when it serves the Cloud service.
pub const FEATURE_CLOUD: &str = "cloud.v1";
/// Seconds between two upload ticks.
pub const UPLOAD_TICK: Duration = Duration::from_secs(5);
/// Seconds between two head syncs (SPEC §21.3.4).
pub const SYNC_EVERY: Duration = Duration::from_secs(300);
/// How long what a server told us about a repo is trusted.
pub const FACTS_TTL_SECS: i64 = 300;
/// Longest one run of either task.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Repositories one run looks at.
pub const MAX_REPOS: usize = 64;
/// Queue entries one repository uploads per run (the rest wait for the next tick).
pub const MAX_UPLOADS_PER_RUN: usize = 16;
/// `GetWorkspace` pages read for one sync (1,000 heads each; the server caps a workspace at
/// 10,000).
pub const MAX_PAGES: usize = 16;
/// Longest error text stored or logged.
const MAX_NOTE: usize = 256;
/// Backoff floor and ceiling (SPEC §21.3.3).
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(300);

// ------------------------------------------------------------------------------ retry policy

/// What a failure means for the queue (SPEC §21.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// The server cannot be reached: the repo pauses and a probe backs off.
    Offline,
    /// Retry with backoff, for ever; the hint is the server's `retry-after-ms`.
    Transient(Option<Duration>),
    /// `state = failed`: retried only when the head changes or on `asc cloud sync`.
    Permanent,
}

/// Codes that never clear by waiting. `QUOTA_EXCEEDED`, `CLOUD_HEADS_LIMIT` and
/// `CLIENT_TOO_OLD` need the user to act, so they park the entry like the three of §21.3.3.
const PERMANENT: &[&str] = &[
    "INVALID_OBJECT",
    "PERMISSION_DENIED",
    "PermissionDenied",
    "PATH_POLICY",
    "QUOTA_EXCEEDED",
    "CLOUD_HEADS_LIMIT",
    "CLIENT_TOO_OLD",
];

/// Text that means the server could not be reached.
const OFFLINE: &[&str] = &[
    "Unavailable",
    "offline (",
    "transport error",
    "connection refused",
];

/// Classifies the text of an upload or sync failure. Permanent codes win over everything, so a
/// refused request is never hammered; text that is nothing in particular is transient.
pub fn classify(text: &str) -> Class {
    if PERMANENT.iter().any(|p| text.contains(p)) {
        Class::Permanent
    } else if OFFLINE.iter().any(|p| text.contains(p)) {
        Class::Offline
    } else {
        Class::Transient(retry_hint(text))
    }
}

/// The `N` of `retry after N ms` that a shed request's error carries.
fn retry_hint(text: &str) -> Option<Duration> {
    let rest = text.split("retry after ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u64>().ok().map(Duration::from_millis)
}

/// Server text made safe for `state.db` and the log: printable characters, bounded.
fn note(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_NOTE)
        .collect()
}

/// Full-jitter backoff from 1 s to 5 min, raised to the server's hint (capped at 5 min).
fn policy() -> RetryPolicy {
    RetryPolicy {
        base: BACKOFF_BASE,
        cap: BACKOFF_CAP,
        max_attempts: u32::MAX,
        ..RetryPolicy::default()
    }
}

fn delay_secs(policy: &RetryPolicy, failed: u32, hint: Option<Duration>) -> i64 {
    let d = policy.delay(failed.max(1), hint);
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX).max(1) + i64::from(d.subsec_nanos() > 0)
}

// ------------------------------------------------------------------------------ cloud wake

/// The daemon's own `Watch KIND_CLOUD` (SPEC §21.3.4, §21.3.8): told when a cloud sync changed
/// the user's drafts on another device, so the live driver of that repository runs a window
/// without waiting for its ref stream. Only the repository's key travels; the receiver decides
/// what to do. A lagging receiver loses old keys, which is harmless: it runs a window anyway.
pub struct CloudWake {
    tx: tokio::sync::broadcast::Sender<RepoKey>,
}

impl CloudWake {
    fn new() -> Self {
        Self {
            tx: tokio::sync::broadcast::channel(64).0,
        }
    }

    /// Tells every subscriber that the cloud drafts of `repo` changed.
    pub fn notify(&self, repo: RepoKey) {
        // No subscriber is not an error.
        let _ = self.tx.send(repo);
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RepoKey> {
        self.tx.subscribe()
    }
}

/// The daemon-wide wake channel of the cloud sync task.
pub fn cloud_wake() -> &'static CloudWake {
    static WAKE: std::sync::LazyLock<CloudWake> = std::sync::LazyLock::new(CloudWake::new);
    &WAKE
}

// ------------------------------------------------------------------------------ the gate

/// What the client knows about one repository's cloud.
#[derive(Clone, Debug)]
pub struct Facts {
    pub settings: ModeSettings,
    pub policy: RepoPolicy,
    /// `Version.features` has `cloud.v1`.
    pub cloud_v1: bool,
}

/// Whether the daemon may send drafts of a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    On,
    Off,
    /// `asc.cloud=on` on a public repo: nothing is sent and the log says why, once.
    Refused(String),
}

/// SPEC §21.3.3 and §21.3.7: cloud is off for `asc.cloud=off`, `asc.branchless=false`, a repo with
/// `branchless` or `cloud` off, a server without `cloud.v1`, and a public repo. The daemon is
/// running by definition, so the `Auto` condition on a daemon holds.
pub fn gate(facts: &Facts) -> Gate {
    match mode::cloud_upload(&facts.settings, &facts.policy, facts.cloud_v1, true) {
        // `asc.branchless=false` turns cloud off even under `asc.cloud=on` (SPEC §21.3.3).
        Ok(true) if facts.settings.branchless => Gate::On,
        Ok(_) => Gate::Off,
        Err(why) => Gate::Refused(note(&why)),
    }
}

/// Whether `ASC_NO_DAEMON` turns the daemon (and so every background upload) off.
pub fn no_daemon(get: &dyn Fn(&str) -> Option<OsString>) -> bool {
    super::lifecycle::disabled(get)
}

/// The workspace name of `asc.cloud.workspace`: `default`, the host name, or a given name; always
/// `[A-Za-z0-9._-]{1,64}` (SPEC §21.3.2).
pub fn workspace_name(setting: &mode::Workspace) -> String {
    let raw = match setting {
        mode::Workspace::Default => "default".to_owned(),
        mode::Workspace::Named(n) => n.clone(),
        mode::Workspace::Host => std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_default(),
    };
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if clean.is_empty() {
        "default".to_owned()
    } else {
        clean
    }
}

// ------------------------------------------------------------------------------ state.db glue

/// B04's `DraftRows` and B13's `UploadQueue` over `state.db`, for one repository.
#[derive(Clone)]
pub struct StateRows {
    db: StateDb,
    repo: RepoKey,
    drafts: Drafts,
}

impl StateRows {
    pub fn new(db: StateDb, repo: RepoKey) -> Self {
        Self {
            db,
            repo,
            drafts: Drafts::new(repo),
        }
    }

    pub fn drafts(&self) -> &Drafts {
        &self.drafts
    }
}

fn rows_err(e: impl std::fmt::Display) -> asc_engine::branchless::Error {
    asc_engine::branchless::Error::Rows(note(&e.to_string()))
}

fn queue_err(e: impl std::fmt::Display) -> upload::Error {
    upload::Error::Queue(note(&e.to_string()))
}

fn head_of(h: drafts::DraftHead) -> DraftHead {
    DraftHead {
        cs: h.cs,
        change: h.change,
        added: h.added,
        hidden_at: h.hidden_at,
    }
}

#[async_trait]
impl DraftRows for StateRows {
    async fn visible_heads(&self, n: usize) -> asc_engine::branchless::Result<Vec<DraftHead>> {
        let rows = self.drafts.visible(&self.db, n).await.map_err(rows_err)?;
        Ok(rows.into_iter().map(head_of).collect())
    }

    async fn hidden_heads(&self, n: usize) -> asc_engine::branchless::Result<Vec<DraftHead>> {
        let rows = self.drafts.hidden(&self.db, n).await.map_err(rows_err)?;
        Ok(rows.into_iter().map(head_of).collect())
    }
}

#[async_trait]
impl UploadQueue for StateRows {
    async fn next_pending(&self, now: i64) -> upload::Result<Option<QueueEntry>> {
        let due = self.drafts.due(&self.db, now, 1).await.map_err(queue_err)?;
        Ok(due.into_iter().next().map(|r| QueueEntry {
            cs: r.cs,
            state: EngineQueueState::Pending,
            attempts: u32::try_from(r.attempts).unwrap_or(u32::MAX),
        }))
    }

    /// A held entry is stored as `failed` with the reason, a too-large one as `too_large`; both
    /// leave `pending` so the uploader does not pick them again. A new head, `asc cloud release`
    /// or `asc cloud sync` puts them back.
    async fn set_state(&self, cs: ChangesetId, state: EngineQueueState) -> upload::Result<()> {
        let (state, why) = match state {
            EngineQueueState::Pending => return Ok(()),
            EngineQueueState::Held => (
                drafts::QueueState::Failed,
                "held by the pre-upload guard (asc cloud release uploads it)",
            ),
            EngineQueueState::TooLarge => (
                drafts::QueueState::TooLarge,
                "over asc.cloud.maxChangesetSize (asc push still uploads it)",
            ),
        };
        self.drafts
            .queue_failed(&self.db, cs, state, 0, why)
            .await
            .map_err(queue_err)
    }

    async fn delete(&self, cs: ChangesetId) -> upload::Result<()> {
        self.drafts
            .queue_done(&self.db, cs)
            .await
            .map_err(queue_err)
    }
}

// ------------------------------------------------------------------------------ cloud.proto glue

/// B11's `CloudStore` over the Cloud service of one repository and workspace.
pub struct RpcCloudStore {
    wire: Wire,
    name: String,
}

impl RpcCloudStore {
    pub fn new(wire: Wire, name: String) -> Self {
        Self { wire, name }
    }

    fn repo_ref(&self) -> wire::RepoRef {
        wire::RepoRef {
            name: self.wire.repo.clone(),
            id_mode: asc_rpc::IdMode::Asc.to_wire(),
        }
    }

    fn stub(&self) -> asc_rpc::services::cloud_client::CloudClient<tonic::transport::Channel> {
        asc_rpc::services::cloud_client::CloudClient::new(self.wire.channel.clone())
            .max_decoding_message_size(asc_rpc::server::MAX_MESSAGE)
            .max_encoding_message_size(asc_rpc::server::MAX_MESSAGE)
            .accept_compressed(CompressionEncoding::Zstd)
    }
}

/// The request id of an update: the first 16 bytes of its hash, so a resend is the same call.
fn op_uuid(op_id: &str) -> Uuid {
    let mut b = [0u8; 16];
    for (i, byte) in b.iter_mut().enumerate() {
        let hex = op_id.get(2 * i..2 * i + 2).unwrap_or("00");
        *byte = u8::from_str_radix(hex, 16).unwrap_or(0);
    }
    Uuid::from_bytes(b)
}

fn from_wire_head(h: &wire_cloud::CloudHead) -> Result<CloudHead, StoreError> {
    let cs = match h.cs {
        asc_rpc::ModeId::Asc(cs) => cs,
        asc_rpc::ModeId::Git(_) => {
            return Err(StoreError::Other("a git id in an asc workspace".into()));
        }
    };
    Ok(CloudHead {
        cs,
        change: h.change.unwrap_or_else(|| ChangeId::from_bytes([0; 16])),
        labels: h.labels.iter().map(|l| l.as_str().to_owned()).collect(),
        added: h.added,
        hidden_at: (h.hidden_at != 0).then_some(h.hidden_at),
    })
}

fn to_wire_head(h: &CloudHead) -> Result<wire_cloud::CloudHead, StoreError> {
    let labels = h
        .labels
        .iter()
        .map(|l| wire_cloud::Label::new(l))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| StoreError::Other(e.to_string()))?;
    Ok(wire_cloud::CloudHead {
        cs: asc_rpc::ModeId::Asc(h.cs),
        change: None,
        labels,
        added: h.added,
        hidden_at: h.hidden_at.unwrap_or(0),
    })
}

fn to_wire_ids(v: &[ChangesetId]) -> Vec<asc_rpc::ModeId<ChangesetId>> {
    v.iter().map(|&c| asc_rpc::ModeId::Asc(c)).collect()
}

#[async_trait]
impl CloudStore for RpcCloudStore {
    async fn get(&self) -> Result<CloudDoc, StoreError> {
        let mut doc = CloudDoc::default();
        let mut token = String::new();
        for page in 0..MAX_PAGES {
            let req = wire::GetWorkspaceRequest {
                repo: Some(self.repo_ref()),
                owner: String::new(),
                name: self.name.clone(),
                since_version: 0,
                // No age cut: a head left out would look removed on the server.
                max_age_s: 0,
                include_hidden: true,
                page_token: token.clone(),
            };
            let got = self
                .stub()
                .get_workspace(self.wire.request(req, None))
                .await
                .map_err(|s| StoreError::Other(note(&CallError::from(s).to_string())))?
                .into_inner();
            let ws = wire_cloud::Workspace::from_response(&got, asc_rpc::IdMode::Asc)
                .map_err(|e| StoreError::Other(e.to_string()))?;
            if ws.omitted > 0 {
                return Err(StoreError::Other("the server left heads out".into()));
            }
            if page == 0 {
                doc.version = ws.version;
            }
            for h in &ws.heads {
                doc.heads.push(from_wire_head(h)?);
            }
            if ws.next_page_token.is_empty() {
                return Ok(doc);
            }
            token = ws.next_page_token;
        }
        Err(StoreError::Other(format!(
            "the workspace is more than {MAX_PAGES} pages"
        )))
    }

    async fn update(&self, op_id: &str, update: &CloudUpdate) -> Result<u64, StoreError> {
        let body = wire_cloud::UpdateWorkspace {
            name: self.name.clone(),
            base_version: update.base_version,
            add: update
                .add
                .iter()
                .map(to_wire_head)
                .collect::<Result<_, _>>()?,
            remove: to_wire_ids(&update.remove),
            hide: to_wire_ids(&update.hide),
            unhide: to_wire_ids(&update.unhide),
        };
        let req = body.to_request(self.repo_ref());
        match self
            .stub()
            .update_workspace(self.wire.request(req, Some(op_uuid(op_id))))
            .await
        {
            Ok(r) => Ok(r.into_inner().version),
            Err(s) => match CallError::from(s) {
                CallError::Rpc(e @ RpcError::VersionConflict(_)) => {
                    Err(StoreError::VersionConflict {
                        current: wire_cloud::conflict_version(&e).unwrap_or(0),
                    })
                }
                other => Err(StoreError::Other(note(&other.to_string()))),
            },
        }
    }
}

/// Runs the head sync of §21.3.4 steps 2 to 4 against `store` and applies its result to the
/// local rows. It is both B13's step 5 ([`HeadSync`]) and the five-minute sync.
pub struct CloudSync<S: CloudStore> {
    rows: StateRows,
    store: S,
    ws_name: String,
    clock: Arc<dyn Clock>,
}

impl<S: CloudStore> CloudSync<S> {
    pub fn new(rows: StateRows, store: S, ws_name: String, clock: Arc<dyn Clock>) -> Self {
        Self {
            rows,
            store,
            ws_name,
            clock,
        }
    }

    /// One sync. A remote head that is new here is not added to the local rows (its objects and
    /// metadata arrive with the pull of B15), so it stays out of the stored base and is offered
    /// again next time; otherwise the next sync would read the missing row as a local removal and
    /// delete the head on the server.
    pub async fn run(&self) -> Result<u64, String> {
        let db = &self.rows.db;
        let d = &self.rows.drafts;
        let now = self.clock.now();
        let visible = d
            .visible(db, drafts::MAX_BATCH)
            .await
            .map_err(|e| note(&e.to_string()))?;
        let hidden = d
            .hidden(db, drafts::MAX_BATCH)
            .await
            .map_err(|e| note(&e.to_string()))?;
        let base: BTreeSet<ChangesetId> = d
            .base(db)
            .await
            .map_err(|e| note(&e.to_string()))?
            .into_iter()
            .collect();
        let heads = visible
            .iter()
            .chain(&hidden)
            .map(|h| CloudHead {
                cs: h.cs,
                change: h.change,
                labels: Vec::new(),
                added: h.added,
                hidden_at: h.hidden_at,
            })
            .collect();
        let local = LocalState {
            heads,
            base,
            preds: HashMap::new(),
        };
        let out = engine_sync::sync(&self.store, &local, HeadSyncOptions::default())
            .await
            .map_err(|e| note(&e.to_string()))?;

        let mut tx = db.begin().await.map_err(|e| note(&e.to_string()))?;
        if !out.local.hide.is_empty() {
            d.hide(&mut tx, &out.local.hide, None, now, drafts::QUEUE_MAX)
                .await
                .map_err(|e| note(&e.to_string()))?;
        }
        for cs in &out.local.unhide {
            if let Some(h) = hidden.iter().find(|h| h.cs == *cs) {
                d.unhide(&mut tx, h.cs, h.change, now, drafts::QUEUE_MAX)
                    .await
                    .map_err(|e| note(&e.to_string()))?;
            }
        }
        tx.commit().await.map_err(|e| note(&e.to_string()))?;

        let unseen: BTreeSet<ChangesetId> = out.local.add.iter().map(|h| h.cs).collect();
        let base: Vec<ChangesetId> = out.base.difference(&unseen).copied().collect();
        d.set_base(db, &base)
            .await
            .map_err(|e| note(&e.to_string()))?;
        d.set_cloud_state(
            db,
            &CloudState {
                ws_name: self.ws_name.clone(),
                version: i64::try_from(out.version).unwrap_or(i64::MAX),
                last_sync: now,
            },
        )
        .await
        .map_err(|e| note(&e.to_string()))?;
        // Drafts that changed on another device wake the live driver (SPEC §21.3.8).
        if !out.local.add.is_empty() || !out.local.hide.is_empty() || !out.local.unhide.is_empty() {
            cloud_wake().notify(self.rows.repo);
        }
        Ok(out.version)
    }
}

#[async_trait]
impl<S: CloudStore> HeadSync for CloudSync<S> {
    async fn sync_heads(&self, _uploaded: ChangesetId) -> upload::Result<()> {
        self.run().await.map(|_| ()).map_err(upload::Error::Queue)
    }
}

// ------------------------------------------------------------------------------ the job

/// Unix seconds, replaceable in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
}

/// The wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// The next queue entry that is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Due {
    pub cs: ChangesetId,
    pub attempts: u32,
}

/// What one upload did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Uploaded,
    /// The guard held it; `asc cloud status` shows why.
    Held,
    TooLarge,
}

/// One repository's cloud, as the job drives it. Errors are text; [`classify`] reads it.
#[async_trait]
pub trait Uplink: Send + Sync {
    /// What the server says about the repo (`Version`, `RepoConfig`) and the user's settings.
    async fn facts(&self) -> Result<Facts, String>;
    /// The user's settings, read from the config files now. The job asks before every run, so
    /// `asc.cloud=off` or `asc.branchless=false` stops uploads at the next tick, not when a cached
    /// answer of the server expires.
    async fn settings(&self) -> Result<ModeSettings, String>;
    /// A cheap call that succeeds when the server is reachable.
    async fn ping(&self) -> Result<(), String>;
    /// The oldest pending entry due at `now`.
    async fn next_due(&self, now: i64) -> Result<Option<Due>, String>;
    /// Uploads that entry (the queue row is updated or deleted).
    async fn upload_next(&self, now: i64) -> Result<Option<Step>, String>;
    /// Records a failed attempt on `cs`: `permanent` parks it as `failed`, else it is retried at
    /// `next_at`.
    async fn record_failure(
        &self,
        cs: ChangesetId,
        permanent: bool,
        next_at: i64,
        error: &str,
    ) -> Result<(), String>;
    /// The head sync of §21.3.4.
    async fn sync(&self) -> Result<(), String>;
}

/// Where the job finds repositories.
#[async_trait]
pub trait Uplinks: Send + Sync {
    /// Repositories worth a look now. An implementation may return every repo with a cloud
    /// remote for either phase: the job caches what the server says, backs off on failure and
    /// leaves an idle repo at the cost of a config read.
    async fn repos(&self, phase: Phase, now: i64) -> Result<Vec<RepoKey>, String>;
    /// `None` when the repo has no asc server remote (local-only, bucket, git remote, mirror
    /// mode): cloud is off.
    async fn open(&self, repo: RepoKey) -> Result<Option<Arc<dyn Uplink>>, String>;
}

/// Which of the two tasks is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Upload,
    Sync,
}

#[derive(Default)]
struct Offline {
    failures: u32,
    until: i64,
}

#[derive(Default)]
struct RepoState {
    facts: Option<(i64, Facts)>,
    offline: Option<Offline>,
    next_sync: i64,
    sync_failures: u32,
    refused: bool,
    /// Consecutive failures to open the repo, read its settings or ask the server for its facts,
    /// and the time before which the repo is left alone.
    setup_failures: u32,
    setup_until: i64,
}

/// What a run did, for the log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub uploaded: usize,
    pub held: usize,
    pub failed: usize,
    pub synced: usize,
    pub paused: usize,
}

/// The uploader and the sync (two scheduler tasks, one state).
pub struct CloudJob {
    links: Arc<dyn Uplinks>,
    clock: Arc<dyn Clock>,
    policy: RetryPolicy,
    state: Mutex<HashMap<RepoKey, RepoState>>,
    locks: Mutex<HashMap<RepoKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Set by a `git fetch` or `asc sl` ask: the next upload tick also syncs.
    sync_now: Arc<AtomicBool>,
    /// The request marker `git fetch` leaves in `run` (B15): found on an upload tick it sets
    /// `sync_now` and is removed.
    marker: Option<PathBuf>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl CloudJob {
    pub fn new(links: Arc<dyn Uplinks>, clock: Arc<dyn Clock>) -> Self {
        Self::with_policy(links, clock, policy())
    }

    pub fn with_policy(
        links: Arc<dyn Uplinks>,
        clock: Arc<dyn Clock>,
        policy: RetryPolicy,
    ) -> Self {
        Self {
            links,
            clock,
            policy,
            state: Mutex::default(),
            locks: Mutex::default(),
            sync_now: Arc::default(),
            marker: None,
        }
    }

    /// Reads the fetch ask from `<run>/cloud-sync-request` (B15).
    pub fn with_marker(mut self, run: &Path) -> Self {
        self.marker = Some(run.join(crate::branchless::pull::MARKER));
        self
    }

    /// Consumes the fetch ask: the marker is removed and the next sync is forced. A marker that
    /// is not a plain file (a link, a directory) is ignored, never followed.
    fn take_marker(&self) {
        let Some(path) = &self.marker else { return };
        let plain = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
        if plain && std::fs::remove_file(path).is_ok() {
            self.sync_now.store(true, Ordering::Release);
        }
    }

    /// The flag a non-blocking sync request sets (SPEC §21.3.4: `git fetch` and `asc sl` ask the
    /// daemon; nothing waits for the answer).
    pub fn sync_request(&self) -> Arc<AtomicBool> {
        self.sync_now.clone()
    }

    fn repo_lock(&self, repo: RepoKey) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.locks).entry(repo).or_default().clone()
    }

    fn with_state<T>(&self, repo: RepoKey, f: impl FnOnce(&mut RepoState) -> T) -> T {
        f(lock(&self.state).entry(repo).or_default())
    }

    /// One run of a task over every repository.
    pub async fn run_phase(&self, phase: Phase) -> Result<Tally, String> {
        let now = self.clock.now();
        let mut repos = self.links.repos(phase, now).await?;
        repos.truncate(MAX_REPOS);
        let mut tally = Tally::default();
        if phase == Phase::Upload {
            self.take_marker();
        }
        let force_sync = phase == Phase::Upload && self.sync_now.swap(false, Ordering::AcqRel);
        for repo in repos {
            let lock = self.repo_lock(repo);
            let _held = lock.lock().await;
            if let Err(e) = self.run_repo(repo, phase, force_sync, &mut tally).await {
                // One repository's trouble never stops the others.
                tracing_note(repo, &e);
            }
        }
        Ok(tally)
    }

    async fn run_repo(
        &self,
        repo: RepoKey,
        phase: Phase,
        force_sync: bool,
        tally: &mut Tally,
    ) -> Result<(), String> {
        let now = self.clock.now();
        let paused = self.with_state(repo, |s| s.offline.as_ref().is_some_and(|o| now < o.until));
        if paused {
            tally.paused += 1;
            return Ok(());
        }
        let waiting = self.with_state(repo, |s| now < s.setup_until);
        if waiting {
            tally.paused += 1;
            return Ok(());
        }
        let link = match self.links.open(repo).await {
            Ok(Some(link)) => link,
            Ok(None) => return Ok(()),
            Err(e) => return self.setup_failed(repo, now, &e),
        };
        // The settings are read afresh on every run: they are the user's, not the server's, and
        // `asc.cloud=off` must stop uploads at once. Only the server's answers are cached.
        let settings = match link.settings().await {
            Ok(s) => s,
            Err(e) => return self.setup_failed(repo, now, &e),
        };
        if settings.cloud == mode::CloudSetting::Off || !settings.branchless {
            return Ok(());
        }
        // Facts come from the server; they are cached so the idle tick costs no request.
        let cached = self.with_state(repo, |s| {
            s.facts
                .as_ref()
                .filter(|(at, _)| now - at < FACTS_TTL_SECS)
                .map(|(_, f)| f.clone())
        });
        let mut facts = match cached {
            Some(f) => f,
            None => match link.facts().await {
                Ok(f) => {
                    self.with_state(repo, |s| s.facts = Some((now, f.clone())));
                    f
                }
                Err(e) => {
                    let done = self.on_failure(repo, None, &link, &e, now, tally).await;
                    if classify(&note(&e)) != Class::Offline {
                        // An unwell server is not asked again every tick.
                        let _ = self.setup_failed(repo, now, &e);
                    }
                    return done;
                }
            },
        };
        self.with_state(repo, |s| s.setup_failures = 0);
        facts.settings = settings;
        match gate(&facts) {
            Gate::On => {}
            Gate::Off => return Ok(()),
            Gate::Refused(why) => {
                let first = self.with_state(repo, |s| !std::mem::replace(&mut s.refused, true));
                if first {
                    tracing_note(repo, &why);
                }
                return Ok(());
            }
        }
        // A paused repo whose time is up is probed before anything is sent.
        let probing = self.with_state(repo, |s| s.offline.is_some());
        if probing {
            match link.ping().await {
                Ok(()) => self.with_state(repo, |s| s.offline = None),
                Err(e) => return self.on_failure(repo, None, &link, &e, now, tally).await,
            }
        }
        if phase == Phase::Upload {
            self.upload(repo, &link, tally).await?;
        }
        let due = self.with_state(repo, |s| now >= s.next_sync);
        if (phase == Phase::Sync && due) || force_sync {
            match link.sync().await {
                Ok(()) => {
                    tally.synced += 1;
                    self.with_state(repo, |s| {
                        s.sync_failures = 0;
                        s.next_sync = now + SYNC_EVERY.as_secs() as i64;
                    });
                }
                Err(e) => {
                    let failures = self.with_state(repo, |s| {
                        s.sync_failures += 1;
                        s.sync_failures
                    });
                    let wait = delay_secs(&self.policy, failures, None);
                    self.with_state(repo, |s| s.next_sync = now + wait);
                    return self.on_failure(repo, None, &link, &e, now, tally).await;
                }
            }
        }
        Ok(())
    }

    async fn upload(
        &self,
        repo: RepoKey,
        link: &Arc<dyn Uplink>,
        tally: &mut Tally,
    ) -> Result<(), String> {
        for _ in 0..MAX_UPLOADS_PER_RUN {
            let now = self.clock.now();
            let Some(due) = link.next_due(now).await? else {
                break;
            };
            match link.upload_next(now).await {
                Ok(Some(Step::Uploaded)) => tally.uploaded += 1,
                Ok(Some(Step::Held | Step::TooLarge)) => tally.held += 1,
                Ok(None) => break,
                Err(e) => {
                    if let Err(text) = self.on_failure(repo, Some(due), link, &e, now, tally).await
                    {
                        tracing_note(repo, &text);
                    }
                    // A permanent failure parks that entry only; anything else stops this run
                    // (the server is unwell, and the entry has its own retry time).
                    if classify(&e) != Class::Permanent {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// A failure to open the repo, read its settings or ask for its facts: the repo is left
    /// alone for the backoff of §21.3.3 (1 s doubling to 5 min), so a broken repo costs one
    /// attempt per wait and not one per tick.
    fn setup_failed(&self, repo: RepoKey, now: i64, text: &str) -> Result<(), String> {
        let failures = self.with_state(repo, |s| {
            s.setup_failures = s.setup_failures.saturating_add(1);
            s.setup_failures
        });
        let wait = delay_secs(&self.policy, failures, None);
        self.with_state(repo, |s| s.setup_until = now + wait);
        Err(note(text))
    }

    /// Applies the retry policy of §21.3.3 to a failure. Returns `Err` with the text so the
    /// caller can log it.
    async fn on_failure(
        &self,
        repo: RepoKey,
        entry: Option<Due>,
        link: &Arc<dyn Uplink>,
        text: &str,
        now: i64,
        tally: &mut Tally,
    ) -> Result<(), String> {
        let text = note(text);
        match classify(&text) {
            Class::Offline => {
                tally.paused += 1;
                let failures = self.with_state(repo, |s| {
                    let o = s.offline.get_or_insert_with(Offline::default);
                    o.failures += 1;
                    o.failures
                });
                let wait = delay_secs(&self.policy, failures, None);
                self.with_state(repo, |s| {
                    if let Some(o) = s.offline.as_mut() {
                        o.until = now + wait;
                    }
                });
                // The queue pauses; no row is touched, so nothing is lost or counted.
                Ok(())
            }
            Class::Transient(hint) => {
                tally.failed += 1;
                if let Some(due) = entry {
                    let wait = delay_secs(&self.policy, due.attempts.saturating_add(1), hint);
                    link.record_failure(due.cs, false, now + wait, &text)
                        .await?;
                }
                Err(text)
            }
            Class::Permanent => {
                tally.failed += 1;
                if let Some(due) = entry {
                    link.record_failure(due.cs, true, 0, &text).await?;
                }
                Err(text)
            }
        }
    }
}

/// A one-line diagnostic on stderr of the daemon (its log): the repo key and bounded text.
fn tracing_note(repo: RepoKey, text: &str) {
    eprintln!("asc daemon: cloud: repo {}: {}", repo.0, note(text));
}

/// The scheduler's view of one task of a [`CloudJob`].
pub struct Task {
    job: Arc<CloudJob>,
    phase: Phase,
}

#[async_trait]
impl Job for Task {
    async fn run(&self) -> Result<String, String> {
        let t = self.job.run_phase(self.phase).await?;
        Ok(format!(
            "uploaded {} held {} failed {} synced {} paused {}",
            t.uploaded, t.held, t.failed, t.synced, t.paused
        ))
    }
}

/// The two tasks for `scheduler::Hosting::extra_jobs`. With `ASC_NO_DAEMON` set there are none, so
/// nothing uploads in the background (SPEC §21.3.3).
pub fn extra_jobs(
    home: AscHome,
    get: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<(Spec, Arc<dyn Job>)> {
    if no_daemon(get) {
        return Vec::new();
    }
    let run = home.run();
    let job = Arc::new(
        CloudJob::new(
            Arc::new(prod::ProdUplinks::new(home)),
            Arc::new(SystemClock),
        )
        .with_marker(&run),
    );
    vec![
        (
            Spec {
                name: "cloud_upload",
                every: Some(UPLOAD_TICK),
                first: UPLOAD_TICK,
                timeout: RUN_TIMEOUT,
            },
            Arc::new(Task {
                job: job.clone(),
                phase: Phase::Upload,
            }),
        ),
        (
            Spec {
                name: "cloud_sync",
                every: Some(SYNC_EVERY),
                first: Duration::from_secs(30),
                timeout: RUN_TIMEOUT,
            },
            Arc::new(Task {
                job,
                phase: Phase::Sync,
            }),
        ),
    ]
}

/// The repositories of `state.db` that have a workspace, bounded.
async fn repos_of(db: &StateDb) -> Result<Vec<RepoKey>, String> {
    let all = workspaces::list(db, None)
        .await
        .map_err(|e| note(&e.to_string()))?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for w in all {
        if seen.insert(w.ws.repo.0) {
            out.push(w.ws.repo);
        }
        if out.len() >= MAX_REPOS {
            break;
        }
    }
    Ok(out)
}

pub(crate) mod prod;

#[cfg(test)]
mod tests;
