//! [`PackIndex`] for bucket mode, where there is no database.
//!
//! Beside each `pack/<id>.pack` sits `pack/<id>.idx`, a sorted array of fixed-size records with a
//! blake3 trailer, written before the pack becomes visible. `pack/MANIFEST` lists the packs and
//! is replaced with a conditional `PUT` (the mechanism `gc/freeze` uses; a store that cannot
//! make the write conditional gets a plain one, which is safe with the single writer that GC and
//! the repack job already are). A GC delete writes `pack/<id>.del`, the sorted keys of that pack
//! that no longer exist.
//!
//! A key is looked up in the live packs oldest first, so a key packed twice resolves to the pack
//! that had it first, as the SQL index keeps the first row. Idx files are immutable: they are read
//! whole once and cached. The manifest is cached for [`MANIFEST_TTL`]; [`PackIndex::refresh`]
//! drops the cache.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::format::{PackEntry, PackId};
use super::index::{PackDetail, PackIndex, PackInfo, PackLoc, PackState};
use crate::key::{KeyClass, ObjKey};
use crate::objects::{Error, Result};

/// How long a read manifest is trusted.
pub const MANIFEST_TTL: Duration = Duration::from_secs(2);
const IDX_MAGIC: &[u8; 8] = b"ASCPIDX1";
const DEL_MAGIC: &[u8; 8] = b"ASCPDEL1";
const REC_LEN: usize = 1 + 32 + 8 + 4;
const DEL_LEN: usize = 1 + 32;
/// A manifest or sidecar larger than this is refused.
const MAX_SIDE_BYTES: u64 = 256 << 20;
const CAS_TRIES: usize = 16;

type RowKey = (u8, [u8; 32]);

fn bad(what: impl std::fmt::Display) -> Error {
    Error::Index(what.to_string())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ManifestPack {
    id: String,
    objects: u64,
    bytes: u64,
    created: i64,
    state: i64,
    retired: i64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
struct Manifest {
    version: u32,
    packs: Vec<ManifestPack>,
}

impl ManifestPack {
    fn of(i: &PackInfo) -> Self {
        Self {
            id: i.id.hex(),
            objects: i.objects,
            bytes: i.bytes,
            created: i.created,
            state: i.state.code(),
            retired: i.retired,
        }
    }

    fn info(&self) -> Option<PackInfo> {
        Some(PackInfo {
            id: PackId::parse_hex(&self.id)?,
            objects: self.objects,
            bytes: self.bytes,
            created: self.created,
            state: PackState::from_code(self.state)?,
            retired: self.retired,
        })
    }
}

struct Loaded {
    infos: Vec<PackInfo>,
    version: Option<UpdateVersion>,
    at: Instant,
}

/// One idx file: records sorted by (class, id).
struct Idx {
    recs: Vec<(RowKey, u64, u32)>,
}

impl Idx {
    fn find(&self, k: &RowKey) -> Option<(u64, u32)> {
        self.recs
            .binary_search_by(|r| r.0.cmp(k))
            .ok()
            .map(|i| (self.recs[i].1, self.recs[i].2))
    }

    fn encode(entries: &[PackEntry]) -> Bytes {
        let mut recs: Vec<(RowKey, u64, u32)> = entries
            .iter()
            .map(|e| ((e.key.class().code(), e.key.id32()), e.offset, e.len))
            .collect();
        recs.sort_by_key(|r| r.0);
        recs.dedup_by(|b, a| a.0 == b.0);
        let mut out = BytesMut::with_capacity(12 + recs.len() * REC_LEN + 32);
        out.put_slice(IDX_MAGIC);
        out.put_u32_le(recs.len() as u32);
        for (k, off, len) in &recs {
            out.put_u8(k.0);
            out.put_slice(&k.1);
            out.put_u64_le(*off);
            out.put_u32_le(*len);
        }
        let sum = blake3::hash(&out);
        out.put_slice(sum.as_bytes());
        out.freeze()
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let body = checked(b, IDX_MAGIC)?;
        let n = u32::from_le_bytes(body[8..12].try_into().map_err(|_| bad("idx header"))?) as usize;
        if body.len() != 12 + n * REC_LEN {
            return Err(bad("idx length"));
        }
        let mut recs = Vec::with_capacity(n);
        for r in body[12..].chunks_exact(REC_LEN) {
            let id: [u8; 32] = r[1..33].try_into().map_err(|_| bad("idx id"))?;
            let off = u64::from_le_bytes(r[33..41].try_into().map_err(|_| bad("idx offset"))?);
            let len = u32::from_le_bytes(r[41..45].try_into().map_err(|_| bad("idx len"))?);
            recs.push(((r[0], id), off, len));
        }
        if recs.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(bad("idx is not sorted"));
        }
        Ok(Self { recs })
    }
}

/// Checks magic, length and trailer; returns the part before the trailer.
fn checked<'a>(b: &'a [u8], magic: &[u8; 8]) -> Result<&'a [u8]> {
    if b.len() < 12 + 32 || &b[..8] != magic {
        return Err(bad("sidecar header"));
    }
    let end = b.len() - 32;
    if blake3::hash(&b[..end]).as_bytes() != &b[end..] {
        return Err(bad("sidecar checksum"));
    }
    Ok(&b[..end])
}

fn encode_del(keys: &[RowKey]) -> Bytes {
    let mut out = BytesMut::new();
    out.put_slice(DEL_MAGIC);
    out.put_u32_le(keys.len() as u32);
    for k in keys {
        out.put_u8(k.0);
        out.put_slice(&k.1);
    }
    let sum = blake3::hash(&out);
    out.put_slice(sum.as_bytes());
    out.freeze()
}

fn decode_del(b: &[u8]) -> Result<Vec<RowKey>> {
    let body = checked(b, DEL_MAGIC)?;
    let n = u32::from_le_bytes(body[8..12].try_into().map_err(|_| bad("del header"))?) as usize;
    if body.len() != 12 + n * DEL_LEN {
        return Err(bad("del length"));
    }
    body[12..]
        .chunks_exact(DEL_LEN)
        .map(|r| Ok((r[0], r[1..].try_into().map_err(|_| bad("del id"))?)))
        .collect()
}

#[derive(Default)]
struct Cache {
    manifest: Option<Loaded>,
    idx: HashMap<PackId, Arc<Idx>>,
    del: HashMap<PackId, Arc<Vec<RowKey>>>,
}

/// The index in a bucket namespace.
pub struct BucketPackIndex {
    store: Arc<dyn ObjectStore>,
    /// The namespace root; packs live under `<root>/pack/`.
    root: Path,
    cache: Mutex<Cache>,
}

impl BucketPackIndex {
    pub fn new(store: Arc<dyn ObjectStore>, root: Path) -> Self {
        Self {
            store,
            root,
            cache: Mutex::new(Cache::default()),
        }
    }

    fn at(&self, rel: &str) -> Path {
        if self.root.as_ref().is_empty() {
            Path::from(rel)
        } else {
            Path::from(format!("{}/{rel}", self.root))
        }
    }

    fn manifest_path(&self) -> Path {
        self.at("pack/MANIFEST")
    }

    fn idx_path(&self, id: PackId) -> Path {
        self.at(&format!("pack/{id}.idx"))
    }

    fn del_path(&self, id: PackId) -> Path {
        self.at(&format!("pack/{id}.del"))
    }

    async fn read(&self, path: &Path) -> Result<Option<(Bytes, UpdateVersion)>> {
        match self.store.get(path).await {
            Ok(got) => {
                let version = UpdateVersion {
                    e_tag: got.meta.e_tag.clone(),
                    version: got.meta.version.clone(),
                };
                let body = crate::bucket::read_capped(got, MAX_SIDE_BYTES)
                    .await?
                    .ok_or_else(|| bad(format!("{path} is too large")))?;
                Ok(Some((body, version)))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The manifest, from the cache while fresh.
    async fn manifest(&self, c: &mut Cache, force: bool) -> Result<Vec<PackInfo>> {
        if !force
            && let Some(m) = &c.manifest
            && m.at.elapsed() < MANIFEST_TTL
        {
            return Ok(m.infos.clone());
        }
        let loaded = match self.read(&self.manifest_path()).await? {
            None => Loaded {
                infos: Vec::new(),
                version: None,
                at: Instant::now(),
            },
            Some((body, version)) => {
                let m: Manifest = serde_json::from_slice(&body).map_err(bad)?;
                let infos = m.packs.iter().filter_map(ManifestPack::info).collect();
                Loaded {
                    infos,
                    version: Some(version),
                    at: Instant::now(),
                }
            }
        };
        let infos = loaded.infos.clone();
        c.manifest = Some(loaded);
        // Deletions by other servers are in the sidecars; read them again with the manifest.
        c.del.clear();
        Ok(infos)
    }

    /// Applies `edit` to the manifest and writes it back, retrying when another writer won.
    async fn update_manifest(
        &self,
        c: &mut Cache,
        mut edit: impl FnMut(&mut Vec<PackInfo>),
    ) -> Result<()> {
        for _ in 0..CAS_TRIES {
            let mut infos = self.manifest(c, true).await?;
            edit(&mut infos);
            let body = serde_json::to_vec(&Manifest {
                version: 1,
                packs: infos.iter().map(ManifestPack::of).collect(),
            })
            .map_err(bad)?;
            let path = self.manifest_path();
            let held = c.manifest.as_ref().and_then(|m| m.version.clone());
            let mode = match held {
                Some(v) if v.e_tag.is_some() || v.version.is_some() => PutMode::Update(v),
                Some(_) => PutMode::Overwrite,
                None => PutMode::Create,
            };
            let opts = PutOptions {
                mode,
                ..PutOptions::default()
            };
            let payload = Bytes::from(body);
            let mut put = self
                .store
                .put_opts(&path, PutPayload::from_bytes(payload.clone()), opts)
                .await;
            if matches!(put, Err(object_store::Error::NotImplemented { .. })) {
                // A store without conditional writes (a local directory) gets a plain write;
                // the single writer GC and the repack job already are keeps it safe.
                let plain = PutOptions {
                    mode: PutMode::Overwrite,
                    ..PutOptions::default()
                };
                put = self
                    .store
                    .put_opts(&path, PutPayload::from_bytes(payload), plain)
                    .await;
            }
            match put {
                Ok(_) => {
                    c.manifest = None;
                    self.manifest(c, true).await?;
                    return Ok(());
                }
                Err(
                    object_store::Error::Precondition { .. }
                    | object_store::Error::AlreadyExists { .. },
                ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(bad("the pack manifest kept changing under us"))
    }

    async fn idx(&self, c: &mut Cache, id: PackId) -> Result<Option<Arc<Idx>>> {
        if let Some(i) = c.idx.get(&id) {
            return Ok(Some(i.clone()));
        }
        let Some((body, _)) = self.read(&self.idx_path(id)).await? else {
            return Ok(None);
        };
        let i = Arc::new(Idx::decode(&body)?);
        c.idx.insert(id, i.clone());
        Ok(Some(i))
    }

    async fn dead(&self, c: &mut Cache, id: PackId) -> Result<Arc<Vec<RowKey>>> {
        if let Some(d) = c.del.get(&id) {
            return Ok(d.clone());
        }
        let keys = match self.read(&self.del_path(id)).await? {
            Some((body, _)) => decode_del(&body)?,
            None => Vec::new(),
        };
        let d = Arc::new(keys);
        c.del.insert(id, d.clone());
        Ok(d)
    }

    /// Live packs, oldest first (the lookup order).
    fn live_oldest_first(infos: &[PackInfo]) -> Vec<PackInfo> {
        let mut live: Vec<PackInfo> = infos
            .iter()
            .filter(|p| p.state == PackState::Live)
            .cloned()
            .collect();
        live.sort_by_key(|p| (p.created, p.id));
        live
    }

    /// The first live pack that holds `k` and has not deleted it.
    async fn resolve(
        &self,
        c: &mut Cache,
        live: &[PackInfo],
        k: &RowKey,
    ) -> Result<Option<(PackId, u64, u32)>> {
        for p in live {
            let Some(idx) = self.idx(c, p.id).await? else {
                continue;
            };
            if let Some((off, len)) = idx.find(k)
                && self.dead(c, p.id).await?.binary_search(k).is_err()
            {
                return Ok(Some((p.id, off, len)));
            }
        }
        Ok(None)
    }
}

fn row_key(k: &ObjKey) -> RowKey {
    (k.class().code(), k.id32())
}

fn entry_of(k: &RowKey, off: u64, len: u32) -> Option<PackEntry> {
    Some(PackEntry {
        key: ObjKey::from_pack(KeyClass::from_code(k.0)?, k.1)?,
        offset: off,
        len,
    })
}

#[async_trait]
impl PackIndex for BucketPackIndex {
    async fn any_live(&self) -> Result<bool> {
        let mut c = self.cache.lock().await;
        let infos = self.manifest(&mut c, false).await?;
        Ok(infos.iter().any(|p| p.state == PackState::Live))
    }

    fn is_cached(&self) -> bool {
        true
    }

    async fn refresh(&self) -> Result<()> {
        let mut c = self.cache.lock().await;
        self.manifest(&mut c, true).await.map(|_| ())
    }

    async fn locate(&self, keys: &[ObjKey]) -> Result<Vec<Option<PackLoc>>> {
        let mut c = self.cache.lock().await;
        let live = Self::live_oldest_first(&self.manifest(&mut c, false).await?);
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            out.push(
                self.resolve(&mut c, &live, &row_key(k))
                    .await?
                    .map(|(pack, offset, len)| PackLoc {
                        pack,
                        offset,
                        len: u64::from(len),
                    }),
            );
        }
        Ok(out)
    }

    async fn commit(
        &self,
        new: &PackInfo,
        entries: &[PackEntry],
        replaces: &[PackId],
    ) -> Result<u64> {
        let mut c = self.cache.lock().await;
        let infos = self.manifest(&mut c, true).await?;
        let live = Self::live_oldest_first(&infos);
        // The entries that get a row: with no `replaces` those no live pack holds; with
        // `replaces` those a replaced pack holds and has not deleted.
        let mut rows = Vec::with_capacity(entries.len());
        let mut kept: Vec<PackEntry> = Vec::new();
        for e in entries {
            let k = row_key(&e.key);
            let here = self.resolve(&mut c, &live, &k).await?;
            match here {
                Some((p, ..)) if replaces.contains(&p) => rows.push(*e),
                None if replaces.is_empty() => rows.push(*e),
                // Another pack's copy is the row; this copy stays unused (no replaces) or is
                // dropped (a rewrite).
                Some(_) if replaces.is_empty() => kept.push(*e),
                _ => {}
            }
        }
        // Unreferenced duplicate copies are listed too, so the idx covers the whole pack file
        // and a rebuild from the packs matches.
        let mut all = rows.clone();
        all.extend(kept);
        let body = Idx::encode(&all);
        self.store
            .put(&self.idx_path(new.id), PutPayload::from_bytes(body))
            .await?;
        let (new, replaces) = (new.clone(), replaces.to_vec());
        self.update_manifest(&mut c, move |infos| {
            infos.retain(|p| p.id != new.id);
            for p in infos.iter_mut() {
                if replaces.contains(&p.id) {
                    p.state = PackState::Retired;
                    p.retired = new.created;
                }
            }
            infos.push(new.clone());
        })
        .await?;
        Ok(rows.len() as u64)
    }

    async fn retire(&self, ids: &[PackId], now: i64) -> Result<()> {
        let mut c = self.cache.lock().await;
        let ids = ids.to_vec();
        self.update_manifest(&mut c, move |infos| {
            for p in infos.iter_mut() {
                if ids.contains(&p.id) {
                    p.state = PackState::Retired;
                    p.retired = now;
                }
            }
        })
        .await
    }

    async fn forget(&self, keys: &[ObjKey]) -> Result<u64> {
        let mut c = self.cache.lock().await;
        let live = Self::live_oldest_first(&self.manifest(&mut c, false).await?);
        let mut touched: HashMap<PackId, Vec<RowKey>> = HashMap::new();
        let mut gone = 0;
        for k in keys {
            let rk = row_key(k);
            let mut had = false;
            for p in &live {
                let Some(idx) = self.idx(&mut c, p.id).await? else {
                    continue;
                };
                if idx.find(&rk).is_some()
                    && self.dead(&mut c, p.id).await?.binary_search(&rk).is_err()
                {
                    touched.entry(p.id).or_default().push(rk);
                    had = true;
                }
            }
            gone += u64::from(had);
        }
        for (id, add) in touched {
            let mut all: Vec<RowKey> = self.dead(&mut c, id).await?.to_vec();
            all.extend(add);
            all.sort();
            all.dedup();
            self.store
                .put(&self.del_path(id), PutPayload::from_bytes(encode_del(&all)))
                .await?;
            c.del.insert(id, Arc::new(all));
        }
        Ok(gone)
    }

    async fn packs(&self) -> Result<Vec<PackDetail>> {
        let mut c = self.cache.lock().await;
        let infos = self.manifest(&mut c, false).await?;
        let live = Self::live_oldest_first(&infos);
        let mut out = Vec::with_capacity(infos.len());
        for info in infos {
            let (mut objects, mut bytes) = (0u64, 0u64);
            if let Some(idx) = self.idx(&mut c, info.id).await? {
                let dead = self.dead(&mut c, info.id).await?;
                for (k, _, len) in &idx.recs {
                    if dead.binary_search(k).is_ok() {
                        continue;
                    }
                    let owner = self.resolve(&mut c, &live, k).await?;
                    if info.state == PackState::Live && owner.map(|o| o.0) == Some(info.id) {
                        objects += 1;
                        bytes += u64::from(*len) + super::format::ENTRY_HEADER_LEN as u64;
                    }
                }
            }
            out.push(PackDetail {
                info,
                live_objects: objects,
                live_bytes: bytes,
            });
        }
        Ok(out)
    }

    async fn rows_of(&self, id: PackId) -> Result<Vec<PackEntry>> {
        let mut c = self.cache.lock().await;
        let infos = self.manifest(&mut c, false).await?;
        let live = Self::live_oldest_first(&infos);
        let Some(idx) = self.idx(&mut c, id).await? else {
            return Ok(Vec::new());
        };
        let dead = self.dead(&mut c, id).await?;
        let mut out = Vec::new();
        for (k, off, len) in &idx.recs {
            if dead.binary_search(k).is_ok() {
                continue;
            }
            if self.resolve(&mut c, &live, k).await?.map(|o| o.0) == Some(id)
                && let Some(e) = entry_of(k, *off, *len)
            {
                out.push(e);
            }
        }
        Ok(out)
    }

    async fn remove(&self, id: PackId) -> Result<()> {
        let mut c = self.cache.lock().await;
        self.update_manifest(&mut c, move |infos| infos.retain(|p| p.id != id))
            .await?;
        for path in [self.idx_path(id), self.del_path(id)] {
            match self.store.delete(&path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        c.idx.remove(&id);
        c.del.remove(&id);
        Ok(())
    }

    async fn list(
        &self,
        class: Option<KeyClass>,
        after: Option<(u8, [u8; 32])>,
        limit: usize,
    ) -> Result<Vec<(ObjKey, i64)>> {
        let mut c = self.cache.lock().await;
        let live = Self::live_oldest_first(&self.manifest(&mut c, false).await?);
        let mut found: std::collections::BTreeMap<RowKey, i64> = std::collections::BTreeMap::new();
        for p in &live {
            let Some(idx) = self.idx(&mut c, p.id).await? else {
                continue;
            };
            let dead = self.dead(&mut c, p.id).await?;
            let start = after.map_or(0, |a| idx.recs.partition_point(|r| r.0 <= a));
            let mut taken = 0;
            for (k, ..) in &idx.recs[start..] {
                if taken >= limit {
                    break;
                }
                if class.is_some_and(|want| want.code() != k.0) || dead.binary_search(k).is_ok() {
                    continue;
                }
                found.entry(*k).or_insert(p.created);
                taken += 1;
            }
        }
        Ok(found
            .into_iter()
            .take(limit)
            .filter_map(|(k, created)| {
                Some((ObjKey::from_pack(KeyClass::from_code(k.0)?, k.1)?, created))
            })
            .collect())
    }

    async fn rebuild(&self, packs: &[(PackInfo, Vec<PackEntry>)]) -> Result<()> {
        let mut c = self.cache.lock().await;
        for (info, entries) in packs {
            self.store
                .put(
                    &self.idx_path(info.id),
                    PutPayload::from_bytes(Idx::encode(entries)),
                )
                .await?;
        }
        let infos: Vec<PackInfo> = packs.iter().map(|(i, _)| i.clone()).collect();
        self.update_manifest(&mut c, move |m| *m = infos.clone())
            .await?;
        c.idx.clear();
        c.del.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use object_store::memory::InMemory;

    use super::*;
    use crate::pack::index::run_index_suite;

    #[tokio::test]
    async fn the_bucket_index_conforms() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        run_index_suite(&BucketPackIndex::new(store, Path::from("ns/0001"))).await;
    }

    #[tokio::test]
    async fn the_bucket_index_conforms_on_local_files() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> =
            Arc::new(object_store::local::LocalFileSystem::new_with_prefix(dir.path()).unwrap());
        run_index_suite(&BucketPackIndex::new(store, Path::default())).await;
    }

    #[tokio::test]
    async fn a_second_reader_sees_what_the_first_committed_after_a_refresh() {
        use asc_core::id::FileId;
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let a = BucketPackIndex::new(store.clone(), Path::default());
        let b = BucketPackIndex::new(store, Path::default());
        assert!(!b.any_live().await.unwrap());
        let k = ObjKey::File(FileId::of(b"x"));
        let id = PackId::random();
        let info = PackInfo {
            id,
            objects: 1,
            bytes: 100,
            created: 5,
            state: PackState::Live,
            retired: 0,
        };
        a.commit(
            &info,
            &[PackEntry {
                key: k,
                offset: 53,
                len: 4,
            }],
            &[],
        )
        .await
        .unwrap();
        assert!(
            !b.any_live().await.unwrap(),
            "the cached manifest is trusted briefly"
        );
        b.refresh().await.unwrap();
        assert_eq!(b.locate(&[k]).await.unwrap()[0].unwrap().pack, id);
        // A delete by one is seen by the other once it reloads the sidecar.
        assert_eq!(a.forget(&[k]).await.unwrap(), 1);
        let c = BucketPackIndex::new(a.store.clone(), Path::default());
        assert_eq!(c.locate(&[k]).await.unwrap(), [None]);
    }

    #[tokio::test]
    async fn concurrent_commits_both_land() {
        use asc_core::id::FileId;
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let a = Arc::new(BucketPackIndex::new(store.clone(), Path::default()));
        let b = Arc::new(BucketPackIndex::new(store, Path::default()));
        let mk = |n: u8| {
            let id = PackId::random();
            (
                PackInfo {
                    id,
                    objects: 1,
                    bytes: 90,
                    created: i64::from(n),
                    state: PackState::Live,
                    retired: 0,
                },
                [PackEntry {
                    key: ObjKey::File(FileId::of(&[n])),
                    offset: 53,
                    len: 4,
                }],
            )
        };
        let mut tasks = Vec::new();
        for n in 0..8u8 {
            let ix = if n % 2 == 0 { a.clone() } else { b.clone() };
            let (info, entries) = mk(n);
            tasks.push(tokio::spawn(async move {
                ix.commit(&info, &entries, &[]).await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        a.refresh().await.unwrap();
        assert_eq!(a.packs().await.unwrap().len(), 8);
    }
}
