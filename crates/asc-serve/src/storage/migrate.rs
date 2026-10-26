//! Moving a repository's objects to another place (docs/BYOS.md, "Migration").
//!
//! The order is the safety argument:
//!
//! 1. **Mirror.** The route gets `mirror_to`: from now on every write and delete goes to the
//!    source and the destination (the source stays authoritative for reads).
//! 2. **Copy.** Every object under the repository's namespace is copied, in key order, with a
//!    persisted cursor; an object already at the destination with the same size is skipped, so a
//!    restart, or a second attempt after a failure, continues instead of starting over.
//! 3. **Verify.** After the replicas had time to see the mirror (`settle`), every object is read
//!    from both places and the hashes are compared; a missing or different one is copied again.
//! 4. **Switch.** One compare-and-swap on the route makes the destination the primary and clears
//!    the mirror and the fallback. Reads and writes never stopped.
//! 5. **Clean up** (only if asked). After `settle` again, the old objects are deleted.
//!
//! A failure or a cancel before step 4 clears the mirror and leaves the source authoritative and
//! untouched. The lease on the row makes a restarted server, or another replica, resume it.

use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, TryStreamExt, stream};
use object_store::path::Path;
use object_store::{ObjectMeta, ObjectStoreExt, PutPayload};
use tokio::sync::{Notify, broadcast};

use super::audit::{Audit, AuditEvent};
use super::db::{Db, Mark, MigrationRow};
use super::live::{Router, sanitize};
use super::routed::Member;
use crate::registry::Ns;

/// Objects copied between two progress writes.
const CHUNK: usize = 64;
/// Concurrent object copies.
const CONCURRENCY: usize = 8;
/// Concurrent object comparisons.
const VERIFY_CONCURRENCY: usize = 4;
/// Largest object the migration moves. The store's own cap on a decoded value is 256 MiB.
const MAX_OBJECT: u64 = 512 * 1024 * 1024;
/// The lease a worker holds on a migration, in seconds.
const LEASE_SECS: i64 = 120;
/// Resolve passes for a route that changes under us.
const ROUTE_RETRIES: usize = 8;

/// The states of a migration row.
pub mod state {
    pub const QUEUED: &str = "queued";
    pub const COPYING: &str = "copying";
    pub const VERIFYING: &str = "verifying";
    pub const SWITCHING: &str = "switching";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
}

/// A progress notice for in-process subscribers (the same facts are logged and audited).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub migration: String,
    pub repo: String,
    pub state: String,
    pub copied: u64,
    pub total: u64,
    pub error: String,
}

/// Why a migration stopped. The text is shown to the user: it names a step and never a
/// credential, endpoint query or provider body.
#[derive(Debug)]
enum Stop {
    /// The migration failed with this message.
    Failed(String),
    /// Another worker holds the lease now; this one just leaves.
    Lost,
}

impl Stop {
    fn new(msg: impl Into<String>) -> Self {
        Self::Failed(msg.into())
    }
}

impl From<sqlx::Error> for Stop {
    fn from(_: sqlx::Error) -> Self {
        Self::new("database error")
    }
}

impl From<object_store::Error> for Stop {
    fn from(e: object_store::Error) -> Self {
        Self::new(sanitize(&e))
    }
}

impl From<super::live::RouteError> for Stop {
    fn from(e: super::live::RouteError) -> Self {
        Self::new(e.to_string())
    }
}

/// Tunables.
#[derive(Clone, Debug)]
pub struct Settings {
    /// How long the replicas need to notice a route change. Waits happen before verifying and
    /// before deleting the source.
    pub settle: Duration,
    /// Migrations run at once on this process.
    pub workers: usize,
    /// How often the supervisor looks for work.
    pub poll: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            settle: Duration::from_secs(5),
            workers: 2,
            poll: Duration::from_secs(5),
        }
    }
}

pub(super) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The migration runner.
pub struct Engine {
    db: Db,
    router: Arc<Router>,
    audit: Arc<dyn Audit>,
    settings: Settings,
    wake: Notify,
    events: broadcast::Sender<Event>,
    running: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl Engine {
    pub fn new(db: Db, router: Arc<Router>, audit: Arc<dyn Audit>, settings: Settings) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            db,
            router,
            audit,
            settings,
            wake: Notify::new(),
            events,
            running: std::sync::Mutex::default(),
        })
    }

    /// Progress notices, for in-process consumers.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Tells the supervisor there is new work.
    pub fn kick(&self) {
        self.wake.notify_one();
    }

    /// Starts the supervisor loop.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                this.tick().await;
                tokio::select! {
                    () = this.wake.notified() => {}
                    () = tokio::time::sleep(this.settings.poll) => {}
                }
            }
        })
    }

    /// One look for runnable migrations; starts as many as there is room for.
    pub async fn tick(self: &Arc<Self>) {
        let room = {
            let running = self.running.lock().map_or(usize::MAX, |r| r.len());
            self.settings.workers.saturating_sub(running)
        };
        if room == 0 {
            return;
        }
        let rows = match self
            .db
            .runnable_migrations(now(), i64::try_from(room).unwrap_or(1))
            .await
        {
            Ok(r) => r,
            Err(_) => return,
        };
        for row in rows {
            if self
                .running
                .lock()
                .map_or(true, |r| r.contains(&row.id))
            {
                continue;
            }
            match self
                .db
                .claim_migration(&row.id, now() + LEASE_SECS, now(), false)
                .await
            {
                Ok(true) => {}
                _ => continue,
            }
            if let Ok(mut r) = self.running.lock() {
                r.insert(row.id.clone());
            }
            let this = self.clone();
            tokio::spawn(async move { this.run(row).await });
        }
    }

    /// Waits until no migration runs on this process. For tests.
    pub async fn wait_idle(&self) {
        loop {
            let idle = self.running.lock().is_ok_and(|r| r.is_empty());
            if idle {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn publish(&self, row: &MigrationRow, state: &str, error: &str) {
        let _ = self.events.send(Event {
            migration: row.id.clone(),
            repo: row.repo_name.clone(),
            state: state.to_owned(),
            copied: u64::try_from(row.objects_copied).unwrap_or(0),
            total: u64::try_from(row.objects_total).unwrap_or(0),
            error: error.to_owned(),
        });
        tracing::info!(migration = %row.id, repo = %row.repo_name, state, "storage migration");
    }

    pub(super) async fn run(self: Arc<Self>, row: MigrationRow) {
        let id = row.id.clone();
        let _forget = Forget {
            running: &self.running,
            id: id.clone(),
        };
        let outcome = self.drive(row.clone()).await;
        let fresh = self.db.migration(&id).await.ok().flatten().unwrap_or(row);
        match outcome {
            Ok(Finish::Done) => {
                self.audit.record(
                    AuditEvent::new("storage.migration.finished")
                        .repo(&fresh.repo_name)
                        .detail("migration", &fresh.id)
                        .detail("outcome", "done")
                        .detail("objects", &fresh.objects_copied.to_string()),
                );
                self.publish(&fresh, state::DONE, "");
            }
            Ok(Finish::Cancelled) => {
                self.audit.record(
                    AuditEvent::new("storage.migration.finished")
                        .repo(&fresh.repo_name)
                        .detail("migration", &fresh.id)
                        .detail("outcome", "cancelled"),
                );
                self.publish(&fresh, state::CANCELLED, "");
            }
            Err(Stop::Lost) => {}
            Err(Stop::Failed(msg)) => {
                let switched = fresh.switched;
                if !switched {
                    let _ = self.unmirror(fresh.repo_id).await;
                }
                let _ = self
                    .db
                    .migration_state(&fresh.id, state::FAILED, &msg, now())
                    .await;
                self.audit.record(
                    AuditEvent::new("storage.migration.finished")
                        .repo(&fresh.repo_name)
                        .detail("migration", &fresh.id)
                        .detail("outcome", "failed"),
                );
                self.publish(&fresh, state::FAILED, &msg);
            }
        }
        let _ = self.db.release_migration(&id).await;
        if let Ok(mut r) = self.running.lock() {
            r.remove(&id);
        }
    }

    async fn renew(&self, id: &str) -> Result<(), Stop> {
        match self
            .db
            .claim_migration(id, now() + LEASE_SECS, now(), true)
            .await
        {
            Ok(true) => Ok(()),
            _ => Err(Stop::Lost),
        }
    }

    /// Reads the row again and stops if a cancel arrived before the switch.
    async fn checkpoint(&self, id: &str) -> Result<MigrationRow, Stop> {
        self.renew(id).await?;
        let row = self
            .db
            .migration(id)
            .await?
            .ok_or_else(|| Stop::new("the migration was removed"))?;
        Ok(row)
    }

    // ------------------------------------------------------------------ the route

    /// The places the repository's objects are in now (primary, then fallback).
    async fn sources(&self, row: &MigrationRow, ns: Ns) -> Result<Vec<Member>, Stop> {
        let route = self.db.route(row.repo_id).await?;
        let (primary, fallback) = match route {
            Some(r) => (r.target_id, r.fallback),
            None => (String::new(), None),
        };
        let mut out = vec![self.router.member(&primary, ns).await?];
        if let Some(f) = fallback {
            out.push(self.router.member(&f, ns).await?);
        }
        Ok(out)
    }

    async fn set_mirror(&self, row: &MigrationRow) -> Result<(), Stop> {
        for _ in 0..ROUTE_RETRIES {
            let cur = self.db.route(row.repo_id).await?;
            let ok = match &cur {
                Some(r) => {
                    self.db
                        .put_route(
                            row.repo_id,
                            &r.target_id,
                            r.fallback.as_deref(),
                            Some(&row.to_target),
                            Some(r.version),
                            now(),
                        )
                        .await?
                }
                None => {
                    self.db
                        .put_route(row.repo_id, "", None, Some(&row.to_target), None, now())
                        .await?
                }
            };
            if ok {
                return Ok(());
            }
        }
        Err(Stop::new("the repository's route kept changing"))
    }

    /// Clears the mirror; a route that then says "default, nowhere else" is removed.
    async fn unmirror(&self, repo_id: i64) -> Result<(), Stop> {
        for _ in 0..ROUTE_RETRIES {
            let Some(r) = self.db.route(repo_id).await? else {
                return Ok(());
            };
            if r.mirror_to.is_none() {
                return Ok(());
            }
            let done = if r.target_id.is_empty() && r.fallback.is_none() {
                self.db.clear_route(repo_id).await?;
                true
            } else {
                self.db
                    .put_route(
                        repo_id,
                        &r.target_id,
                        r.fallback.as_deref(),
                        None,
                        Some(r.version),
                        now(),
                    )
                    .await?
            };
            if done {
                return Ok(());
            }
        }
        Err(Stop::new("the repository's route kept changing"))
    }

    async fn switch(&self, row: &MigrationRow) -> Result<(), Stop> {
        for _ in 0..ROUTE_RETRIES {
            let cur = self.db.route(row.repo_id).await?;
            let ok = match &cur {
                Some(r) if r.mirror_to.as_deref() == Some(row.to_target.as_str()) => {
                    if row.to_target.is_empty() {
                        // Back to the instance default: no row says the same.
                        self.db.clear_route(row.repo_id).await?;
                        true
                    } else {
                        self.db
                            .put_route(
                                row.repo_id,
                                &row.to_target,
                                None,
                                None,
                                Some(r.version),
                                now(),
                            )
                            .await?
                    }
                }
                // Already switched by an earlier attempt of this migration.
                Some(r) if r.target_id == row.to_target && r.mirror_to.is_none() => true,
                None if row.to_target.is_empty() => true,
                _ => return Err(Stop::new("the repository's route changed during the migration")),
            };
            if ok {
                return Ok(());
            }
        }
        Err(Stop::new("the repository's route kept changing"))
    }

    // ------------------------------------------------------------------ the phases

    async fn drive(&self, mut row: MigrationRow) -> Result<Finish, Stop> {
        let ns = {
            let repo = self.db.repo(&row.repo_name).await?;
            match repo {
                Some((id, ns)) if id == row.repo_id => {
                    Ns::from_bytes(&ns).ok_or_else(|| Stop::new("the repository's namespace is invalid"))?
                }
                _ => return Err(Stop::new("the repository no longer exists")),
            }
        };
        let dest = self.router.member(&row.to_target, ns).await?;
        loop {
            row = self.checkpoint(&row.id).await?;
            if row.cancel && !row.switched {
                self.unmirror(row.repo_id).await?;
                self.db
                    .migration_state(&row.id, state::CANCELLED, "", now())
                    .await?;
                return Ok(Finish::Cancelled);
            }
            match row.state.as_str() {
                state::QUEUED => {
                    let sources = self.sources(&row, ns).await?;
                    if sources.iter().any(|s| same_place(s, &dest)) {
                        return Err(Stop::new("the destination is where the objects already are"));
                    }
                    self.set_mirror(&row).await?;
                    self.db.migration_mark(&row.id, Mark::MirrorSince, now()).await?;
                    let mut total = 0i64;
                    for s in &sources {
                        total += count(s).await?;
                    }
                    self.db
                        .migration_progress(&row.id, Some(total), 0, 0, "", now())
                        .await?;
                    self.db
                        .migration_state(&row.id, state::COPYING, "", now())
                        .await?;
                    self.publish(&row, state::COPYING, "");
                }
                state::COPYING => {
                    let sources = self.sources(&row, ns).await?;
                    if self.copy(&row, &sources, &dest).await?.is_none() {
                        continue;
                    }
                    self.db
                        .migration_state(&row.id, state::VERIFYING, "", now())
                        .await?;
                    self.publish(&row, state::VERIFYING, "");
                }
                state::VERIFYING => {
                    self.settle_since(row.mirror_since).await?;
                    let sources = self.sources(&row, ns).await?;
                    let Some(verified) = self.verify(&row, &sources, &dest).await? else {
                        continue;
                    };
                    self.db
                        .migration_progress(
                            &row.id,
                            Some(verified),
                            verified,
                            row.bytes_copied,
                            &row.cursor,
                            now(),
                        )
                        .await?;
                    self.db
                        .migration_state(&row.id, state::SWITCHING, "", now())
                        .await?;
                    self.publish(&row, state::SWITCHING, "");
                }
                state::SWITCHING => {
                    let sources = self.sources(&row, ns).await?;
                    if !row.switched {
                        self.switch(&row).await?;
                        self.db.migration_mark(&row.id, Mark::Switched, now()).await?;
                        self.router.forget_route(ns);
                    }
                    if row.delete_source {
                        tokio::time::sleep(self.settings.settle).await;
                        // After a restart the route already names the destination, so the
                        // sources are the places recorded in the row, not the route's.
                        let places = if row.switched {
                            self.old_places(&row, ns).await?
                        } else {
                            sources
                        };
                        for place in places.iter().filter(|p| !same_place(p, &dest)) {
                            self.renew(&row.id).await?;
                            delete_all(place).await?;
                        }
                    }
                    self.db.migration_state(&row.id, state::DONE, "", now()).await?;
                    return Ok(Finish::Done);
                }
                state::DONE | state::FAILED | state::CANCELLED => return Ok(Finish::Done),
                _ => return Err(Stop::new("unknown migration state")),
            }
        }
    }

    /// The places the objects were in before the switch: `from_target` and what was its
    /// fallback is no longer known after the switch, so only the recorded source is cleaned.
    async fn old_places(&self, row: &MigrationRow, ns: Ns) -> Result<Vec<Member>, Stop> {
        Ok(vec![self.router.member(&row.from_target, ns).await?])
    }

    async fn settle_since(&self, since: i64) -> Result<(), Stop> {
        let wait = i64::try_from(self.settings.settle.as_secs()).unwrap_or(0) - (now() - since);
        if wait > 0 {
            tokio::time::sleep(Duration::from_secs(u64::try_from(wait).unwrap_or(0))).await;
        }
        Ok(())
    }

    /// Copies objects in key order from the persisted cursor. `Some(())` when everything is
    /// copied; `None` when the loop should re-read the row (cancel).
    async fn copy(
        &self,
        row: &MigrationRow,
        sources: &[Member],
        dest: &Member,
    ) -> Result<Option<()>, Stop> {
        let (first, mut after_rel) = parse_cursor(&row.cursor);
        let (mut copied, mut bytes) = (row.objects_copied, row.bytes_copied);
        for (si, src) in sources.iter().enumerate().skip(first) {
            let resume = if si == first { after_rel.take() } else { None };
            let mut list = match resume.and_then(|rel| under(&src.root, &rel)) {
                Some(off) => src.store.list_with_offset(Some(&src.root), &off),
                None => src.store.list(Some(&src.root)),
            }
            .fuse();
            loop {
                let mut chunk = Vec::with_capacity(CHUNK);
                while chunk.len() < CHUNK {
                    match list.try_next().await? {
                        Some(m) => chunk.push(m),
                        None => break,
                    }
                }
                if chunk.is_empty() {
                    break;
                }
                let last = chunk.last().map(|m| m.location.clone());
                let moved: Vec<u64> = stream::iter(chunk)
                    .map(|meta| copy_one(src, dest, meta))
                    .buffered(CONCURRENCY)
                    .try_collect()
                    .await?;
                copied += i64::try_from(moved.len()).unwrap_or(0);
                bytes += i64::try_from(moved.iter().sum::<u64>()).unwrap_or(0);
                // The cursor is a source index and the last key written, relative to the source.
                let rel = last.as_ref().and_then(|p| relative(p, &src.root));
                let cursor = rel.map_or_else(String::new, |r| format!("{si}:{r}"));
                self.db
                    .migration_progress(&row.id, None, copied, bytes, &cursor, now())
                    .await?;
                self.renew(&row.id).await?;
                if self.db.migration(&row.id).await?.is_some_and(|f| f.cancel) {
                    return Ok(None);
                }
                let _ = self.events.send(Event {
                    migration: row.id.clone(),
                    repo: row.repo_name.clone(),
                    state: state::COPYING.into(),
                    copied: u64::try_from(copied).unwrap_or(0),
                    total: u64::try_from(row.objects_total).unwrap_or(0),
                    error: String::new(),
                });
            }
        }
        Ok(Some(()))
    }

    /// Compares every source object with its copy; copies again what is missing or different.
    async fn verify(
        &self,
        row: &MigrationRow,
        sources: &[Member],
        dest: &Member,
    ) -> Result<Option<i64>, Stop> {
        let mut verified = 0i64;
        let mut bad = 0u64;
        for src in sources {
            let mut list = src.store.list(Some(&src.root)).fuse();
            loop {
                let mut chunk = Vec::with_capacity(CHUNK);
                while chunk.len() < CHUNK {
                    match list.try_next().await? {
                        Some(m) => chunk.push(m),
                        None => break,
                    }
                }
                if chunk.is_empty() {
                    break;
                }
                let results: Vec<bool> = stream::iter(chunk)
                    .map(|meta| compare_or_repair(src, dest, meta))
                    .buffered(VERIFY_CONCURRENCY)
                    .try_collect()
                    .await?;
                verified += i64::try_from(results.len()).unwrap_or(0);
                bad += results.iter().filter(|ok| !**ok).count() as u64;
                self.renew(&row.id).await?;
                if self.db.migration(&row.id).await?.is_some_and(|f| f.cancel) {
                    return Ok(None);
                }
            }
        }
        if bad > 0 {
            return Err(Stop::new(format!(
                "verification failed: {bad} objects differ after being copied again"
            )));
        }
        Ok(Some(verified))
    }
}

/// Takes a migration out of the running set when its run ends, however it ends.
struct Forget<'a> {
    running: &'a std::sync::Mutex<std::collections::HashSet<String>>,
    id: String,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        if let Ok(mut r) = self.running.lock() {
            r.remove(&self.id);
        }
    }
}

/// How a run ended.
enum Finish {
    Done,
    Cancelled,
}

fn parse_cursor(c: &str) -> (usize, Option<String>) {
    match c.split_once(':') {
        Some((i, rest)) => (i.parse().unwrap_or(0), Some(rest.to_owned())),
        None => (0, None),
    }
}

/// `root/rel` for a relative path made of already-encoded segments.
fn under(root: &Path, rel: &str) -> Option<Path> {
    let rel = Path::parse(rel).ok()?;
    Some(root.parts().chain(rel.parts()).collect())
}

/// `p` below `root`, as the path relative to it.
fn relative(p: &Path, root: &Path) -> Option<String> {
    let parts: Vec<String> = p.prefix_match(root)?.map(|s| s.as_ref().to_owned()).collect();
    Some(parts.join("/"))
}

fn same_place(a: &Member, b: &Member) -> bool {
    std::sync::Arc::ptr_eq(&a.store, &b.store) && a.root == b.root
}

fn dest_path(dest: &Member, rel: &str) -> Result<Path, Stop> {
    under(&dest.root, rel).ok_or_else(|| Stop::new("object name is not valid"))
}

async fn count(m: &Member) -> Result<i64, Stop> {
    let mut n = 0i64;
    let mut list = m.store.list(Some(&m.root)).fuse();
    while list.try_next().await?.is_some() {
        n += 1;
    }
    Ok(n)
}

/// Copies one object unless the destination already has one of the same size. Returns the bytes
/// moved.
async fn copy_one(src: &Member, dest: &Member, meta: ObjectMeta) -> Result<u64, Stop> {
    let rel = relative(&meta.location, &src.root).ok_or_else(|| Stop::new("object outside the namespace"))?;
    let to = dest_path(dest, &rel)?;
    if let Ok(h) = dest.store.head(&to).await
        && h.size == meta.size
    {
        return Ok(0);
    }
    if meta.size > MAX_OBJECT {
        return Err(Stop::new("an object is larger than a migration moves"));
    }
    let body = match src.store.get(&meta.location).await {
        Ok(g) => g.bytes().await?,
        // Deleted since it was listed (garbage collection): nothing to move.
        Err(object_store::Error::NotFound { .. }) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let n = body.len() as u64;
    dest.store.put(&to, PutPayload::from_bytes(body)).await?;
    Ok(n)
}

/// Reads the object from both places and compares hashes; on a mismatch copies it again and
/// compares once more. `Ok(false)` only if it still differs.
async fn compare_or_repair(src: &Member, dest: &Member, meta: ObjectMeta) -> Result<bool, Stop> {
    let rel = relative(&meta.location, &src.root).ok_or_else(|| Stop::new("object outside the namespace"))?;
    let to = dest_path(dest, &rel)?;
    for attempt in 0..2 {
        let original = match src.store.get(&meta.location).await {
            Ok(g) => g.bytes().await?,
            // Removed from the source since: the mirror removed it from the destination too.
            Err(object_store::Error::NotFound { .. }) => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        let copy = match dest.store.get(&to).await {
            Ok(g) => Some(g.bytes().await?),
            Err(object_store::Error::NotFound { .. }) => None,
            Err(e) => return Err(e.into()),
        };
        if copy.as_ref().is_some_and(|c| blake3::hash(c) == blake3::hash(&original)) {
            return Ok(true);
        }
        if attempt == 0 {
            dest.store.put(&to, PutPayload::from_bytes(original)).await?;
        }
    }
    Ok(false)
}

async fn delete_all(m: &Member) -> Result<(), Stop> {
    let locations = m
        .store
        .list(Some(&m.root))
        .map_ok(|meta| meta.location)
        .boxed();
    let mut deleted = m.store.delete_stream(locations);
    while deleted.try_next().await?.is_some() {}
    Ok(())
}
