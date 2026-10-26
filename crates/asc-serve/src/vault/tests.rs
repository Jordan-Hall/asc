use std::io::Write as _;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sqlx::{AnyPool, Row};

use super::keyring::KeyRing;
use super::*;

/// A feature's table for the tests: `(id, owner, sealed, key_gen)`.
struct Rows {
    pool: AnyPool,
}

const KIND: &str = "test";

#[async_trait]
impl Resealer for Rows {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn counts(&self) -> Result<Vec<(u32, u64)>, VaultError> {
        let rows = sqlx::query("SELECT key_gen, COUNT(*) AS n FROM t_secrets GROUP BY key_gen")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| VaultError::sql(&e))?;
        rows.iter()
            .map(|r| {
                let g: i64 = r.try_get("key_gen").map_err(|e| VaultError::sql(&e))?;
                let n: i64 = r.try_get("n").map_err(|e| VaultError::sql(&e))?;
                Ok((u32::try_from(g).unwrap_or(0), u64::try_from(n).unwrap_or(0)))
            })
            .collect()
    }

    async fn reseal_batch(
        &self,
        vault: &Vault,
        target: u32,
        limit: usize,
    ) -> Result<usize, VaultError> {
        let rows = sqlx::query(
            "SELECT id, owner, sealed, key_gen FROM t_secrets WHERE key_gen <> $1 LIMIT $2",
        )
        .bind(i64::from(target))
        .bind(i64::try_from(limit).unwrap_or(1))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| VaultError::sql(&e))?;
        let mut n = 0;
        for r in rows {
            let id: String = r.try_get("id").unwrap();
            let owner: String = r.try_get("owner").unwrap();
            let sealed: Vec<u8> = r.try_get("sealed").unwrap();
            let gen_: i64 = r.try_get("key_gen").unwrap();
            let ctx = SecretContext {
                kind: KIND,
                owner: &owner,
                id: &id,
            };
            if let Some((g, blob)) = vault.reseal(&ctx, &sealed)? {
                let done = sqlx::query(
                    "UPDATE t_secrets SET sealed = $1, key_gen = $2 WHERE id = $3 AND key_gen = $4",
                )
                .bind(blob)
                .bind(i64::from(g))
                .bind(&id)
                .bind(gen_)
                .execute(&self.pool)
                .await
                .map_err(|e| VaultError::sql(&e))?;
                n += done.rows_affected() as usize;
            }
        }
        Ok(n)
    }
}

async fn pool() -> AnyPool {
    let pool = crate::migrate::connect("sqlite::memory:", 1).await.unwrap();
    crate::migrate::migrate(&pool).await.unwrap();
    sqlx::query(
        "CREATE TABLE t_secrets (id TEXT PRIMARY KEY, owner TEXT NOT NULL, sealed BLOB NOT NULL, key_gen BIGINT NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

async fn put(vault: &Vault, pool: &AnyPool, id: &str, owner: &str, plain: &[u8]) {
    let (g, blob) = vault
        .seal(
            &SecretContext {
                kind: KIND,
                owner,
                id,
            },
            plain,
        )
        .unwrap();
    sqlx::query("INSERT INTO t_secrets (id, owner, sealed, key_gen) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(owner)
        .bind(blob)
        .bind(i64::from(g))
        .execute(pool)
        .await
        .unwrap();
}

async fn get(vault: &Vault, pool: &AnyPool, id: &str, owner: &str) -> Result<Vec<u8>, VaultError> {
    let row = sqlx::query("SELECT sealed FROM t_secrets WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    let sealed: Vec<u8> = row.try_get("sealed").unwrap();
    vault
        .open(
            &SecretContext {
                kind: KIND,
                owner,
                id,
            },
            &sealed,
        )
        .map(|p| p.to_vec())
}

async fn fixture() -> (Arc<Vault>, AnyPool) {
    let pool = pool().await;
    let vault = Vault::in_memory(KeyRing::generate(1).unwrap(), pool.clone());
    vault.register(Arc::new(Rows { pool: pool.clone() }));
    (vault, pool)
}

#[tokio::test]
async fn seal_and_open_are_bound_to_owner_and_row() {
    let (vault, pool) = fixture().await;
    put(&vault, &pool, "a", "user:alice", b"alice-secret").await;
    assert_eq!(get(&vault, &pool, "a", "user:alice").await.unwrap(), b"alice-secret");
    assert!(matches!(
        get(&vault, &pool, "a", "user:bob").await,
        Err(VaultError::Corrupt)
    ));
}

#[tokio::test]
async fn rotation_reseals_everything_and_keeps_values_readable() {
    let (vault, pool) = fixture().await;
    for i in 0..450 {
        put(&vault, &pool, &format!("id{i}"), "org:acme", format!("secret-{i}").as_bytes()).await;
    }
    let before = vault.status().await;
    assert_eq!(before.generations.len(), 1);
    assert_eq!(before.generations[0].secrets, 450);
    let started = vault.rotate(false).await.unwrap();
    assert_eq!(started.to_generation, 2);
    assert_eq!(started.total, 450);
    // A second rotation while one runs is refused.
    assert!(matches!(vault.rotate(false).await, Err(VaultError::RotationActive)));
    vault.wait_idle().await;
    let done = vault.rotation(&started.id).await.unwrap().unwrap();
    assert!(done.finished, "{done:?}");
    assert_eq!(done.done, 450);
    assert_eq!(done.error, "");
    let after = vault.status().await;
    assert!(!after.rotation_active);
    let by_gen: Vec<_> = after.generations.iter().map(|g| (g.generation, g.active, g.secrets)).collect();
    assert_eq!(by_gen, vec![(1, false, 0), (2, true, 450)]);
    for i in [0, 199, 200, 449] {
        assert_eq!(
            get(&vault, &pool, &format!("id{i}"), "org:acme").await.unwrap(),
            format!("secret-{i}").as_bytes()
        );
    }
    // New values seal under the new generation.
    put(&vault, &pool, "late", "org:acme", b"late").await;
    let gen_: i64 = sqlx::query_scalar("SELECT key_gen FROM t_secrets WHERE id = 'late'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(gen_, 2);
    // A later rotation can start once the first ended.
    assert!(vault.rotate(true).await.is_ok());
    vault.wait_idle().await;
    let s = vault.status().await;
    assert_eq!(s.generations.len(), 1, "retire_old dropped the unused generations: {s:?}");
    assert_eq!(s.generations[0].generation, 3);
    assert_eq!(get(&vault, &pool, "id7", "org:acme").await.unwrap(), b"secret-7");
}

#[tokio::test]
async fn retire_keeps_a_generation_that_is_still_in_use() {
    let (vault, pool) = fixture().await;
    put(&vault, &pool, "x", "user:u", b"x").await;
    // A resealer that never finishes its work would leave generation 1 in use; model it by
    // sealing a value under generation 1 after the rotation moved on.
    let started = vault.rotate(false).await.unwrap();
    vault.wait_idle().await;
    assert!(vault.rotation(&started.id).await.unwrap().unwrap().finished);
    let ring_one = KeyRing::from_keys(&[(1, *vault.ring().unwrap().get(1).unwrap().key())], 1);
    let g = ring_one.active_key();
    let old = super::crypto::seal(
        g.key(),
        1,
        &SecretContext { kind: KIND, owner: "user:u", id: "y" },
        b"y",
    )
    .unwrap();
    sqlx::query("INSERT INTO t_secrets (id, owner, sealed, key_gen) VALUES ('y', 'user:u', $1, 1)")
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
    // `retire_unused` runs at the end of a rotation with retire_old; call the last stage.
    vault.retire_unused().await.unwrap();
    let s = vault.status().await;
    assert_eq!(s.generations.len(), 2, "generation 1 still seals a value: {s:?}");
    assert_eq!(get(&vault, &pool, "y", "user:u").await.unwrap(), b"y");
}

#[tokio::test]
async fn a_rotation_that_cannot_open_a_value_stops_with_an_error() {
    let (vault, pool) = fixture().await;
    put(&vault, &pool, "ok", "user:u", b"fine").await;
    // A row sealed under a different owner than the row says: it can never open.
    let (g, blob) = vault
        .seal(&SecretContext { kind: KIND, owner: "user:other", id: "bad" }, b"nope")
        .unwrap();
    sqlx::query("INSERT INTO t_secrets (id, owner, sealed, key_gen) VALUES ('bad', 'user:u', $1, $2)")
        .bind(blob)
        .bind(i64::from(g))
        .execute(&pool)
        .await
        .unwrap();
    let r = vault.rotate(false).await.unwrap();
    vault.wait_idle().await;
    let done = vault.rotation(&r.id).await.unwrap().unwrap();
    assert!(done.finished);
    assert!(!done.error.is_empty());
    assert!(!done.error.contains("nope"));
}

#[tokio::test]
async fn an_unfinished_rotation_is_resumed() {
    let (vault, pool) = fixture().await;
    put(&vault, &pool, "a", "user:u", b"a").await;
    put(&vault, &pool, "b", "user:u", b"b").await;
    // Model a crash: the ring moved on, the row says unfinished and stale.
    let ring = vault.ring().unwrap();
    *vault.state.write().unwrap() = State::Ready(Arc::new(ring.with_new_generation(5).unwrap()));
    sqlx::query(
        "INSERT INTO vault_rotations (id, to_generation, total, done, finished, retire_old, error, started, updated) \
         VALUES ('r1', 2, 2, 0, 0, 0, NULL, 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(vault.resume().await.unwrap().as_deref(), Some("r1"));
    vault.wait_idle().await;
    let r = vault.rotation("r1").await.unwrap().unwrap();
    assert!(r.finished && r.error.is_empty() && r.done == 2, "{r:?}");
    assert_eq!(get(&vault, &pool, "a", "user:u").await.unwrap(), b"a");
    let s = vault.status().await;
    assert_eq!(s.generations.iter().find(|g| g.active).unwrap().secrets, 2);
    // Nothing left to resume.
    assert_eq!(vault.resume().await.unwrap(), None);
}

#[tokio::test]
async fn a_fresh_rotation_row_is_not_taken_over() {
    let (vault, pool) = fixture().await;
    sqlx::query(
        "INSERT INTO vault_rotations (id, to_generation, total, done, finished, retire_old, error, started, updated) \
         VALUES ('live', 2, 2, 0, 0, 0, NULL, $1, $1)",
    )
    .bind(super::now())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(vault.resume().await.unwrap(), None);
    assert!(matches!(vault.rotate(false).await, Err(VaultError::RotationActive)));
    assert!(vault.status().await.rotation_active);
}

#[tokio::test]
async fn the_key_file_is_created_private_and_a_second_start_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys").join("vault.keys");
    let pool = pool().await;
    let a = Vault::open_file(path.clone(), pool.clone());
    assert!(a.ready());
    let s = a.status().await;
    assert!(s.ready, "{s:?}");
    assert_eq!(s.key_file_mode, "0600");
    let (_, blob) = a
        .seal(&SecretContext { kind: KIND, owner: "user:u", id: "1" }, b"v")
        .unwrap();
    let b = Vault::open_file(path, pool);
    assert_eq!(
        &b.open(&SecretContext { kind: KIND, owner: "user:u", id: "1" }, &blob).unwrap()[..],
        b"v"
    );
}

#[tokio::test]
async fn a_replica_that_did_not_rotate_picks_up_the_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.keys");
    let pool = pool().await;
    let one = Vault::open_file(path.clone(), pool.clone());
    let two = Vault::open_file(path, pool.clone());
    one.register(Arc::new(Rows { pool: pool.clone() }));
    two.register(Arc::new(Rows { pool: pool.clone() }));
    one.rotate(false).await.unwrap();
    one.wait_idle().await;
    let (g, blob) = one
        .seal(&SecretContext { kind: KIND, owner: "user:u", id: "1" }, b"v")
        .unwrap();
    assert_eq!(g, 2);
    // `two` has never seen generation 2 and reads the file again to open it, and seals new
    // values under it.
    assert_eq!(
        &two.open(&SecretContext { kind: KIND, owner: "user:u", id: "1" }, &blob).unwrap()[..],
        b"v"
    );
    assert_eq!(two.active_generation().unwrap(), 2);
}

#[tokio::test]
async fn an_unusable_key_file_leaves_the_vault_locked_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.keys");
    std::fs::write(&path, "garbage").unwrap();
    let pool = pool().await;
    // A file written the ordinary way is rejected for its mode on unix, or for its content.
    let v = Vault::open_file(path.clone(), pool.clone());
    assert!(!v.ready());
    let err = v
        .seal(&SecretContext { kind: KIND, owner: "o", id: "i" }, b"x")
        .unwrap_err();
    assert!(matches!(err, VaultError::Locked(_)), "{err}");
    let s = v.status().await;
    assert!(!s.ready && s.locked_reason.is_some() && s.generations.is_empty());
    // Repaired in place: the next call after the retry delay unlocks it.
    std::fs::remove_file(&path).unwrap();
    *v.last_unlock.lock().unwrap() = Instant::now() - Duration::from_secs(60);
    keyring::load_or_create(&path, 1).unwrap();
    assert!(v.ready());
    let none = Vault::locked("set [vault] key_file", pool);
    assert!(!none.ready());
    assert_eq!(
        none.status().await.locked_reason.as_deref(),
        Some("set [vault] key_file")
    );
}

#[test]
fn default_key_file_follows_the_database_then_the_cache_dir() {
    use std::path::Path;
    assert_eq!(
        default_key_file(Some("sqlite:/var/lib/asc/asc.db"), None),
        Some(PathBuf::from("/var/lib/asc/vault.keys"))
    );
    assert_eq!(
        default_key_file(Some("sqlite:///var/lib/asc/asc.db?mode=rwc"), None),
        Some(PathBuf::from("/var/lib/asc/vault.keys"))
    );
    assert_eq!(
        default_key_file(Some("sqlite::memory:"), Some(Path::new("/cache"))),
        Some(PathBuf::from("/cache/vault.keys"))
    );
    assert_eq!(
        default_key_file(Some("postgres://h/db"), Some(Path::new("/cache"))),
        Some(PathBuf::from("/cache/vault.keys"))
    );
    assert_eq!(default_key_file(Some("postgres://h/db"), None), None);
}

/// A log writer the test can read back.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

#[tokio::test]
async fn a_planted_secret_and_the_master_key_never_reach_logs_errors_or_debug_output() {
    const PLANTED: &str = "PLANTED-SECRET-9f3a7c1e";
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.keys");
    let pool = pool().await;
    let vault = Vault::open_file(path.clone(), pool.clone());
    vault.register(Arc::new(Rows { pool: pool.clone() }));
    put(&vault, &pool, "p", "user:u", PLANTED.as_bytes()).await;
    let mut shown = String::new();
    shown += &format!("{vault:?}");
    let key_hex: String = vault
        .ring()
        .unwrap()
        .active_key()
        .key()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    shown += &format!("{:?}", vault.ring().unwrap());
    // Errors from a wrong owner, a damaged blob and a locked vault.
    shown += &format!("{:?}", get(&vault, &pool, "p", "user:x").await.unwrap_err());
    shown += &get(&vault, &pool, "p", "user:x").await.unwrap_err().to_string();
    vault.rotate(true).await.unwrap();
    vault.wait_idle().await;
    shown += &format!("{:?}", vault.status().await);
    shown += &format!("{:?}", vault.latest_rotation().await.unwrap());
    let locked = Vault::open_file(path.join("inside-a-file"), pool.clone());
    shown += &format!("{:?}", locked.status().await);
    let logged = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
    for text in [shown, logged] {
        assert!(!text.contains(PLANTED));
        assert!(!text.contains(&key_hex));
    }
    // The file holds keys, never the plaintext.
    let on_disk = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&on_disk).contains(PLANTED));
    let stored: Vec<u8> = sqlx::query_scalar("SELECT sealed FROM t_secrets WHERE id = 'p'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!stored.windows(PLANTED.len()).any(|w| w == PLANTED.as_bytes()));
    let _ = std::io::stdout().flush();
}
