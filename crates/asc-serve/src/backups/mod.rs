//! Scheduled backups of the server (`backups.proto`, docs/BUILDOUT.md section 26, docs/runbooks/backup.md).
//!
//! The hub runs the same backup as `asc serve backup` on a schedule or on request, keeps the
//! record of each run, copies the result to a target (a local directory, or an S3 bucket with
//! every file but the manifest encrypted before upload), checks it, applies retention, and stages
//! a restore. A running server cannot replace its own database, so a restore is written down as
//! a marker file and applied by [`apply_pending`] when the server next starts, before the
//! database is opened.

pub mod config;
pub mod crypto;
pub mod place;
pub mod plan;
pub mod rpc;
pub mod store;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::AnyPool;
use tokio::task::JoinHandle;
use tonic::Status;

use crate::ai::audit::{Audit, Entry};
use crate::ai::clock;
use crate::ai::ops::Ops;
use crate::ai::rand_id;
use crate::config::ServeConfig;
use config::BackupsConfig;
use place::Place;
use store::{Restore, Run, Schedule, Store};

/// What `Version.features` adds for this service.
pub const FEATURE: &str = "backups.v1";
/// The word a person types to confirm a restore.
pub const CONFIRM: &str = "RESTORE";
/// Longest note kept on a run.
const MAX_NOTE: usize = 240;
/// How often the scheduler looks at the clock.
const TICK: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------------------ errors

/// Why a backups call failed.
#[derive(Debug)]
pub enum BackupsError {
    NotFound,
    Invalid(&'static str),
    /// A precondition does not hold; the text says which.
    Precondition(&'static str),
    Sql(String),
}

impl From<sqlx::Error> for BackupsError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(crate::ai::store::class(&e))
    }
}

impl BackupsError {
    pub fn status(&self) -> Status {
        match self {
            Self::NotFound => Status::not_found("no such backup"),
            Self::Invalid(m) => Status::invalid_argument(*m),
            Self::Precondition(m) => Status::failed_precondition(*m),
            Self::Sql(class) => {
                tracing::error!(class, "backups database error");
                Status::internal("internal error")
            }
        }
    }
}

// ------------------------------------------------------------------------------ the engine

/// What a dump takes.
#[derive(Clone, Debug, Default)]
pub struct DumpOptions {
    pub key_file: Option<PathBuf>,
    pub include_vault_key: bool,
    pub include_objects: bool,
}

/// Writes and checks backup directories. The server uses [`RealEngine`]; tests use a fake.
#[async_trait]
pub trait Engine: Send + Sync {
    /// Writes a backup into `out`, which must not exist.
    async fn dump(&self, out: &Path, o: &DumpOptions) -> Result<(), String>;
    /// Checks a backup directory; the number of objects it holds.
    fn verify(&self, dir: &Path) -> Result<u64, String>;
}

/// `asc serve backup` and its check.
pub struct RealEngine {
    pub cfg: Arc<ServeConfig>,
}

#[async_trait]
impl Engine for RealEngine {
    async fn dump(&self, out: &Path, o: &DumpOptions) -> Result<(), String> {
        crate::backup::backup(
            &self.cfg,
            &crate::backup::BackupOptions {
                out: out.to_owned(),
                key_file: o.key_file.clone(),
                include_vault_key: o.include_vault_key,
                no_objects: !o.include_objects,
            },
        )
        .await
        .map(drop)
        .map_err(|e| e.0)
    }

    fn verify(&self, dir: &Path) -> Result<u64, String> {
        crate::backup::verify(dir).map_err(|e| e.0)
    }
}

// ------------------------------------------------------------------------------ manifest

/// The fields of `manifest.json` the board shows.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ManifestView {
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub db_kind: String,
    #[serde(default)]
    pub vault_key: String,
    #[serde(default)]
    pub objects: u64,
    #[serde(default)]
    pub object_bytes: u64,
}

/// The cut-down text of an error, safe to show: one line, bounded, no control characters.
pub fn note_of(e: &str) -> String {
    let line = e.lines().next().unwrap_or("");
    let mut s: String = line.chars().filter(|c| !c.is_control()).collect();
    if s.len() > MAX_NOTE {
        let mut cut = MAX_NOTE;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// Where the hub keeps its marker files and staging: `<data dir>/backups`.
pub fn state_dir(cfg: &ServeConfig) -> PathBuf {
    crate::workdir::default_dir(cfg, "backups")
}

const PENDING: &str = "restore.pending.json";

/// What `apply_pending` reads.
#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    backup_id: String,
    from: PathBuf,
    key_file: Option<PathBuf>,
    /// The directory was downloaded for this restore and is removed after it.
    cleanup: bool,
}

/// Applies a staged restore, if there is one. Call it before the database is opened. The marker
/// is renamed first, so a restore that fails is not tried again on the next start; it is
/// reported and the server then refuses to start with the error.
pub async fn apply_pending(cfg: &ServeConfig) -> Result<Option<String>, String> {
    let dir = state_dir(cfg);
    let marker = dir.join(PENDING);
    let Ok(raw) = std::fs::read(&marker) else {
        return Ok(None);
    };
    let attempt = dir.join("restore.running.json");
    std::fs::rename(&marker, &attempt).map_err(|e| format!("restore marker: {e}"))?;
    let m: Marker =
        serde_json::from_slice(&raw).map_err(|_| "restore marker is damaged".to_owned())?;
    let result = crate::backup::restore(
        cfg,
        &crate::backup::RestoreOptions {
            from: m.from.clone(),
            key_file: m.key_file.clone(),
            force: true,
        },
    )
    .await;
    if m.cleanup {
        let _ = std::fs::remove_dir_all(&m.from);
    }
    match result {
        Ok(report) => {
            let _ = std::fs::rename(&attempt, dir.join("restore.done.json"));
            tracing::info!(backup = %m.backup_id, "a staged restore was applied");
            Ok(Some(report))
        }
        Err(e) => {
            let _ = std::fs::rename(&attempt, dir.join("restore.failed.json"));
            Err(format!(
                "the staged restore of {} failed: {}",
                m.backup_id, e.0
            ))
        }
    }
}

// ------------------------------------------------------------------------------ the hub

/// The state the `Backups` service shares.
pub struct BackupsHub {
    pub(crate) store: Store,
    pub(crate) cfg: BackupsConfig,
    engine: Arc<dyn Engine>,
    state_dir: PathBuf,
    places: Mutex<std::collections::HashMap<String, Arc<Place>>>,
    pub(crate) audit: Arc<dyn Audit>,
    pub(crate) ops: Ops,
    start: tokio::sync::Mutex<()>,
    key: OnceLock<Option<[u8; 32]>>,
    stopping: AtomicBool,
}

/// The place the server's single [`BackupsHub`] is registered, once.
#[derive(Default)]
pub struct BackupsSlot(OnceLock<Arc<BackupsHub>>);

impl BackupsSlot {
    pub fn set(&self, hub: Arc<BackupsHub>) -> bool {
        self.0.set(hub).is_ok()
    }
    pub fn get(&self) -> Option<&Arc<BackupsHub>> {
        self.0.get()
    }
}

impl BackupsHub {
    pub fn new(
        pool: AnyPool,
        cfg: BackupsConfig,
        engine: Arc<dyn Engine>,
        state_dir: PathBuf,
        audit: Arc<dyn Audit>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store: Store::new(pool.clone()),
            cfg,
            engine,
            state_dir,
            places: Mutex::default(),
            audit,
            ops: Ops::new(pool),
            start: tokio::sync::Mutex::new(()),
            key: OnceLock::new(),
            stopping: AtomicBool::new(false),
        })
    }

    /// Installs an opened target under `name` (tests, and a host with its own storage).
    pub fn set_place(&self, name: &str, place: Place) {
        self.places
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), Arc::new(place));
    }

    fn place(&self, name: &str) -> Result<Arc<Place>, BackupsError> {
        if let Some(p) = self
            .places
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
        {
            return Ok(p.clone());
        }
        let t = self
            .cfg
            .targets
            .get(name)
            .ok_or(BackupsError::Precondition("no such backup target"))?;
        let p = Arc::new(
            place::open(t)
                .map_err(|_| BackupsError::Precondition("the backup target cannot be opened"))?,
        );
        self.places
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), p.clone());
        Ok(p)
    }

    /// Installs the backup key instead of reading `key_file` (tests, and a host with its own key store).
    pub fn set_key(&self, k: Option<[u8; 32]>) -> bool {
        self.key.set(k).is_ok()
    }

    /// The backup key, read once.
    fn backup_key(&self) -> Result<Option<[u8; 32]>, String> {
        if let Some(k) = self.key.get() {
            return Ok(*k);
        }
        let k = match &self.cfg.key_file {
            Some(p) => Some(crate::backup::load_backup_key(p).map_err(|e| e.0)?),
            None => None,
        };
        let _ = self.key.set(k);
        Ok(k)
    }

    pub async fn record(&self, entry: Entry<'_>) {
        self.audit.record(entry).await;
    }

    /// The target names and what they are, without a credential.
    pub fn targets(&self) -> Vec<(String, &'static str, String, bool)> {
        self.cfg
            .targets
            .iter()
            .map(|(n, t)| {
                (
                    n.clone(),
                    t.kind(),
                    t.location(),
                    t.kind() == "s3" && self.cfg.key_file.is_some(),
                )
            })
            .collect()
    }

    // -------------------------------------------------------------------------- reads

    pub async fn list(&self, limit: u32) -> Result<Vec<Run>, BackupsError> {
        let n = if limit == 0 { 50 } else { limit.min(200) };
        Ok(self.store.list_runs(i64::from(n)).await?)
    }

    pub async fn run(&self, id: &str) -> Result<Run, BackupsError> {
        if id.len() > 40 || !id.starts_with("bk_") {
            return Err(BackupsError::NotFound);
        }
        self.store.get_run(id).await?.ok_or(BackupsError::NotFound)
    }

    pub async fn schedule(&self) -> Result<Schedule, BackupsError> {
        Ok(self.store.schedule().await?)
    }

    /// The run that is next due, 0 when the schedule is off.
    pub async fn next_run(&self, s: &Schedule) -> Result<i64, BackupsError> {
        if !s.enabled {
            return Ok(0);
        }
        let last = self.store.last_scheduled().await?.max(s.updated);
        Ok(plan::next_run(last, s.interval_hours, s.minute_of_day))
    }

    // -------------------------------------------------------------------------- schedule

    /// Validates and stores a schedule.
    pub async fn set_schedule(&self, mut s: Schedule) -> Result<Schedule, BackupsError> {
        if !(1..=168).contains(&s.interval_hours) {
            return Err(BackupsError::Invalid("interval_hours is 1 to 168"));
        }
        if s.minute_of_day > 1439 {
            return Err(BackupsError::Invalid("minute_of_day is 0 to 1439"));
        }
        if !(1..=3650).contains(&s.keep_days) {
            return Err(BackupsError::Invalid("keep_days is 1 to 3650"));
        }
        if s.keep_weeks > 520 {
            return Err(BackupsError::Invalid("keep_weeks is 0 to 520"));
        }
        if s.enabled && !self.cfg.usable() {
            return Err(BackupsError::Precondition(
                "backups are not enabled in the server configuration, or it names no target",
            ));
        }
        if (s.enabled || !s.target.is_empty()) && !self.cfg.targets.contains_key(&s.target) {
            return Err(BackupsError::Precondition("no such backup target"));
        }
        if s.include_vault_key && self.cfg.key_file.is_none() {
            return Err(BackupsError::Precondition(
                "the vault key is included only when the configuration names a backup key file",
            ));
        }
        s.updated = clock::now();
        self.store.set_schedule(&s).await?;
        Ok(s)
    }

    // -------------------------------------------------------------------------- runs

    /// Starts a backup and returns its record at once; the copy runs in the background.
    pub async fn start_run(
        self: &Arc<Self>,
        target: &str,
        trig: i64,
        actor: &str,
    ) -> Result<Run, BackupsError> {
        if !self.cfg.usable() {
            return Err(BackupsError::Precondition(
                "backups are not enabled in the server configuration, or it names no target",
            ));
        }
        let sched = self.store.schedule().await?;
        let target = if target.is_empty() {
            sched.target.as_str()
        } else {
            target
        };
        if target.is_empty() || !self.cfg.targets.contains_key(target) {
            return Err(BackupsError::Precondition("no such backup target"));
        }
        self.place(target)?;
        let _guard = self.start.lock().await;
        if self.store.running().await? > 0 {
            return Err(BackupsError::Precondition("a backup is already running"));
        }
        let id = rand_id("bk_", 20);
        let run = Run {
            id: id.clone(),
            state: store::RUNNING,
            trig,
            target: target.to_owned(),
            started: clock::now(),
            actor: actor.to_owned(),
            location: id,
            encrypted: self
                .cfg
                .targets
                .get(target)
                .is_some_and(|t| t.kind() == "s3")
                && self.cfg.key_file.is_some(),
            ..Run::default()
        };
        self.store.insert_run(&run).await?;
        let (hub, r, s) = (self.clone(), run.clone(), sched);
        tokio::spawn(async move { hub.execute(r, s).await });
        Ok(run)
    }

    async fn execute(self: Arc<Self>, run: Run, sched: Schedule) {
        let outcome = self.copy(&run, &sched).await;
        let now = clock::now();
        let (state, size, objects, note) = match &outcome {
            Ok((size, objects)) => (store::SUCCEEDED, *size, *objects, String::new()),
            Err(m) => (store::FAILED, 0, 0, note_of(m)),
        };
        if let Err(e) = self
            .store
            .finish(
                &run.id,
                state,
                now,
                i64::try_from(size).unwrap_or(i64::MAX),
                i64::try_from(objects).unwrap_or(i64::MAX),
                run.encrypted && outcome.is_ok(),
                &note,
            )
            .await
        {
            tracing::error!(class = %crate::ai::store::class(&e), "backup record");
        }
        let result = if outcome.is_ok() { "ok" } else { "failed" };
        self.record(Entry {
            target: &run.id,
            details: &[
                ("target", run.target.clone()),
                ("result", result.to_owned()),
                ("bytes", size.to_string()),
            ],
            ..Entry::new(
                "backup.finished",
                if run.actor.is_empty() {
                    "system"
                } else {
                    &run.actor
                },
            )
        })
        .await;
        if outcome.is_ok() {
            self.prune().await;
        }
    }

    /// Writes the backup and moves it to its target. `(stored bytes, objects)`.
    async fn copy(&self, run: &Run, sched: &Schedule) -> Result<(u64, u64), String> {
        let key = self.backup_key()?;
        if sched.include_vault_key && key.is_none() {
            return Err("the vault key is included only with a backup key file".into());
        }
        let place = self.place(&run.target).map_err(|e| format!("{e:?}"))?;
        let opts = DumpOptions {
            key_file: self.cfg.key_file.clone(),
            include_vault_key: sched.include_vault_key,
            include_objects: sched.include_objects,
        };
        let out = match place.local_dir(&run.location) {
            Some(d) => d,
            None => self.staging().join(&run.location),
        };
        if let Some(parent) = out.parent() {
            crate::workdir::create_private_all(parent).map_err(|e| e.to_string())?;
        }
        let dumped = self.engine.dump(&out, &opts).await;
        let sizes = match dumped {
            Ok(()) => place::list_dir(&out).map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        let files = match sizes {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&out);
                return Err(e);
            }
        };
        let total: u64 = files.iter().map(|(_, n)| n).sum();
        let view: ManifestView = std::fs::read(out.join(place::MANIFEST))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if place.is_local() {
            let _ = self
                .store
                .progress(
                    &run.id,
                    i64::try_from(total).unwrap_or(0),
                    i64::try_from(total).unwrap_or(0),
                )
                .await;
            return Ok((total, view.objects));
        }
        let _ = self
            .store
            .progress(&run.id, 0, i64::try_from(total).unwrap_or(0))
            .await;
        let (store_, id) = (self.store.clone(), run.id.clone());
        let progress = move |sent: u64| {
            let (s, i) = (store_.clone(), id.clone());
            tokio::spawn(async move {
                let _ = s
                    .progress(
                        &i,
                        i64::try_from(sent).unwrap_or(0),
                        i64::try_from(total).unwrap_or(0),
                    )
                    .await;
            });
        };
        let up = place.upload_dir(&run.location, &out, key, &progress).await;
        let _ = std::fs::remove_dir_all(&out);
        up.map(|stored| (stored, view.objects))
    }

    fn staging(&self) -> PathBuf {
        self.cfg
            .staging_dir
            .clone()
            .unwrap_or_else(|| self.state_dir.join("staging"))
    }

    // -------------------------------------------------------------------------- verify

    /// Starts a check of a finished backup; the answer is in the record when it is done.
    pub async fn start_verify(self: &Arc<Self>, id: &str) -> Result<Run, BackupsError> {
        let run = self.run(id).await?;
        if run.state != store::SUCCEEDED || run.purged {
            return Err(BackupsError::Precondition(
                "only a finished backup that is still held can be verified",
            ));
        }
        if run.verify == store::V_RUNNING {
            return Err(BackupsError::Precondition(
                "a check of this backup is already running",
            ));
        }
        self.store.set_verify(id, store::V_RUNNING, 0, None).await?;
        let (hub, r) = (self.clone(), run.clone());
        tokio::spawn(async move { hub.verify_job(r).await });
        Ok(Run {
            verify: store::V_RUNNING,
            ..run
        })
    }

    async fn verify_job(self: Arc<Self>, run: Run) {
        let outcome = self.check(&run).await;
        let now = clock::now();
        let (v, note) = match &outcome {
            Ok(note) => (store::V_OK, note.clone()),
            Err(m) => (store::V_FAILED, note_of(m)),
        };
        if let Err(e) = self.store.set_verify(&run.id, v, now, Some(&note)).await {
            tracing::error!(class = %crate::ai::store::class(&e), "backup verify record");
        }
        self.record(Entry {
            target: &run.id,
            details: &[(
                "result",
                if outcome.is_ok() { "ok" } else { "failed" }.to_owned(),
            )],
            ..Entry::new("backup.verified", "system")
        })
        .await;
    }

    async fn check(&self, run: &Run) -> Result<String, String> {
        let place = self.place(&run.target).map_err(|e| format!("{e:?}"))?;
        match place.local_dir(&run.location) {
            Some(dir) => {
                if !dir.exists() {
                    return Err("the backup's files are not on the target".into());
                }
                let engine = self.engine.clone();
                tokio::task::spawn_blocking(move || engine.verify(&dir))
                    .await
                    .map_err(|e| e.to_string())??;
                Ok("Restore test passed".into())
            }
            None => {
                place.check(&run.location).await?;
                Ok("Files present at their stored size".into())
            }
        }
    }

    // -------------------------------------------------------------------------- restore

    /// What a restore of `id` would do, and whether it may go ahead.
    pub async fn preview(&self, id: &str) -> Result<(Run, ManifestView), BackupsError> {
        let run = self.run(id).await?;
        if run.state != store::SUCCEEDED || run.purged {
            return Err(BackupsError::Precondition(
                "only a finished backup that is still held can be restored",
            ));
        }
        let place = self.place(&run.target)?;
        let raw = place
            .read_manifest(&run.location)
            .await
            .map_err(|_| BackupsError::Precondition("the backup's files are not on the target"))?;
        let view = serde_json::from_slice(&raw)
            .map_err(|_| BackupsError::Precondition("the backup's manifest is damaged"))?;
        Ok((run, view))
    }

    /// Stages a restore. `confirm` must be exactly [`CONFIRM`].
    pub async fn restore(
        self: &Arc<Self>,
        id: &str,
        confirm: &str,
        actor: &str,
    ) -> Result<Run, BackupsError> {
        if confirm != CONFIRM {
            return Err(BackupsError::Invalid("type RESTORE to confirm"));
        }
        let (run, _) = self.preview(id).await?;
        if run.verify != store::V_OK {
            return Err(BackupsError::Precondition(
                "verify the backup before restoring it",
            ));
        }
        if self.store.pending_restore().await?.is_some() {
            return Err(BackupsError::Precondition("a restore is already staged"));
        }
        let place = self.place(&run.target)?;
        let rid = rand_id("rs_", 20);
        self.store
            .insert_restore(&Restore {
                id: rid.clone(),
                backup_id: run.id.clone(),
                state: store::R_PREPARING,
                requested: clock::now(),
                actor: actor.to_owned(),
                note: String::new(),
            })
            .await?;
        match place.local_dir(&run.location) {
            Some(dir) => {
                self.write_marker(&run, dir, false).map_err(|_| {
                    BackupsError::Precondition("the restore could not be written down")
                })?;
                self.store.set_restore(&rid, store::R_STAGED, "").await?;
            }
            None => {
                let (hub, r) = (self.clone(), run.clone());
                tokio::spawn(async move { hub.prepare_download(r, rid).await });
            }
        }
        Ok(run)
    }

    fn write_marker(&self, run: &Run, from: PathBuf, cleanup: bool) -> Result<(), String> {
        crate::workdir::create_private_all(&self.state_dir).map_err(|e| e.to_string())?;
        let m = Marker {
            backup_id: run.id.clone(),
            from,
            key_file: self.cfg.key_file.clone(),
            cleanup,
        };
        let tmp = self.state_dir.join("restore.pending.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = crypto::create_private_file(&tmp).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut f, &serde_json::to_vec(&m).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, self.state_dir.join(PENDING)).map_err(|e| e.to_string())
    }

    async fn prepare_download(self: Arc<Self>, run: Run, rid: String) {
        let res = async {
            let key = self.backup_key()?;
            let place = self.place(&run.target).map_err(|e| format!("{e:?}"))?;
            let dir = self.state_dir.join("restore").join(&run.id);
            let _ = std::fs::remove_dir_all(&dir);
            place.download_dir(&run.location, &dir, key).await?;
            let (engine, d) = (self.engine.clone(), dir.clone());
            tokio::task::spawn_blocking(move || engine.verify(&d))
                .await
                .map_err(|e| e.to_string())??;
            self.write_marker(&run, dir, true)
        }
        .await;
        let (state, note) = match &res {
            Ok(()) => (store::R_STAGED, String::new()),
            Err(m) => (store::R_FAILED, note_of(m)),
        };
        if res.is_err() {
            let _ = std::fs::remove_dir_all(self.state_dir.join("restore").join(&run.id));
        }
        if let Err(e) = self.store.set_restore(&rid, state, &note).await {
            tracing::error!(class = %crate::ai::store::class(&e), "restore record");
        }
    }

    /// Whether a restore waits for the next start, and for which backup.
    pub async fn pending(&self) -> Result<Option<Restore>, BackupsError> {
        Ok(self
            .store
            .pending_restore()
            .await?
            .filter(|r| r.state == store::R_STAGED))
    }

    // -------------------------------------------------------------------------- retention

    /// Removes the backups retention no longer keeps from their targets.
    pub async fn prune(&self) {
        let Ok(sched) = self.store.schedule().await else {
            return;
        };
        let Ok(held) = self.store.held().await else {
            return;
        };
        let now = clock::now();
        let kept: Vec<plan::Kept> = held
            .iter()
            .map(|r| plan::Kept {
                id: r.id.clone(),
                finished: r.finished,
                succeeded: r.state == store::SUCCEEDED,
            })
            .collect();
        for id in plan::expired(&kept, now, sched.keep_days, sched.keep_weeks) {
            let Some(run) = held.iter().find(|r| r.id == id) else {
                continue;
            };
            let removed = match self.place(&run.target) {
                Ok(p) => p.delete(&run.location).await.is_ok(),
                // The target left the configuration: nothing to remove, the record is closed.
                Err(_) => true,
            };
            if removed {
                let _ = self.store.mark_purged(&id).await;
                self.record(Entry {
                    target: &id,
                    details: &[("target", run.target.clone())],
                    ..Entry::new("backup.purged", "system")
                })
                .await;
            }
        }
        let horizon = i64::from(sched.keep_days) + i64::from(sched.keep_weeks) * 7 + 400;
        let _ = self.store.forget_purged(now - horizon * 86_400).await;
    }

    // -------------------------------------------------------------------------- scheduler

    /// Starts the scheduler. A run left running by a dead process is closed first.
    pub fn spawn_scheduler(self: &Arc<Self>) -> JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            if let Err(e) = hub.store.fail_orphans(clock::now()).await {
                tracing::error!(class = %crate::ai::store::class(&e), "backup recovery");
            }
            while !hub.stopping.load(Ordering::Relaxed) {
                hub.tick().await;
                tokio::time::sleep(TICK).await;
            }
        })
    }

    /// One look at the clock: starts the scheduled run when it is due.
    pub async fn tick(self: &Arc<Self>) {
        let Ok(s) = self.store.schedule().await else {
            return;
        };
        if !s.enabled || !self.cfg.usable() {
            return;
        }
        let Ok(due) = self.next_run(&s).await else {
            return;
        };
        if clock::now() >= due {
            match self.start_run(&s.target, store::TRIG_SCHEDULE, "").await {
                Ok(_) | Err(BackupsError::Precondition(_)) => {}
                Err(e) => tracing::error!(error = ?e, "scheduled backup"),
            }
        }
        self.prune().await;
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}
