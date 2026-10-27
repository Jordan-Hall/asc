//! The server's secret vault (docs/BUILDOUT.md, "Secrets"; `asc.v1.Vault`).
//!
//! Storage credentials, AI provider keys and every other stored secret are sealed here
//! ([`Vault::seal`]) with XChaCha20-Poly1305 under a data key derived from the master key, bound
//! to the kind, owner and row ([`SecretContext`]). The master key ring is a private file
//! ([`keyring`]); [`Vault::rotate`] adds a generation and re-seals every value in the background
//! through the [`Resealer`] each feature registers.
//!
//! Nothing in this module logs, formats or returns a key or a plaintext. Types that hold one
//! print `<redacted>`; [`VaultError`] says what went wrong and never with what.
//!
//! A feature that stores a secret does three things: seal with [`Vault::seal`] (keeping the
//! generation next to the blob), open with [`Vault::open`], and register a [`Resealer`] so a
//! rotation reaches its rows. `storage` is the first user; AI keys follow the same shape.

pub mod crypto;
pub mod keyring;

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use sqlx::{AnyPool, Row};
use zeroize::Zeroizing;

pub use crypto::SecretContext;
use keyring::KeyRing;

/// Values re-sealed per batch.
const RESEAL_BATCH: usize = 200;
/// A rotation that has not made progress for this long is taken over by the next start.
const STALE_AFTER_SECS: i64 = 120;
/// Wait between attempts to unlock a locked vault.
const UNLOCK_RETRY: Duration = Duration::from_secs(5);

/// `[vault]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VaultConfig {
    /// The master key file. Created on first start with mode `0600`. When unset, the file is
    /// `vault.keys` next to the SQLite database, or in `cache.disk_dir`; with neither, the vault
    /// stays locked and says so. Several replicas share one file.
    pub key_file: Option<PathBuf>,
}

/// Why a vault call failed. The text never holds a secret.
#[derive(Debug, Clone, thiserror::Error)]
pub enum VaultError {
    /// The key file cannot be used: missing directory, wrong mode, unparseable, unset.
    #[error("vault locked: {0}")]
    Locked(String),
    /// The value does not open: another owner or row, a changed byte, or a generation the key
    /// file does not hold.
    #[error("sealed value does not open")]
    Corrupt,
    #[error("the secret is too large")]
    TooLarge,
    #[error("cryptographic failure")]
    Crypto,
    #[error("a rotation is already running")]
    RotationActive,
    #[error("vault database: {0}")]
    Sql(String),
}

impl VaultError {
    fn sql(e: &sqlx::Error) -> Self {
        // The driver error can quote a statement; keep only its class.
        let class = match e {
            sqlx::Error::Database(_) => "statement failed",
            sqlx::Error::PoolTimedOut => "pool timed out",
            sqlx::Error::Io(_) => "io",
            _ => "error",
        };
        Self::Sql(class.into())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A feature's sealed rows, as the vault sees them for a rotation.
#[async_trait]
pub trait Resealer: Send + Sync {
    /// The `kind` of the contexts this feature seals under.
    fn kind(&self) -> &'static str;

    /// How many sealed values exist per generation.
    async fn counts(&self) -> Result<Vec<(u32, u64)>, VaultError>;

    /// Re-seals up to `limit` values that are not under `target`: open with the old generation,
    /// [`Vault::seal`] again, and replace the row only if it still holds the old blob. Returns
    /// how many were re-sealed; 0 means none are left. A value that does not open is an error
    /// (the rotation stops with it), never skipped.
    async fn reseal_batch(
        &self,
        vault: &Vault,
        target: u32,
        limit: usize,
    ) -> Result<usize, VaultError>;
}

enum State {
    Ready(Arc<KeyRing>),
    Locked(String),
}

/// Facts for `Vault.Status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub ready: bool,
    /// Why the vault is locked, when it is. Contains no secret.
    pub locked_reason: Option<String>,
    pub generations: Vec<GenerationInfo>,
    pub key_file_mode: String,
    pub rotation_active: bool,
}

/// One master key generation, without the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInfo {
    pub generation: u32,
    pub created: i64,
    pub active: bool,
    /// Sealed values still under it.
    pub secrets: u64,
}

/// A rotation's progress (`vault_rotations`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rotation {
    pub id: String,
    pub to_generation: u32,
    pub total: u64,
    pub done: u64,
    pub finished: bool,
    pub error: String,
}

/// The vault. Share one `Arc<Vault>`.
pub struct Vault {
    path: Option<PathBuf>,
    state: RwLock<State>,
    last_unlock: Mutex<Instant>,
    /// Size and modification time of the key file when the ring was last read.
    file_stamp: Mutex<Option<(u64, SystemTime)>>,
    pool: AnyPool,
    resealers: RwLock<Vec<Arc<dyn Resealer>>>,
    running: AtomicBool,
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vault")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn stamp(path: &std::path::Path) -> Option<(u64, SystemTime)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

impl Vault {
    /// Opens (or creates) the key file at `path`. A file that cannot be used leaves the vault
    /// locked; the server keeps running and every sealing call answers `VAULT_LOCKED`.
    pub fn open_file(path: PathBuf, pool: AnyPool) -> Arc<Self> {
        let state = match keyring::load_or_create(&path, now()) {
            Ok(ring) => State::Ready(Arc::new(ring)),
            Err(e) => {
                tracing::error!(error = %e, "the vault is locked");
                State::Locked(reason_of(&e))
            }
        };
        let file_stamp = stamp(&path);
        Arc::new(Self {
            path: Some(path),
            state: RwLock::new(state),
            last_unlock: Mutex::new(Instant::now()),
            file_stamp: Mutex::new(file_stamp),
            pool,
            resealers: RwLock::default(),
            running: AtomicBool::new(false),
        })
    }

    /// A vault that has no key file to use (nothing configured). Locked, and says why.
    pub fn locked(reason: &str, pool: AnyPool) -> Arc<Self> {
        Arc::new(Self {
            path: None,
            state: RwLock::new(State::Locked(reason.to_owned())),
            last_unlock: Mutex::new(Instant::now()),
            file_stamp: Mutex::new(None),
            pool,
            resealers: RwLock::default(),
            running: AtomicBool::new(false),
        })
    }

    /// A vault over a ring that lives only in memory. For tests.
    pub fn in_memory(ring: KeyRing, pool: AnyPool) -> Arc<Self> {
        Arc::new(Self {
            path: None,
            state: RwLock::new(State::Ready(Arc::new(ring))),
            last_unlock: Mutex::new(Instant::now()),
            file_stamp: Mutex::new(None),
            pool,
            resealers: RwLock::default(),
            running: AtomicBool::new(false),
        })
    }

    /// Registers a feature's sealed rows for rotation and status counts.
    pub fn register(&self, resealer: Arc<dyn Resealer>) {
        if let Ok(mut r) = self.resealers.write() {
            r.retain(|x| x.kind() != resealer.kind());
            r.push(resealer);
        }
    }

    fn resealers(&self) -> Vec<Arc<dyn Resealer>> {
        self.resealers.read().map(|r| r.clone()).unwrap_or_default()
    }

    /// The current ring: reloaded when the file changed (another replica rotated), and retried
    /// when locked.
    fn ring(&self) -> Result<Arc<KeyRing>, VaultError> {
        if let Some(path) = &self.path {
            let now_stamp = stamp(path);
            let changed = self.file_stamp.lock().is_ok_and(|s| *s != now_stamp);
            let locked = matches!(self.state.read().as_deref(), Ok(State::Locked(_)));
            let retry = locked
                && self
                    .last_unlock
                    .lock()
                    .is_ok_and(|t| t.elapsed() >= UNLOCK_RETRY);
            if changed || retry {
                if let Ok(mut t) = self.last_unlock.lock() {
                    *t = Instant::now();
                }
                match keyring::load(path) {
                    Ok(ring) => {
                        if let Ok(mut s) = self.state.write() {
                            *s = State::Ready(Arc::new(ring));
                        }
                        if let Ok(mut s) = self.file_stamp.lock() {
                            *s = now_stamp;
                        }
                    }
                    Err(e) => {
                        if locked || changed {
                            tracing::error!(error = %e, "the vault key file cannot be used");
                        }
                        if let Ok(mut s) = self.state.write() {
                            *s = State::Locked(reason_of(&e));
                        }
                    }
                }
            }
        }
        match &*self.state.read().map_err(|_| VaultError::Crypto)? {
            State::Ready(r) => Ok(r.clone()),
            State::Locked(why) => Err(VaultError::Locked(why.clone())),
        }
    }

    /// Whether sealing works now.
    pub fn ready(&self) -> bool {
        self.ring().is_ok()
    }

    /// The generation new values are sealed under.
    pub fn active_generation(&self) -> Result<u32, VaultError> {
        Ok(self.ring()?.active())
    }

    /// Seals `plaintext` for `ctx` under the active generation. Returns the generation and the
    /// blob to store (keep both: the generation in a column makes a rotation cheap to resume).
    pub fn seal(
        &self,
        ctx: &SecretContext<'_>,
        plaintext: &[u8],
    ) -> Result<(u32, Vec<u8>), VaultError> {
        let ring = self.ring()?;
        let g = ring.active_key();
        let blob = crypto::seal(g.key(), g.generation, ctx, plaintext)?;
        Ok((g.generation, blob))
    }

    /// Opens a sealed value. A generation the file does not hold is reported as locked after one
    /// reload, since the usual cause is a key file that does not match the database.
    pub fn open(
        &self,
        ctx: &SecretContext<'_>,
        sealed: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let generation = crypto::generation_of(sealed).ok_or(VaultError::Corrupt)?;
        let ring = self.ring()?;
        let Some(g) = ring.get(generation) else {
            return Err(VaultError::Locked(format!(
                "the key file has no generation {generation}"
            )));
        };
        crypto::open(g.key(), ctx, sealed)
    }

    /// Re-seals one value under the active generation. `None` when it already is.
    pub fn reseal(
        &self,
        ctx: &SecretContext<'_>,
        sealed: &[u8],
    ) -> Result<Option<(u32, Vec<u8>)>, VaultError> {
        let active = self.active_generation()?;
        if crypto::generation_of(sealed) == Some(active) {
            return Ok(None);
        }
        let plain = self.open(ctx, sealed)?;
        self.seal(ctx, &plain).map(Some)
    }

    // -------------------------------------------------------------------------- status

    /// What `Vault.Status` reports.
    pub async fn status(&self) -> Status {
        let rotation_active = self.rotation_in_progress().await.unwrap_or(false);
        let mode = self
            .path
            .as_deref()
            .map_or_else(|| "none".to_owned(), keyring::mode_text);
        let ring = self.ring();
        let locked_reason = match &ring {
            Err(VaultError::Locked(why)) => Some(why.clone()),
            Err(e) => Some(e.to_string()),
            Ok(_) => None,
        };
        let mut generations = Vec::new();
        if let Ok(ring) = &ring {
            let mut counts: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
            for r in self.resealers() {
                for (g, n) in r.counts().await.unwrap_or_default() {
                    *counts.entry(g).or_default() += n;
                }
            }
            for g in ring.generations() {
                generations.push(GenerationInfo {
                    generation: g.generation,
                    created: g.created,
                    active: g.generation == ring.active(),
                    secrets: counts.get(&g.generation).copied().unwrap_or(0),
                });
            }
        }
        Status {
            ready: ring.is_ok() && (mode == "0600" || self.path.is_none()),
            locked_reason,
            generations,
            key_file_mode: mode,
            rotation_active,
        }
    }

    // -------------------------------------------------------------------------- rotation

    async fn rotation_in_progress(&self) -> Result<bool, VaultError> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM vault_rotations WHERE finished = 0 AND updated > $1",
        )
        .bind(now() - STALE_AFTER_SECS)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| VaultError::sql(&e))?;
        Ok(n > 0 || self.running.load(Ordering::SeqCst))
    }

    /// Adds a master key generation, makes it active and re-seals every value in the background.
    /// The key file is written before anything else changes: if that fails, nothing did.
    pub async fn rotate(self: &Arc<Self>, retire_old: bool) -> Result<Rotation, VaultError> {
        if self.rotation_in_progress().await? {
            return Err(VaultError::RotationActive);
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(VaultError::RotationActive);
        }
        let started = self.start_rotation(retire_old).await;
        if started.is_err() {
            self.running.store(false, Ordering::SeqCst);
        }
        started
    }

    async fn start_rotation(self: &Arc<Self>, retire_old: bool) -> Result<Rotation, VaultError> {
        let ring = self.ring()?;
        let next = ring.with_new_generation(now())?;
        // Only one replica may take the step from this generation to the next; the others are
        // told a rotation is running. This comes before the key file is written.
        let holder = uuid::Uuid::new_v4().simple().to_string();
        self.claim_generation(ring.active(), next.active(), &holder)
            .await?;
        if let Some(path) = &self.path {
            if let Err(e) = keyring::store(path, &next) {
                self.release_generation(ring.active(), next.active(), &holder)
                    .await;
                return Err(e);
            }
            if let Ok(mut s) = self.file_stamp.lock() {
                *s = stamp(path);
            }
        }
        let target = next.active();
        if let Ok(mut s) = self.state.write() {
            *s = State::Ready(Arc::new(next));
        }
        let total = self.count_not_under(target).await?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let t = now();
        sqlx::query(
            "INSERT INTO vault_rotations (id, to_generation, total, done, finished, retire_old, error, started, updated) \
             VALUES ($1, $2, $3, 0, 0, $4, NULL, $5, $5)",
        )
        .bind(&id)
        .bind(i64::from(target))
        .bind(i64::try_from(total).unwrap_or(i64::MAX))
        .bind(i64::from(retire_old))
        .bind(t)
        .execute(&self.pool)
        .await
        .map_err(|e| VaultError::sql(&e))?;
        tracing::info!(
            generation = target,
            values = total,
            "vault rotation started"
        );
        let vault = self.clone();
        let job = id.clone();
        tokio::spawn(async move { vault.run_rotation(job, retire_old, 0).await });
        Ok(Rotation {
            id,
            to_generation: target,
            total,
            done: 0,
            finished: false,
            error: String::new(),
        })
    }

    /// Moves the shared generation from `old` to `new` (compare-and-swap). A claim that was made
    /// and never finished (its replica died before it wrote the key file) can be taken again
    /// once it is stale.
    async fn claim_generation(&self, old: u32, new: u32, holder: &str) -> Result<(), VaultError> {
        let sql = |e: sqlx::Error| VaultError::sql(&e);
        sqlx::query(
            "INSERT INTO vault_state (id, generation, holder, updated) VALUES (1, $1, '', 0)              ON CONFLICT (id) DO NOTHING",
        )
        .bind(i64::from(old))
        .execute(&self.pool)
        .await
        .map_err(sql)?;
        let t = now();
        let won = sqlx::query(
            "UPDATE vault_state SET generation = $2, holder = $3, updated = $4              WHERE id = 1 AND (generation = $1 OR (generation = $2 AND updated < $5))",
        )
        .bind(i64::from(old))
        .bind(i64::from(new))
        .bind(holder)
        .bind(t)
        .bind(t - STALE_AFTER_SECS)
        .execute(&self.pool)
        .await
        .map_err(sql)?
        .rows_affected();
        if won == 1 {
            Ok(())
        } else {
            Err(VaultError::RotationActive)
        }
    }

    /// Undoes [`claim_generation`](Self::claim_generation) when the key file could not be written.
    async fn release_generation(&self, old: u32, new: u32, holder: &str) {
        let _ = sqlx::query(
            "UPDATE vault_state SET generation = $1 WHERE id = 1 AND generation = $2 AND holder = $3",
        )
        .bind(i64::from(old))
        .bind(i64::from(new))
        .bind(holder)
        .execute(&self.pool)
        .await;
    }

    async fn count_not_under(&self, target: u32) -> Result<u64, VaultError> {
        let mut n = 0u64;
        for r in self.resealers() {
            for (g, c) in r.counts().await? {
                if g != target {
                    n = n.saturating_add(c);
                }
            }
        }
        Ok(n)
    }

    async fn run_rotation(self: Arc<Self>, id: String, retire_old: bool, done_before: u64) {
        let outcome = self.reseal_everything(&id, done_before).await;
        let outcome = match outcome {
            Ok(()) if retire_old => self.retire_unused().await,
            other => other,
        };
        let (error, msg) = match &outcome {
            Ok(()) => (None, "finished"),
            Err(e) => (Some(e.to_string()), "failed"),
        };
        let _ = sqlx::query(
            "UPDATE vault_rotations SET finished = 1, error = $1, updated = $2 WHERE id = $3",
        )
        .bind(error)
        .bind(now())
        .bind(&id)
        .execute(&self.pool)
        .await;
        self.running.store(false, Ordering::SeqCst);
        tracing::info!(outcome = msg, "vault rotation ended");
    }

    async fn reseal_everything(&self, id: &str, mut done: u64) -> Result<(), VaultError> {
        let target = self.active_generation()?;
        for r in self.resealers() {
            loop {
                let n = r.reseal_batch(self, target, RESEAL_BATCH).await?;
                if n == 0 {
                    break;
                }
                done = done.saturating_add(n as u64);
                sqlx::query("UPDATE vault_rotations SET done = $1, updated = $2 WHERE id = $3")
                    .bind(i64::try_from(done).unwrap_or(i64::MAX))
                    .bind(now())
                    .bind(id)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| VaultError::sql(&e))?;
            }
        }
        Ok(())
    }

    /// Drops the retired generations nothing is sealed under, and rewrites the key file.
    async fn retire_unused(&self) -> Result<(), VaultError> {
        let ring = self.ring()?;
        let mut used = std::collections::HashSet::new();
        for r in self.resealers() {
            for (g, c) in r.counts().await? {
                if c > 0 {
                    used.insert(g);
                }
            }
        }
        let drop: Vec<u32> = ring
            .generations()
            .iter()
            .map(|g| g.generation)
            .filter(|g| *g != ring.active() && !used.contains(g))
            .collect();
        if drop.is_empty() {
            return Ok(());
        }
        let next = ring.without(&drop);
        if let Some(path) = &self.path {
            keyring::store(path, &next)?;
            if let Ok(mut s) = self.file_stamp.lock() {
                *s = stamp(path);
            }
        }
        if let Ok(mut s) = self.state.write() {
            *s = State::Ready(Arc::new(next));
        }
        Ok(())
    }

    /// Takes over a rotation a stopped process left unfinished. Call once at start.
    pub async fn resume(self: &Arc<Self>) -> Result<Option<String>, VaultError> {
        let row = sqlx::query(
            "SELECT id, retire_old, done FROM vault_rotations WHERE finished = 0 AND updated <= $1 \
             ORDER BY started LIMIT 1",
        )
        .bind(now() - STALE_AFTER_SECS)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VaultError::sql(&e))?;
        let Some(row) = row else { return Ok(None) };
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(None);
        }
        let id: String = row.try_get("id").map_err(|e| VaultError::sql(&e))?;
        let retire: i64 = row.try_get("retire_old").map_err(|e| VaultError::sql(&e))?;
        let done: i64 = row.try_get("done").map_err(|e| VaultError::sql(&e))?;
        // Claim it so a second replica does not also take it over.
        let claimed = sqlx::query(
            "UPDATE vault_rotations SET updated = $1 WHERE id = $2 AND finished = 0 AND updated <= $3",
        )
        .bind(now())
        .bind(&id)
        .bind(now() - STALE_AFTER_SECS)
        .execute(&self.pool)
        .await
        .map_err(|e| VaultError::sql(&e))?;
        if claimed.rows_affected() != 1 {
            self.running.store(false, Ordering::SeqCst);
            return Ok(None);
        }
        tracing::info!("resuming an unfinished vault rotation");
        let vault = self.clone();
        let job = id.clone();
        tokio::spawn(async move {
            vault
                .run_rotation(job, retire != 0, u64::try_from(done).unwrap_or(0))
                .await;
        });
        Ok(Some(id))
    }

    /// A rotation by id.
    pub async fn rotation(&self, id: &str) -> Result<Option<Rotation>, VaultError> {
        let row = sqlx::query(
            "SELECT id, to_generation, total, done, finished, error FROM vault_rotations WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VaultError::sql(&e))?;
        row.map(|r| rotation_of(&r)).transpose()
    }

    /// The newest rotation, finished or not.
    pub async fn latest_rotation(&self) -> Result<Option<Rotation>, VaultError> {
        let row = sqlx::query(
            "SELECT id, to_generation, total, done, finished, error FROM vault_rotations \
             ORDER BY started DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VaultError::sql(&e))?;
        row.map(|r| rotation_of(&r)).transpose()
    }

    /// Waits until no rotation runs in this process. For tests.
    pub async fn wait_idle(&self) {
        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn rotation_of(r: &sqlx::any::AnyRow) -> Result<Rotation, VaultError> {
    let e = |e: sqlx::Error| VaultError::sql(&e);
    let to: i64 = r.try_get("to_generation").map_err(e)?;
    let total: i64 = r.try_get("total").map_err(e)?;
    let done: i64 = r.try_get("done").map_err(e)?;
    let finished: i64 = r.try_get("finished").map_err(e)?;
    let error: Option<String> = r.try_get("error").map_err(e)?;
    Ok(Rotation {
        id: r.try_get("id").map_err(e)?,
        to_generation: u32::try_from(to).unwrap_or(0),
        total: u64::try_from(total).unwrap_or(0),
        done: u64::try_from(done).unwrap_or(0),
        finished: finished != 0,
        error: error.unwrap_or_default(),
    })
}

/// The reason a locked vault shows to the operator: the error without the `vault locked:` lead.
fn reason_of(e: &VaultError) -> String {
    match e {
        VaultError::Locked(why) => why.clone(),
        other => other.to_string(),
    }
}

/// The key file used when `[vault] key_file` is unset: `vault.keys` next to the SQLite database,
/// else in `dir` (the cache disk directory).
pub fn default_key_file(db: Option<&str>, dir: Option<&std::path::Path>) -> Option<PathBuf> {
    if let Some(db) = db
        && let Some(rest) = db.strip_prefix("sqlite:")
    {
        let file = rest
            .trim_start_matches("//")
            .split('?')
            .next()
            .unwrap_or("");
        if !file.is_empty()
            && !file.starts_with(':')
            && let Some(parent) = std::path::Path::new(file).parent()
        {
            return Some(parent.join("vault.keys"));
        }
    }
    dir.map(|d| d.join("vault.keys"))
}

/// The vault the server config asks for: the key file named by `[vault] key_file`, else
/// [`default_key_file`]; locked, with the reason, when there is none.
pub fn from_config(
    cfg: &VaultConfig,
    db: Option<&str>,
    dir: Option<&std::path::Path>,
    pool: AnyPool,
) -> Arc<Vault> {
    match cfg.key_file.clone().or_else(|| default_key_file(db, dir)) {
        Some(path) => Vault::open_file(path, pool),
        None => Vault::locked(
            "set [vault] key_file (no SQLite database or cache.disk_dir to put it next to)",
            pool,
        ),
    }
}

#[cfg(test)]
mod tests;
