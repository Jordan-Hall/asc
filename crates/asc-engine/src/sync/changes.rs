//! Transport of change metadata (SPEC §20.2).
//!
//! Push: [`ChangesExt`] adds the `m/` object of every new changeset to the push closure, but
//! only after the server advertised `changes.v1`, so an old reader never meets the new key
//! kind. Fetch: `LogEntry.change` and `LogEntry.meta` arrive as [`LogMeta`]; [`ChangesExt::apply`]
//! fetches each claimed `m/` object, verifies it against the claim and indexes it. Nothing a
//! remote sends is believed before its hash and its contents agree with the entry.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use asc_core::change::{ChangeId, ChangeMeta, MetaId};
use asc_core::codec::{CodecError, decode_envelope};
use asc_core::id::ChangesetId;
use asc_store::key::ObjKey;
use asc_store::objects::{self, decode_verified};
use futures::StreamExt;

use super::{Remote, SyncExt};
use crate::change::{self, ChangeRow, ChangeRows, MAX_PREDS};

/// The `Version.features` entry that allows uploading `m/` objects.
pub const FEATURE: &str = "changes.v1";

/// Most entries held for one fetch.
pub const MAX_PENDING: usize = 1_000_000;
/// Keys per `get_many` request.
pub const FETCH_BATCH: usize = 256;

/// The change fields of one log entry (engine-native; the proto conversion is C17x).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogMeta {
    pub id: ChangesetId,
    pub change: Option<ChangeId>,
    pub meta: Option<MetaId>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Rows(#[from] change::Error),
    #[error(transparent)]
    Sync(#[from] super::Error),
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error("change metadata {0} does not decode: {1}")]
    Codec(MetaId, CodecError),
    #[error("change metadata {0} contradicts the log entry for {1}")]
    Mismatch(MetaId, ChangesetId),
    #[error("change metadata {0} lists {1} predecessors, over the limit of {MAX_PREDS}")]
    TooManyPreds(MetaId, usize),
    #[error("more than {MAX_PENDING} log entries carry change metadata")]
    TooMany,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Implements [`SyncExt`] over a [`ChangeRows`] index.
pub struct ChangesExt<R: ChangeRows> {
    rows: R,
    /// The server advertised [`FEATURE`] (or the remote is a bucket).
    advertised: bool,
    /// Meta ids of changesets [`warm`](Self::warm) loaded, for the synchronous hook.
    known: Mutex<BTreeMap<ChangesetId, MetaId>>,
    pending: Mutex<Vec<LogMeta>>,
}

impl<R: ChangeRows> ChangesExt<R> {
    /// `features` is `Version.features` of the remote.
    pub fn new<S: AsRef<str>>(rows: R, features: &[S]) -> Self {
        Self::with_advertised(rows, features.iter().any(|f| f.as_ref() == FEATURE))
    }

    pub fn with_advertised(rows: R, advertised: bool) -> Self {
        Self {
            rows,
            advertised,
            known: Mutex::default(),
            pending: Mutex::default(),
        }
    }

    pub fn advertised(&self) -> bool {
        self.advertised
    }

    /// Loads the meta ids of `candidates`; call it before a push with every changeset the push
    /// may send (the hook itself is synchronous). A no-op when `changes.v1` is not advertised.
    pub async fn warm(&self, candidates: &[ChangesetId]) -> Result<()> {
        if !self.advertised {
            return Ok(());
        }
        for &cs in candidates {
            if let Some(row) = self.rows.get(cs).await? {
                self.known
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(cs, row.meta);
            }
        }
        Ok(())
    }

    /// Takes note of the change fields of fetched log entries; entries without both a change
    /// and a meta are ignored. Nothing is trusted until [`apply`](Self::apply).
    pub fn note(&self, entries: &[LogMeta]) -> Result<()> {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for e in entries {
            if e.change.is_some() && e.meta.is_some() {
                if p.len() >= MAX_PENDING {
                    return Err(Error::TooMany);
                }
                p.push(*e);
            }
        }
        Ok(())
    }

    /// Fetches, verifies and indexes the metas noted so far. Returns how many rows were new.
    /// A meta that fails verification fails the call and indexes nothing from its batch.
    ///
    /// `fetched` is the set of changesets this fetch received and verified. An entry for any
    /// other changeset is ignored, so a remote cannot attach a change to a local commit (the
    /// client half of the §20.2 ingest rule).
    pub async fn apply(
        &self,
        remote: &dyn Remote,
        fetched: &HashSet<ChangesetId>,
    ) -> Result<usize> {
        let noted = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        let mut seen = HashSet::new();
        let mut todo = Vec::new();
        for e in noted {
            if fetched.contains(&e.id) && self.rows.get(e.id).await?.is_none() && seen.insert(e.id)
            {
                todo.push(e);
            }
        }
        let mut added = 0;
        for batch in todo.chunks(FETCH_BATCH) {
            let claims: BTreeMap<MetaId, &LogMeta> = batch
                .iter()
                .filter_map(|e| e.meta.map(|m| (m, e)))
                .collect();
            let keys: Vec<ObjKey> = claims.keys().map(|&m| ObjKey::ChangeMeta(m)).collect();
            let mut got = Vec::new();
            let mut stream = remote.get_many(keys);
            while let Some(item) = stream.next().await {
                let (key, stored) = item?;
                let ObjKey::ChangeMeta(id) = key else {
                    continue;
                };
                let Some(claim) = claims.get(&id) else {
                    continue; // not asked for
                };
                let data = decode_verified(&key, &stored)?;
                let meta = decode_envelope::<ChangeMeta>(&data).map_err(|e| Error::Codec(id, e))?;
                if meta.cs != claim.id || Some(meta.change) != claim.change {
                    return Err(Error::Mismatch(id, claim.id));
                }
                if meta.preds.len() > MAX_PREDS {
                    return Err(Error::TooManyPreds(id, meta.preds.len()));
                }
                got.push(meta);
            }
            for meta in got {
                let row = ChangeRow {
                    cs: meta.cs,
                    change: meta.change,
                    meta: meta.id(),
                    op: meta.op,
                };
                if self.rows.insert(&row, &meta.preds).await? {
                    added += 1;
                }
            }
        }
        Ok(added)
    }
}

impl<R: ChangeRows> SyncExt for ChangesExt<R> {
    fn extra_closure(&self, new: &[ChangesetId]) -> Vec<ObjKey> {
        if !self.advertised {
            return Vec::new();
        }
        let known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        new.iter()
            .filter_map(|cs| known.get(cs).map(|&m| ObjKey::ChangeMeta(m)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use asc_core::change::RewriteOp;
    use asc_core::codec::Enveloped;
    use asc_store::codec::encode_stored;
    use asc_store::refs::{RefEntry, RefUpdate};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::{self, BoxStream};

    use super::*;
    use crate::change::MemRows;
    use crate::sync::Result as SyncResult;

    fn cs(n: u8) -> ChangesetId {
        ChangesetId::of(&[n])
    }

    fn meta(n: u8, change: u8, preds: Vec<ChangesetId>) -> ChangeMeta {
        ChangeMeta::new(
            cs(n),
            ChangeId::from_bytes([change; 16]),
            preds,
            RewriteOp::Amend,
        )
    }

    fn stored(m: &ChangeMeta) -> (ObjKey, Bytes) {
        (ObjKey::ChangeMeta(m.id()), encode_stored(&m.to_envelope()))
    }

    #[derive(Default)]
    struct Mem(Mutex<HashMap<ObjKey, Bytes>>);

    #[async_trait]
    impl Remote for Mem {
        fn name(&self) -> &str {
            "mem"
        }
        async fn list_refs(&self) -> SyncResult<Vec<RefEntry>> {
            Ok(Vec::new())
        }
        async fn has_many(&self, keys: &[ObjKey]) -> SyncResult<Vec<bool>> {
            let g = self.0.lock().unwrap();
            Ok(keys.iter().map(|k| g.contains_key(k)).collect())
        }
        fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, SyncResult<(ObjKey, Bytes)>> {
            let g = self.0.lock().unwrap();
            let out: Vec<_> = keys
                .into_iter()
                .filter_map(|k| g.get(&k).map(|b| Ok((k, b.clone()))))
                .collect();
            Box::pin(stream::iter(out))
        }
        async fn put_many(&self, objects: Vec<(ObjKey, Bytes)>) -> SyncResult<()> {
            self.0.lock().unwrap().extend(objects);
            Ok(())
        }
        async fn update_refs(&self, _: &[RefUpdate]) -> SyncResult<()> {
            Ok(())
        }
    }

    async fn indexed(rows: &MemRows, m: &ChangeMeta) {
        let row = ChangeRow {
            cs: m.cs,
            change: m.change,
            meta: m.id(),
            op: m.op,
        };
        rows.insert(&row, &m.preds).await.unwrap();
    }

    #[tokio::test]
    async fn push_closure_holds_metas_only_after_the_feature_is_advertised() {
        let m = meta(1, 7, vec![cs(9)]);
        for (features, want) in [(vec![], 0), (vec!["changes.v1"], 1)] {
            let rows = MemRows::default();
            indexed(&rows, &m).await;
            let ext = ChangesExt::new(rows, &features);
            ext.warm(&[cs(1), cs(2)]).await.unwrap();
            let extra = ext.extra_closure(&[cs(1), cs(2)]);
            assert_eq!(extra.len(), want);
            if want == 1 {
                assert_eq!(extra, vec![ObjKey::ChangeMeta(m.id())]);
            }
        }
    }

    fn ids(ns: &[u8]) -> HashSet<ChangesetId> {
        ns.iter().map(|&n| cs(n)).collect()
    }

    #[tokio::test]
    async fn an_entry_for_a_changeset_not_fetched_indexes_nothing() {
        let m = meta(1, 7, vec![]);
        let remote = Mem::default();
        remote.put_many(vec![stored(&m)]).await.unwrap();
        let ext = ChangesExt::with_advertised(MemRows::default(), true);
        ext.note(&[LogMeta {
            id: cs(1),
            change: Some(m.change),
            meta: Some(m.id()),
        }])
        .unwrap();
        assert_eq!(ext.apply(&remote, &ids(&[2])).await.unwrap(), 0);
        assert!(ext.rows.get(cs(1)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fetch_indexes_verified_metas_and_skips_known_ones() {
        let m = meta(1, 7, vec![cs(9)]);
        let remote = Mem::default();
        remote.put_many(vec![stored(&m)]).await.unwrap();
        let ext = ChangesExt::with_advertised(MemRows::default(), true);
        let entry = LogMeta {
            id: cs(1),
            change: Some(m.change),
            meta: Some(m.id()),
        };
        let bare = LogMeta {
            id: cs(2),
            change: None,
            meta: None,
        };
        ext.note(&[entry, bare]).unwrap();
        assert_eq!(ext.apply(&remote, &ids(&[1, 2])).await.unwrap(), 1);
        let row = ext.rows.get(cs(1)).await.unwrap().unwrap();
        assert_eq!((row.change, row.meta), (m.change, m.id()));
        assert_eq!(ext.rows.preds(cs(1)).await.unwrap(), vec![cs(9)]);
        assert!(ext.rows.get(cs(2)).await.unwrap().is_none());
        ext.note(&[entry]).unwrap();
        assert_eq!(ext.apply(&remote, &ids(&[1, 2])).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn fetch_rejects_a_meta_that_contradicts_the_entry() {
        let m = meta(1, 7, vec![]);
        let remote = Mem::default();
        remote.put_many(vec![stored(&m)]).await.unwrap();
        for (id, change) in [(cs(2), m.change), (cs(1), ChangeId::from_bytes([8; 16]))] {
            let ext = ChangesExt::with_advertised(MemRows::default(), true);
            ext.note(&[LogMeta {
                id,
                change: Some(change),
                meta: Some(m.id()),
            }])
            .unwrap();
            let err = ext.apply(&remote, &ids(&[1, 2])).await.unwrap_err();
            assert!(matches!(err, Error::Mismatch(..)), "{err}");
            assert!(ext.rows.get(id).await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn fetch_rejects_corrupt_bytes_under_a_claimed_id() {
        let m = meta(1, 7, vec![]);
        let other = meta(1, 8, vec![]);
        let remote = Mem::default();
        remote
            .put_many(vec![(
                ObjKey::ChangeMeta(m.id()),
                encode_stored(&other.to_envelope()),
            )])
            .await
            .unwrap();
        let ext = ChangesExt::with_advertised(MemRows::default(), true);
        ext.note(&[LogMeta {
            id: cs(1),
            change: Some(m.change),
            meta: Some(m.id()),
        }])
        .unwrap();
        let err = ext.apply(&remote, &ids(&[1, 2])).await.unwrap_err();
        assert!(matches!(err, Error::Store(_)), "{err}");
        assert!(ext.rows.get(cs(1)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_claimed_meta_the_remote_lacks_indexes_nothing() {
        let ext = ChangesExt::with_advertised(MemRows::default(), true);
        ext.note(&[LogMeta {
            id: cs(1),
            change: Some(ChangeId::from_bytes([1; 16])),
            meta: Some(MetaId::from_bytes([3; 32])),
        }])
        .unwrap();
        assert_eq!(ext.apply(&Mem::default(), &ids(&[1])).await.unwrap(), 0);
    }
}
