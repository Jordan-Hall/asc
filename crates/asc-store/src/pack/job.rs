//! Maintenance of packs: pack old loose objects, merge small packs, compact packs GC has
//! hollowed out, delete what nothing references, check and rebuild the index.
//!
//! Crash safety, in order for every pack written: (a) the pack file is written and read back; (b)
//! one index commit makes its rows live; (c) only then are loose copies deleted, and only under
//! the GC fence. A crash after (a) leaves an unreferenced file the orphan sweep removes; a crash
//! after (b) leaves both copies, and readers prefer the pack; no step makes an object unreadable.

use std::collections::HashSet;
use std::time::SystemTime;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use object_store::ObjectStoreExt;

use super::PackConfig;
use super::format::{self, PackId};
use super::index::{PackDetail, PackInfo, PackState};
use super::layer::PackLayer;
use crate::bucket::BucketObjects;
use crate::gc::Fence;
use crate::key::{KeyClass, ObjKey};
use crate::objects::{Error, PUT_CONCURRENCY, Result, decode_verified};

/// Classes a pack may hold, in sweep order.
const PACK_CLASSES: [KeyClass; 7] = [
    KeyClass::File,
    KeyClass::ChunkList,
    KeyClass::Tree,
    KeyClass::Changeset,
    KeyClass::Tag,
    KeyClass::GitTreeOverride,
    KeyClass::ChangeMeta,
];

/// Loose candidates considered in one run; the rest wait for the next.
const MAX_CANDIDATES: usize = 2_000_000;
/// Merges attempted in one run.
const MAX_MERGES: usize = 16;

/// What one [`BucketObjects::pack_run`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackReport {
    /// Old loose small objects found.
    pub loose_found: u64,
    /// The run left loose objects alone because there were fewer than `min_loose`.
    pub below_threshold: bool,
    /// Loose objects now in packs.
    pub packed: u64,
    pub packs_written: u64,
    /// Loose copies deleted after their pack was live.
    pub loose_deleted: u64,
    /// Packs merged into another.
    pub merged: u64,
    /// Packs rewritten without their dead entries.
    pub compacted: u64,
    /// Packs retired with nothing live in them.
    pub emptied: u64,
    /// Retired pack files deleted.
    pub deleted: u64,
    /// Unreferenced files deleted.
    pub orphans: u64,
    /// Objects or packs left alone because they did not verify.
    pub damaged: u64,
}

/// A repository's loose small objects and packs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackStatus {
    pub loose_small: u64,
    pub loose_small_bytes: u64,
    pub live_packs: u64,
    pub retired_packs: u64,
    pub packed_objects: u64,
    pub pack_bytes: u64,
    /// Bytes of live packs no index row references.
    pub dead_bytes: u64,
}

/// What [`BucketObjects::pack_verify`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub packs: u64,
    pub objects: u64,
    pub problems: Vec<String>,
}

/// What [`BucketObjects::pack_reindex`] rebuilt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReindexReport {
    pub packs: u64,
    pub objects: u64,
    /// Pack files that did not parse and were left out.
    pub skipped: Vec<String>,
}

fn fence_err(e: impl std::fmt::Display) -> Error {
    Error::Index(format!("fence: {e}"))
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl BucketObjects {
    fn layer(&self) -> Result<&std::sync::Arc<PackLayer>> {
        self.packs
            .as_ref()
            .ok_or_else(|| Error::Index("this store has no pack layer".into()))
    }

    fn list_loose_meta(&self, class: KeyClass) -> BoxStream<'_, Result<(ObjKey, SystemTime, u64)>> {
        let dir = self.loc(class.dir());
        let strip = if self.root.as_ref().is_empty() {
            String::new()
        } else {
            format!("{}/", self.root)
        };
        self.store
            .list(Some(&dir))
            .filter_map(move |r| {
                let item = match r {
                    Err(e) => Some(Err(e.into())),
                    Ok(meta) => meta
                        .location
                        .as_ref()
                        .strip_prefix(&strip)
                        .and_then(ObjKey::parse)
                        .map(|k| Ok((k, SystemTime::from(meta.last_modified), meta.size))),
                };
                std::future::ready(item)
            })
            .boxed()
    }

    /// The whole pack file, size-capped.
    async fn read_pack(&self, cfg: &PackConfig, id: PackId) -> Result<Option<Bytes>> {
        let cap = cfg.max_size.max(64 << 20).saturating_mul(2);
        match self.store.get(&self.pack_path(id)).await {
            Ok(r) => crate::bucket::read_capped(r, cap)
                .await?
                .map(Some)
                .ok_or_else(|| Error::Index(format!("pack {id} is larger than {cap} bytes"))),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_loose(&self, key: &ObjKey) -> Result<()> {
        self.delete_file(&self.key_path(key)).await
    }

    async fn delete_file(&self, path: &object_store::path::Path) -> Result<()> {
        match self.store.delete(path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // ------------------------------------------------------------------------------ run

    /// One maintenance pass: pack old loose objects, compact, merge, then delete retired packs
    /// and orphans. `force` packs even when fewer than `min_loose` objects are waiting.
    /// `fence` is entered around every step that deletes loose objects or retires a pack.
    pub async fn pack_run(
        &self,
        fence: &dyn Fence,
        now: SystemTime,
        force: bool,
    ) -> Result<PackReport> {
        let l = self.layer()?.clone();
        let mut rep = PackReport::default();
        if l.config().enabled {
            self.pack_loose(&l, fence, now, force, &mut rep).await?;
            self.pack_compact_below(&l, fence, now, l.config().compact_below, &mut rep)
                .await?;
            self.pack_merge(&l, fence, now, &mut rep).await?;
        }
        self.sweep_retired(&l, fence, now, &mut rep).await?;
        self.sweep_orphans(&l, now, &mut rep).await?;
        Ok(rep)
    }

    /// Rewrites every pack that holds any dead entry. GC calls it after an operator purge: the
    /// bytes must go, not wait for half a pack to die.
    pub async fn pack_compact_all(&self, fence: &dyn Fence, now: SystemTime) -> Result<PackReport> {
        let l = self.layer()?.clone();
        let mut rep = PackReport::default();
        self.pack_compact_below(&l, fence, now, 1.0, &mut rep)
            .await?;
        Ok(rep)
    }

    async fn pack_loose(
        &self,
        l: &PackLayer,
        fence: &dyn Fence,
        now: SystemTime,
        force: bool,
        rep: &mut PackReport,
    ) -> Result<()> {
        let cfg = l.config().clone();
        let mut cands: Vec<ObjKey> = Vec::new();
        'classes: for class in PACK_CLASSES {
            let mut s = self.list_loose_meta(class);
            while let Some((key, modified, size)) = s.try_next().await? {
                let old = now.duration_since(modified).is_ok_and(|a| a >= cfg.min_age);
                if size as usize <= cfg.max_object && old {
                    cands.push(key);
                    if cands.len() >= MAX_CANDIDATES {
                        break 'classes;
                    }
                }
            }
        }
        rep.loose_found = cands.len() as u64;
        if cands.len() < cfg.min_loose && !force {
            rep.below_threshold = true;
            return Ok(());
        }
        let mut group: Vec<(ObjKey, Bytes)> = Vec::new();
        let mut size = format::HEADER_LEN + format::TRAILER_LEN;
        // Read a few at a time; `join_all` over named calls keeps the future provably `Send`
        // where a stream of closures borrowing `self` does not (this runs under `tokio::spawn`).
        for keys in cands.chunks(PUT_CONCURRENCY) {
            let reads = futures::future::join_all(keys.iter().map(|k| self.read_loose(*k))).await;
            for (key, got) in reads {
                self.take_loose(l, fence, &cfg, key, got, &mut group, &mut size, rep)
                    .await?;
            }
        }
        if !group.is_empty() {
            self.flush_group(l, fence, group, rep).await?;
        }
        Ok(())
    }

    async fn read_loose(&self, key: ObjKey) -> (ObjKey, Result<Option<Bytes>>) {
        (key, self.get_loose_stored(&key).await)
    }

    /// Adds one read loose object to `group`, writing the group out first when it is full.
    #[allow(clippy::too_many_arguments)]
    async fn take_loose(
        &self,
        l: &PackLayer,
        fence: &dyn Fence,
        cfg: &PackConfig,
        key: ObjKey,
        got: Result<Option<Bytes>>,
        group: &mut Vec<(ObjKey, Bytes)>,
        size: &mut usize,
        rep: &mut PackReport,
    ) -> Result<()> {
        {
            let stored = match got {
                Ok(Some(s)) if s.len() <= cfg.max_object => s,
                // Gone (a GC delete) or grown past the limit: not ours to pack.
                Ok(_) => return Ok(()),
                Err(Error::Codec(..)) => {
                    rep.damaged += 1;
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            if decode_verified(&key, &stored).is_err() {
                // A damaged loose object stays where it is for an operator to see.
                rep.damaged += 1;
                return Ok(());
            }
            let add = format::entry_size(stored.len());
            if !group.is_empty()
                && (group.len() >= cfg.max_objects || (*size + add) as u64 > cfg.max_size)
            {
                self.flush_group(l, fence, std::mem::take(group), rep)
                    .await?;
                *size = format::HEADER_LEN + format::TRAILER_LEN;
            }
            *size += add;
            group.push((key, stored));
        }
        Ok(())
    }

    /// Writes one pack of loose objects, then removes their loose copies.
    async fn flush_group(
        &self,
        l: &PackLayer,
        fence: &dyn Fence,
        group: Vec<(ObjKey, Bytes)>,
        rep: &mut PackReport,
    ) -> Result<()> {
        let keys: Vec<ObjKey> = group.iter().map(|(k, _)| *k).collect();
        self.write_pack(l, group).await?;
        rep.packs_written += 1;
        // Delete a loose copy only where the index now answers for the key.
        let at = l.index.locate(&keys).await?;
        let doomed: Vec<ObjKey> = keys
            .into_iter()
            .zip(at)
            .filter_map(|(k, loc)| loc.map(|_| k))
            .collect();
        rep.packed += doomed.len() as u64;
        let settle = l.settle_for();
        if !settle.is_zero() {
            tokio::time::sleep(settle).await;
        }
        let _guard = fence.enter().await.map_err(fence_err)?;
        for chunk in doomed.chunks(PUT_CONCURRENCY) {
            let done = futures::future::join_all(chunk.iter().map(|k| self.delete_loose(k))).await;
            done.into_iter().collect::<Result<Vec<()>>>()?;
        }
        rep.loose_deleted += doomed.len() as u64;
        Ok(())
    }

    // ----------------------------------------------------------------- merge and compact

    /// Compacts live packs whose referenced share of their bytes is below `below`; retires those
    /// with nothing referenced.
    async fn pack_compact_below(
        &self,
        l: &PackLayer,
        fence: &dyn Fence,
        now: SystemTime,
        below: f64,
        rep: &mut PackReport,
    ) -> Result<()> {
        let packs = l.index.packs().await?;
        let mut empty = Vec::new();
        for d in packs
            .into_iter()
            .filter(|d| d.info.state == PackState::Live)
        {
            let share = d.live_bytes as f64 / d.info.bytes.max(1) as f64;
            if d.live_objects == 0 {
                empty.push(d.info.id);
            } else if share < below
                && d.live_bytes < d.info.bytes
                && self
                    .rewrite(l, fence, now, std::slice::from_ref(&d), rep)
                    .await?
            {
                rep.compacted += 1;
            }
        }
        if !empty.is_empty() {
            let _guard = fence.enter().await.map_err(fence_err)?;
            l.index.retire(&empty, secs(now)).await?;
            rep.emptied += empty.len() as u64;
        }
        Ok(())
    }

    /// Merges small live packs into one while at least `merge_count` of them are waiting.
    async fn pack_merge(
        &self,
        l: &PackLayer,
        fence: &dyn Fence,
        now: SystemTime,
        rep: &mut PackReport,
    ) -> Result<()> {
        let cfg = l.config().clone();
        for _ in 0..MAX_MERGES {
            let mut small: Vec<PackDetail> = l
                .index
                .packs()
                .await?
                .into_iter()
                .filter(|d| {
                    d.info.state == PackState::Live
                        && d.info.bytes < cfg.merge_below
                        && d.live_objects > 0
                })
                .collect();
            if small.len() < cfg.merge_count.max(2) {
                return Ok(());
            }
            small.sort_by_key(|d| (d.info.created, d.info.id));
            // Take packs, oldest first, while the result fits one pack.
            let (mut bytes, mut objects) = (0u64, 0u64);
            let mut take = Vec::new();
            for d in small {
                if !take.is_empty()
                    && (bytes + d.live_bytes > cfg.max_size
                        || objects + d.live_objects > cfg.max_objects as u64)
                {
                    break;
                }
                bytes += d.live_bytes;
                objects += d.live_objects;
                take.push(d);
            }
            if take.len() < 2 {
                return Ok(());
            }
            let n = take.len() as u64;
            if !self.rewrite(l, fence, now, &take, rep).await? {
                return Ok(());
            }
            rep.merged += n;
        }
        Ok(())
    }

    /// Copies the entries `olds` still reference into one new pack and retires `olds`. Packs that
    /// do not read back whole, or hold an entry that does not verify, are left alone and counted
    /// as damaged. Returns whether a rewrite happened.
    async fn rewrite(
        &self,
        l: &PackLayer,
        fence: &dyn Fence,
        now: SystemTime,
        olds: &[PackDetail],
        rep: &mut PackReport,
    ) -> Result<bool> {
        let cfg = l.config().clone();
        let mut items: Vec<(ObjKey, Bytes)> = Vec::new();
        let mut replaced: Vec<PackId> = Vec::new();
        for d in olds {
            let id = d.info.id;
            let Some(bytes) = self.read_pack(&cfg, id).await? else {
                tracing::warn!(pack = %id, "pack file is missing; left in the index");
                rep.damaged += 1;
                continue;
            };
            if format::parse(&bytes).is_err() {
                tracing::warn!(pack = %id, "pack does not verify; left alone");
                rep.damaged += 1;
                continue;
            }
            let mut mine = Vec::new();
            let mut ok = true;
            for row in l.index.rows_of(id).await? {
                let stored = format::slice(&bytes, &row).filter(|s| s.len() == row.len as usize);
                match stored {
                    Some(s) if decode_verified(&row.key, &s).is_ok() => mine.push((row.key, s)),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                tracing::warn!(pack = %id, "pack has an entry that does not verify; left alone");
                rep.damaged += 1;
                continue;
            }
            items.extend(mine);
            replaced.push(id);
        }
        if replaced.is_empty() {
            return Ok(false);
        }
        if items.is_empty() {
            let _guard = fence.enter().await.map_err(fence_err)?;
            l.index.retire(&replaced, secs(now)).await?;
            rep.emptied += replaced.len() as u64;
            return Ok(true);
        }
        l.allow_rewrite().await?;
        let built =
            format::build(PackId::random(), items).map_err(|e| Error::Index(e.to_string()))?;
        self.store_pack(&built).await?;
        let info = PackInfo {
            id: built.id,
            objects: built.entries.len() as u64,
            bytes: built.bytes.len() as u64,
            created: secs(now),
            state: PackState::Live,
            retired: 0,
        };
        let _guard = fence.enter().await.map_err(fence_err)?;
        l.index.commit(&info, &built.entries, &replaced).await?;
        rep.packs_written += 1;
        Ok(true)
    }

    // ------------------------------------------------------------------------- clean-up

    /// Deletes files of packs retired longer than `retire_age` ago.
    async fn sweep_retired(
        &self,
        l: &PackLayer,
        fence: &dyn Fence,
        now: SystemTime,
        rep: &mut PackReport,
    ) -> Result<()> {
        let age = i64::try_from(l.config().retire_age.as_secs()).unwrap_or(i64::MAX);
        let due: Vec<PackId> = l
            .index
            .packs()
            .await?
            .into_iter()
            .filter(|d| d.info.state == PackState::Retired)
            .filter(|d| secs(now).saturating_sub(d.info.retired) >= age)
            .map(|d| d.info.id)
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        let _guard = fence.enter().await.map_err(fence_err)?;
        for id in due {
            // A retired pack with rows still pointing into it (a rewrite that was partly
            // refused) keeps its file: the file goes only when nothing needs it.
            if !l.index.rows_of(id).await?.is_empty() {
                continue;
            }
            self.delete_file(&self.pack_path(id)).await?;
            l.index.remove(id).await?;
            rep.deleted += 1;
        }
        Ok(())
    }

    /// Deletes pack-directory files that no pack in the index accounts for and that are older
    /// than `orphan_age`: the leftovers of a writer that died before its index commit.
    async fn sweep_orphans(
        &self,
        l: &PackLayer,
        now: SystemTime,
        rep: &mut PackReport,
    ) -> Result<()> {
        let known: HashSet<PackId> = l.index.packs().await?.iter().map(|d| d.info.id).collect();
        let age = l.config().orphan_age;
        let mut doomed = Vec::new();
        let mut files = self.list_pack_files();
        while let Some((name, _, modified)) = files.try_next().await? {
            let Some((stem, ext)) = name.rsplit_once('.') else {
                continue;
            };
            if !matches!(ext, "pack" | "idx" | "del") {
                continue;
            }
            let Some(id) = PackId::parse_hex(stem) else {
                continue;
            };
            let old = now.duration_since(modified).is_ok_and(|a| a >= age);
            if old && !known.contains(&id) {
                doomed.push(name);
            }
        }
        for name in doomed {
            self.delete_file(&self.loc(&format!("pack/{name}"))).await?;
            rep.orphans += 1;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ status and repair

    /// Counts loose small objects and describes the packs.
    pub async fn pack_status(&self) -> Result<PackStatus> {
        let l = self.layer()?.clone();
        let mut st = PackStatus::default();
        for class in PACK_CLASSES {
            let mut s = self.list_loose_meta(class);
            while let Some((_, _, size)) = s.try_next().await? {
                if size as usize <= l.config().max_object {
                    st.loose_small += 1;
                    st.loose_small_bytes += size;
                }
            }
        }
        for d in l.index.packs().await? {
            match d.info.state {
                PackState::Live => {
                    st.live_packs += 1;
                    st.packed_objects += d.live_objects;
                    st.pack_bytes += d.info.bytes;
                    let overhead = (format::HEADER_LEN + format::TRAILER_LEN) as u64;
                    st.dead_bytes += d
                        .info
                        .bytes
                        .saturating_sub(d.live_bytes)
                        .saturating_sub(overhead);
                }
                PackState::Retired => st.retired_packs += 1,
            }
        }
        Ok(st)
    }

    /// Checks every live pack: the file is there and its checksum holds, every index row names
    /// an entry that is in the pack, and every referenced value decodes to its key.
    pub async fn pack_verify(&self) -> Result<VerifyReport> {
        let l = self.layer()?.clone();
        let mut rep = VerifyReport::default();
        for d in l.index.packs().await? {
            if d.info.state != PackState::Live {
                continue;
            }
            let id = d.info.id;
            rep.packs += 1;
            let Some(bytes) = self.read_pack(l.config(), id).await? else {
                rep.problems
                    .push(format!("pack {id} is in the index but its file is missing"));
                continue;
            };
            let entries = match format::parse(&bytes) {
                Ok(e) => e,
                Err(e) => {
                    rep.problems.push(format!("pack {id}: {e}"));
                    continue;
                }
            };
            let have: HashSet<(ObjKey, u64, u32)> =
                entries.iter().map(|e| (e.key, e.offset, e.len)).collect();
            for row in l.index.rows_of(id).await? {
                rep.objects += 1;
                if !have.contains(&(row.key, row.offset, row.len)) {
                    rep.problems.push(format!(
                        "pack {id}: the index names {} but the pack has no such entry",
                        row.key
                    ));
                    continue;
                }
                let stored = format::slice(&bytes, &row);
                if stored.is_none_or(|s| decode_verified(&row.key, &s).is_err()) {
                    rep.problems
                        .push(format!("pack {id}: {} does not verify", row.key));
                }
            }
        }
        Ok(rep)
    }

    /// Rebuilds the index from the pack files alone (disaster recovery). Pack files that do not
    /// parse are left out and named. Retirement is not recorded in a pack, so every pack is
    /// live afterwards; the next run compacts the duplicates away.
    pub async fn pack_reindex(&self) -> Result<ReindexReport> {
        let l = self.layer()?.clone();
        let mut files = Vec::new();
        let mut s = self.list_pack_files();
        while let Some((name, _, modified)) = s.try_next().await? {
            if let Some(stem) = name.strip_suffix(".pack")
                && let Some(id) = PackId::parse_hex(stem)
            {
                files.push((id, modified));
            }
        }
        drop(s);
        files.sort_by_key(|(id, t)| (*t, *id));
        let mut rep = ReindexReport::default();
        let mut packs = Vec::new();
        for (id, modified) in files {
            let parsed = match self.read_pack(l.config(), id).await? {
                Some(b) => format::parse(&b).map(|e| (b.len() as u64, e)),
                None => continue,
            };
            match parsed {
                Ok((bytes, entries)) => {
                    rep.packs += 1;
                    rep.objects += entries.len() as u64;
                    packs.push((
                        PackInfo {
                            id,
                            objects: entries.len() as u64,
                            bytes,
                            created: secs(modified),
                            state: PackState::Live,
                            retired: 0,
                        },
                        entries,
                    ));
                }
                Err(e) => rep.skipped.push(format!("{id}: {e}")),
            }
        }
        l.index.rebuild(&packs).await?;
        if !packs.is_empty() {
            l.note_live();
        }
        Ok(rep)
    }
}
