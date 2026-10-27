//! The advisory source: OSV over the network, an advisory file, or both.
//!
//! * OSV (`https://api.osv.dev`, or a mirror with the same API): one `POST /v1/querybatch` per up to
//!   1,000 package versions answers with advisory ids, `GET /v1/vulns/<id>` gives each record. Both are
//!   cached in `deps_cache` for their TTL.
//! * A file of OSV records ([`FileSource`]), read again when it changes, judged with this crate's own
//!   range evaluation.
//!
//! A package whose answer could not be had is [`Lookup::Unknown`], never an empty list: a failed batch,
//! a record that could not be read, an advisory file that does not load. With both sources configured
//! both have to answer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::json;
use sqlx::AnyPool;

use super::advisory::{Index, Record};
use super::config::Endpoint;
use super::egress::{Egress, Limits};
use super::model::{Advisory, Ecosystem, Lookup};
use super::store;

/// Most package versions in one `querybatch`.
pub const BATCH: usize = 1000;
/// Most result pages followed for one package.
const MAX_PAGES: usize = 10;
/// Most advisories kept for one package version.
const MAX_IDS: usize = 500;
/// Most error lines an answer reports.
const MAX_ERRORS: usize = 20;
/// Longest advisory id read.
const MAX_ID: usize = 128;
/// How long the advisory file is trusted before its modification time is looked at again.
const FILE_RECHECK: Duration = Duration::from_secs(30);

/// The clock, replaceable in tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(crate::ai::clock::now)
}

/// A package version to ask about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Query {
    pub eco: Ecosystem,
    pub name: String,
    pub version: String,
}

/// The answers, one per query in order, and why some are unknown.
#[derive(Debug, Default)]
pub struct Answer {
    pub lookups: Vec<Lookup>,
    pub errors: Vec<String>,
}

// ------------------------------------------------------------------------------ the file

struct FileState {
    stamp: Option<(SystemTime, u64)>,
    checked: Option<Instant>,
    index: Option<Arc<Index>>,
}

/// An advisory file that is read when it changes.
pub struct FileSource {
    path: PathBuf,
    max_bytes: u64,
    state: tokio::sync::Mutex<FileState>,
}

impl FileSource {
    pub fn new(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            state: tokio::sync::Mutex::new(FileState {
                stamp: None,
                checked: None,
                index: None,
            }),
        }
    }

    /// The index of the file as it is now. `refresh` looks at the file even if it was looked at lately.
    pub async fn current(&self, refresh: bool) -> Result<Arc<Index>, String> {
        let mut st = self.state.lock().await;
        if !refresh
            && let (Some(i), Some(at)) = (&st.index, st.checked)
            && at.elapsed() < FILE_RECHECK
        {
            return Ok(i.clone());
        }
        let meta = tokio::fs::metadata(&self.path)
            .await
            .map_err(|e| format!("the advisory file cannot be read ({})", e.kind()))?;
        let stamp = (
            meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            meta.len(),
        );
        if let Some(i) = &st.index
            && st.stamp == Some(stamp)
        {
            st.checked = Some(Instant::now());
            return Ok(i.clone());
        }
        let path = self.path.clone();
        let max = self.max_bytes;
        let index = tokio::task::spawn_blocking(move || Index::load(&path, max))
            .await
            .map_err(|_| "the advisory file reader stopped".to_owned())?
            .map_err(|e| {
                format!(
                    "the advisory file is not usable: {}",
                    e.replace(['\n', '\r'], " ")
                )
            })?;
        let index = Arc::new(index);
        st.stamp = Some(stamp);
        st.checked = Some(Instant::now());
        st.index = Some(index.clone());
        Ok(index)
    }
}

// ------------------------------------------------------------------------------ OSV

#[derive(Deserialize)]
struct BatchResponse {
    #[serde(default)]
    results: Vec<BatchResult>,
}

#[derive(Deserialize, Default)]
struct BatchResult {
    #[serde(default)]
    vulns: Vec<VulnRef>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct VulnRef {
    id: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// `base` with `segments` appended as path segments (percent-encoded).
pub fn join(base: &str, segments: &[&str]) -> Result<String, String> {
    let mut url = url::Url::parse(base).map_err(|_| "the base URL is not a URL".to_owned())?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| "the base URL cannot carry a path".to_owned())?;
        path.pop_if_empty();
        path.extend(segments);
    }
    Ok(url.to_string())
}

fn push_error(errors: &mut Vec<String>, e: String) {
    if errors.len() < MAX_ERRORS && !errors.contains(&e) {
        errors.push(e);
    }
}

fn osv_name(q: &Query) -> String {
    match q.eco {
        Ecosystem::PyPi => q.eco.canonical(&q.name),
        _ => q.name.clone(),
    }
}

pub struct Osv {
    egress: Arc<Egress>,
    pool: AnyPool,
    endpoint: Endpoint,
    file: Option<Arc<FileSource>>,
    osv_ttl: u64,
    vuln_ttl: u64,
    concurrency: usize,
    clock: Clock,
}

/// What the cache and the network gave for the ids of one package version.
enum Ids {
    Found(Vec<String>),
    Failed(String),
}

impl Osv {
    pub fn new(
        egress: Arc<Egress>,
        pool: AnyPool,
        cfg: &super::config::Config,
        clock: Clock,
    ) -> Self {
        Self {
            egress,
            pool,
            endpoint: cfg.osv.clone(),
            file: cfg
                .advisory_file
                .clone()
                .map(|p| Arc::new(FileSource::new(p, cfg.advisory_max_bytes.0))),
            osv_ttl: cfg.osv_ttl_secs,
            vuln_ttl: cfg.vuln_ttl_secs,
            concurrency: cfg.concurrency,
            clock,
        }
    }

    /// `osv`, `osv-mirror`, `file` or `osv+file`.
    pub fn source_name(&self) -> &'static str {
        match (self.endpoint.enabled(), self.file.is_some()) {
            (true, true) => "osv+file",
            (true, false) if self.endpoint.url == "https://api.osv.dev" => "osv",
            (true, false) => "osv-mirror",
            _ => "file",
        }
    }

    fn limits(&self) -> Limits {
        Limits {
            timeout: Duration::from_secs(self.endpoint.timeout_secs),
            max_bytes: usize::try_from(self.endpoint.max_bytes.0).unwrap_or(usize::MAX),
        }
    }

    /// Whether the OSV API is asked about `eco`.
    fn api_covers(&self, eco: Ecosystem) -> bool {
        self.endpoint.enabled() && eco.osv_covered()
    }

    /// The cache key is exactly what is sent to OSV, so two spellings OSV tells apart never share an
    /// answer.
    fn cache_key(q: &Query) -> String {
        format!("{}|{}|{}", q.eco.osv_name(), osv_name(q), q.version)
    }

    async fn cached_ids(&self, q: &Query, now: i64) -> Option<Vec<String>> {
        let text = store::cache_get(&self.pool, "osv", &Self::cache_key(q), self.osv_ttl, now)
            .await
            .ok()
            .flatten()?;
        serde_json::from_str::<Vec<String>>(&text).ok()
    }

    /// Asks the API about `qs` (all covered), a batch at a time.
    async fn fetch_ids(&self, qs: &[&Query], errors: &mut Vec<String>) -> Vec<Ids> {
        let lim = self.limits();
        let mut out: Vec<Ids> = Vec::with_capacity(qs.len());
        let url = match join(&self.endpoint.url, &["v1", "querybatch"]) {
            Ok(u) => u,
            Err(e) => {
                push_error(errors, format!("OSV: {e}"));
                return qs.iter().map(|_| Ids::Failed(e.clone())).collect();
            }
        };
        for chunk in qs.chunks(BATCH) {
            let queries: Vec<serde_json::Value> = chunk
                .iter()
                .map(|q| {
                    json!({
                        "package": {"name": osv_name(q), "ecosystem": q.eco.osv_name()},
                        "version": q.version,
                    })
                })
                .collect();
            let body = json!({ "queries": queries }).to_string();
            let answer = self
                .egress
                .post_retry(&url, body.as_bytes(), &lim)
                .await
                .map_err(|e| format!("OSV query failed: {e}"))
                .and_then(|b| {
                    serde_json::from_slice::<BatchResponse>(&b)
                        .map_err(|_| "OSV query failed: the answer is not a result list".to_owned())
                })
                .and_then(|r| {
                    if r.results.len() == chunk.len() {
                        Ok(r.results)
                    } else {
                        Err("OSV query failed: the answer does not match the question".to_owned())
                    }
                });
            let results = match answer {
                Ok(r) => r,
                Err(e) => {
                    push_error(errors, e.clone());
                    out.extend(chunk.iter().map(|_| Ids::Failed(e.clone())));
                    continue;
                }
            };
            for (q, mut r) in chunk.iter().zip(results) {
                let mut ids: Vec<String> = r.vulns.drain(..).map(|v| v.id).collect();
                let mut token = r.next_page_token.take();
                let mut failed = None;
                let mut pages = 0;
                while let Some(t) = token.take() {
                    pages += 1;
                    if pages > MAX_PAGES {
                        failed = Some("OSV query failed: too many result pages".to_owned());
                        break;
                    }
                    let body = json!({"queries": [{
                        "package": {"name": osv_name(q), "ecosystem": q.eco.osv_name()},
                        "version": q.version,
                        "page_token": t,
                    }]})
                    .to_string();
                    let page = self
                        .egress
                        .post_retry(&url, body.as_bytes(), &lim)
                        .await
                        .map_err(|e| format!("OSV query failed: {e}"))
                        .and_then(|b| {
                            serde_json::from_slice::<BatchResponse>(&b).map_err(|_| {
                                "OSV query failed: the answer is not a result list".to_owned()
                            })
                        })
                        .and_then(|mut r| {
                            if r.results.len() == 1 {
                                Ok(r.results.remove(0))
                            } else {
                                Err("OSV query failed: the answer does not match the question"
                                    .to_owned())
                            }
                        });
                    match page {
                        Ok(mut p) => {
                            ids.extend(p.vulns.drain(..).map(|v| v.id));
                            token = p.next_page_token.take();
                        }
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                }
                if let Some(e) = failed {
                    push_error(errors, e.clone());
                    out.push(Ids::Failed(e));
                    continue;
                }
                ids.sort();
                ids.dedup();
                if ids.len() > MAX_IDS {
                    let e = "OSV query failed: more advisories than can be kept".to_owned();
                    push_error(errors, e.clone());
                    out.push(Ids::Failed(e));
                    continue;
                }
                out.push(Ids::Found(ids));
            }
        }
        out
    }

    /// The record `id`, from the cache or the API.
    async fn record(&self, id: &str, refresh: bool) -> Result<Arc<Record>, String> {
        if !valid_id(id) {
            return Err("an advisory id is not a plain identifier".into());
        }
        let now = (self.clock)();
        if !refresh
            && let Ok(Some(text)) =
                store::cache_get(&self.pool, "vuln", id, self.vuln_ttl, now).await
            && let Ok(r) = serde_json::from_str::<Record>(&text)
        {
            return Ok(Arc::new(r));
        }
        let url = join(&self.endpoint.url, &["v1", "vulns", id])?;
        let body = self
            .egress
            .get_retry(&url, &self.limits(), None)
            .await
            .map_err(|e| format!("advisory {id} could not be read: {e}"))?;
        let text = String::from_utf8(body)
            .map_err(|_| format!("advisory {id} could not be read: not UTF-8"))?;
        let record: Record = serde_json::from_str(&text)
            .map_err(|_| format!("advisory {id} could not be read: not an OSV record"))?;
        if record.id != id {
            return Err(format!(
                "advisory {id} could not be read: the record is for another id"
            ));
        }
        // A cache that cannot be written costs a call next time, not the answer.
        let _ = store::cache_put(&self.pool, "vuln", id, &text, now).await;
        Ok(Arc::new(record))
    }

    /// One answer per query, in order.
    pub async fn lookup(&self, queries: &[Query], refresh: bool) -> Answer {
        let now = (self.clock)();
        let mut errors: Vec<String> = Vec::new();
        // Distinct package versions, and which queries want each.
        let mut slot_of: HashMap<(Ecosystem, String, String), usize> = HashMap::new();
        let mut unique: Vec<Query> = Vec::new();
        let mut want: Vec<usize> = Vec::with_capacity(queries.len());
        for q in queries {
            let key = (q.eco, osv_name(q), q.version.clone());
            let i = *slot_of.entry(key).or_insert_with(|| {
                unique.push(q.clone());
                unique.len() - 1
            });
            want.push(i);
        }
        let mut results: Vec<Option<Lookup>> = vec![None; unique.len()];

        // The file first: it is local.
        let mut from_file: Vec<Vec<Advisory>> = vec![Vec::new(); unique.len()];
        if let Some(f) = &self.file {
            match f.current(refresh).await {
                Ok(index) => {
                    for (i, q) in unique.iter().enumerate() {
                        from_file[i] = index.lookup(q.eco, &q.name, &q.version);
                    }
                }
                Err(e) => {
                    push_error(&mut errors, e.clone());
                    for r in &mut results {
                        *r = Some(Lookup::Unknown(e.clone()));
                    }
                }
            }
        }

        // The API: the cache, then the network for what is missing.
        let mut ids: Vec<Option<Ids>> = (0..unique.len()).map(|_| None).collect();
        let mut missing: Vec<usize> = Vec::new();
        for (i, q) in unique.iter().enumerate() {
            if results[i].is_some() || !self.api_covers(q.eco) {
                continue;
            }
            match if refresh {
                None
            } else {
                self.cached_ids(q, now).await
            } {
                Some(found) => ids[i] = Some(Ids::Found(found)),
                None => missing.push(i),
            }
        }
        if !missing.is_empty() {
            let asked: Vec<&Query> = missing.iter().map(|i| &unique[*i]).collect();
            let got = self.fetch_ids(&asked, &mut errors).await;
            for (i, g) in missing.iter().zip(got) {
                if let Ids::Found(list) = &g
                    && let Ok(text) = serde_json::to_string(list)
                {
                    let _ = store::cache_put(
                        &self.pool,
                        "osv",
                        &Self::cache_key(&unique[*i]),
                        &text,
                        now,
                    )
                    .await;
                }
                ids[*i] = Some(g);
            }
        }

        // The records of every advisory named.
        let mut wanted: Vec<String> = ids
            .iter()
            .flatten()
            .filter_map(|i| match i {
                Ids::Found(l) => Some(l.iter().cloned()),
                Ids::Failed(_) => None,
            })
            .flatten()
            .collect();
        wanted.sort();
        wanted.dedup();
        let fetched: Vec<(String, Result<Arc<Record>, String>)> =
            futures::stream::iter(wanted.into_iter().map(|id| async move {
                let r = self.record(&id, refresh).await;
                (id, r)
            }))
            .buffer_unordered(self.concurrency.max(1))
            .collect()
            .await;
        let records: BTreeMap<String, Result<Arc<Record>, String>> = fetched.into_iter().collect();

        for (i, q) in unique.iter().enumerate() {
            if results[i].is_some() {
                continue;
            }
            let api: Option<Result<Vec<Advisory>, String>> = match ids[i].take() {
                None => None,
                Some(Ids::Failed(e)) => Some(Err(e)),
                Some(Ids::Found(list)) => {
                    let mut adv = Vec::new();
                    let mut err = None;
                    for id in list {
                        match records.get(&id) {
                            Some(Ok(r)) => adv.push(r.advisory(q.eco, &q.name)),
                            Some(Err(e)) => {
                                push_error(&mut errors, e.clone());
                                err = Some(e.clone());
                            }
                            None => err = Some(format!("advisory {id} was not read")),
                        }
                    }
                    Some(err.map_or(Ok(adv), Err))
                }
            };
            results[i] = Some(match api {
                None if self.file.is_none() => Lookup::Uncovered,
                None => Lookup::Known(std::mem::take(&mut from_file[i])),
                Some(Err(e)) => Lookup::Unknown(e),
                Some(Ok(mut adv)) => {
                    let have: HashSet<String> = adv.iter().map(|a| a.id.clone()).collect();
                    adv.extend(
                        std::mem::take(&mut from_file[i])
                            .into_iter()
                            .filter(|a| !have.contains(&a.id)),
                    );
                    adv.sort_by(|a, b| a.id.cmp(&b.id));
                    Lookup::Known(adv)
                }
            });
        }
        Answer {
            lookups: want
                .into_iter()
                .map(|i| {
                    results[i]
                        .clone()
                        .unwrap_or(Lookup::Unknown("no answer".into()))
                })
                .collect(),
            errors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(eco: Ecosystem, name: &str) -> Query {
        Query {
            eco,
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    #[test]
    fn names_osv_tells_apart_never_share_a_cache_entry() {
        // npm names are case-sensitive: OSV is asked about each spelling separately.
        for eco in [Ecosystem::Npm, Ecosystem::Go, Ecosystem::Maven, Ecosystem::Packagist, Ecosystem::NuGet, Ecosystem::CratesIo] {
            assert_ne!(
                Osv::cache_key(&q(eco, "Foo")),
                Osv::cache_key(&q(eco, "foo")),
                "{eco}"
            );
        }
        assert_ne!(
            Osv::cache_key(&q(Ecosystem::CratesIo, "foo_bar")),
            Osv::cache_key(&q(Ecosystem::CratesIo, "foo-bar"))
        );
        // PyPI names are normalised before they are sent, so their spellings are one query.
        assert_eq!(
            Osv::cache_key(&q(Ecosystem::PyPi, "Foo_Bar")),
            Osv::cache_key(&q(Ecosystem::PyPi, "foo-bar"))
        );
        assert_eq!(osv_name(&q(Ecosystem::PyPi, "Foo_Bar")), "foo-bar");
        assert_eq!(osv_name(&q(Ecosystem::Npm, "Foo_Bar")), "Foo_Bar");
        // Versions and ecosystems are part of the key.
        let mut v2 = q(Ecosystem::Npm, "foo");
        v2.version = "2.0.0".into();
        assert_ne!(Osv::cache_key(&q(Ecosystem::Npm, "foo")), Osv::cache_key(&v2));
        assert_ne!(
            Osv::cache_key(&q(Ecosystem::Npm, "foo")),
            Osv::cache_key(&q(Ecosystem::Hex, "foo"))
        );
    }
}
