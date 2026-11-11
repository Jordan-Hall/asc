//! The server tier of the build cache (docs/designs/build-cache.md §3): an entry and each output
//! file travel separately on the file-level routes (§3.1), so one changed file is the only upload
//! and, on a machine that holds the rest, the only download. A server without those routes gets
//! the whole entry as one packed artefact named `asc-<key>`. Each revision lists the keys its
//! builds used, so `asc cache warm` can fetch them before the next build.

use std::io::Write;
use std::process::ExitCode;

use asc_buildcache::{
    Action, ActionKey, MAX_ENTRY_DOC, Store, artifact_name, hex, human_size, key_of_artifact,
    parse_entry,
};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::{Method, StatusCode};
use serde_json::json;

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
const MAX_FILE_BODY: usize = 512 << 20;

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
    let blobs = match parse_entry(&doc) {
        Ok((_, blobs)) => blobs,
        Err(e) => return Ok(Fetched::Refused(e.to_string())),
    };
    let wanted: Vec<[u8; 32]> = blobs.into_iter().filter(|b| !store.has_blob(b)).collect();
    let mut bytes = doc.len() as u64;
    let mut got = futures::stream::iter(wanted)
        .map(|id| async move { (id, fetch_file(api, store, &id).await) })
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

async fn fetch_file(api: &HttpApi, store: &Store, id: &[u8; 32]) -> Result<FileGot> {
    let res = api
        .turbo(
            Method::GET,
            &format!("/v8/asc/cas/{}", hex(id)),
            Bytes::new(),
            MAX_FILE_BODY,
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
    Ok(match store.put_blob_zstd(id, &body) {
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

/// A remote hit for `key`, or `None` on a miss, without a server, or when the server cannot be
/// reached (the build goes on as on a miss).
pub(crate) async fn fetch_entry(ctx: &Ctx, store: &Store, key: &ActionKey) -> Option<Action> {
    let api = http_api(ctx, server(ctx, None).as_deref()).await.ok()?;
    match fetch(&api, store, key).await {
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
enum Sent {
    Uploaded(u64),
    Present,
    Gone,
    TooLarge,
}

async fn send(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Sent> {
    let r = send_one(api, store, key).await;
    record(store, &r);
    r
}

async fn send_one(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Sent> {
    match send_files(api, store, key).await? {
        Some(sent) => Ok(sent),
        None => send_artifact(api, store, key).await,
    }
}

/// Sends `key` on the file-level routes: asks which files the server lacks, uploads those, then
/// the entry. `None` when the server has no such routes, or a file cannot go alone.
async fn send_files(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Option<Sent>> {
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
        .turbo(Method::PUT, &entry, Bytes::from(doc), SMALL)
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
async fn send_artifact(api: &HttpApi, store: &Store, key: &ActionKey) -> Result<Sent> {
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
        .turbo(Method::PUT, &path, Bytes::from(packed), SMALL)
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
    let rev = revision(ctx, rev).await?;
    let queued = store.queued().map_err(io_err)?;
    let api = http_api(ctx, server(ctx, remote).as_deref()).await?;
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
    let api = http_api(ctx, server(ctx, remote).as_deref()).await?;
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
