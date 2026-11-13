//! The server tier of the build cache (docs/designs/build-cache.md §3): an entry and each output
//! file travel separately on the file-level routes (§3.1), so one changed file is the only upload
//! and, on a machine that holds the rest, the only download. A server without those routes gets
//! the whole entry as one packed artefact named `asc-<key>`. Each revision lists the keys its
//! builds used, so `asc cache warm` can fetch them before the next build.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use asc_buildcache::{
    Action, ActionKey, MAX_ENTRY_DOC, Store, artifact_name, blob_limits, hex, human_size,
    key_of_artifact, parse_entry,
};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::{Method, StatusCode};
use serde_json::json;
use tokio::sync::Semaphore;

use super::{io_err, open_store};
use crate::cli::ci::cache::{HttpApi, failure, fatal, http_api, whole};
use crate::cli::common::{CliError, Ctx, Result};

/// Largest artefact sent or fetched. A larger entry stays local.
pub(crate) const MAX_ARTIFACT: usize = 512 << 20;
/// Body limit of a reply that is only a status.
const SMALL: usize = 64 << 10;
/// Artefacts or files moved at once.
const PARALLEL: usize = 8;
/// Largest compressed file fetched on its own.
const MAX_FILE_BODY: usize = 520 << 20;
/// Compressed bodies held in memory at once during one fetch, in MiB.
const IN_FLIGHT_MIB: u64 = 1024;

/// `asc.buildcache.remote`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Read,
    ReadWrite,
}

/// `asc.buildcache.remote`: `off`, `read` or `read-write`. Unset, it is `read-write` under CI
/// (`CI` set and not `false`) and `read` elsewhere.
pub(crate) fn mode(ctx: &Ctx) -> Result<Mode> {
    match ctx.config.get("asc.buildcache.remote").as_deref() {
        Some("off" | "false") => Ok(Mode::Off),
        Some("read") => Ok(Mode::Read),
        Some("read-write" | "readwrite") => Ok(Mode::ReadWrite),
        Some(other) => Err(CliError::Fatal(format!(
            "bad value '{other}' for 'asc.buildcache.remote' (use off, read or read-write)"
        ))),
        None => Ok(
            match std::env::var("CI")
                .ok()
                .filter(|v| !v.is_empty() && v != "false")
            {
                Some(_) => Mode::ReadWrite,
                None => Mode::Read,
            },
        ),
    }
}

/// The server of the build cache: `--remote`, else `asc.buildcache.server` (a remote name or an
/// https URL), else `origin`. A job whose checkout points at a forge without the build cache
/// names the asc server here.
fn server(ctx: &Ctx, remote: Option<&str>) -> Option<String> {
    remote.map(str::to_owned).or_else(|| {
        ctx.config
            .get("asc.buildcache.server")
            .filter(|s| !s.trim().is_empty())
    })
}

fn require_on(ctx: &Ctx) -> Result<()> {
    if mode(ctx)? == Mode::Off {
        return Err(CliError::Fatal(
            "the build cache's server tier is off (asc.buildcache.remote)".into(),
        ));
    }
    Ok(())
}

/// The file that holds the outcome of the last exchange with the server tier.
const STATE: &str = "remote.json";

/// The outcome of the last exchange with the server tier, for `asc cache status`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct State {
    /// Unix seconds.
    pub at: u64,
    /// Why it failed; `None` when the server answered.
    pub error: Option<String>,
}

/// Records how an exchange went. A store that cannot be written keeps the old state: the state
/// only informs, and the build goes on either way.
fn record<T>(store: &Store, r: &Result<T>) {
    let error = r.as_ref().err().map(|e| e.to_string().trim().to_owned());
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let Ok(text) = serde_json::to_vec(&State { at, error }) else {
        return;
    };
    // Written beside and renamed, so a reader never sees half a file.
    let tmp = store
        .root()
        .join(format!("{STATE}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, text).is_err()
        || std::fs::rename(&tmp, store.root().join(STATE)).is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The outcome of the last exchange, if there was one.
pub(crate) fn state(store: &Store) -> Option<State> {
    let text = std::fs::read(store.root().join(STATE)).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Directory of the store that holds how long each queued task ran, and under which revision,
/// one file per key: the milliseconds, a newline, then the 64-hex revision or nothing.
const DURATIONS: &str = "durations";
/// Largest durations file read.
const MAX_RECORD: u64 = 128;
/// Longest duration believed: one week, in milliseconds.
const MAX_MS: u64 = 7 * 24 * 3600 * 1000;

/// What the machine that ran a task recorded about it.
#[derive(Debug, PartialEq, Eq)]
struct Recorded {
    ms: u64,
    rev: Option<String>,
}

fn record_path(store: &Store, key: &ActionKey) -> std::path::PathBuf {
    store.root().join(DURATIONS).join(key.to_string())
}

/// Remembers that the task of `key` ran for `elapsed` while the build was at `rev`, until `key`
/// is pushed. Best effort: a store that cannot be written just sends no duration. A run too
/// short to measure in whole milliseconds is not recorded (a zero is never sent). The file is
/// written beside and renamed, so a push never reads half of it.
pub(crate) fn note_duration(
    store: &Store,
    key: &ActionKey,
    elapsed: std::time::Duration,
    rev: Option<&str>,
) {
    let ms = elapsed.as_millis();
    if ms == 0 || ms > u128::from(MAX_MS) {
        return;
    }
    let rev = rev
        .filter(|r| crate::cli::ci::cache::rev_header_ok(r))
        .unwrap_or("");
    let dir = store.root().join(DURATIONS);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join(format!(".{key}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{ms}\n{rev}\n")).is_err()
        || std::fs::rename(&tmp, record_path(store, key)).is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Drops the record of `key`.
pub(crate) fn forget_duration(store: &Store, key: &ActionKey) {
    let _ = std::fs::remove_file(record_path(store, key));
}

/// Records the run of the task of `key` and queues `key` for upload; when queueing fails the
/// record goes too.
pub(crate) fn queue_timed(
    store: &Store,
    key: &ActionKey,
    elapsed: std::time::Duration,
    rev: Option<&str>,
) -> std::io::Result<()> {
    note_duration(store, key, elapsed, rev);
    store
        .queue_push(key)
        .inspect_err(|_| forget_duration(store, key))
}

/// The record of `key`, if it is well formed; a malformed one is removed.
fn recorded(store: &Store, key: &ActionKey) -> Option<Recorded> {
    use std::io::Read;
    let path = record_path(store, key);
    let mut text = String::new();
    let read =
        std::fs::File::open(&path).and_then(|f| f.take(MAX_RECORD + 1).read_to_string(&mut text));
    if read.is_err() {
        // Absent, or unreadable: nothing to send.
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        return None;
    }
    let parsed = (text.len() as u64 <= MAX_RECORD)
        .then(|| {
            let mut lines = text.strip_suffix('\n')?.split('\n');
            let ms = lines.next()?;
            let rev = lines.next();
            if lines.next().is_some() {
                return None;
            }
            if ms.is_empty() || !ms.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let ms: u64 = ms.parse().ok().filter(|m| (1..=MAX_MS).contains(m))?;
            let rev = match rev {
                None | Some("") => None,
                Some(r) if crate::cli::ci::cache::rev_header_ok(r) => Some(r.to_owned()),
                Some(_) => return None,
            };
            Some(Recorded { ms, rev })
        })
        .flatten();
    if parsed.is_none() {
        let _ = std::fs::remove_file(&path);
    }
    parsed
}

/// Removes the records of keys that are no longer queued (an interrupted run, a failed push).
/// Looks at no more than [`PRUNE_LIMIT`] files.
pub(crate) fn prune_durations(store: &Store) {
    let Ok(queued) = store.queued() else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(store.root().join(DURATIONS)) else {
        return;
    };
    for e in rd.take(PRUNE_LIMIT).flatten() {
        let name = e.file_name();
        let keep = name
            .to_str()
            .and_then(ActionKey::parse)
            .is_some_and(|k| queued.contains(&k));
        if !keep {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Most files [`prune_durations`] looks at in one call.
const PRUNE_LIMIT: usize = 4096;

static BUILD_REV: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The revision the running build is at: HEAD of the work tree, resolved once per process. `None`
/// outside a work tree or when it cannot be resolved; the build never waits on more than that.
pub(crate) async fn build_rev(ctx: &Ctx) -> Option<String> {
    if let Some(r) = BUILD_REV.get() {
        return r.clone();
    }
    let r = revision(ctx, None)
        .await
        .ok()
        .flatten()
        .filter(|r| crate::cli::ci::cache::rev_header_ok(r));
    BUILD_REV.get_or_init(|| r).clone()
}

/// The revision a push indexes or a warm reads: `rev` resolved in the work tree, else its HEAD
/// commit. `None` outside a work tree when `rev` is not given, or on an unborn HEAD.
async fn revision(ctx: &Ctx, rev: Option<&str>) -> Result<Option<String>> {
    if rev.is_none() && ctx.ws_opt().is_none() {
        return Ok(None);
    }
    let Some(rev) = rev else {
        return Ok(crate::cli::restore::resolve_commit(ctx, "HEAD")
            .await?
            .map(|id| id.to_string()));
    };
    // A branch, a tag, a changeset id, or the git spelling of a commit.
    match crate::cli::switch::resolve_rev(ctx, ctx.ws()?, rev).await? {
        Some(r) => Ok(Some(r.commit.to_string())),
        None => Err(CliError::Fatal(format!("unknown revision '{rev}'"))),
    }
}

/// `api` tied to `rev` when there is one.
fn with_rev(api: HttpApi, rev: Option<&str>) -> HttpApi {
    match rev {
        Some(rev) => api.with_rev(rev),
        None => api,
    }
}

/// What fetching one key did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// Unpacked into the store; the artefact's size.
    Stored(u64),
    /// The server has no artefact for the key.
    Missing,
    /// The artefact is over [`MAX_ARTIFACT`].
    TooLarge,
    /// The artefact failed verification (tampered, truncated, or with an unsafe output); its
    /// entry was not stored.
    Refused(String),
}

/// Fetches the artefact of `key` into `store`. Of a refused artefact only blobs that matched
/// their ids are kept (the next `gc` sweeps them).
pub(crate) async fn fetch(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Fetched> {
    let r = fetch_one(api, store, key).await;
    record(store, &r);
    r
}

async fn fetch_one(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Fetched> {
    match fetch_files(api, store, key).await? {
        Fetched::Missing => fetch_artifact(api, store, key).await,
        got => Ok(got),
    }
}

/// Fetches the entry of `key` on the file-level routes, then only the files the store lacks.
/// `Missing` when the server has no such entry or no such routes.
async fn fetch_files(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Fetched> {
    let res = api
        .turbo(
            Method::GET,
            &format!("/v8/asc/ac/{key}"),
            Bytes::new(),
            MAX_ENTRY_DOC,
        )
        .await
        .map_err(fatal)?;
    if matches!(
        res.status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) {
        return Ok(Fetched::Missing);
    }
    if !res.status.is_success() {
        return Err(fatal(failure(res)));
    }
    if res.truncated {
        return Ok(Fetched::TooLarge);
    }
    let doc = whole(res).map_err(fatal)?;
    let action = match parse_entry(&doc) {
        Ok((action, _)) => action,
        Err(e) => return Ok(Fetched::Refused(e.to_string())),
    };
    let Some(limits) = blob_limits(&action) else {
        return Ok(Fetched::TooLarge);
    };
    // Bodies are held in memory until stored: at most IN_FLIGHT_MIB of them at once.
    let budget = Arc::new(Semaphore::new(IN_FLIGHT_MIB as usize));
    let wanted: Vec<([u8; 32], u64)> = limits
        .into_iter()
        .filter(|(b, _)| !store.has_blob(b))
        .collect();
    let mut bytes = doc.len() as u64;
    let mut got = futures::stream::iter(wanted)
        .map(|(id, max)| {
            let budget = budget.clone();
            async move {
                let body = body_limit(max);
                let mib = (body as u64).div_ceil(1 << 20).clamp(1, IN_FLIGHT_MIB) as u32;
                let _permit = budget.acquire_many(mib).await.expect("never closed");
                (id, fetch_file(api, store, &id, max, body).await)
            }
        })
        .buffer_unordered(PARALLEL);
    while let Some((id, r)) = got.next().await {
        match r? {
            FileGot::Stored(n) => bytes += n,
            // The entry outlived one of its files: the next build stores it again.
            FileGot::Missing => return Ok(Fetched::Missing),
            FileGot::Refused(m) => {
                return Ok(Fetched::Refused(format!("file {}: {m}", hex(&id))));
            }
        }
    }
    drop(got);
    Ok(match store.accept_entry(key, &doc) {
        Ok(_) => Fetched::Stored(bytes),
        Err(e) => Fetched::Refused(e.to_string()),
    })
}

/// What fetching one file did.
enum FileGot {
    /// Stored; the compressed bytes received.
    Stored(u64),
    Missing,
    Refused(String),
}

/// The largest compressed body of a file of at most `max` bytes (zstd adds a little to bytes
/// it cannot shrink).
fn body_limit(max: u64) -> usize {
    usize::try_from(max + max / 64 + 4096)
        .unwrap_or(usize::MAX)
        .min(MAX_FILE_BODY)
}

async fn fetch_file(
    api: &HttpApi,
    store: &Store,
    id: &[u8; 32],
    max: u64,
    body_max: usize,
) -> Result<FileGot> {
    let res = api
        .turbo(
            Method::GET,
            &format!("/v8/asc/cas/{}", hex(id)),
            Bytes::new(),
            body_max,
        )
        .await
        .map_err(fatal)?;
    if res.status == StatusCode::NOT_FOUND {
        return Ok(FileGot::Missing);
    }
    if !res.status.is_success() {
        return Err(fatal(failure(res)));
    }
    if res.truncated {
        return Ok(FileGot::Refused("too large".into()));
    }
    let body = whole(res).map_err(fatal)?;
    Ok(match store.put_blob_zstd(id, &body, max) {
        Ok(_) => FileGot::Stored(body.len() as u64),
        Err(e) => FileGot::Refused(e.to_string()),
    })
}

/// Fetches the whole entry as one packed artefact (a server without the file-level routes, or an
/// entry stored before them).
async fn fetch_artifact(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Fetched> {
    let path = format!("/v8/artifacts/{}", artifact_name(key));
    let res = api
        .turbo(Method::GET, &path, Bytes::new(), MAX_ARTIFACT)
        .await
        .map_err(fatal)?;
    if res.status == StatusCode::NOT_FOUND {
        return Ok(Fetched::Missing);
    }
    if !res.status.is_success() {
        return Err(fatal(failure(res)));
    }
    if res.truncated {
        return Ok(Fetched::TooLarge);
    }
    let body = whole(res).map_err(fatal)?;
    Ok(match store.unpack(key, &mut &body[..]) {
        Ok(_) => Fetched::Stored(body.len() as u64),
        Err(e) => Fetched::Refused(e.to_string()),
    })
}

/// The client of the build cache's server for `ctx`: `asc.buildcache.server`, else `origin`.
pub(crate) async fn api(ctx: &Ctx) -> Result<HttpApi> {
    http_api(ctx, server(ctx, None).as_deref()).await
}

/// A remote hit for `key`, or `None` on a miss, without a server, or when the server cannot be
/// reached (the build goes on as on a miss).
pub(crate) async fn fetch_entry(ctx: &Ctx, store: &Store, key: &ActionKey) -> Option<Action> {
    let api = with_rev(
        http_api(ctx, server(ctx, None).as_deref()).await.ok()?,
        build_rev(ctx).await.as_deref(),
    );
    fetch_entry_on(&api, store, key).await
}

async fn fetch_entry_on(api: &HttpApi, store: &Store, key: &ActionKey) -> Option<Action> {
    match fetch(api, store, key).await {
        Ok(Fetched::Stored(_)) => store.get_action(key).ok().flatten(),
        Ok(Fetched::Refused(m)) => {
            eprintln!("warning: build cache artefact refused: {m}");
            None
        }
        Ok(Fetched::Missing | Fetched::TooLarge) => None,
        Err(e) => {
            eprintln!("warning: build cache server: {e}");
            None
        }
    }
}

/// What sending one queued key did.
pub(crate) enum Sent {
    Uploaded(u64),
    Present,
    Gone,
    TooLarge,
}

pub(crate) async fn send(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Sent> {
    let r = send_one(api, store, key).await;
    record(store, &r);
    if r.is_ok() {
        let _ = std::fs::remove_file(store.root().join(DURATIONS).join(key.to_string()));
    }
    r
}

async fn send_one(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Sent> {
    let rec = recorded(store, key);
    match send_files(api, store, key, rec.as_ref()).await? {
        Some(sent) => Ok(sent),
        None => send_artifact(api, store, key, rec.as_ref()).await,
    }
}

/// Sends `key` on the file-level routes: asks which files the server lacks, uploads those, then
/// the entry. `None` when the server has no such routes, or a file cannot go alone.
async fn send_files(
    api: &HttpApi,
    store: &Store,
    key: &ActionKey,
    rec: Option<&Recorded>,
) -> Result<Option<Sent>> {
    let entry = format!("/v8/asc/ac/{key}");
    let head = api
        .turbo(Method::HEAD, &entry, Bytes::new(), SMALL)
        .await
        .map_err(fatal)?;
    if head.status.is_success() {
        return Ok(Some(Sent::Present));
    }
    let Some((doc, blobs)) = store.entry_doc(key).map_err(io_err)? else {
        return Ok(Some(Sent::Gone));
    };
    let ids: Vec<String> = blobs.iter().map(hex).collect();
    let res = api
        .turbo(
            Method::POST,
            "/v8/asc/cas/missing",
            Bytes::from(json!({ "ids": ids }).to_string()),
            MAX_ENTRY_DOC,
        )
        .await
        .map_err(fatal)?;
    if matches!(
        res.status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ) {
        return Ok(None);
    }
    if !res.status.is_success() {
        return Err(fatal(failure(res)));
    }
    let answer: serde_json::Value = serde_json::from_slice(&whole(res).map_err(fatal)?)
        .map_err(|e| CliError::Fatal(format!("build cache: bad answer: {e}")))?;
    let missing: Vec<[u8; 32]> = answer["missing"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| asc_buildcache::unhex(v.as_str()?))
        .filter(|id| blobs.contains(id))
        .collect();
    let mut bytes = doc.len() as u64;
    let mut sent = futures::stream::iter(missing)
        .map(|id| async move { send_file(api, store, &id).await })
        .buffer_unordered(PARALLEL);
    while let Some(r) = sent.next().await {
        match r? {
            Some(n) => bytes += n,
            // Evicted since, or too large to send alone: the artefact route decides.
            None => return Ok(None),
        }
    }
    drop(sent);
    let res = api
        .turbo_timed(
            Method::PUT,
            &entry,
            Bytes::from(doc),
            SMALL,
            rec.map(|r| r.ms),
            rec.and_then(|r| r.rev.as_deref()),
        )
        .await
        .map_err(fatal)?;
    if !res.status.is_success() {
        return Err(fatal(failure(res)));
    }
    Ok(Some(Sent::Uploaded(bytes)))
}

/// Uploads one file, compressed: its compressed size, or `None` when it is gone or too large.
async fn send_file(api: &HttpApi, store: &Store, id: &[u8; 32]) -> Result<Option<u64>> {
    let Some(z) = store.blob_zstd(id).map_err(io_err)? else {
        return Ok(None);
    };
    let n = z.len() as u64;
    let res = api
        .turbo(
            Method::PUT,
            &format!("/v8/asc/cas/{}", hex(id)),
            Bytes::from(z),
            SMALL,
        )
        .await
        .map_err(fatal)?;
    if !res.status.is_success() {
        return Err(fatal(failure(res)));
    }
    Ok(Some(n))
}

/// Sends `key` as one packed artefact (a server without the file-level routes).
async fn send_artifact(
    api: &HttpApi,
    store: &Store,
    key: &ActionKey,
    rec: Option<&Recorded>,
) -> Result<Sent> {
    let path = format!("/v8/artifacts/{}", artifact_name(key));
    let head = api
        .turbo(Method::HEAD, &path, Bytes::new(), SMALL)
        .await
        .map_err(fatal)?;
    if head.status.is_success() {
        return Ok(Sent::Present);
    }
    if head.status != StatusCode::NOT_FOUND {
        return Err(fatal(failure(head)));
    }
    let mut packed = Vec::new();
    if !store.pack(key, &mut packed).map_err(io_err)? {
        return Ok(Sent::Gone);
    }
    if packed.len() > MAX_ARTIFACT {
        return Ok(Sent::TooLarge);
    }
    let size = packed.len() as u64;
    let res = api
        .turbo_timed(
            Method::PUT,
            &path,
            Bytes::from(packed),
            SMALL,
            rec.map(|r| r.ms),
            rec.and_then(|r| r.rev.as_deref()),
        )
        .await
        .map_err(fatal)?;
    if !res.status.is_success() {
        return Err(fatal(failure(res)));
    }
    Ok(Sent::Uploaded(size))
}

/// `asc cache push`: uploads the queued entries the server lacks, then lists every queued key
/// under the revision.
pub(crate) async fn push(
    ctx: &Ctx,
    remote: Option<&str>,
    rev: Option<&str>,
    json_out: bool,
    out: &mut impl Write,
) -> Result<ExitCode> {
    require_on(ctx)?;
    let store = open_store(ctx)?;
    let explicit = rev.is_some();
    let rev = revision(ctx, rev).await?;
    prune_durations(&store);
    let queued = store.queued().map_err(io_err)?;
    // Only an explicit `--rev` overrides the revision each task recorded; HEAD now is not the
    // revision the build ran at.
    let api = with_rev(
        http_api(ctx, server(ctx, remote).as_deref()).await?,
        rev.as_deref().filter(|_| explicit),
    );
    let (mut uploaded, mut present, mut gone, mut too_large, mut bytes) = (0, 0, 0, 0, 0u64);
    let mut listed = Vec::new();
    let mut results = futures::stream::iter(queued.iter())
        .map(|key| {
            let (api, store) = (&api, &store);
            async move { (key, send(api, store, key).await) }
        })
        .buffer_unordered(PARALLEL);
    while let Some((key, sent)) = results.next().await {
        match sent? {
            Sent::Uploaded(n) => {
                uploaded += 1;
                bytes += n;
                listed.push(artifact_name(key));
            }
            Sent::Present => {
                present += 1;
                listed.push(artifact_name(key));
            }
            Sent::Gone => gone += 1,
            Sent::TooLarge => too_large += 1,
        }
        store.unqueue(key).map_err(io_err)?;
    }
    drop(results);
    listed.sort();
    if let Some(rev) = &rev
        && !listed.is_empty()
    {
        api.post_turbo(
            &format!("/v8/asc/build-revs/{rev}"),
            &json!({ "keys": listed }),
        )
        .await
        .map_err(fatal)?;
    }
    if json_out {
        let v = json!({
            "uploaded": uploaded,
            "present": present,
            "gone": gone,
            "too_large": too_large,
            "bytes": bytes,
            "rev": rev,
        });
        writeln!(out, "{v}")?;
    } else {
        writeln!(
            out,
            "uploaded {uploaded} ({}), {present} already on the server{}{}",
            human_size(bytes),
            if gone > 0 {
                format!(", {gone} evicted before upload")
            } else {
                String::new()
            },
            if too_large > 0 {
                format!(
                    ", {too_large} over {} kept local",
                    human_size(MAX_ARTIFACT as u64)
                )
            } else {
                String::new()
            },
        )?;
        if let Some(rev) = &rev
            && !listed.is_empty()
        {
            writeln!(out, "indexed {} keys under {rev}", listed.len())?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `asc cache warm`: fetches the entries a revision's builds used that the store lacks.
pub(crate) async fn warm(
    ctx: &Ctx,
    remote: Option<&str>,
    rev: Option<&str>,
    json_out: bool,
    out: &mut impl Write,
) -> Result<ExitCode> {
    require_on(ctx)?;
    let store = open_store(ctx)?;
    let Some(rev) = revision(ctx, rev).await? else {
        return Err(CliError::Usage(
            "error: name a revision (outside a work tree, or on an unborn HEAD)".into(),
        ));
    };
    let api = with_rev(
        http_api(ctx, server(ctx, remote).as_deref()).await?,
        Some(&rev),
    );
    let res = api
        .turbo(
            Method::GET,
            &format!("/v8/asc/build-revs/{rev}"),
            Bytes::new(),
            64 << 20,
        )
        .await
        .map_err(fatal);
    record(&store, &res);
    let res = res?;
    if !res.status.is_success() {
        return Err(fatal(failure(res)));
    }
    let doc: serde_json::Value = serde_json::from_slice(&whole(res).map_err(fatal)?)
        .map_err(|e| CliError::Fatal(format!("build cache: bad revision index: {e}")))?;
    let keys: Vec<ActionKey> = doc["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| key_of_artifact(k.as_str()?))
        .collect();
    let wanted: Vec<&ActionKey> = keys
        .iter()
        .filter(|k| !store.entry_path(k).exists())
        .collect();
    let local = keys.len() - wanted.len();
    let (mut fetched, mut missing, mut refused, mut too_large, mut bytes) = (0, 0, 0, 0, 0u64);
    let mut results = futures::stream::iter(wanted)
        .map(|key| {
            let (api, store) = (&api, &store);
            async move { fetch(api, store, key).await }
        })
        .buffer_unordered(PARALLEL);
    while let Some(got) = results.next().await {
        match got {
            Ok(Fetched::Stored(n)) => {
                fetched += 1;
                bytes += n;
            }
            Ok(Fetched::Missing) => missing += 1,
            Ok(Fetched::TooLarge) => too_large += 1,
            Ok(Fetched::Refused(m)) => {
                eprintln!("warning: build cache artefact refused: {m}");
                refused += 1;
            }
            Err(e) => return Err(e),
        }
    }
    drop(results);
    if json_out {
        let v = json!({
            "rev": rev,
            "keys": keys.len(),
            "local": local,
            "fetched": fetched,
            "missing": missing,
            "too_large": too_large,
            "refused": refused,
            "bytes": bytes,
        });
        writeln!(out, "{v}")?;
    } else {
        writeln!(
            out,
            "{rev}: {} keys, {local} already local, fetched {fetched} ({}){}{}",
            keys.len(),
            human_size(bytes),
            if missing + too_large > 0 {
                format!(", {} unavailable", missing + too_large)
            } else {
                String::new()
            },
            if refused > 0 {
                format!(", {refused} refused")
            } else {
                String::new()
            },
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use asc_buildcache::Origin;
    use axum::Router;
    use axum::http::{HeaderMap, Method, StatusCode};

    use super::*;

    const REV: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn key(n: u8) -> ActionKey {
        ActionKey([n; 32])
    }

    /// (method, path, x-asc-rev, x-artifact-duration) of every request.
    type Seen = Arc<Mutex<Vec<(Method, String, Option<String>, Option<String>)>>>;

    async fn server() -> (String, Seen) {
        let seen: Seen = Arc::default();
        let log = seen.clone();
        let app = Router::new().fallback(move |m: Method, u: axum::http::Uri, h: HeaderMap| {
            let log = log.clone();
            async move {
                let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
                log.lock().unwrap().push((
                    m.clone(),
                    u.path().to_owned(),
                    get("x-asc-rev"),
                    get("x-artifact-duration"),
                ));
                if m == Method::PUT {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                }
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(l, app).await });
        (url, seen)
    }

    fn queued_entry(store: &Store, k: &ActionKey) {
        store
            .put_action(k, &Action::new("run", "t", Origin::default()))
            .unwrap();
        store.queue_push(k).unwrap();
    }

    fn put_headers(seen: &Seen) -> (Option<String>, Option<String>) {
        let seen = seen.lock().unwrap();
        let puts: Vec<_> = seen.iter().filter(|r| r.0 == Method::PUT).collect();
        assert_eq!(puts.len(), 1, "{seen:?}");
        (puts[0].2.clone(), puts[0].3.clone())
    }

    #[test]
    fn a_recorded_run_round_trips_with_and_without_a_rev() {
        let (_d, store) = store();
        note_duration(&store, &key(1), Duration::from_millis(1500), Some(REV));
        note_duration(&store, &key(2), Duration::from_millis(7), None);
        note_duration(&store, &key(3), Duration::from_millis(9), Some("not-a-rev"));
        note_duration(&store, &key(4), Duration::from_micros(300), Some(REV));
        let rec = |n| recorded(&store, &key(n));
        assert_eq!(
            rec(1),
            Some(Recorded {
                ms: 1500,
                rev: Some(REV.into())
            })
        );
        assert_eq!(rec(2), Some(Recorded { ms: 7, rev: None }));
        assert_eq!(rec(3), Some(Recorded { ms: 9, rev: None }));
        assert_eq!(rec(4), None, "under a millisecond is not recorded");
        let leftovers = std::fs::read_dir(store.root().join(DURATIONS))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_planted_record_reads_as_absent_and_is_removed() {
        let (_d, store) = store();
        let dir = store.root().join(DURATIONS);
        std::fs::create_dir_all(&dir).unwrap();
        let week = 7 * 24 * 3600 * 1000u64;
        let planted = [
            "-5\n".to_owned(),
            "+5\n".to_owned(),
            " 5\n".to_owned(),
            "5 \n".to_owned(),
            "abc\n".to_owned(),
            "0\n".to_owned(),
            "99999999999999999999999\n".to_owned(),
            format!("{}\n", week + 1),
            format!("5\n{}\n", REV.to_uppercase()),
            format!("5\n{REV}\nextra\n"),
            format!("5\n{}\n", "1".repeat(1000)),
            "5".to_owned(),
        ];
        for (i, text) in planted.iter().enumerate() {
            let k = key(i as u8 + 1);
            let path = dir.join(k.to_string());
            std::fs::write(&path, text).unwrap();
            assert_eq!(recorded(&store, &k), None, "{text:?}");
            assert!(!path.exists(), "{text:?}");
        }
        let ok = key(99);
        std::fs::write(dir.join(ok.to_string()), format!("{week}\n")).unwrap();
        assert_eq!(
            recorded(&store, &ok),
            Some(Recorded {
                ms: week,
                rev: None
            })
        );
    }

    #[tokio::test]
    async fn the_push_sends_the_recorded_rev_and_duration_then_forgets_them() {
        let (_d, store) = store();
        let (url, seen) = server().await;
        let k = key(5);
        queued_entry(&store, &k);
        note_duration(&store, &k, Duration::from_millis(2500), Some(REV));
        // The client has no rev of its own (HEAD now is not the build's), so the recorded one goes.
        let api = HttpApi::new(&url, "o/r", None).unwrap();
        assert!(matches!(
            send(&api, &store, &k).await.unwrap(),
            Sent::Uploaded(_)
        ));
        assert_eq!(put_headers(&seen), (Some(REV.into()), Some("2500".into())));
        assert!(!record_path(&store, &k).exists());
    }

    #[tokio::test]
    async fn an_explicit_rev_wins_over_the_recorded_one() {
        let (_d, store) = store();
        let (url, seen) = server().await;
        let k = key(6);
        queued_entry(&store, &k);
        note_duration(&store, &k, Duration::from_millis(10), Some(REV));
        let api = HttpApi::new(&url, "o/r", None).unwrap().with_rev(OTHER);
        send(&api, &store, &k).await.unwrap();
        assert_eq!(put_headers(&seen), (Some(OTHER.into()), Some("10".into())));
    }

    #[tokio::test]
    async fn without_a_record_the_push_sends_neither_header() {
        let (_d, store) = store();
        let (url, seen) = server().await;
        let k = key(7);
        queued_entry(&store, &k);
        let api = HttpApi::new(&url, "o/r", None).unwrap();
        send(&api, &store, &k).await.unwrap();
        assert_eq!(put_headers(&seen), (None, None));
    }

    #[tokio::test]
    async fn a_planted_record_sends_no_header_and_is_removed() {
        let (_d, store) = store();
        let (url, seen) = server().await;
        let k = key(8);
        queued_entry(&store, &k);
        let dir = store.root().join(DURATIONS);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(k.to_string()), "-3\n").unwrap();
        let api = HttpApi::new(&url, "o/r", None).unwrap();
        send(&api, &store, &k).await.unwrap();
        assert_eq!(put_headers(&seen), (None, None));
        assert!(!dir.join(k.to_string()).exists());
    }

    #[tokio::test]
    async fn the_read_of_a_build_carries_the_rev() {
        let (_d, store) = store();
        let (url, seen) = server().await;
        let api = HttpApi::new(&url, "o/r", None).unwrap().with_rev(REV);
        assert!(fetch_entry_on(&api, &store, &key(9)).await.is_none());
        let seen = seen.lock().unwrap();
        let gets: Vec<_> = seen.iter().filter(|r| r.0 == Method::GET).collect();
        assert_eq!(gets.len(), 2, "{seen:?}");
        assert!(gets.iter().all(|r| r.2.as_deref() == Some(REV)));
    }

    #[test]
    fn pruning_keeps_only_records_of_queued_keys() {
        let (_d, store) = store();
        let (a, b) = (key(10), key(11));
        store.queue_push(&a).unwrap();
        note_duration(&store, &a, Duration::from_millis(5), None);
        note_duration(&store, &b, Duration::from_millis(5), None);
        prune_durations(&store);
        assert!(record_path(&store, &a).exists());
        assert!(!record_path(&store, &b).exists());
    }

    #[test]
    fn a_failed_queueing_leaves_no_record() {
        let (_d, store) = store();
        // A file where the outbox directory belongs: queueing fails.
        std::fs::write(store.root().join("outbox"), b"").unwrap();
        let k = key(12);
        assert!(queue_timed(&store, &k, Duration::from_millis(5), Some(REV)).is_err());
        assert!(!record_path(&store, &k).exists());
    }
}
