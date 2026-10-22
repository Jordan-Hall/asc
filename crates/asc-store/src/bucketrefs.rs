//! `BucketRefs`: shared refs inside a bucket, using create-only puts (SPEC §3.4).
//!
//! The ref log is the truth: `rl/<ref>/<v:020>` holds one [`BucketRefEntry`] per change, and the
//! current value is the entry with the highest `v`. A change creates `v+1` with `PutMode::Create`,
//! so one writer wins each version. `r/<ref>` is a hint for the latest `v`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::id::RefTarget;
use asc_core::path::RefName;
use futures::{StreamExt, TryStreamExt, stream};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::refs::{
    Actor, Expect, RefConflict, RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey, Result,
};

/// Version byte of a stored [`BucketRefEntry`] (SPEC §2.2a).
const ENTRY_VERSION: u8 = 1;
/// Largest entry read or written.
const MAX_ENTRY: u64 = 4096;
const MAX_ACTOR: usize = 1024;
/// Longest percent-encoded ref segment; file systems cap names at 255 bytes.
const MAX_SEGMENT: usize = 255;
/// Ref log versions beyond this are treated as corrupt (bounds the forward probe).
const MAX_VERSION: u64 = 1 << 40;
const MAX_REFS: usize = 100_000;
const MAX_LOG: u32 = 10_000;
const MAX_ATTEMPTS: usize = 16;
const READ_CONCURRENCY: usize = 8;
/// The longest `gc/freeze` body read.
const MAX_FREEZE: u64 = 256;

#[derive(Debug, thiserror::Error)]
pub enum BucketRefsError {
    /// The entry was written by a newer `asc`; nothing is written.
    #[error("upgrade asc: ref entry version {0} is not supported")]
    UpgradeAsc(u8),
    #[error("corrupt bucket ref entry: {0}")]
    Corrupt(String),
    #[error("bucket is being garbage-collected; retry after {since}")]
    Frozen { since: String },
    #[error("bucket refs: {0}")]
    Unsupported(&'static str),
    #[error("bucket refs: {0}")]
    TooLong(&'static str),
    #[error("bucket refs need conditional writes (exclusive create); use SQL refs: {0}")]
    NoExclusiveCreate(String),
    #[error("bucket refs: gave up after {MAX_ATTEMPTS} attempts")]
    Exhausted,
}

impl From<BucketRefsError> for RefError {
    fn from(e: BucketRefsError) -> Self {
        Self::Backend(Box::new(e))
    }
}

fn store_err(e: object_store::Error) -> RefError {
    RefError::Backend(Box::new(e))
}

/// One ref change in the log (SPEC §3.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketRefEntry {
    pub old: Option<RefTarget>,
    /// `None` deletes the ref.
    pub new: Option<RefTarget>,
    pub op_id: [u8; 16],
    /// `id ‖ 0x00 ‖ via`.
    pub actor: Vec<u8>,
    /// Unix seconds.
    pub at: i64,
}

impl BucketRefEntry {
    /// `version ‖ postcard(self)`.
    pub fn encode(&self) -> Result<Bytes, BucketRefsError> {
        if self.actor.len() > MAX_ACTOR {
            return Err(BucketRefsError::TooLong("actor is too long"));
        }
        let mut out = vec![ENTRY_VERSION];
        let body =
            postcard::to_stdvec(self).map_err(|e| BucketRefsError::Corrupt(e.to_string()))?;
        out.extend_from_slice(&body);
        Ok(out.into())
    }

    /// Rejects unknown versions with `upgrade asc`, and trailing or oversized data.
    pub fn decode(bytes: &[u8]) -> Result<Self, BucketRefsError> {
        let Some((&version, body)) = bytes.split_first() else {
            return Err(BucketRefsError::Corrupt("empty entry".into()));
        };
        if version != ENTRY_VERSION {
            return Err(BucketRefsError::UpgradeAsc(version));
        }
        if bytes.len() as u64 > MAX_ENTRY {
            return Err(BucketRefsError::Corrupt("entry too large".into()));
        }
        let (entry, rest) = postcard::take_from_bytes::<Self>(body)
            .map_err(|e| BucketRefsError::Corrupt(e.to_string()))?;
        if !rest.is_empty() || entry.actor.len() > MAX_ACTOR {
            return Err(BucketRefsError::Corrupt("bad entry framing".into()));
        }
        Ok(entry)
    }
}

fn encode_actor(a: &Actor) -> Vec<u8> {
    let mut out = a.id.as_bytes().to_vec();
    out.push(0);
    out.extend_from_slice(a.via.as_bytes());
    out
}

fn decode_actor(b: &[u8]) -> Actor {
    let (id, via) = match b.iter().position(|&c| c == 0) {
        Some(i) => (&b[..i], &b[i + 1..]),
        None => (b, &b[..0]),
    };
    Actor {
        id: String::from_utf8_lossy(id).into_owned(),
        via: String::from_utf8_lossy(via).into_owned(),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Refs stored in a bucket namespace.
///
/// # Requirement (SPEC §3)
///
/// The store must support exclusive create (conditional writes): a second `PutMode::Create` of
/// one key must fail with `AlreadyExists`. Without it two writers both succeed and a ref update
/// is lost. The `Create` to overwrite fallback that is safe for content-addressed objects
/// (SPEC §3, the U29 put fallback) must therefore never apply here; [`Self::from_url_with`]
/// builds the store without it. [`Self::connect`] runs [`crate::probe::probe`] and refuses a
/// store that is not exclusive; [`Self::new`] defers the same probe to the first `cas`, which
/// fails closed. A `NotImplemented` store error at CAS time is a hard error. Stores without
/// conditional writes need SQL refs instead.
#[derive(Clone)]
pub struct BucketRefs {
    store: Arc<dyn ObjectStore>,
    root: Path,
    /// Set once the probe has shown the store is exclusive; failures are not cached.
    verified: Arc<tokio::sync::OnceCell<()>>,
}

impl BucketRefs {
    /// `root` is the namespace root (the bucket URL prefix).
    pub fn new(store: Arc<dyn ObjectStore>, root: Path) -> Self {
        Self {
            store,
            root,
            verified: Arc::default(),
        }
    }

    /// Like [`Self::new`], but probes the store now and refuses unless create is exclusive.
    pub async fn connect(store: Arc<dyn ObjectStore>, root: Path) -> Result<Self> {
        let this = Self::new(store, root);
        this.require_exclusive().await?;
        Ok(this)
    }

    /// Fails closed unless the store passed the probe with `create_exclusive`.
    async fn require_exclusive(&self) -> Result<()> {
        self.verified
            .get_or_try_init(|| async {
                let r = crate::probe::probe(&*self.store, &self.root).await;
                if r.create_exclusive {
                    Ok(())
                } else {
                    Err(RefError::from(BucketRefsError::NoExclusiveCreate(r.detail)))
                }
            })
            .await
            .map(|_| ())
    }

    /// Opens a bucket URL the way [`crate::bucket::BucketObjects::from_url_with`] does.
    pub fn from_url_with(
        url: &str,
        credentials: Option<crate::s3::S3Credentials>,
    ) -> Result<Self, crate::objects::Error> {
        let url = url::Url::parse(url).map_err(|e| crate::objects::Error::Url(e.to_string()))?;
        if url.scheme() == "s3" {
            let (store, root) = crate::s3::open_strict(&url, credentials)?;
            return Ok(Self::new(store, root));
        }
        let (store, root) = object_store::parse_url(&url)?;
        Ok(Self::new(Arc::from(store), root))
    }

    /// One path segment per ref: `/` is percent-encoded so refs `a` and `a/b` never collide.
    fn segment(&self, kind: &str, name: &str) -> Result<Path> {
        let p = self.root.clone().join(kind).join(name);
        let seg = p.filename().unwrap_or_default();
        if seg.len() > MAX_SEGMENT {
            return Err(BucketRefsError::TooLong("ref name is too long").into());
        }
        Ok(p)
    }

    fn entry_path(&self, name: &str, v: u64) -> Result<Path> {
        Ok(self.segment("rl", name)?.join(format!("{v:020}")))
    }

    fn hint_path(&self, name: &str) -> Result<Path> {
        self.segment("r", name)
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        match self.store.head(path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(store_err(e)),
        }
    }

    /// A bounded read; `None` if absent.
    async fn read_capped(&self, path: &Path, cap: u64) -> Result<Option<Bytes>> {
        let got = match self.store.get(path).await {
            Ok(g) => g,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(store_err(e)),
        };
        match crate::bucket::read_capped(got, cap)
            .await
            .map_err(store_err)?
        {
            Some(b) => Ok(Some(b)),
            None => Err(BucketRefsError::Corrupt(format!("{path} is too large")).into()),
        }
    }

    async fn read_entry(&self, name: &str, v: u64) -> Result<Option<BucketRefEntry>> {
        let path = self.entry_path(name, v)?;
        match self.read_capped(&path, MAX_ENTRY).await? {
            Some(b) => Ok(Some(BucketRefEntry::decode(&b)?)),
            None => Ok(None),
        }
    }

    async fn read_hint(&self, name: &str) -> Option<u64> {
        let path = self.hint_path(name).ok()?;
        let b = self.read_capped(&path, 32).await.ok()??;
        std::str::from_utf8(&b).ok()?.trim().parse().ok()
    }

    async fn write_hint(&self, name: &str, v: u64) {
        let Ok(path) = self.hint_path(name) else {
            return;
        };
        let _ = self
            .store
            .put(&path, PutPayload::from_bytes(Bytes::from(v.to_string())))
            .await;
    }

    /// The highest existing version and its entry. The hint is untrusted: it is checked, then the
    /// log is probed forward by doubling and bisected, so a stale hint costs `O(log n)`.
    async fn latest(&self, name: &str) -> Result<(u64, Option<BucketRefEntry>)> {
        let mut lo = 0;
        if let Some(h) = self
            .read_hint(name)
            .await
            .filter(|&h| (1..=MAX_VERSION).contains(&h))
            && self.exists(&self.entry_path(name, h)?).await?
        {
            lo = h;
        }
        let mut step = 1u64;
        let mut hi;
        loop {
            hi = lo + step;
            if hi > MAX_VERSION {
                return Err(BucketRefsError::Corrupt("ref log is too long".into()).into());
            }
            if !self.exists(&self.entry_path(name, hi)?).await? {
                break;
            }
            lo = hi;
            step *= 2;
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.exists(&self.entry_path(name, mid)?).await? {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return Ok((0, None));
        }
        match self.read_entry(name, lo).await? {
            Some(e) => Ok((lo, Some(e))),
            None => Err(BucketRefsError::Corrupt(format!("entry {lo} vanished")).into()),
        }
    }

    /// The GC freeze check, immediately before a CAS (SPEC §3.6).
    async fn check_freeze(&self) -> Result<()> {
        let path = self.root.clone().join("gc").join("freeze");
        if let Some(b) = self.read_capped(&path, MAX_FREEZE).await? {
            let since = String::from_utf8_lossy(&b).trim().to_owned();
            return Err(BucketRefsError::Frozen { since }.into());
        }
        Ok(())
    }

    /// Compare-and-swap one ref; returns the version it now has.
    pub async fn cas(
        &self,
        name: &RefName,
        expect: Expect,
        new: Option<RefTarget>,
        actor: &Actor,
        op_id: Uuid,
    ) -> Result<u64> {
        let text = std::str::from_utf8(name.as_bytes())
            .map_err(|_| RefError::NonUtf8Name(name.clone()))?;
        let mine = *op_id.as_bytes();
        let actor = encode_actor(actor);
        self.require_exclusive().await?;
        for _ in 0..MAX_ATTEMPTS {
            let (v, latest) = self.latest(text).await?;
            let current = latest.as_ref().and_then(|e| e.new);
            // A retry of a change that already landed.
            if latest
                .as_ref()
                .is_some_and(|e| e.op_id == mine && e.new == new)
            {
                return Ok(v);
            }
            let holds = match expect {
                Expect::Any => true,
                Expect::Absent => current.is_none(),
                Expect::Value(t) => current == Some(t),
            };
            if !holds {
                return Err(RefConflict {
                    name: name.clone(),
                    actual: current,
                }
                .into());
            }
            let entry = BucketRefEntry {
                old: current,
                new,
                op_id: mine,
                actor: actor.clone(),
                at: now(),
            };
            let body = entry.encode()?;
            let path = self.entry_path(text, v + 1)?;
            self.check_freeze().await?;
            let opts = PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            };
            match self
                .store
                .put_opts(&path, PutPayload::from_bytes(body), opts)
                .await
            {
                Ok(_) => {
                    self.write_hint(text, v + 1).await;
                    return Ok(v + 1);
                }
                // Someone took `v+1`: success only if it was my earlier attempt.
                Err(object_store::Error::AlreadyExists { .. }) => {
                    if self.landed(text, v + 1, mine).await? {
                        self.write_hint(text, v + 1).await;
                        return Ok(v + 1);
                    }
                }
                // The response may have been lost after the store applied the put.
                Err(e) => {
                    if matches!(self.landed(text, v + 1, mine).await, Ok(true)) {
                        self.write_hint(text, v + 1).await;
                        return Ok(v + 1);
                    }
                    return Err(store_err(e));
                }
            }
        }
        Err(BucketRefsError::Exhausted.into())
    }

    async fn landed(&self, name: &str, v: u64, op_id: [u8; 16]) -> Result<bool> {
        Ok(self
            .read_entry(name, v)
            .await?
            .is_some_and(|e| e.op_id == op_id))
    }

    async fn resolve(&self, name: &RefName) -> Result<Option<RefEntry>> {
        let text = std::str::from_utf8(name.as_bytes())
            .map_err(|_| RefError::NonUtf8Name(name.clone()))?;
        let (version, latest) = self.latest(text).await?;
        Ok(latest.and_then(|e| e.new).map(|target| RefEntry {
            name: name.clone(),
            target,
            version,
            expires_at: None,
        }))
    }

    /// Ref names under `r/` (hints) and `rl/` (logs); the union also finds a ref whose hint write
    /// was lost. Foreign or malformed keys are skipped.
    async fn names(&self, prefix: &str) -> Result<BTreeSet<RefName>> {
        let mut names = BTreeSet::new();
        for kind in ["r", "rl"] {
            let dir = self.root.clone().join(kind);
            let listed = self
                .store
                .list_with_delimiter(Some(&dir))
                .await
                .map_err(store_err)?;
            let segments = listed
                .objects
                .iter()
                .map(|m| &m.location)
                .chain(listed.common_prefixes.iter());
            for loc in segments {
                let Some(raw) = loc.filename() else { continue };
                let Ok(decoded) = Path::from_url_path(raw) else {
                    continue;
                };
                let Ok(name) = RefName::new(decoded.as_ref().as_bytes()) else {
                    continue;
                };
                // Only canonical encodings: anything else is not ours.
                if !name.as_bytes().starts_with(prefix.as_bytes())
                    || Path::default().join(decoded.as_ref()).as_ref() != raw
                {
                    continue;
                }
                if names.len() >= MAX_REFS {
                    return Err(BucketRefsError::TooLong("too many refs").into());
                }
                names.insert(name);
            }
        }
        Ok(names)
    }
}

#[async_trait]
impl Refs for BucketRefs {
    async fn get(&self, _repo: RepoKey, name: &RefName) -> Result<Option<RefEntry>> {
        self.resolve(name).await
    }

    async fn list(&self, _repo: RepoKey, prefix: &str) -> Result<Vec<RefEntry>> {
        let names: Vec<_> = self.names(prefix).await?.into_iter().collect();
        let mut out: Vec<RefEntry> = stream::iter(names)
            .map(|n| async move { self.resolve(&n).await })
            .buffered(READ_CONCURRENCY)
            .try_filter_map(|e| async move { Ok(e) })
            .try_collect()
            .await?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Applies the updates in order. Atomicity covers one ref; if a later update fails, the
    /// earlier ones stay applied and the error names the failing ref.
    async fn update(&self, _repo: RepoKey, txn: &RefTxn) -> Result<()> {
        if txn.updates.iter().any(|u| u.expires_at.is_some()) {
            return Err(BucketRefsError::Unsupported("ref expiry needs an asc server").into());
        }
        for u in &txn.updates {
            self.cas(&u.name, u.expect, u.new, &txn.actor, txn.op_id)
                .await?;
        }
        Ok(())
    }

    /// Newest first. The reason is not stored in a bucket entry.
    async fn log(&self, _repo: RepoKey, name: &RefName, limit: u32) -> Result<Vec<RefLogEntry>> {
        let text = std::str::from_utf8(name.as_bytes())
            .map_err(|_| RefError::NonUtf8Name(name.clone()))?;
        let (top, _) = self.latest(text).await?;
        let n = u64::from(limit.min(MAX_LOG));
        let versions: Vec<u64> = (1..=top)
            .rev()
            .take(usize::try_from(n).unwrap_or(0))
            .collect();
        stream::iter(versions)
            .map(|v| async move {
                let e = self.read_entry(text, v).await?.ok_or_else(|| {
                    RefError::from(BucketRefsError::Corrupt(format!("entry {v} vanished")))
                })?;
                Ok(RefLogEntry {
                    seq: i64::try_from(v).unwrap_or(i64::MAX),
                    name: name.clone(),
                    old: e.old,
                    new: e.new,
                    actor: decode_actor(&e.actor),
                    reason: String::new(),
                    op_id: Uuid::from_bytes(e.op_id),
                    at: e.at,
                })
            })
            .buffered(READ_CONCURRENCY)
            .try_collect()
            .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fmt;
    use std::ops::Range;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use asc_core::id::ChangesetId;
    use futures::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
        PutMultipartOptions, PutResult, RenameOptions,
    };

    use super::*;

    /// An `InMemory` store that can ignore `Create` or lose the response of a put.
    #[derive(Default)]
    pub(crate) struct Wrap {
        inner: InMemory,
        ignore_create: AtomicBool,
        /// Puts under `rl/` that apply and then report a timeout.
        lose: AtomicUsize,
    }

    impl Wrap {
        pub(crate) fn new() -> Self {
            Self::default()
        }
        pub(crate) fn ignore_create(&self) {
            self.ignore_create.store(true, Ordering::SeqCst);
        }
        fn lose_next(&self, n: usize) {
            self.lose.store(n, Ordering::SeqCst);
        }
    }

    impl fmt::Debug for Wrap {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Wrap")
        }
    }
    impl fmt::Display for Wrap {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Wrap(memory)")
        }
    }

    #[async_trait]
    impl ObjectStore for Wrap {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            mut opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            if self.ignore_create.load(Ordering::SeqCst) {
                opts.mode = PutMode::Overwrite;
            }
            let res = self.inner.put_opts(location, payload, opts).await?;
            if location.as_ref().contains("/rl/") || location.as_ref().starts_with("rl/") {
                let lost = self
                    .lose
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok();
                if lost {
                    return Err(object_store::Error::Generic {
                        store: "Wrap",
                        source: "timeout".into(),
                    });
                }
            }
            Ok(res)
        }
        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }
        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.inner.get_opts(location, options).await
        }
        async fn get_ranges(
            &self,
            location: &Path,
            ranges: &[Range<u64>],
        ) -> object_store::Result<Vec<Bytes>> {
            self.inner.get_ranges(location, ranges).await
        }
        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }
        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }
        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
        async fn rename_opts(
            &self,
            from: &Path,
            to: &Path,
            options: RenameOptions,
        ) -> object_store::Result<()> {
            self.inner.rename_opts(from, to, options).await
        }
    }

    fn actor() -> Actor {
        Actor {
            id: "me".into(),
            via: "cli".into(),
        }
    }

    fn tgt(s: &str) -> RefTarget {
        RefTarget::Changeset(ChangesetId::of(s.as_bytes()))
    }

    fn name(s: &str) -> RefName {
        RefName::new(s).unwrap()
    }

    /// A store whose `Create` puts fail with `NotImplemented`.
    struct NoCreate(Wrap);

    impl fmt::Debug for NoCreate {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("NoCreate")
        }
    }
    impl fmt::Display for NoCreate {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("NoCreate")
        }
    }

    #[async_trait]
    impl ObjectStore for NoCreate {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            if matches!(opts.mode, PutMode::Create) {
                return Err(object_store::Error::NotImplemented {
                    operation: "put_opts".into(),
                    implementer: "NoCreate".into(),
                });
            }
            self.0.put_opts(location, payload, opts).await
        }
        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.0.put_multipart_opts(location, opts).await
        }
        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.0.get_opts(location, options).await
        }
        async fn get_ranges(
            &self,
            location: &Path,
            ranges: &[Range<u64>],
        ) -> object_store::Result<Vec<Bytes>> {
            self.0.get_ranges(location, ranges).await
        }
        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.0.delete_stream(locations)
        }
        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.0.list(prefix)
        }
        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.0.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.0.copy_opts(from, to, options).await
        }
        async fn rename_opts(
            &self,
            from: &Path,
            to: &Path,
            options: RenameOptions,
        ) -> object_store::Result<()> {
            self.0.rename_opts(from, to, options).await
        }
    }

    #[tokio::test]
    async fn connect_refuses_store_without_exclusive_create() {
        let s: Arc<dyn ObjectStore> = Arc::new(NoCreate(Wrap::new()));
        let err = BucketRefs::connect(s, Path::from("ns/x"))
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("conditional writes"), "{err}");
        let ignoring = Wrap::new();
        ignoring.ignore_create();
        assert!(
            BucketRefs::connect(Arc::new(ignoring), Path::from("ns/x"))
                .await
                .is_err()
        );
        assert!(
            BucketRefs::connect(Arc::new(Wrap::new()), Path::from("ns/x"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn cas_against_store_without_create_errors_and_writes_nothing() {
        let w = Arc::new(NoCreate(Wrap::new()));
        let r = BucketRefs::new(w, Path::from("ns/x"));
        let a = name("refs/heads/a");
        let err = r
            .cas(&a, Expect::Absent, Some(tgt("1")), &actor(), Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("conditional writes"), "{err}");
        assert!(r.get(REPO, &a).await.unwrap().is_none());
    }

    fn refs() -> (Arc<Wrap>, BucketRefs) {
        let w = Arc::new(Wrap::new());
        (w.clone(), BucketRefs::new(w, Path::from("ns/x")))
    }

    const REPO: RepoKey = RepoKey(1);

    #[tokio::test]
    async fn create_get_update_delete_list_log() {
        let (_, r) = refs();
        let (a, b) = (name("refs/heads/a"), name("refs/heads/a/b"));
        assert!(r.get(REPO, &a).await.unwrap().is_none());
        r.cas(&a, Expect::Absent, Some(tgt("1")), &actor(), Uuid::now_v7())
            .await
            .unwrap();
        // `a` and `a/b` coexist because names are one encoded segment.
        r.cas(&b, Expect::Absent, Some(tgt("2")), &actor(), Uuid::now_v7())
            .await
            .unwrap();
        r.cas(
            &a,
            Expect::Value(tgt("1")),
            Some(tgt("3")),
            &actor(),
            Uuid::now_v7(),
        )
        .await
        .unwrap();
        let got = r.get(REPO, &a).await.unwrap().unwrap();
        assert_eq!((got.target, got.version), (tgt("3"), 2));
        let listed = r.list(REPO, "refs/heads/").await.unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].name < listed[1].name);
        assert!(r.list(REPO, "refs/tags/").await.unwrap().is_empty());

        r.cas(&b, Expect::Value(tgt("2")), None, &actor(), Uuid::now_v7())
            .await
            .unwrap();
        assert!(r.get(REPO, &b).await.unwrap().is_none());
        assert_eq!(r.list(REPO, "").await.unwrap().len(), 1);
        let log = r.log(REPO, &b, 10).await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!((log[0].old, log[0].new), (Some(tgt("2")), None));
        assert_eq!((log[1].old, log[1].new), (None, Some(tgt("2"))));
        assert_eq!(log[0].actor, actor());
        assert_eq!(r.log(REPO, &a, 1).await.unwrap().len(), 1);
        // A recreated ref continues the version sequence.
        r.cas(&b, Expect::Absent, Some(tgt("4")), &actor(), Uuid::now_v7())
            .await
            .unwrap();
        assert_eq!(r.get(REPO, &b).await.unwrap().unwrap().version, 3);
    }

    #[tokio::test]
    async fn wrong_expectation_conflicts() {
        let (_, r) = refs();
        let a = name("refs/heads/a");
        r.cas(&a, Expect::Absent, Some(tgt("1")), &actor(), Uuid::now_v7())
            .await
            .unwrap();
        for expect in [Expect::Absent, Expect::Value(tgt("9"))] {
            let e = r
                .cas(&a, expect, Some(tgt("2")), &actor(), Uuid::now_v7())
                .await
                .unwrap_err();
            assert!(matches!(e, RefError::Conflict(c) if c.actual == Some(tgt("1"))));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn race_has_one_winner_per_version() {
        let (_, r) = refs();
        let a = name("refs/heads/race");
        let mut expect = Expect::Absent;
        for round in 1..=3u64 {
            let mut tasks = Vec::new();
            for i in 0..32 {
                let (r, a) = (r.clone(), a.clone());
                tasks.push(tokio::spawn(async move {
                    r.cas(
                        &a,
                        expect,
                        Some(tgt(&format!("{round}-{i}"))),
                        &actor(),
                        Uuid::now_v7(),
                    )
                    .await
                }));
            }
            let mut wins = Vec::new();
            for t in tasks {
                match t.await.unwrap() {
                    Ok(v) => wins.push(v),
                    Err(RefError::Conflict(_)) => {}
                    Err(e) => panic!("{e}"),
                }
            }
            assert_eq!(wins, vec![round], "round {round}");
            expect = Expect::Value(r.get(REPO, &a).await.unwrap().unwrap().target);
        }
        assert_eq!(r.log(REPO, &a, 10).await.unwrap().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn any_writers_all_land_with_distinct_versions() {
        let (_, r) = refs();
        let a = name("refs/heads/any");
        let mut tasks = Vec::new();
        for i in 0..8 {
            let (r, a) = (r.clone(), a.clone());
            tasks.push(tokio::spawn(async move {
                r.cas(
                    &a,
                    Expect::Any,
                    Some(tgt(&i.to_string())),
                    &actor(),
                    Uuid::now_v7(),
                )
                .await
                .unwrap()
            }));
        }
        let mut vs = Vec::new();
        for t in tasks {
            vs.push(t.await.unwrap());
        }
        vs.sort_unstable();
        assert_eq!(vs, (1..=8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn lost_response_then_retry_succeeds() {
        let (w, r) = refs();
        let a = name("refs/heads/a");
        let op = Uuid::now_v7();
        w.lose_next(1);
        // The put applied but reported a timeout: the change is detected as landed.
        let v = r
            .cas(&a, Expect::Absent, Some(tgt("1")), &actor(), op)
            .await
            .unwrap();
        assert_eq!(v, 1);
        // A caller-level retry with the same op id is also a success, not a conflict.
        let v = r
            .cas(&a, Expect::Absent, Some(tgt("1")), &actor(), op)
            .await
            .unwrap();
        assert_eq!(v, 1);
        assert_eq!(r.log(REPO, &a, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retried_create_after_overtaken_version_still_succeeds() {
        let (_, r) = refs();
        let a = name("refs/heads/a");
        let op = Uuid::now_v7();
        r.cas(&a, Expect::Absent, Some(tgt("1")), &actor(), op)
            .await
            .unwrap();
        r.cas(&a, Expect::Any, Some(tgt("2")), &actor(), Uuid::now_v7())
            .await
            .unwrap();
        // Replaying the first create at v=1 finds my op id.
        assert!(r.landed("refs/heads/a", 1, *op.as_bytes()).await.unwrap());
    }

    #[tokio::test]
    async fn versions_only_increase_so_aba_is_impossible() {
        let (w, r) = refs();
        let a = name("refs/heads/a");
        let mut cur = Expect::Absent;
        for t in ["A", "B", "A"] {
            r.cas(&a, cur, Some(tgt(t)), &actor(), Uuid::now_v7())
                .await
                .unwrap();
            cur = Expect::Value(tgt(t));
        }
        let log = r.log(REPO, &a, 10).await.unwrap();
        assert_eq!(log.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 2, 1]);
        // A writer that read v=1 (A) cannot create v=2 again, though the value is A once more.
        let stale = r.entry_path("refs/heads/a", 2).unwrap();
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        let err = w
            .put_opts(&stale, PutPayload::from_static(b"x"), opts)
            .await
            .unwrap_err();
        assert!(matches!(err, object_store::Error::AlreadyExists { .. }));
    }

    #[tokio::test]
    async fn unknown_entry_version_fails_and_never_writes() {
        let (w, r) = refs();
        let a = name("refs/heads/a");
        let p = r.entry_path("refs/heads/a", 1).unwrap();
        w.put(&p, PutPayload::from_static(&[9, 0, 0]))
            .await
            .unwrap();
        let before = w
            .list_with_delimiter(None)
            .await
            .unwrap()
            .common_prefixes
            .len();
        let e = r
            .cas(&a, Expect::Any, Some(tgt("1")), &actor(), Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("upgrade asc"), "{e}");
        assert!(
            r.get(REPO, &a)
                .await
                .unwrap_err()
                .to_string()
                .contains("upgrade asc")
        );
        assert!(
            !r.exists(&r.entry_path("refs/heads/a", 2).unwrap())
                .await
                .unwrap()
        );
        assert_eq!(
            before,
            w.list_with_delimiter(None)
                .await
                .unwrap()
                .common_prefixes
                .len()
        );
    }

    #[tokio::test]
    async fn freeze_blocks_cas() {
        let (w, r) = refs();
        let a = name("refs/heads/a");
        let freeze = Path::from("ns/x/gc/freeze");
        w.put(&freeze, PutPayload::from_static(b"2026-01-01T00:00:00Z"))
            .await
            .unwrap();
        let e = r
            .cas(&a, Expect::Absent, Some(tgt("1")), &actor(), Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("garbage-collected"), "{e}");
        assert!(r.get(REPO, &a).await.unwrap().is_none());
        w.delete(&freeze).await.unwrap();
        r.cas(&a, Expect::Absent, Some(tgt("1")), &actor(), Uuid::now_v7())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stale_and_forged_hints_do_not_matter() {
        let (w, r) = refs();
        let a = name("refs/heads/a");
        for i in 0..20 {
            r.cas(
                &a,
                Expect::Any,
                Some(tgt(&i.to_string())),
                &actor(),
                Uuid::now_v7(),
            )
            .await
            .unwrap();
        }
        let hint = r.hint_path("refs/heads/a").unwrap();
        for body in ["3", "99999", "junk", "0"] {
            w.put(&hint, PutPayload::from_bytes(Bytes::from(body)))
                .await
                .unwrap();
            assert_eq!(
                r.get(REPO, &a).await.unwrap().unwrap().version,
                20,
                "{body}"
            );
        }
        // A lost hint still lists the ref.
        w.delete(&hint).await.unwrap();
        assert_eq!(r.list(REPO, "").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn expiry_is_refused() {
        let (_, r) = refs();
        let txn = RefTxn {
            updates: vec![crate::refs::RefUpdate {
                name: name("refs/heads/a"),
                expect: Expect::Any,
                new: Some(tgt("1")),
                expires_at: Some(5),
            }],
            actor: actor(),
            reason: String::new(),
            op_id: Uuid::now_v7(),
        };
        assert!(r.update(REPO, &txn).await.is_err());
    }

    #[test]
    fn entry_round_trip_and_bounds() {
        let e = BucketRefEntry {
            old: None,
            new: Some(tgt("1")),
            op_id: [7; 16],
            actor: b"me".to_vec(),
            at: 5,
        };
        let b = e.encode().unwrap();
        assert_eq!(b[0], 1);
        assert_eq!(BucketRefEntry::decode(&b).unwrap(), e);
        let mut trailing = b.to_vec();
        trailing.push(0);
        assert!(BucketRefEntry::decode(&trailing).is_err());
        assert!(BucketRefEntry::decode(&[]).is_err());
        let big = BucketRefEntry {
            actor: vec![1; MAX_ACTOR + 1],
            ..e
        };
        assert!(big.encode().is_err());
    }

    #[tokio::test]
    async fn local_file_bucket_works() {
        let dir = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(dir.path()).unwrap();
        let r = BucketRefs::from_url_with(url.as_str(), None).unwrap();
        let a = name("refs/heads/a");
        r.cas(&a, Expect::Absent, Some(tgt("1")), &actor(), Uuid::now_v7())
            .await
            .unwrap();
        r.cas(
            &name("refs/heads/a/b"),
            Expect::Absent,
            Some(tgt("2")),
            &actor(),
            Uuid::now_v7(),
        )
        .await
        .unwrap();
        assert_eq!(r.list(REPO, "").await.unwrap().len(), 2);
    }
}
