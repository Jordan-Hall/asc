//! `asc serve backup` and `asc serve restore` (docs/runbooks/backup.md).
//!
//! A backup is a directory, created with mode 0700, whose files are mode 0600:
//!
//! * `db.sqlite`, a consistent snapshot made with `VACUUM INTO` while the server runs, or
//!   `db.pgdump`, from `pg_dump --format=custom`, for a Postgres `db`;
//! * `objects/…` and `objects.idx`, every object of the instance bucket (`store`) with its size
//!   and SHA-256; `--no-objects` leaves them out when the bucket is versioned and backed up on
//!   its own. Bring-your-own storage buckets belong to their owners and are not copied;
//! * `vault.keys.sealed`, the vault master key file sealed under a backup key (`--include-vault-key`
//!   with `--key-file`); without both it is left out. The key is never written in the clear, and a
//!   database backup without it cannot open a stored secret;
//! * `manifest.json`, the version, the time and the size and SHA-256 of every other file.
//!
//! `restore` reads the manifest, checks every file against it, refuses to write over a database,
//! a bucket or a key file that already holds data unless `--force`, then writes the pieces and
//! checks the result (SQLite `integrity_check`, the object count and sizes).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use futures::StreamExt as _;
use hkdf::Hkdf;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{AnyPool, Row};

use crate::config::ServeConfig;
use crate::migrate;
use crate::registry::Registry;

/// The manifest's file name.
pub const MANIFEST: &str = "manifest.json";
const FORMAT: u32 = 1;
const DB_SQLITE: &str = "db.sqlite";
const DB_PG: &str = "db.pgdump";
const OBJECT_INDEX: &str = "objects.idx";
const OBJECTS_DIR: &str = "objects";
const SEALED_KEY: &str = "vault.keys.sealed";
const SEAL_CONTEXT: &[u8] = b"asc-backup-v1";
const NONCE_LEN: usize = 24;
/// Longest manifest read.
const MAX_MANIFEST: u64 = 16 * 1024 * 1024;

/// Why a backup or restore stopped. The text says what to do.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BackupError(pub String);

fn fail<T>(m: impl Into<String>) -> Result<T, BackupError> {
    Err(BackupError(m.into()))
}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        Self(format!("io: {e}"))
    }
}
impl From<sqlx::Error> for BackupError {
    fn from(e: sqlx::Error) -> Self {
        Self(format!("database: {e}"))
    }
}
impl From<object_store::Error> for BackupError {
    fn from(e: object_store::Error) -> Self {
        Self(format!("object store: {e}"))
    }
}
impl From<serde_json::Error> for BackupError {
    fn from(e: serde_json::Error) -> Self {
        Self(format!("manifest: {e}"))
    }
}

/// What `backup` takes.
#[derive(Debug, Clone, Default)]
pub struct BackupOptions {
    /// The directory to write; it must not exist or must be empty.
    pub out: PathBuf,
    /// The file with the 32-byte (raw or hex) key the vault key is sealed under.
    pub key_file: Option<PathBuf>,
    /// Put the sealed vault master key in the backup (needs `key_file`).
    pub include_vault_key: bool,
    /// Leave the bucket's objects out.
    pub no_objects: bool,
}

/// What `restore` takes.
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    pub from: PathBuf,
    pub key_file: Option<PathBuf>,
    /// Write over a database, bucket or key file that holds data.
    pub force: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileEntry {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    created: i64,
    asc_version: String,
    /// `sqlite` or `postgres`.
    db_kind: String,
    /// `none` or `sealed`.
    vault_key: String,
    objects: u64,
    object_bytes: u64,
    files: Vec<FileEntry>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The size and SHA-256 of a file, read in pieces.
fn digest_file(path: &Path) -> Result<(u64, String), BackupError> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let n = std::io::copy(&mut f, &mut HashWriter(&mut h))?;
    Ok((n, hex(&h.finalize())))
}

struct HashWriter<'a>(&'a mut Sha256);

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A new file for a backup: mode 0600, never over an existing one.
fn create_file(path: &Path) -> Result<std::fs::File, BackupError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    Ok(o.open(path)?)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), BackupError> {
    let mut f = create_file(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// The key a vault key is sealed under: 32 raw bytes or 64 hex digits, in a file only this user
/// can read.
fn load_backup_key(path: &Path) -> Result<[u8; 32], BackupError> {
    let bad = |m: &str| BackupError(format!("backup key file {}: {m}", path.display()));
    let file = crate::authz::key_file::open_private(path).map_err(|e| bad(&e.to_string()))?;
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, 130), &mut raw)?;
    let key: Vec<u8> = if raw.len() == 32 {
        raw
    } else {
        let t = std::str::from_utf8(&raw)
            .map_err(|_| bad("not a 32-byte key"))?
            .trim();
        if t.len() != 64 || !t.is_ascii() {
            return Err(bad("not a 32-byte key"));
        }
        (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&t[i..i + 2], 16))
            .collect::<Result<_, _>>()
            .map_err(|_| bad("not a 32-byte key"))?
    };
    key.try_into().map_err(|_| bad("not a 32-byte key"))
}

fn cipher(key: &[u8; 32]) -> Result<XChaCha20Poly1305, BackupError> {
    let mut okm = [0u8; 32];
    Hkdf::<Sha256>::new(None, key)
        .expand(SEAL_CONTEXT, &mut okm)
        .map_err(|_| BackupError("key derivation failed".into()))?;
    XChaCha20Poly1305::new_from_slice(&okm).map_err(|_| BackupError("cipher".into()))
}

/// `nonce || ciphertext`, bound to the backup format.
fn seal(key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>, BackupError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| BackupError("no randomness".into()))?;
    let ct = cipher(key)?
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plain,
                aad: SEAL_CONTEXT,
            },
        )
        .map_err(|_| BackupError("sealing failed".into()))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

fn unseal(key: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>, BackupError> {
    if sealed.len() < NONCE_LEN + 16 {
        return fail("the sealed vault key is truncated");
    }
    let nonce: [u8; NONCE_LEN] = sealed[..NONCE_LEN].try_into().unwrap_or([0; NONCE_LEN]);
    cipher(key)?
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &sealed[NONCE_LEN..],
                aad: SEAL_CONTEXT,
            },
        )
        .map_err(|_| {
            BackupError(
                "the sealed vault key does not open: wrong backup key or a changed file".into(),
            )
        })
}

// ------------------------------------------------------------------------------ the database

fn is_postgres(db: &str) -> bool {
    db.starts_with("postgres://") || db.starts_with("postgresql://")
}

/// The `PG*` environment of a connection URL, so no password is put on a command line.
pub fn pg_env(db: &str) -> Result<Vec<(String, String)>, BackupError> {
    let url = url::Url::parse(db).map_err(|e| BackupError(format!("db url: {e}")))?;
    let mut env = Vec::new();
    if let Some(h) = url.host_str() {
        env.push(("PGHOST".into(), h.to_owned()));
    }
    if let Some(p) = url.port() {
        env.push(("PGPORT".into(), p.to_string()));
    }
    if !url.username().is_empty() {
        env.push(("PGUSER".into(), percent_decode(url.username())));
    }
    if let Some(pw) = url.password() {
        env.push(("PGPASSWORD".into(), percent_decode(pw)));
    }
    let name = url.path().trim_start_matches('/');
    if name.is_empty() {
        return fail("the db url names no database");
    }
    env.push(("PGDATABASE".into(), percent_decode(name)));
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "sslmode" => env.push(("PGSSLMODE".into(), v.into_owned())),
            "options" => env.push(("PGOPTIONS".into(), v.into_owned())),
            _ => {}
        }
    }
    Ok(env)
}

/// `%xx` escapes undone; `+` stays a plus.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex2 = |h: u8, l: u8| -> Option<u8> {
            let d = |c: u8| (c as char).to_digit(16);
            u8::try_from(d(h)? * 16 + d(l)?).ok()
        };
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = hex2(b[i + 1], b[i + 2])
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn run_pg(tool: &str, args: &[&str], env: &[(String, String)]) -> Result<(), BackupError> {
    let out = std::process::Command::new(tool)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .map_err(|e| BackupError(format!("{tool}: {e}; install the PostgreSQL client tools")))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        fail(format!(
            "{tool} failed: {}",
            err.trim().lines().last().unwrap_or("")
        ))
    }
}

/// Writes the database snapshot; returns its file name and kind.
async fn dump_db(
    db: &str,
    pool: &AnyPool,
    out: &Path,
) -> Result<(&'static str, &'static str), BackupError> {
    if is_postgres(db) {
        let env = pg_env(db)?;
        let file = out.join(DB_PG);
        drop(create_file(&file)?);
        run_pg(
            "pg_dump",
            &[
                "--format=custom",
                "--no-owner",
                "--file",
                &file.to_string_lossy(),
            ],
            &env,
        )?;
        return Ok((DB_PG, "postgres"));
    }
    let file = out.join(DB_SQLITE);
    // VACUUM INTO writes a transactionally consistent copy while the server keeps working.
    let quoted = file.to_string_lossy().replace('\'', "''");
    sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{quoted}'")))
        .execute(pool)
        .await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((DB_SQLITE, "sqlite"))
}

/// The file a SQLite `db` names.
fn sqlite_target(db: &str) -> Result<PathBuf, BackupError> {
    crate::workdir::sqlite_file(db)
        .ok_or_else(|| BackupError("`db` is not a SQLite file; restore needs a file path".into()))
}

/// Whether a SQLite database holds rows in any table of its own (the schema alone is empty).
async fn sqlite_has_data(pool: &AnyPool) -> Result<bool, BackupError> {
    let tables = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         AND name NOT LIKE '\\_%' ESCAPE '\\'",
    )
    .fetch_all(pool)
    .await?;
    for t in tables {
        let name: String = t.try_get("name")?;
        if name.starts_with("refs_schema") {
            continue;
        }
        let q = format!(
            "SELECT CAST(COUNT(*) AS BIGINT) FROM \"{}\"",
            name.replace('"', "\"\"")
        );
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(q))
            .fetch_one(pool)
            .await?;
        if n > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

// ------------------------------------------------------------------------------ the objects

/// `(path relative to the bucket root, size, sha256)` of every object written.
async fn dump_objects(
    store: &dyn ObjectStore,
    root: &ObjPath,
    out: &Path,
) -> Result<(u64, u64), BackupError> {
    let mut index = create_file(&out.join(OBJECT_INDEX))?;
    let (mut count, mut total) = (0u64, 0u64);
    let mut list = store.list(Some(root));
    while let Some(meta) = list.next().await {
        let meta = meta?;
        let full = meta.location.as_ref().to_owned();
        let prefix = if root.as_ref().is_empty() {
            String::new()
        } else {
            format!("{}/", root.as_ref())
        };
        let Some(rel) = full.strip_prefix(&prefix) else {
            return fail(format!("object {full} is outside the bucket root"));
        };
        check_relative(rel)?;
        let dest = out.join(OBJECTS_DIR).join(rel);
        let mut file = create_file(&dest)?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut body = store.get(&meta.location).await?.into_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            hash.update(&chunk);
            size += chunk.len() as u64;
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        writeln!(index, "{} {} {}", hex(&hash.finalize()), size, rel)?;
        count += 1;
        total += size;
    }
    index.sync_all()?;
    Ok((count, total))
}

/// A path read from a backup stays inside the directory it is joined to.
fn check_relative(rel: &str) -> Result<(), BackupError> {
    let ok = !rel.is_empty()
        && !rel.split('/').any(str::is_empty)
        && !rel.contains(['\\', '\0'])
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(())
    } else {
        fail(format!("unsafe path in the backup: {rel:?}"))
    }
}

fn parse_index(text: &str) -> Result<Vec<(String, u64, String)>, BackupError> {
    text.lines()
        .map(|l| {
            let mut it = l.splitn(3, ' ');
            let (Some(sha), Some(size), Some(rel)) = (it.next(), it.next(), it.next()) else {
                return fail("objects.idx has a malformed line");
            };
            check_relative(rel)?;
            let size = size
                .parse()
                .map_err(|_| BackupError("objects.idx has a malformed size".into()))?;
            Ok((sha.to_owned(), size, rel.to_owned()))
        })
        .collect()
}

// ------------------------------------------------------------------------------ backup

fn open_registry(cfg: &ServeConfig, pool: &AnyPool) -> Result<Registry, BackupError> {
    let store = cfg
        .store
        .as_deref()
        .ok_or_else(|| BackupError("`store` is required".into()))?;
    Registry::new(pool.clone(), store).map_err(|e| BackupError(e.to_string()))
}

fn vault_key_file(cfg: &ServeConfig) -> Option<PathBuf> {
    cfg.vault.key_file.clone().or_else(|| {
        crate::vault::default_key_file(cfg.db.as_deref(), cfg.cache.disk_dir.as_deref())
    })
}

/// Writes a backup of the server `cfg` names into `opts.out`. Returns the report text.
pub async fn backup(cfg: &ServeConfig, opts: &BackupOptions) -> Result<String, BackupError> {
    let Some(db) = cfg.db.as_deref() else {
        return fail("`db` is required");
    };
    if opts.include_vault_key && opts.key_file.is_none() {
        return fail(
            "--include-vault-key needs --key-file: the vault key is never written in the clear",
        );
    }
    let backup_key = opts.key_file.as_deref().map(load_backup_key).transpose()?;
    if opts.out.exists() && std::fs::read_dir(&opts.out)?.next().is_some() {
        return fail(format!("{} is not empty", opts.out.display()));
    }
    crate::workdir::create_private_all(&opts.out)?;
    let pool = migrate::connect(db, 2).await?;
    migrate::ensure_current(&pool)
        .await
        .map_err(|e| BackupError(e.to_string()))?;

    let (db_file, db_kind) = dump_db(db, &pool, &opts.out).await?;
    let mut files = vec![db_file.to_owned()];
    let (mut objects, mut object_bytes) = (0, 0);
    if !opts.no_objects {
        let registry = open_registry(cfg, &pool)?;
        let (store, root) = registry.default_place();
        (objects, object_bytes) = dump_objects(store.as_ref(), &root, &opts.out).await?;
        files.push(OBJECT_INDEX.to_owned());
    }
    let mut vault_key = "none";
    if opts.include_vault_key
        && let Some(key) = backup_key.as_ref()
    {
        let Some(path) = vault_key_file(cfg) else {
            return fail("there is no vault key file to back up (set [vault] key_file)");
        };
        let raw = zeroize::Zeroizing::new(std::fs::read(&path)?);
        write_file(&opts.out.join(SEALED_KEY), &seal(key, &raw)?)?;
        files.push(SEALED_KEY.to_owned());
        vault_key = "sealed";
    }
    let mut entries = Vec::new();
    for f in &files {
        let (bytes, sha256) = digest_file(&opts.out.join(f))?;
        entries.push(FileEntry {
            path: f.clone(),
            bytes,
            sha256,
        });
    }
    let manifest = Manifest {
        format: FORMAT,
        created: now(),
        asc_version: env!("CARGO_PKG_VERSION").to_owned(),
        db_kind: db_kind.to_owned(),
        vault_key: vault_key.to_owned(),
        objects,
        object_bytes,
        files: entries,
    };
    write_file(
        &opts.out.join(MANIFEST),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(format!(
        "backup written to {}\n  database: {db_kind}\n  objects: {objects} ({object_bytes} bytes)\n  vault key: {vault_key}\n",
        opts.out.display()
    ))
}

// ------------------------------------------------------------------------------ restore

fn read_manifest(dir: &Path) -> Result<Manifest, BackupError> {
    let f = std::fs::File::open(dir.join(MANIFEST))
        .map_err(|e| BackupError(format!("{}: {e}", dir.join(MANIFEST).display())))?;
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(f, MAX_MANIFEST + 1), &mut raw)?;
    if raw.len() as u64 > MAX_MANIFEST {
        return fail("the manifest is too large");
    }
    let m: Manifest = serde_json::from_slice(&raw)?;
    if m.format != FORMAT {
        return fail(format!(
            "backup format {} is not supported (this is {FORMAT})",
            m.format
        ));
    }
    Ok(m)
}

/// Checks every file the manifest lists, and every object the index lists, against its digest.
pub fn verify(dir: &Path) -> Result<u64, BackupError> {
    let m = read_manifest(dir)?;
    let listed: BTreeMap<&str, &FileEntry> = m.files.iter().map(|f| (f.path.as_str(), f)).collect();
    for f in &m.files {
        check_relative(&f.path)?;
        let (bytes, sha) =
            digest_file(&dir.join(&f.path)).map_err(|e| BackupError(format!("{}: {e}", f.path)))?;
        if bytes != f.bytes || sha != f.sha256 {
            return fail(format!(
                "{} does not match the manifest: the backup is damaged",
                f.path
            ));
        }
    }
    let mut count = 0u64;
    if listed.contains_key(OBJECT_INDEX) {
        let text = std::fs::read_to_string(dir.join(OBJECT_INDEX))?;
        for (sha, size, rel) in parse_index(&text)? {
            let (bytes, got) = digest_file(&dir.join(OBJECTS_DIR).join(&rel))
                .map_err(|e| BackupError(format!("object {rel}: {e}")))?;
            if bytes != size || got != sha {
                return fail(format!(
                    "object {rel} does not match the index: the backup is damaged"
                ));
            }
            count += 1;
        }
        if count != m.objects {
            return fail(format!(
                "the index lists {count} objects, the manifest {}",
                m.objects
            ));
        }
    }
    Ok(count)
}

/// Restores the backup in `opts.from` into the server `cfg` names. Stop the server first.
pub async fn restore(cfg: &ServeConfig, opts: &RestoreOptions) -> Result<String, BackupError> {
    let Some(db) = cfg.db.as_deref() else {
        return fail("`db` is required");
    };
    // Nothing is touched before the whole backup has been checked.
    let objects_checked = verify(&opts.from)?;
    let m = read_manifest(&opts.from)?;
    let backup_key = opts.key_file.as_deref().map(load_backup_key).transpose()?;
    if m.vault_key == "sealed" && backup_key.is_none() {
        return fail("the backup holds a sealed vault key: give --key-file");
    }
    if (m.db_kind == "postgres") != is_postgres(db) {
        return fail(format!(
            "the backup is a {} database but `db` is not",
            m.db_kind
        ));
    }
    let key_target = vault_key_file(cfg);
    if m.vault_key == "sealed" && key_target.is_none() {
        return fail("there is nowhere to restore the vault key: set [vault] key_file");
    }

    // 1. What would be written over.
    let mut occupied = Vec::new();
    let sqlite_file = if m.db_kind == "sqlite" {
        Some(sqlite_target(db)?)
    } else {
        None
    };
    if let Some(file) = &sqlite_file
        && file.exists()
        && std::fs::metadata(file)?.len() > 0
    {
        let pool = migrate::connect(&format!("sqlite:{}?mode=ro", file.display()), 1).await?;
        if sqlite_has_data(&pool).await? {
            occupied.push(format!("the database {}", file.display()));
        }
        pool.close().await;
    }
    let objects = m.files.iter().any(|f| f.path == OBJECT_INDEX);
    let place = if objects {
        let store = cfg
            .store
            .as_deref()
            .ok_or_else(|| BackupError("`store` is required".into()))?;
        // The registry needs a pool only to exist; the store is what is used.
        let scratch = migrate::connect("sqlite::memory:", 1).await?;
        let (s, root) = Registry::new(scratch, store)
            .map_err(|e| BackupError(e.to_string()))?
            .default_place();
        if s.list(Some(&root)).next().await.is_some() {
            occupied.push("the object bucket".to_owned());
        }
        Some((s, root))
    } else {
        None
    };
    if let Some(k) = &key_target
        && m.vault_key == "sealed"
        && k.exists()
    {
        occupied.push(format!("the vault key file {}", k.display()));
    }
    if !occupied.is_empty() && !opts.force {
        return fail(format!(
            "refusing to overwrite {}: restore into an empty install, or pass --force",
            occupied.join(" and ")
        ));
    }

    // 2. Write.
    let mut report = String::new();
    if let Some(file) = &sqlite_file {
        if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = file.with_extension("restore-tmp");
        let _ = std::fs::remove_file(&tmp);
        std::fs::copy(opts.from.join(DB_SQLITE), &tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        let check = migrate::connect(&format!("sqlite:{}?mode=ro", tmp.display()), 1).await?;
        let verdict: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&check)
            .await?;
        check.close().await;
        if verdict != "ok" {
            let _ = std::fs::remove_file(&tmp);
            return fail(format!(
                "the restored database fails its integrity check: {verdict}"
            ));
        }
        for sibling in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{sibling}", file.display()));
        }
        std::fs::rename(&tmp, file)?;
        report.push_str(&format!("database restored to {}\n", file.display()));
    } else {
        let env = pg_env(db)?;
        let dump = opts.from.join(DB_PG);
        let dump = dump.to_string_lossy();
        let mut args = vec!["--no-owner"];
        if opts.force {
            args.extend(["--clean", "--if-exists"]);
        }
        args.extend([
            "--dbname",
            env.iter()
                .find(|(k, _)| k == "PGDATABASE")
                .map_or("", |(_, v)| v.as_str()),
            &dump,
        ]);
        run_pg("pg_restore", &args, &env)?;
        report.push_str("database restored with pg_restore\n");
    }
    if let Some((store, root)) = place {
        let text = std::fs::read_to_string(opts.from.join(OBJECT_INDEX))?;
        let mut done = 0u64;
        for (_, _, rel) in parse_index(&text)? {
            let loc = if root.as_ref().is_empty() {
                ObjPath::parse(&rel)
            } else {
                ObjPath::parse(format!("{}/{rel}", root.as_ref()))
            }
            .map_err(|e| BackupError(format!("object {rel}: {e}")))?;
            let body = std::fs::read(opts.from.join(OBJECTS_DIR).join(&rel))?;
            store
                .put(&loc, object_store::PutPayload::from(body))
                .await?;
            done += 1;
        }
        // The bucket now holds what the backup holds.
        let mut seen = 0u64;
        let mut list = store.list(Some(&root));
        while let Some(o) = list.next().await {
            o?;
            seen += 1;
        }
        if seen < done {
            return fail(format!(
                "only {seen} of {done} objects are in the bucket after the restore"
            ));
        }
        report.push_str(&format!("objects restored: {done}\n"));
    }
    if m.vault_key == "sealed"
        && let (Some(key), Some(target)) = (backup_key.as_ref(), key_target.as_ref())
    {
        let plain =
            zeroize::Zeroizing::new(unseal(key, &std::fs::read(opts.from.join(SEALED_KEY))?)?);
        if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = target.with_extension("restore-tmp");
        let _ = std::fs::remove_file(&tmp);
        write_file(&tmp, &plain)?;
        std::fs::rename(&tmp, target)?;
        report.push_str(&format!(
            "vault key restored to {} (mode 0600)\n",
            target.display()
        ));
    }
    report.push_str(&format!(
        "verified before writing: {} files, {objects_checked} objects\nrun `asc serve migrate` if this release is newer than the backup\n",
        m.files.len()
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        [9u8; 32]
    }

    #[test]
    fn a_sealed_key_opens_only_under_its_key_and_unchanged() {
        let sealed = seal(&key(), b"master key bytes").unwrap();
        assert_eq!(unseal(&key(), &sealed).unwrap(), b"master key bytes");
        assert!(unseal(&[1u8; 32], &sealed).is_err());
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(unseal(&key(), &bad).is_err());
        assert!(unseal(&key(), &sealed[..10]).is_err());
        assert!(
            !sealed.windows(6).any(|w| w == b"master"),
            "not in the clear"
        );
    }

    #[test]
    fn paths_from_a_backup_stay_inside_it() {
        for ok in ["a/b/c", "ns/ab/cd.bin"] {
            assert!(check_relative(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a\\b",
            "a//b",
            "./a",
        ] {
            assert!(check_relative(bad).is_err(), "{bad:?}");
        }
        assert!(parse_index("abc 3 ../x").is_err());
        assert!(parse_index("abc three a").is_err());
        assert_eq!(
            parse_index("abc 3 a/b").unwrap(),
            [("abc".into(), 3, "a/b".into())]
        );
    }

    #[test]
    fn the_postgres_url_becomes_the_environment_not_arguments() {
        let env = pg_env("postgres://asc:p%40ss@db.internal:5433/asc_hub?sslmode=require").unwrap();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PGHOST"), Some("db.internal"));
        assert_eq!(get("PGPORT"), Some("5433"));
        assert_eq!(get("PGUSER"), Some("asc"));
        assert_eq!(get("PGPASSWORD"), Some("p@ss"));
        assert_eq!(get("PGDATABASE"), Some("asc_hub"));
        assert_eq!(get("PGSSLMODE"), Some("require"));
        assert!(pg_env("postgres://h").is_err());
    }

    #[cfg(unix)]
    mod roundtrip {
        use std::os::unix::fs::PermissionsExt as _;

        use super::*;

        struct Site {
            cfg: ServeConfig,
            dir: PathBuf,
        }

        fn site(root: &Path, name: &str) -> Site {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let mut cfg = ServeConfig {
                db: Some(format!("sqlite:{}/asc.db?mode=rwc", dir.display())),
                store: Some(format!("file://{}/bucket", dir.display())),
                ..ServeConfig::default()
            };
            cfg.vault.key_file = Some(dir.join("vault.keys"));
            std::fs::create_dir_all(dir.join("bucket")).unwrap();
            Site { cfg, dir }
        }

        fn private(path: &Path, bytes: &[u8]) {
            std::fs::write(path, bytes).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        async fn fill(s: &Site) {
            let pool = migrate::connect(s.cfg.db.as_deref().unwrap(), 1)
                .await
                .unwrap();
            migrate::migrate(&pool).await.unwrap();
            sqlx::query("INSERT INTO repos (name, ns, created) VALUES ('acme/app', X'0102', 1)")
                .execute(&pool)
                .await
                .unwrap();
            let reg = open_registry(&s.cfg, &pool).unwrap();
            let (store, root) = reg.default_place();
            for (name, body) in [("ns/ab/one", "first object"), ("ns/cd/two", "second")] {
                let full = format!("{}/{name}", root.as_ref());
                let loc = ObjPath::parse(full.trim_start_matches('/')).unwrap();
                store
                    .put(
                        &loc,
                        object_store::PutPayload::from(body.as_bytes().to_vec()),
                    )
                    .await
                    .unwrap();
            }
            private(&s.dir.join("vault.keys"), b"the master key file");
            pool.close().await;
        }

        async fn rows(s: &Site) -> i64 {
            let pool = migrate::connect(s.cfg.db.as_deref().unwrap(), 1)
                .await
                .unwrap();
            let n = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM repos")
                .fetch_one(&pool)
                .await
                .unwrap();
            pool.close().await;
            n
        }

        fn opts(out: &Path, key: &Path) -> BackupOptions {
            BackupOptions {
                out: out.to_owned(),
                key_file: Some(key.to_owned()),
                include_vault_key: true,
                no_objects: false,
            }
        }

        #[tokio::test]
        async fn a_backup_restores_into_an_empty_install_and_is_checked_every_step() {
            let t = tempfile::tempdir().unwrap();
            let live = site(t.path(), "live");
            fill(&live).await;
            let bkey = t.path().join("backup.key");
            private(&bkey, &[4u8; 32]);
            let out = t.path().join("bk");
            let text = backup(&live.cfg, &opts(&out, &bkey)).await.unwrap();
            assert!(text.contains("objects: 2"), "{text}");
            // The directory and every file in it are private; the key is not in the clear.
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&out), 0o700);
            for f in [DB_SQLITE, OBJECT_INDEX, SEALED_KEY, MANIFEST] {
                assert_eq!(mode(&out.join(f)), 0o600, "{f}");
            }
            let sealed = std::fs::read(out.join(SEALED_KEY)).unwrap();
            assert!(!sealed.windows(10).any(|w| w == b"master key"));
            // A backup does not go into a directory that has things in it.
            assert!(backup(&live.cfg, &opts(&out, &bkey)).await.is_err());
            assert_eq!(verify(&out).unwrap(), 2);

            // Restore into an empty install.
            let fresh = site(t.path(), "fresh");
            let r = RestoreOptions {
                from: out.clone(),
                key_file: Some(bkey.clone()),
                force: false,
            };
            let text = restore(&fresh.cfg, &r).await.unwrap();
            assert!(
                text.contains("objects restored: 2") && text.contains("vault key restored"),
                "{text}"
            );
            assert_eq!(rows(&fresh).await, 1);
            let key_path = fresh.dir.join("vault.keys");
            assert_eq!(std::fs::read(&key_path).unwrap(), b"the master key file");
            assert_eq!(mode(&key_path), 0o600);
            assert_eq!(
                std::fs::read(fresh.dir.join("bucket/ns/cd/two")).unwrap(),
                b"second"
            );

            // A second restore finds data and refuses; --force writes over it.
            let e = restore(&fresh.cfg, &r).await.unwrap_err();
            assert!(
                e.0.contains("refusing to overwrite") && e.0.contains("--force"),
                "{e}"
            );
            let forced = RestoreOptions {
                force: true,
                ..r.clone()
            };
            restore(&fresh.cfg, &forced).await.unwrap();
            assert_eq!(rows(&fresh).await, 1);
        }

        #[tokio::test]
        async fn a_damaged_backup_or_a_wrong_key_changes_nothing() {
            let t = tempfile::tempdir().unwrap();
            let live = site(t.path(), "live");
            fill(&live).await;
            let bkey = t.path().join("backup.key");
            private(&bkey, &[4u8; 32]);
            let out = t.path().join("bk");
            backup(&live.cfg, &opts(&out, &bkey)).await.unwrap();
            let fresh = site(t.path(), "fresh");
            let r = RestoreOptions {
                from: out.clone(),
                key_file: Some(bkey.clone()),
                force: false,
            };

            // The vault key cannot be restored without the backup key.
            let none = RestoreOptions {
                key_file: None,
                ..r.clone()
            };
            let e = restore(&fresh.cfg, &none).await.unwrap_err();
            assert!(e.0.contains("--key-file"), "{e}");
            // A changed object is found before anything is written.
            std::fs::write(out.join("objects/ns/ab/one"), b"tampered!!!!").unwrap();
            let e = restore(&fresh.cfg, &r).await.unwrap_err();
            assert!(e.0.contains("does not match"), "{e}");
            assert!(
                !fresh.dir.join("asc.db").exists(),
                "no database was written"
            );
            assert!(!fresh.dir.join("vault.keys").exists());
            assert!(!fresh.dir.join("bucket/ns").exists());
        }

        #[tokio::test]
        async fn the_vault_key_is_left_out_unless_asked_for_and_sealed() {
            let t = tempfile::tempdir().unwrap();
            let live = site(t.path(), "live");
            fill(&live).await;
            // Asking for it without a backup key to seal it under is refused.
            let o = BackupOptions {
                out: t.path().join("a"),
                include_vault_key: true,
                ..BackupOptions::default()
            };
            assert!(
                backup(&live.cfg, &o)
                    .await
                    .unwrap_err()
                    .0
                    .contains("--key-file")
            );
            // Not asked for: no key in the backup; --no-objects leaves the bucket out.
            let o = BackupOptions {
                out: t.path().join("b"),
                no_objects: true,
                ..BackupOptions::default()
            };
            let text = backup(&live.cfg, &o).await.unwrap();
            assert!(
                text.contains("vault key: none") && text.contains("objects: 0"),
                "{text}"
            );
            assert!(!t.path().join("b").join(SEALED_KEY).exists());
            assert!(!t.path().join("b").join(OBJECT_INDEX).exists());
            // A migrated database with no rows is an empty install: restore needs no --force.
            let fresh = site(t.path(), "fresh");
            let pool = migrate::connect(fresh.cfg.db.as_deref().unwrap(), 1)
                .await
                .unwrap();
            migrate::migrate(&pool).await.unwrap();
            pool.close().await;
            let r = RestoreOptions {
                from: t.path().join("b"),
                ..RestoreOptions::default()
            };
            restore(&fresh.cfg, &r).await.unwrap();
            assert_eq!(rows(&fresh).await, 1);
        }
    }
}
