//! The pack layer inside [`BucketObjects`]: how reads, bulk writes, deletes and listings use
//! packs. `BucketObjects` calls these from its `Objects` implementation.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};

use super::format::{self, BuiltPack, PackId};
use super::index::{PackIndex, PackInfo, PackLoc, PackState};
use super::{PackConfig, unix_now};
use crate::bucket::BucketObjects;
use crate::key::{KeyClass, KeyPrefix, ObjKey};
use crate::objects::{Error, PUT_CONCURRENCY, Result, decode_verified};

/// Asked before a pack is written, so a deployment can refuse (the repository is read-only for
/// a restore) or record that the repository now holds packs and needs a reader to be served.
#[async_trait]
pub trait PackWriteGate: Send + Sync {
    async fn before_write(&self) -> Result<()>;
}

/// Packs in one namespace: the index, the tuning and what is known about the namespace.
pub struct PackLayer {
    pub(crate) index: Arc<dyn PackIndex>,
    cfg: PackConfig,
    /// A live pack has been seen. Sticky: the cheap test that keeps a repository without packs
    /// from paying an index lookup per read.
    seen_live: AtomicBool,
    gate: Option<Arc<dyn PackWriteGate>>,
    gated: AtomicBool,
    /// When the index last said there were no packs.
    negative_at: Mutex<Option<Instant>>,
    /// When this process first learned of a live pack.
    live_since: Mutex<Option<Instant>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PackLayer {
    pub fn new(index: Arc<dyn PackIndex>, cfg: PackConfig) -> Self {
        Self {
            index,
            cfg,
            seen_live: AtomicBool::new(false),
            gate: None,
            gated: AtomicBool::new(false),
            negative_at: Mutex::new(None),
            live_since: Mutex::new(None),
        }
    }

    /// Asks `gate` before the first pack this layer writes (and again after an error).
    #[must_use]
    pub fn with_gate(mut self, gate: Arc<dyn PackWriteGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    pub fn index(&self) -> &Arc<dyn PackIndex> {
        &self.index
    }

    pub fn config(&self) -> &PackConfig {
        &self.cfg
    }

    /// True when packs may hold something. Free once a pack has been seen; before that, the
    /// index is asked at most once per `negative_ttl`, so a repository without packs pays no
    /// query per read (a query needs a pooled connection, and a caller may hold one).
    pub(crate) async fn maybe(&self) -> Result<bool> {
        if self.seen_live.load(Ordering::Relaxed) {
            return Ok(true);
        }
        if let Some(t) = *lock(&self.negative_at)
            && t.elapsed() < self.cfg.negative_ttl
        {
            return Ok(false);
        }
        let any = self.index.any_live().await?;
        if any {
            self.note_live();
        } else {
            *lock(&self.negative_at) = Some(Instant::now());
        }
        Ok(any)
    }

    /// Tells the layer what the caller already knows about the namespace (from a query made
    /// outside any transaction): whether it holds packs.
    pub fn prime(&self, has_packs: bool) {
        if has_packs {
            self.note_live();
        } else if !self.seen_live.load(Ordering::Relaxed) {
            *lock(&self.negative_at) = Some(Instant::now());
        }
    }

    pub(crate) fn note_live(&self) {
        if !self.seen_live.swap(true, Ordering::Relaxed) {
            *lock(&self.live_since) = Some(Instant::now());
        }
    }

    /// After a miss in both forms: whether to look again with a fresh index. Only an index that
    /// caches (bucket mode) can be behind; the SQL index answered authoritatively.
    pub(crate) async fn recheck(&self) -> Result<bool> {
        if !self.index.is_cached() {
            return Ok(false);
        }
        self.index.refresh().await?;
        let any = self.index.any_live().await?;
        if any {
            self.note_live();
        }
        Ok(any)
    }

    /// How much longer loose copies must stay after a pack went live: readers elsewhere may not
    /// have noticed yet that the namespace has packs (see `negative_ttl`).
    pub(crate) fn settle_for(&self) -> Duration {
        match *lock(&self.live_since) {
            Some(t) => self.cfg.negative_ttl.saturating_sub(t.elapsed()),
            None => self.cfg.negative_ttl,
        }
    }

    /// True when packs may be written from a bulk write. A reader on another server learns that
    /// a namespace holds packs within `negative_ttl`; until then it would not look, so objects
    /// that exist only in a pack wait until this process has known of packs that long.
    pub(crate) fn writes_ready(&self) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        self.cfg.negative_ttl.is_zero()
            || lock(&self.live_since).is_some_and(|t| t.elapsed() >= self.cfg.negative_ttl)
    }

    /// Permission to write a pack of new objects: the writer must be on.
    pub(crate) async fn allow_write(&self) -> Result<()> {
        if !self.cfg.enabled {
            return Err(Error::Index("pack writing is not enabled".into()));
        }
        self.allow_rewrite().await
    }

    /// Permission to rewrite packs that exist (merge, compaction, purge). This needs no switch:
    /// it makes no object appear, and a purge must be able to remove bytes.
    pub(crate) async fn allow_rewrite(&self) -> Result<()> {
        if let Some(g) = &self.gate
            && !self.gated.load(Ordering::Relaxed)
        {
            g.before_write().await?;
            self.gated.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

fn from_format(e: format::FormatError) -> Error {
    Error::Index(e.to_string())
}

/// Page size when listing packed keys.
const LIST_PAGE: usize = 2_000;

impl BucketObjects {
    /// Uses `layer` for packed objects.
    #[must_use]
    pub fn with_packs(mut self, layer: Arc<PackLayer>) -> Self {
        self.packs = Some(layer);
        self
    }

    /// Uses the namespace's own bucket-mode pack index (`pack/*.idx` and `pack/MANIFEST`).
    /// With `cfg.enabled` off it only reads; that is the default of every bucket-mode opener.
    #[must_use]
    pub fn with_bucket_packs(self, cfg: super::PackConfig) -> Self {
        let index = Arc::new(super::BucketPackIndex::new(
            self.store.clone(),
            self.root.clone(),
        ));
        let layer = Arc::new(PackLayer::new(index, cfg));
        self.with_packs(layer)
    }

    /// The pack layer, when this store has one.
    pub fn pack_layer(&self) -> Option<&Arc<PackLayer>> {
        self.packs.as_ref()
    }

    pub(crate) fn pack_path(&self, id: PackId) -> Path {
        self.loc(&format!("pack/{id}.pack"))
    }

    /// The layer if it can hold `key`.
    fn layer_for(&self, key: &ObjKey) -> Option<&Arc<PackLayer>> {
        self.packs.as_ref().filter(|_| key.packable())
    }

    /// Reads the bytes a row names. A pack that is missing or shorter than the row says counts
    /// as no copy: the caller falls back to the loose object.
    pub(crate) async fn read_loc(&self, loc: &PackLoc) -> Result<Option<Bytes>> {
        let path = self.pack_path(loc.pack);
        let range = loc.offset..loc.offset.saturating_add(loc.len);
        match self.store.get_range(&path, range).await {
            Ok(b) if b.len() as u64 == loc.len => Ok(Some(b)),
            Ok(b) => {
                tracing::warn!(pack = %loc.pack, got = b.len(), want = loc.len, "pack is shorter than its index says");
                Ok(None)
            }
            Err(object_store::Error::NotFound { .. }) => {
                tracing::warn!(pack = %loc.pack, "pack file is missing");
                Ok(None)
            }
            Err(e) => {
                // A range past the end of a short pack is an error in most stores; a size check
                // tells that apart from a failing store, which must not read as "no copy".
                match self.store.head(&path).await {
                    Err(object_store::Error::NotFound { .. }) => Ok(None),
                    Ok(m) if m.size < loc.offset.saturating_add(loc.len) => {
                        tracing::warn!(pack = %loc.pack, size = m.size, "pack is shorter than its index says");
                        Ok(None)
                    }
                    _ => Err(e.into()),
                }
            }
        }
    }

    /// The packed stored bytes of `key`, if packs hold it and are readable.
    pub(crate) async fn read_packed(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let Some(l) = self.layer_for(key) else {
            return Ok(None);
        };
        if !l.maybe().await? {
            return Ok(None);
        }
        let Some(loc) = l.index.locate(&[*key]).await?.pop().flatten() else {
            return Ok(None);
        };
        self.read_loc(&loc).await
    }

    /// After a miss in both forms: other writers may have packed the key since the index was
    /// read, so ask the index to forget its cache and look once more.
    pub(crate) async fn read_packed_fresh(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let Some(l) = self.layer_for(key) else {
            return Ok(None);
        };
        if !l.recheck().await? {
            return Ok(None);
        }
        let Some(loc) = l.index.locate(&[*key]).await?.pop().flatten() else {
            return Ok(None);
        };
        self.read_loc(&loc).await
    }

    /// Reads `key` from packs then loose; a damaged packed copy yields to a good loose one.
    pub(crate) async fn get_with_packs(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        let mut damaged = None;
        if let Some(stored) = self.read_packed(key).await? {
            match decode_verified(key, &stored) {
                Ok(d) => return Ok(Some(d)),
                Err(e @ (Error::Corrupt(_) | Error::Codec(..))) => damaged = Some(e),
                Err(e) => return Err(e),
            }
        }
        match self.get_loose_stored(key).await? {
            Some(stored) => decode_verified(key, &stored).map(Some),
            None => {
                if let Some(e) = damaged {
                    return Err(e);
                }
                match self.read_packed_fresh(key).await? {
                    Some(stored) => decode_verified(key, &stored).map(Some),
                    None => Ok(None),
                }
            }
        }
    }

    /// `get_stored` over packs and loose objects.
    pub(crate) async fn get_stored_with_packs(&self, key: &ObjKey) -> Result<Option<Bytes>> {
        if let Some(stored) = self.read_packed(key).await? {
            return Ok(Some(stored));
        }
        match self.get_loose_stored(key).await? {
            Some(s) => Ok(Some(s)),
            None => self.read_packed_fresh(key).await,
        }
    }

    /// Presence of each key: the index answers for packed ones in one query, the store for the
    /// rest.
    pub(crate) async fn has_many_with_packs(&self, keys: &[ObjKey]) -> Result<Vec<bool>> {
        let mut found = vec![false; keys.len()];
        let mut ask: Vec<usize> = Vec::new();
        if let Some(l) = &self.packs
            && l.maybe().await?
        {
            let idx: Vec<usize> = (0..keys.len()).filter(|&i| keys[i].packable()).collect();
            let want: Vec<ObjKey> = idx.iter().map(|&i| keys[i]).collect();
            for (i, loc) in idx.into_iter().zip(l.index.locate(&want).await?) {
                found[i] = loc.is_some();
            }
        }
        for (i, present) in found.iter().enumerate() {
            if !present {
                ask.push(i);
            }
        }
        let loose_keys: Vec<ObjKey> = ask.iter().map(|&i| keys[i]).collect();
        for (i, present) in ask.iter().zip(self.has_many_loose(&loose_keys).await?) {
            found[*i] = present;
        }
        // A key that is nowhere may have been packed since the index was read.
        if let Some(l) = &self.packs {
            let missing: Vec<usize> = (0..keys.len())
                .filter(|&i| !found[i] && keys[i].packable())
                .collect();
            if !missing.is_empty() && l.recheck().await? {
                let want: Vec<ObjKey> = missing.iter().map(|&i| keys[i]).collect();
                for (i, loc) in missing.into_iter().zip(l.index.locate(&want).await?) {
                    found[i] = loc.is_some();
                }
            }
        }
        Ok(found)
    }

    /// True when `key` is packed and its packed copy is whole.
    pub(crate) async fn packed_and_valid(&self, key: &ObjKey) -> Result<bool> {
        match self.read_packed(key).await? {
            Some(stored) => Ok(decode_verified(key, &stored).is_ok()),
            None => Ok(false),
        }
    }

    /// Makes loose copy `key` the one readers find, after finding the packed copy damaged.
    pub(crate) async fn forget_packed(&self, keys: &[ObjKey]) -> Result<u64> {
        let Some(l) = &self.packs else {
            return Ok(0);
        };
        let packable: Vec<ObjKey> = keys.iter().filter(|k| k.packable()).copied().collect();
        if packable.is_empty() || !l.maybe().await? {
            return Ok(0);
        }
        l.index.forget(&packable).await
    }

    /// Lists the keys packs hold, in key order, with their pack's creation time.
    pub(crate) fn list_packed(
        &self,
        prefix: &KeyPrefix,
    ) -> BoxStream<'_, Result<(ObjKey, SystemTime)>> {
        let Some(l) = self.packs.clone() else {
            return stream::empty().boxed();
        };
        let class: Option<KeyClass> = prefix.class_of();
        if class.is_some_and(|c| !c.packable()) {
            return stream::empty().boxed();
        }
        stream::unfold(
            (l, None::<(u8, [u8; 32])>, false),
            move |(l, after, done)| async move {
                if done {
                    return None;
                }
                match l.index.list(class, after, LIST_PAGE).await {
                    Err(e) => Some((vec![Err(e)], (l, after, true))),
                    Ok(page) if page.is_empty() => None,
                    Ok(page) => {
                        let last = page.last().map(|(k, _)| (k.class().code(), k.id32()));
                        let end = page.len() < LIST_PAGE;
                        let items = page
                            .into_iter()
                            .map(|(k, created)| {
                                let t = SystemTime::UNIX_EPOCH
                                    + Duration::from_secs(u64::try_from(created).unwrap_or(0));
                                Ok((k, t))
                            })
                            .collect::<Vec<_>>();
                        Some((items, (l, last, end)))
                    }
                }
            },
        )
        .flat_map(stream::iter)
        .boxed()
    }

    // ---------------------------------------------------------------------- bulk writes

    /// Writes `items` as loose objects, concurrently.
    pub(crate) async fn put_loose_many(&self, items: Vec<(ObjKey, Bytes)>) -> Result<()> {
        stream::iter(items)
            .map(|(k, v)| async move { self.put_loose(&k, v).await })
            .buffer_unordered(PUT_CONCURRENCY)
            .try_collect::<Vec<()>>()
            .await
            .map(|_| ())
    }

    /// The bulk write: verifies every value, skips those already packed, puts small ones in a
    /// new pack when packing is on and there are enough of them, and the rest loose.
    pub(crate) async fn put_many_with_packs(&self, items: Vec<(ObjKey, Bytes)>) -> Result<()> {
        let mut seen: HashSet<ObjKey> = HashSet::with_capacity(items.len());
        let mut todo: Vec<(ObjKey, Bytes)> = Vec::with_capacity(items.len());
        for (key, stored) in items {
            if !seen.insert(key) {
                continue;
            }
            crate::codec::check_codec(&stored).map_err(|e| Error::Codec(key, e))?;
            decode_verified(&key, &stored)?;
            todo.push((key, stored));
        }
        let Some(l) = self.packs.clone() else {
            return self.put_loose_many(todo).await;
        };
        // An object already in a pack is left alone; the index is trusted, as a loose object's
        // existence is.
        if l.maybe().await? {
            let packable: Vec<usize> = (0..todo.len()).filter(|&i| todo[i].0.packable()).collect();
            let want: Vec<ObjKey> = packable.iter().map(|&i| todo[i].0).collect();
            let at = l.index.locate(&want).await?;
            let skip: HashSet<usize> = packable
                .into_iter()
                .zip(at)
                .filter_map(|(i, loc)| loc.map(|_| i))
                .collect();
            if !skip.is_empty() {
                todo = todo
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, kv)| (!skip.contains(&i)).then_some(kv))
                    .collect();
            }
        }
        let cfg = l.config();
        let (small, loose): (Vec<_>, Vec<_>) = todo
            .into_iter()
            .partition(|(k, v)| k.packable() && v.len() <= cfg.max_object);
        if !l.writes_ready() || small.len() < cfg.min_batch {
            let mut all = small;
            all.extend(loose);
            return self.put_loose_many(all).await;
        }
        let mut loose = loose;
        let mut groups: Vec<Vec<(ObjKey, Bytes)>> = Vec::new();
        let mut cur: Vec<(ObjKey, Bytes)> = Vec::new();
        let mut size = format::HEADER_LEN + format::TRAILER_LEN;
        for (k, v) in small {
            let add = format::entry_size(v.len());
            if !cur.is_empty()
                && (cur.len() >= cfg.max_objects || (size + add) as u64 > cfg.max_size)
            {
                groups.push(std::mem::take(&mut cur));
                size = format::HEADER_LEN + format::TRAILER_LEN;
            }
            size += add;
            cur.push((k, v));
        }
        if !cur.is_empty() {
            groups.push(cur);
        }
        for g in groups {
            if g.len() < cfg.min_batch.min(2) {
                loose.extend(g);
            } else {
                self.write_pack(&l, g).await?;
            }
        }
        self.put_loose_many(loose).await
    }

    /// Writes one pack file for `items` (verified by the caller), checks it landed whole, and
    /// makes it live in the index. A failure before the index commit leaves an unreferenced
    /// file that the repack job deletes after `orphan_age`.
    pub(crate) async fn write_pack(
        &self,
        l: &PackLayer,
        items: Vec<(ObjKey, Bytes)>,
    ) -> Result<PackInfo> {
        l.allow_write().await?;
        let built = format::build(PackId::random(), items).map_err(from_format)?;
        self.store_pack(&built).await?;
        let info = PackInfo {
            id: built.id,
            objects: built.entries.len() as u64,
            bytes: built.bytes.len() as u64,
            created: unix_now(),
            state: PackState::Live,
            retired: 0,
        };
        l.index.commit(&info, &built.entries, &[]).await?;
        l.note_live();
        Ok(info)
    }

    /// Puts the pack file and reads the end of it back: a store that acknowledged a write it
    /// did not keep must not get a row pointing at it.
    pub(crate) async fn store_pack(&self, built: &BuiltPack) -> Result<()> {
        let path = self.pack_path(built.id);
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        self.store
            .put_opts(&path, PutPayload::from_bytes(built.bytes.clone()), opts)
            .await?;
        let n = built.bytes.len() as u64;
        let tail = self
            .store
            .get_range(&path, n.saturating_sub(format::TRAILER_LEN as u64)..n)
            .await;
        let ok = matches!(&tail, Ok(t) if t[..] == built.checksum()[..]);
        if !ok {
            let _ = self.store.delete(&path).await;
            return Err(Error::Index(format!(
                "pack {} did not read back whole after it was written",
                built.id
            )));
        }
        Ok(())
    }

    /// Lists the pack-directory files with their sizes and times.
    pub(crate) fn list_pack_files(&self) -> BoxStream<'_, Result<(String, u64, SystemTime)>> {
        let dir = self.loc("pack");
        let strip = format!("{dir}/");
        self.store
            .list(Some(&dir))
            .map(move |r| {
                let meta = r?;
                let name = meta
                    .location
                    .as_ref()
                    .strip_prefix(&strip)
                    .unwrap_or_default()
                    .to_owned();
                Ok((name, meta.size, SystemTime::from(meta.last_modified)))
            })
            .boxed()
    }
}
