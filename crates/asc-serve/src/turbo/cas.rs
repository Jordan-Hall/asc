//! The build cache's file-level routes (docs/designs/build-cache.md §3.1). An entry and each file
//! it names travel separately, so a build that changed one output file uploads that one file, and a
//! machine that already holds the other files downloads only the changed one.
//!
//! ```text
//! turbo/<owner>.<repo>/<scope>/cas/<file id>   one output file, zstd-compressed, checked against its id
//! turbo/<owner>.<repo>/<scope>/ac/<key>        the entry (JSON): output paths, kinds, sizes, file ids
//! ```
//!
//! Scopes work as for artefacts: writes go to the caller's scope, reads try the caller's scope and
//! then `trusted`. A file is stored under the id of its content, which the server checks, so a
//! writer cannot put other bytes under an id. An entry is accepted only when every file it names
//! is on the server. Under path rules a shared (`trusted`) entry is served only where CI registered
//! its directory, as for artefacts, and shared files are served only through entries the caller
//! can read: a caller with restricting path rules gets files of its own scope only.

use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use asc_rpc::auth::Access;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{StreamExt, TryStreamExt};
use object_store::ObjectStoreExt;
use object_store::path::Path as StorePath;
use serde::Deserialize;

use super::{
    ArtifactHash, BATCH_CONCURRENCY, Caller, MAX_ITEMS, RepoQuery, Scope, TOUCH_AFTER_SECS,
    TurboError, TurboState, now_secs, read_json,
};

/// The directory of files in a scope.
pub(super) const CAS_DIR: &str = "cas";
/// The directory of entries in a scope.
pub(super) const AC_DIR: &str = "ac";
/// Largest file, uncompressed. A larger output keeps its entry on the artefact route.
pub const MAX_FILE: u64 = 512 << 20;
/// Largest compressed upload of one file.
const MAX_FILE_BODY: usize = 512 << 20;
/// Largest entry document.
pub const MAX_ENTRY: usize = 4 << 20;
/// A file no live entry names is kept this long, so an upload whose entry is not written yet
/// survives a sweep.
const ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);

/// A 64-hex id: a file id (blake3) or an action key.
fn id_ok(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn check_id(s: &str) -> Result<(), TurboError> {
    if id_ok(s) {
        Ok(())
    } else {
        Err(TurboError::BadRequest("invalid id"))
    }
}

/// What the server reads of an entry: the files it names. Everything else is the client's.
#[derive(Deserialize)]
struct EntryFiles {
    #[serde(default)]
    outputs: Vec<EntryOutput>,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
}

#[derive(Deserialize)]
struct EntryOutput {
    blob: String,
}

/// The file ids an entry names, or an error when one is not a 64-hex id.
fn entry_files(body: &[u8]) -> Result<BTreeSet<String>, TurboError> {
    let e: EntryFiles =
        serde_json::from_slice(body).map_err(|_| TurboError::BadRequest("invalid entry"))?;
    let all: Vec<String> = e
        .outputs
        .into_iter()
        .map(|o| o.blob)
        .chain(e.stdout)
        .chain(e.stderr)
        .collect();
    if all.len() > MAX_ITEMS {
        return Err(TurboError::BadRequest("the entry names too many files"));
    }
    if !all.iter().all(|b| id_ok(b)) {
        return Err(TurboError::BadRequest("a file id of the entry is invalid"));
    }
    Ok(all.into_iter().collect())
}

/// Checks a compressed upload against `id`: the content must hash to it and stay within
/// [`MAX_FILE`].
fn verify(id: &str, compressed: &[u8]) -> Result<(), TurboError> {
    let mut z = zstd::stream::read::Decoder::new(compressed)
        .map_err(|_| TurboError::BadRequest("the body is not zstd"))?;
    let mut h = asc_core::id::FileHasher::new();
    let mut buf = vec![0u8; 64 << 10];
    let mut total = 0u64;
    loop {
        let n = z
            .read(&mut buf)
            .map_err(|_| TurboError::BadRequest("the body is not zstd"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_FILE {
            return Err(TurboError::TooLarge);
        }
        h.update(&buf[..n]);
    }
    if h.finalize().to_string() != id {
        return Err(TurboError::BadRequest("the content does not match its id"));
    }
    Ok(())
}

impl TurboState {
    fn cas_path(&self, c: &Caller, scope: &Scope, id: &str) -> StorePath {
        self.path(&c.slug, scope, CAS_DIR).join(id)
    }

    fn ac_path(&self, c: &Caller, scope: &Scope, key: &str) -> StorePath {
        self.path(&c.slug, scope, AC_DIR).join(key)
    }

    /// The scopes a caller reads files from: its own, then `trusted` unless path rules restrict it.
    fn file_scopes(c: &Caller) -> Vec<Scope> {
        let mut out = vec![c.scope.clone()];
        if c.scope != Scope::Trusted && c.rules.is_none() {
            out.push(Scope::Trusted);
        }
        out
    }

    /// Where file `id` is for this caller, if anywhere.
    async fn find_file(&self, c: &Caller, id: &str) -> Result<Option<StorePath>, TurboError> {
        for scope in Self::file_scopes(c) {
            let p = self.cas_path(c, &scope, id);
            match self.store.head(&p).await {
                Ok(_) => return Ok(Some(p)),
                Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    /// The ids of `ids` the caller cannot read on the server.
    async fn missing(&self, c: &Caller, ids: Vec<String>) -> Result<Vec<String>, TurboError> {
        let found: Vec<(String, bool)> = futures::stream::iter(ids)
            .map(|id| async move {
                let has = self.find_file(c, &id).await?.is_some();
                Ok::<_, TurboError>((id, has))
            })
            .buffered(BATCH_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(found
            .into_iter()
            .filter_map(|(id, has)| (!has).then_some(id))
            .collect())
    }

    /// The first scope (caller's, then `trusted`) holding entry `key` that the caller may read.
    async fn find_entry(&self, c: &Caller, key: &str) -> Result<Option<StorePath>, TurboError> {
        let mut scopes = vec![c.scope.clone()];
        if c.scope != Scope::Trusted {
            scopes.push(Scope::Trusted);
        }
        for scope in scopes {
            if let Some(rules) = &c.rules
                && scope != c.scope
            {
                // The same registration an artefact of this key has (`asc-<key>`).
                let hash = ArtifactHash::parse(&format!("asc-{key}"))?;
                let dir = self.read_dir(&self.dir_path(&c.slug, &hash)).await?;
                if dir.is_none_or(|d| rules.level(d.as_bytes()) < asc_core::pathacl::Level::Build) {
                    continue;
                }
            }
            let p = self.ac_path(c, &scope, key);
            match self.store.head(&p).await {
                Ok(_) => return Ok(Some(p)),
                Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    /// Whether `hash` names a build cache entry (`asc-<key>`) stored in `trusted`, so trusted CI
    /// can register its directory as it does for an artefact.
    pub(super) async fn has_trusted_entry(
        &self,
        c: &Caller,
        hash: &ArtifactHash,
    ) -> Result<bool, TurboError> {
        let Some(key) = hash.as_str().strip_prefix("asc-").filter(|k| id_ok(k)) else {
            return Ok(false);
        };
        match self
            .store
            .head(&self.ac_path(c, &Scope::Trusted, key))
            .await
        {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Removes entries past their scope's retention and files no remaining entry of the
    /// repository names (after [`ORPHAN_GRACE`]). Returns (scanned, deleted).
    pub(super) async fn sweep_files(
        &self,
        now: SystemTime,
        retention: impl Fn(&str) -> Duration,
    ) -> Result<(u64, u64), TurboError> {
        let prefix = self.root.clone().join("turbo");
        let skip = prefix.parts().count();
        let (mut scanned, mut deleted) = (0u64, 0u64);
        let mut named: HashSet<(String, String)> = HashSet::new();
        let mut files: Vec<(String, String, StorePath, Duration)> = Vec::new();
        let mut list = self.store.list(Some(&prefix));
        while let Some(obj) = list.try_next().await? {
            let parts: Vec<String> = obj
                .location
                .parts()
                .skip(skip)
                .map(|p| p.as_ref().to_owned())
                .collect();
            let [slug, scope, dir, name] = parts.as_slice() else {
                continue;
            };
            let age = now
                .duration_since(SystemTime::from(obj.last_modified))
                .unwrap_or_default();
            if dir == AC_DIR && id_ok(name) {
                scanned += 1;
                if age > retention(scope) {
                    match self.store.delete(&obj.location).await {
                        Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                        Err(e) => return Err(e.into()),
                    }
                    deleted += 1;
                    continue;
                }
                if obj.size as usize > MAX_ENTRY {
                    continue;
                }
                let body = match self.store.get(&obj.location).await {
                    Ok(g) => g.bytes().await?,
                    Err(object_store::Error::NotFound { .. }) => continue,
                    Err(e) => return Err(e.into()),
                };
                if let Ok(ids) = entry_files(&body) {
                    named.extend(ids.into_iter().map(|id| (slug.clone(), id)));
                }
            } else if dir == CAS_DIR && id_ok(name) {
                files.push((slug.clone(), name.clone(), obj.location.clone(), age));
            }
        }
        for (slug, id, location, age) in files {
            scanned += 1;
            if age <= ORPHAN_GRACE || named.contains(&(slug, id)) {
                continue;
            }
            match self.store.delete(&location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
            deleted += 1;
        }
        Ok((scanned, deleted))
    }
}

#[derive(Deserialize)]
pub(super) struct MissingRequest {
    ids: Vec<String>,
}

/// `POST /v8/asc/cas/missing`: which of these files the server lacks for the caller. Asked by a
/// writer before it uploads, so only the changed files travel.
pub(super) async fn post_missing(
    State(s): State<Arc<TurboState>>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, TurboError> {
    let caller = s.gate(&headers, &q, Access::Write).await?;
    let req: MissingRequest = read_json(body).await?;
    if req.ids.len() > MAX_ITEMS {
        return Err(TurboError::BadRequest("too many ids"));
    }
    let mut ids = req.ids;
    for id in &ids {
        check_id(id)?;
    }
    ids.sort();
    ids.dedup();
    let missing = s.missing(&caller, ids).await?;
    Ok(axum::Json(serde_json::json!({ "missing": missing })).into_response())
}

/// `GET|HEAD /v8/asc/cas/<id>`: one file, zstd-compressed.
pub(super) async fn get_file(
    State(s): State<Arc<TurboState>>,
    method: Method,
    Path(id): Path<String>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
) -> Result<Response, TurboError> {
    check_id(&id)?;
    let caller = s.gate(&headers, &q, Access::Read).await?;
    let p = s
        .find_file(&caller, &id)
        .await?
        .ok_or(TurboError::NotFound)?;
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zstd"),
    );
    if method == Method::HEAD {
        return Ok((StatusCode::OK, h, Body::empty()).into_response());
    }
    let got = s.store.get(&p).await.map_err(|e| match e {
        object_store::Error::NotFound { .. } => TurboError::NotFound,
        e => e.into(),
    })?;
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(got.meta.size));
    Ok((StatusCode::OK, h, Body::from_stream(got.into_stream())).into_response())
}

/// `PUT /v8/asc/cas/<id>`: one file, zstd-compressed. Refused unless its content hashes to `id`.
pub(super) async fn put_file(
    State(s): State<Arc<TurboState>>,
    Path(id): Path<String>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, TurboError> {
    check_id(&id)?;
    let caller = s.gate(&headers, &q, Access::Write).await?;
    let compressed = axum::body::to_bytes(body, MAX_FILE_BODY)
        .await
        .map_err(|_| TurboError::TooLarge)?;
    let p = s.cas_path(&caller, &caller.scope, &id);
    if s.store.head(&p).await.is_ok() {
        return Ok(StatusCode::OK.into_response());
    }
    let check = compressed.clone();
    let idc = id.clone();
    tokio::task::spawn_blocking(move || verify(&idc, &check))
        .await
        .map_err(|_| TurboError::BadRequest("verification failed"))??;
    s.store.put(&p, compressed.into()).await?;
    Ok(StatusCode::CREATED.into_response())
}

/// `GET|HEAD /v8/asc/ac/<key>`: the entry of `key`.
pub(super) async fn get_entry(
    State(s): State<Arc<TurboState>>,
    method: Method,
    Path(key): Path<String>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
) -> Result<Response, TurboError> {
    check_id(&key)?;
    let caller = s.gate(&headers, &q, Access::Read).await?;
    let p = s
        .find_entry(&caller, &key)
        .await?
        .ok_or(TurboError::NotFound)?;
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if method == Method::HEAD {
        return Ok((StatusCode::OK, h, Body::empty()).into_response());
    }
    let got = match s.store.get(&p).await {
        Ok(g) => g,
        Err(object_store::Error::NotFound { .. }) => return Err(TurboError::NotFound),
        Err(e) => return Err(e.into()),
    };
    let modified = got.meta.last_modified.timestamp();
    if got.meta.size as usize > MAX_ENTRY {
        return Err(TurboError::NotFound);
    }
    let body = got.bytes().await?;
    // A used entry is rewritten once a day, so its retention counts from its last use.
    if now_secs() - modified > TOUCH_AFTER_SECS
        && let Err(e) = s.store.put(&p, body.clone().into()).await
    {
        tracing::debug!(event = "turbo.touch_failed", error = %e);
    }
    Ok((StatusCode::OK, h, Body::from(body)).into_response())
}

/// `PUT /v8/asc/ac/<key>`: the entry of `key`, in the caller's scope. Refused while a file it
/// names is not on the server.
pub(super) async fn put_entry(
    State(s): State<Arc<TurboState>>,
    Path(key): Path<String>,
    Query(q): Query<RepoQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, TurboError> {
    check_id(&key)?;
    let caller = s.gate(&headers, &q, Access::Write).await?;
    let body = axum::body::to_bytes(body, MAX_ENTRY)
        .await
        .map_err(|_| TurboError::TooLarge)?;
    let ids = entry_files(&body)?;
    if !s
        .missing(&caller, ids.into_iter().collect())
        .await?
        .is_empty()
    {
        return Err(TurboError::BadRequest(
            "a file of the entry is not on the server",
        ));
    }
    s.store
        .put(&s.ac_path(&caller, &caller.scope, &key), body.into())
        .await?;
    Ok(StatusCode::CREATED.into_response())
}
