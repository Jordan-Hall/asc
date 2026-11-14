//! The state `DepsService` shares, and the read side of it: scanning a revision, findings, dismissals,
//! the repository settings and the landing check.
//!
//! The settings that bind a call (`.asc/deps.toml`: ecosystems, ignores, thresholds) are read from the
//! *trunk tip*, never from the revision being scanned or the change being judged, so a change cannot lift
//! the rules that apply to it. The updates and proposals are in [`super::updates`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use asc_core::id::ChangesetId;
use asc_core::policy::DepSeverity;
use asc_rpc::server::{CallCtx, MAX_STRING, wire};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use super::config::Config;
use super::egress::Egress;
use super::gate::{CheckRequest, DepsCheck, MAX_LINES, dependency_files};
use super::intel::{Feed, FeedState, FeedStatus, Intel, IntelSource};
use super::latest::Latest;
use super::model::{Advisory, Dep, Ecosystem, Lookup};
use super::osv::{Clock, Osv, Query};
use super::parse::{MAX_FILE_BYTES, parse_file};
use super::plan::effective;
use super::progress::{Phase, Scans, Tick};
use super::repo_config::{self, Config as RepoConfig};
use super::scan::{self, Snapshot, count_by_ecosystem, distinct};
use super::store::{self, FindingRow, NewFinding, PackageKey, State};
use crate::agents::repo::RepoView;
use crate::audit::NewEvent;
use crate::audit::actor::actor_of;
use crate::idmode::IdCodec;
use crate::mutations::ServerMutations;
use crate::registry::{Registry, RepoEntry};
use crate::review::{after_token, internal, page_limit, parse_after, registry_status, sql};

/// Findings in one `ListFindings` page when the request names no limit.
pub const DEFAULT_PAGE: usize = 50;
/// Findings in one `ListFindings` page, at most.
pub const MAX_PAGE: usize = 200;
/// Longest note on a finding, in bytes.
pub const MAX_NOTE: usize = 1024;
/// Findings the exploited sort and `only_exploited` judge, at most (the newest ones).
pub const MAX_SORTED: usize = 10_000;
/// Most file notes in one `Sbom` answer.
pub const MAX_SBOM_NOTES: usize = 20;
/// Most dependency files one landing check reads.
pub const MAX_GATE_FILES: usize = 200;
/// Most new package versions one landing check looks up.
pub const MAX_GATE_PACKAGES: usize = 2000;

pub struct DepsHub {
    pub cfg: Config,
    pub(crate) registry: Arc<Registry>,
    pub(crate) view: RepoView,
    pub(crate) osv: Osv,
    /// The KEV and EPSS feeds, held in memory.
    pub(crate) intel: IntelSource,
    pub(crate) latest: Latest,
    pub(crate) commits: Arc<ServerMutations>,
    permits: Semaphore,
    /// The https URL clients use, for the scheduled workflow.
    pub(crate) server_url: Option<String>,
    pub(crate) clock: Clock,
    /// The scans running on this process, for `ScanStatus`.
    pub(crate) scans: Arc<Scans>,
}

/// The repository settings in force.
pub struct Loaded {
    pub config: RepoConfig,
    /// `file` or `default`.
    pub source: &'static str,
    pub text: String,
    /// Why the file cannot be used, or empty.
    pub error: String,
}

/// What the advisory source said about the packages of a revision.
pub struct Assessed {
    pub answers: HashMap<(Ecosystem, String, String), Lookup>,
    pub errors: Vec<String>,
    /// More distinct package versions than `max_packages`.
    pub capped: bool,
    pub source: &'static str,
}

impl Assessed {
    fn answer(&self, d: &Dep) -> Option<&Lookup> {
        self.answers
            .get(&(d.ecosystem, d.name.clone(), d.version.clone()))
    }
}

pub fn ecosystems_of(list: &[String]) -> Result<Vec<Ecosystem>, Status> {
    let mut out = Vec::new();
    for e in list {
        let eco = Ecosystem::parse(e)
            .ok_or_else(|| Status::invalid_argument(format!("{e:?} is not a known ecosystem")))?;
        if !out.contains(&eco) {
            out.push(eco);
        }
    }
    Ok(out)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn sev_wire(s: Option<DepSeverity>) -> i32 {
    (match s {
        None => wire::DepSeverity::Unspecified,
        Some(DepSeverity::Low) => wire::DepSeverity::Low,
        Some(DepSeverity::Moderate) => wire::DepSeverity::Moderate,
        Some(DepSeverity::High) => wire::DepSeverity::High,
        Some(DepSeverity::Critical) => wire::DepSeverity::Critical,
    }) as i32
}

pub fn sev_from_wire(n: i32) -> Result<Option<DepSeverity>, Status> {
    Ok(
        match wire::DepSeverity::try_from(n)
            .map_err(|_| Status::invalid_argument("unknown severity"))?
        {
            wire::DepSeverity::Unspecified => None,
            wire::DepSeverity::Low => Some(DepSeverity::Low),
            wire::DepSeverity::Moderate => Some(DepSeverity::Moderate),
            wire::DepSeverity::High => Some(DepSeverity::High),
            wire::DepSeverity::Critical => Some(DepSeverity::Critical),
        },
    )
}

pub fn kind_wire(k: Option<super::version::UpdateKind>) -> i32 {
    use super::version::UpdateKind as K;
    (match k {
        None => wire::DepUpdateKind::Unspecified,
        Some(K::Patch) => wire::DepUpdateKind::Patch,
        Some(K::Minor) => wire::DepUpdateKind::Minor,
        Some(K::Major) => wire::DepUpdateKind::Major,
    }) as i32
}

pub fn kind_from_wire(n: i32) -> Result<Option<super::version::UpdateKind>, Status> {
    use super::version::UpdateKind as K;
    Ok(
        match wire::DepUpdateKind::try_from(n)
            .map_err(|_| Status::invalid_argument("unknown update kind"))?
        {
            wire::DepUpdateKind::Unspecified => None,
            wire::DepUpdateKind::Patch => Some(K::Patch),
            wire::DepUpdateKind::Minor => Some(K::Minor),
            wire::DepUpdateKind::Major => Some(K::Major),
        },
    )
}

fn state_wire(s: State) -> i32 {
    (match s {
        State::Open => wire::DepFindingState::Open,
        State::Dismissed => wire::DepFindingState::Dismissed,
        State::Fixed => wire::DepFindingState::Fixed,
    }) as i32
}

fn reason_wire(code: i64) -> i32 {
    i32::try_from(code).unwrap_or(0)
}

/// The exploitation feeds as they stand for one call: what they hold, and which of them the operator
/// configured (a feed that is not configured says nothing, so it cannot make an answer unknown).
pub struct IntelView {
    pub(crate) intel: Intel,
    pub(crate) kev: FeedStatus,
    pub(crate) epss: FeedStatus,
}

impl IntelView {
    pub(crate) fn kev_usable(&self) -> bool {
        self.intel.kev.is_some()
    }

    /// Whether KEV lists the advisory named by `ids` (its id and aliases).
    fn kev_lists<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> bool {
        matches!(self.intel.lookup(ids).kev, Feed::Listed(_))
    }

    /// Why a feed cannot be used, in words that carry no URL or path.
    fn feed_problem(s: &FeedStatus) -> &'static str {
        match s.state {
            FeedState::Unconfigured => "not configured",
            FeedState::Unknown => "not loaded, or its copy is too old",
            FeedState::Current => "current",
        }
    }

    /// What the feeds say about `a`. Listed when KEV lists it or EPSS has a row; not listed only when
    /// every configured feed is loaded and has none; unknown otherwise.
    pub fn of(&self, a: &Advisory) -> wire::DepIntel {
        let ids = std::iter::once(a.id.as_str()).chain(a.aliases.iter().map(String::as_str));
        let hit = self.intel.lookup(ids);
        let kev = match &hit.kev {
            Feed::Listed(k) => Some(k),
            _ => None,
        };
        let epss = match &hit.epss {
            Feed::Listed(e) => Some(*e),
            _ => None,
        };
        let configured = |s: &FeedStatus| s.state != FeedState::Unconfigured;
        let settled = |unknown: bool, s: &FeedStatus| !configured(s) || !unknown;
        let kev_unknown = matches!(hit.kev, Feed::Unknown);
        let epss_unknown = matches!(hit.epss, Feed::Unknown);
        let status = if kev.is_some() || epss.is_some() {
            wire::DepIntelStatus::Listed
        } else if (configured(&self.kev) || configured(&self.epss))
            && settled(kev_unknown, &self.kev)
            && settled(epss_unknown, &self.epss)
        {
            wire::DepIntelStatus::NotListed
        } else {
            wire::DepIntelStatus::Unknown
        };
        wire::DepIntel {
            status: status as i32,
            cve: hit.cve.first().cloned().unwrap_or_default(),
            kev: kev.is_some(),
            kev_added: kev.and_then(|k| k.date_added.clone()).unwrap_or_default(),
            kev_due: kev.and_then(|k| k.due_date.clone()).unwrap_or_default(),
            kev_ransomware: kev.is_some_and(|k| k.ransomware),
            epss: epss.map_or(0.0, |e| e.score),
            epss_percentile: epss.map_or(0.0, |e| e.percentile),
            epss_date: if epss.is_some() {
                self.epss.date.clone().unwrap_or_default()
            } else {
                String::new()
            },
        }
    }
}

fn feed_wire(s: &FeedStatus) -> wire::DepIntelFeeds {
    wire::DepIntelFeeds {
        state: (match s.state {
            FeedState::Unconfigured => wire::DepFeedState::Unconfigured,
            FeedState::Unknown => wire::DepFeedState::Unknown,
            FeedState::Current => wire::DepFeedState::Current,
        }) as i32,
        version: s.version.clone().unwrap_or_default(),
        count: u32::try_from(s.count).unwrap_or(u32::MAX),
        loaded_at: s.loaded_at.unwrap_or(0),
        age_seconds: s.age_secs.unwrap_or(0),
    }
}

/// A page token for the in-memory orders: the offset of the next finding, as plain digits.
/// An offset at or past the end is a valid token: the page it names is empty.
fn offset_of(scope: &str, who: &str, token: &str) -> Result<usize, Status> {
    if token.is_empty() {
        return Ok(0);
    }
    let bad = || Status::invalid_argument("invalid page token");
    let payload = crate::pagetoken::open(scope, who, token)?;
    if payload.is_empty()
        || !payload.bytes().all(|b| b.is_ascii_digit())
        || (payload.len() > 1 && payload.starts_with('0'))
    {
        return Err(bad());
    }
    let n: usize = payload.parse().map_err(|_| bad())?;
    if n == 0 {
        return Err(bad());
    }
    Ok(n)
}

/// Cuts `rows` to `cap`; true when something was cut (the store returns one row more than it was asked for).
pub(crate) fn cut<T>(rows: &mut Vec<T>, cap: usize) -> bool {
    let over = rows.len() > cap;
    rows.truncate(cap);
    over
}

/// A hash of the filters of a findings list, so that an offset token is only good for the filters it
/// was issued under.
pub(crate) fn filter_hash(
    states: &[State],
    ecosystems: &[String],
    min: i64,
    name: &str,
    sort: wire::DepFindingSort,
    only_exploited: bool,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut st: Vec<i64> = states.iter().map(|s| *s as i64).collect();
    st.sort_unstable();
    st.hash(&mut h);
    let mut eco = ecosystems.to_vec();
    eco.sort_unstable();
    eco.hash(&mut h);
    (min, name, sort as i32, only_exploited).hash(&mut h);
    h.finish()
}

/// The order of the exploited sort: KEV-listed first, then EPSS descending, then CVSS descending, then the
/// newer finding.
fn exploited_order(
    a: &(FindingRow, wire::DepIntel),
    b: &(FindingRow, wire::DepIntel),
) -> std::cmp::Ordering {
    b.1.kev
        .cmp(&a.1.kev)
        .then_with(|| b.1.epss.total_cmp(&a.1.epss))
        .then_with(|| b.0.advisory.cvss.total_cmp(&a.0.advisory.cvss))
        .then_with(|| b.0.id.cmp(&a.0.id))
}

pub fn finding_wire(r: &FindingRow, intel: &IntelView) -> wire::DepFinding {
    finding_with(r, intel.of(&r.advisory))
}

fn finding_with(r: &FindingRow, intel: wire::DepIntel) -> wire::DepFinding {
    wire::DepFinding {
        id: u64::try_from(r.id).unwrap_or(0),
        ecosystem: r.ecosystem.clone(),
        name: r.name.clone(),
        version: r.version.clone(),
        direct: r.direct,
        path: Bytes::from(r.path.clone().into_bytes()),
        advisory: Some(wire::DepAdvisory {
            id: r.advisory.id.clone(),
            aliases: r.advisory.aliases.clone(),
            summary: r.advisory.summary.clone(),
            severity: sev_wire(r.advisory.severity),
            cvss: r.advisory.cvss,
            fixed_versions: r.advisory.fixed.clone(),
            url: r.advisory.url.clone(),
            intel: Some(intel),
        }),
        state: state_wire(r.state),
        fixed_in: r.fixed_in.clone(),
        first_seen: r.first_seen,
        last_seen: r.last_seen,
        dismissed_by: r.dismissed_by.clone(),
        dismissed_at: r.dismissed_at,
        dismiss_reason: reason_wire(r.dismiss_reason),
        note: r.note.clone(),
    }
}

/// A finding as a scan that is not recorded reports it.
fn unrecorded(f: &NewFinding, now: i64) -> FindingRow {
    FindingRow {
        id: 0,
        ecosystem: f.ecosystem.clone(),
        name: f.name.clone(),
        version: f.version.clone(),
        path: f.path.clone(),
        direct: f.direct,
        advisory: f.advisory.clone(),
        state: State::Open,
        fixed_in: f.fixed_in.clone(),
        first_seen: now,
        last_seen: now,
        dismissed_by: String::new(),
        dismissed_at: 0,
        dismiss_reason: 0,
        note: String::new(),
    }
}

impl DepsHub {
    pub fn new(
        registry: Arc<Registry>,
        commits: Arc<ServerMutations>,
        egress: Arc<Egress>,
        cfg: Config,
        server_url: Option<String>,
        clock: Clock,
    ) -> Arc<Self> {
        let pool = registry.pool().clone();
        let osv = Osv::new(egress.clone(), pool.clone(), &cfg, clock.clone());
        let intel = IntelSource::new(egress.clone(), &cfg, clock.clone());
        let latest = Latest::new(
            egress,
            pool,
            cfg.registries.clone(),
            cfg.latest_ttl_secs,
            clock.clone(),
        )
        .with_archive(cfg.archive_max_bytes.0, cfg.archive_timeout_secs)
        .with_sumdb_key(
            super::sumdb::parse_key(&cfg.go_sumdb_key).unwrap_or_else(|_| {
                super::sumdb::parse_key(super::sumdb::DEFAULT_KEY)
                    .expect("the built-in key is valid")
            }),
        )
        .with_npm_unsigned(cfg.npm_unsigned)
        .with_artifact_hosts(cfg.artifact_hosts.clone());
        Arc::new(Self {
            view: RepoView::new(registry.clone()),
            permits: Semaphore::new(cfg.max_concurrent_scans),
            registry,
            osv,
            intel,
            latest,
            commits,
            server_url: server_url.map(|u| u.trim_end_matches('/').to_owned()),
            clock,
            cfg,
            scans: Arc::new(Scans::default()),
        })
    }

    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    /// The exploitation feeds (KEV, EPSS): `refresh` brings them up to date, `get` reads them.
    pub fn intel(&self) -> &IntelSource {
        &self.intel
    }

    /// Keeps the exploitation feeds (KEV, EPSS) up to date: refreshes once at start, then every
    /// `IntelSource::poll_every` (a URL is fetched only when `intel_ttl_secs` has run out, a feed file is
    /// looked at for changes), until `shutdown` is cancelled. Run it as one background task; no request
    /// waits for it. Failures are logged and the last good copy stays until it goes stale.
    pub async fn run_intel(self: Arc<Self>, shutdown: CancellationToken) {
        self.intel.run(&shutdown, self.intel.poll_every()).await;
    }

    /// The feeds as they stand now, for one call.
    pub fn intel_view(&self) -> IntelView {
        let (intel, status) = self.intel.snapshot();
        IntelView {
            intel,
            kev: status.kev,
            epss: status.epss,
        }
    }

    pub fn today(&self) -> String {
        repo_config::date_of(self.now())
    }

    pub(crate) async fn permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, Status> {
        self.permits
            .acquire()
            .await
            .map_err(|e| internal("deps: permits", &e))
    }

    pub(crate) async fn entry(&self, ctx: &CallCtx) -> Result<Arc<RepoEntry>, Status> {
        self.registry.open(&ctx.repo).await.map_err(registry_status)
    }

    pub(crate) fn codec(&self, ctx: &CallCtx, entry: &RepoEntry) -> IdCodec {
        IdCodec::new(ctx.id_mode, &self.registry, entry).for_viewer(&ctx.principal)
    }

    pub(crate) async fn trunk(&self, entry: &RepoEntry) -> Result<Option<ChangesetId>, Status> {
        self.view.trunk_tip(entry).await
    }

    /// The changeset `spec` names; trunk when it is empty.
    pub(crate) async fn rev(
        &self,
        ids: &IdCodec,
        entry: &RepoEntry,
        spec: &str,
    ) -> Result<ChangesetId, Status> {
        if spec.len() > MAX_STRING {
            return Err(Status::invalid_argument("revision is too long"));
        }
        if spec.is_empty() {
            return self
                .trunk(entry)
                .await?
                .ok_or_else(|| Status::failed_precondition("the repository has no commits"));
        }
        ids.resolve(spec, &entry.default_branch, |e| internal("history", &e))
            .await?
            .ok_or_else(|| Status::not_found(format!("unknown revision {spec:?}")))
    }

    /// `.asc/deps.toml` at `at` (the trunk tip when given), or the defaults.
    pub async fn repo_config(
        &self,
        entry: &RepoEntry,
        at: Option<ChangesetId>,
    ) -> Result<Loaded, Status> {
        let defaults = |error: String| Loaded {
            config: RepoConfig::default(),
            source: "default",
            text: String::new(),
            error,
        };
        let Some(cs) = at else {
            return Ok(defaults(String::new()));
        };
        let root = self.view.root_of(entry, cs).await?;
        match self
            .view
            .read(
                entry,
                root,
                repo_config::PATH,
                repo_config::MAX_BYTES as u64,
            )
            .await
        {
            Ok(None) => Ok(defaults(String::new())),
            Ok(Some(b)) => {
                let text = String::from_utf8_lossy(&b).into_owned();
                Ok(match RepoConfig::parse(&b) {
                    Ok(config) => Loaded {
                        config,
                        source: "file",
                        text,
                        error: String::new(),
                    },
                    Err(e) => Loaded {
                        text,
                        ..defaults(e)
                    },
                })
            }
            Err(s) if s.code() == tonic::Code::FailedPrecondition => {
                Ok(defaults(s.message().to_owned()))
            }
            Err(s) => Err(s),
        }
    }

    pub(crate) async fn snapshot(
        &self,
        entry: &RepoEntry,
        rev: ChangesetId,
        ecosystems: &[Ecosystem],
    ) -> Result<Snapshot, Status> {
        let root = self.view.root_of(entry, rev).await?;
        scan::read(entry.backend.engine(), rev, root, &self.cfg, ecosystems).await
    }

    /// Asks the advisory source about every distinct package version of `deps`.
    pub async fn assess(&self, deps: &[Dep], refresh: bool) -> Assessed {
        self.assess_with(deps, refresh, None).await
    }

    /// [`Self::assess`], counting its steps in `tick`.
    pub async fn assess_with(&self, deps: &[Dep], refresh: bool, tick: Option<&Tick>) -> Assessed {
        let (pkgs, capped) = distinct(deps, self.cfg.max_packages);
        let queries: Vec<Query> = pkgs
            .iter()
            .map(|(eco, name, version)| Query {
                eco: *eco,
                name: name.clone(),
                version: version.clone(),
            })
            .collect();
        let answer = self.osv.lookup_with(&queries, refresh, tick).await;
        let mut answers = HashMap::new();
        for (q, l) in queries.iter().zip(answer.lookups) {
            answers.insert((q.eco, q.name.clone(), q.version.clone()), l);
        }
        Assessed {
            answers,
            errors: answer.errors,
            capped,
            source: self.osv.source_name(),
        }
    }

    // -------------------------------------------------------------------------- Scan

    pub async fn scan(
        &self,
        ctx: &CallCtx,
        req: wire::ScanDepsRequest,
    ) -> Result<wire::ScanDepsResponse, Status> {
        let _permit = self.permit().await?;
        let entry = self.entry(ctx).await?;
        let progress = self.scans.start(entry.key.0, self.now());
        let ids = self.codec(ctx, &entry);
        let trunk = self.trunk(&entry).await?;
        let rev = self.rev(&ids, &entry, &req.rev).await?;
        let persist = req.rev.is_empty() || Some(rev) == trunk;
        let loaded = self.repo_config(&entry, trunk).await?;
        let mut ecos = ecosystems_of(&req.ecosystems)?;
        if ecos.is_empty() {
            ecos = loaded.config.ecosystems.clone();
        }
        let snap = self.snapshot(&entry, rev, &ecos).await?;
        progress.phase(Phase::Advisories, self.now());
        let assessed = self
            .assess_with(&snap.deps, req.refresh, Some(progress.tick()))
            .await;
        progress.phase(Phase::Recording, self.now());
        let today = self.today();
        let now = self.now();
        let (found, unjudged) = findings_of(&snap.deps, &assessed, &loaded.config, &today);
        let (distinct_pkgs, _) = distinct(&snap.deps, usize::MAX);
        let by_ecosystem = count_by_ecosystem(&distinct_pkgs);
        let (mut known, mut failed, mut uncovered) = (0u32, 0u32, 0u32);
        for (eco, name, version) in &distinct_pkgs {
            match assessed.answers.get(&(*eco, name.clone(), version.clone())) {
                Some(Lookup::Known(_)) => known += 1,
                Some(Lookup::Uncovered) => uncovered += 1,
                _ => failed += 1,
            }
        }
        let lookup = if failed == 0 && !assessed.capped {
            wire::DepLookupStatus::Complete
        } else if known == 0 && failed > 0 {
            wire::DepLookupStatus::Failed
        } else {
            wire::DepLookupStatus::Partial
        };
        let covered: HashSet<String> = if ecos.is_empty() {
            Ecosystem::ALL
                .iter()
                .map(|e| e.osv_name().to_owned())
                .collect()
        } else {
            ecos.iter().map(|e| e.osv_name().to_owned()).collect()
        };
        let pool = self.registry.pool();
        let rows: Vec<FindingRow> = if persist {
            let rows = store::record_scan(
                pool,
                entry.key.0,
                &store::Scan {
                    found: &found,
                    unjudged: &unjudged,
                    ecosystems: &covered,
                    complete: !snap.truncated && !assessed.capped,
                },
                now,
            )
            .await
            .map_err(sql)?;
            store::put_scan(
                pool,
                entry.key.0,
                &store::ScanRow {
                    at: now,
                    rev: rev.as_bytes().to_vec(),
                    lookup: lookup as i64,
                    packages: i64::try_from(distinct_pkgs.len()).unwrap_or(i64::MAX),
                    unknown: i64::from(failed + uncovered),
                    by_ecosystem: by_ecosystem.clone(),
                },
            )
            .await
            .map_err(sql)?;
            let _ = store::prune(pool, now).await;
            rows
        } else {
            found.iter().map(|f| unrecorded(f, now)).collect()
        };
        let view = self.intel_view();
        let mut findings: Vec<wire::DepFinding> =
            rows.iter().map(|r| finding_wire(r, &view)).collect();
        findings.sort_by(|a, b| {
            let sev = |f: &wire::DepFinding| f.advisory.as_ref().map_or(0, |x| x.severity);
            sev(b)
                .cmp(&sev(a))
                .then_with(|| a.ecosystem.cmp(&b.ecosystem))
                .then_with(|| a.name.cmp(&b.name))
        });
        let scanned = ids.changeset_out(rev).await?;
        progress.finish(self.now());
        Ok(wire::ScanDepsResponse {
            files: snap
                .files
                .iter()
                .map(|f| wire::DepFileResult {
                    path: Bytes::from(f.path.clone().into_bytes()),
                    format: f.format.name().to_owned(),
                    ecosystem: f.format.ecosystem().osv_name().to_owned(),
                    packages: u32::try_from(f.packages).unwrap_or(u32::MAX),
                    error: f.error.clone(),
                    notes: f.notes.clone(),
                    direct: u32::try_from(f.direct).unwrap_or(u32::MAX),
                })
                .collect(),
            packages: u32::try_from(distinct_pkgs.len()).unwrap_or(u32::MAX),
            findings,
            lookup: lookup as i32,
            unknown_packages: failed + uncovered,
            lookup_errors: assessed.errors.clone(),
            scanned,
            truncated: snap.truncated || assessed.capped,
            source: assessed.source.to_owned(),
            packages_by_ecosystem: by_ecosystem.into_iter().collect(),
        })
    }

    /// A CycloneDX bill of materials of a revision. Read like [`Self::scan`] (the same limits and the same
    /// concurrency limit), but nothing is looked up, recorded or sent anywhere.
    pub async fn sbom(
        &self,
        ctx: &CallCtx,
        req: wire::SbomRequest,
    ) -> Result<wire::SbomResponse, Status> {
        let _permit = self.permit().await?;
        let entry = self.entry(ctx).await?;
        let ids = self.codec(ctx, &entry);
        let trunk = self.trunk(&entry).await?;
        let rev = self.rev(&ids, &entry, &req.rev).await?;
        let loaded = self.repo_config(&entry, trunk).await?;
        let mut ecos = ecosystems_of(&req.ecosystems)?;
        if ecos.is_empty() {
            ecos = loaded.config.ecosystems.clone();
        }
        let snap = self.snapshot(&entry, rev, &ecos).await?;
        let sbom = super::sbom::cyclonedx(
            &snap.deps,
            &ctx.repo.to_string(),
            &hex(rev.as_bytes()),
            self.cfg.max_packages,
        );
        let notes: Vec<String> = snap
            .files
            .iter()
            .filter(|f| !f.error.is_empty())
            .take(MAX_SBOM_NOTES)
            .map(|f| format!("{}: {}", f.path, f.error))
            .collect();
        Ok(wire::SbomResponse {
            document: Bytes::from(sbom.document),
            components: u32::try_from(sbom.components).unwrap_or(u32::MAX),
            scanned: ids.changeset_out(rev).await?,
            truncated: snap.truncated || sbom.truncated,
            notes,
        })
    }

    /// What the newest running scan of the repository has done (docs/DEPENDENCIES.md "Progress").
    pub async fn scan_status(&self, ctx: &CallCtx) -> Result<wire::DepScanStatusResponse, Status> {
        let entry = self.entry(ctx).await?;
        let s = self.scans.status(entry.key.0, self.now());
        Ok(wire::DepScanStatusResponse {
            running: s.phase.is_some(),
            phase: match s.phase {
                None => wire::DepScanPhase::Unspecified,
                Some(Phase::Reading) => wire::DepScanPhase::Reading,
                Some(Phase::Advisories) => wire::DepScanPhase::Advisories,
                Some(Phase::Recording) => wire::DepScanPhase::Recording,
            } as i32,
            done: s.done,
            total: s.total,
            started_at: s.started_at,
            eta_seconds: s.eta_secs,
            permille: s.permille,
            last_took_seconds: s.last_took_secs,
        })
    }

    // -------------------------------------------------------------------------- findings

    pub async fn list_findings(
        &self,
        ctx: &CallCtx,
        req: wire::ListDepFindingsRequest,
    ) -> Result<wire::ListDepFindingsResponse, Status> {
        let mut states = Vec::new();
        for s in &req.states {
            match wire::DepFindingState::try_from(*s)
                .map_err(|_| Status::invalid_argument("unknown finding state"))?
            {
                wire::DepFindingState::Unspecified => {}
                wire::DepFindingState::Open => states.push(State::Open),
                wire::DepFindingState::Dismissed => states.push(State::Dismissed),
                wire::DepFindingState::Fixed => states.push(State::Fixed),
            }
        }
        if req.ecosystems.len() > 16 || req.name.len() > MAX_STRING {
            return Err(Status::invalid_argument(
                "too many ecosystems or a name too long",
            ));
        }
        let ecosystems: Vec<String> = ecosystems_of(&req.ecosystems)?
            .iter()
            .map(|e| e.osv_name().to_owned())
            .collect();
        let min = store::severity_code(sev_from_wire(req.min_severity)?);
        let limit = page_limit(req.limit, DEFAULT_PAGE, MAX_PAGE);
        let sort = wire::DepFindingSort::try_from(req.sort)
            .map_err(|_| Status::invalid_argument("unknown sort"))?;
        let in_memory = sort == wire::DepFindingSort::Exploited || req.only_exploited;
        let tag = match (sort, req.only_exploited) {
            (wire::DepFindingSort::Newest, false) => "",
            (wire::DepFindingSort::Newest, true) => ".newest-exploited",
            (wire::DepFindingSort::Exploited, false) => ".exploited",
            (wire::DepFindingSort::Exploited, true) => ".exploited-only",
        };
        // An offset token is only good for the filters it was issued under.
        let filters = filter_hash(
            &states,
            &ecosystems,
            min,
            &req.name,
            sort,
            req.only_exploited,
        );
        let scope = if in_memory {
            format!("Deps.ListFindings{tag}|{}|{filters:016x}", ctx.repo)
        } else {
            format!("Deps.ListFindings{tag}|{}", ctx.repo)
        };
        let after = if in_memory {
            0
        } else {
            parse_after(&scope, &ctx.principal.id, &req.page_token)?
        };
        // The in-memory orders decode up to MAX_SORTED rows, so they queue like a scan.
        let _permit = if in_memory {
            Some(self.permit().await?)
        } else {
            None
        };
        let entry = self.entry(ctx).await?;
        let pool = self.registry.pool();
        let view = self.intel_view();
        if req.only_exploited && !view.kev_usable() {
            return Err(Status::failed_precondition(format!(
                "only_exploited needs the KEV feed, which is {}",
                IntelView::feed_problem(&view.kev)
            )));
        }
        let filter = store::Filter {
            states,
            ecosystems,
            min_severity: min,
            name: req.name,
        };
        let mut truncated = false;
        let (page, next_page_token) = if in_memory {
            let mut rows = store::list(pool, entry.key.0, &filter, 0, MAX_SORTED)
                .await
                .map_err(sql)?;
            truncated = cut(&mut rows, MAX_SORTED);
            let mut judged: Vec<(FindingRow, wire::DepIntel)> = rows
                .into_iter()
                .map(|r| {
                    let i = view.of(&r.advisory);
                    (r, i)
                })
                .filter(|(_, i)| !req.only_exploited || i.kev)
                .collect();
            if sort == wire::DepFindingSort::Exploited {
                judged.sort_by(exploited_order);
            }
            let start = offset_of(&scope, &ctx.principal.id, &req.page_token)?.min(judged.len());
            let end = start.saturating_add(limit).min(judged.len());
            let next = if end < judged.len() {
                crate::pagetoken::seal(&scope, &ctx.principal.id, &end.to_string())
            } else {
                String::new()
            };
            let page: Vec<wire::DepFinding> = judged
                .drain(start..end)
                .map(|(r, i)| finding_with(&r, i))
                .collect();
            (page, next)
        } else {
            let mut rows = store::list(pool, entry.key.0, &filter, after, limit)
                .await
                .map_err(sql)?;
            let next = if rows.len() > limit {
                rows.truncate(limit);
                rows.last()
                    .map(|r| after_token(&scope, &ctx.principal.id, r.id))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (rows.iter().map(|r| finding_wire(r, &view)).collect(), next)
        };
        // The count reads up to MAX_SORTED rows: only the first page pays for it, under a scan slot.
        let open_exploited = if view.kev_usable() && req.page_token.is_empty() {
            let _count_permit = if _permit.is_none() {
                Some(self.permit().await?)
            } else {
                None
            };
            let names = store::open_names(pool, entry.key.0, MAX_SORTED)
                .await
                .map_err(sql)?;
            let n = names
                .iter()
                .filter(|(id, aliases)| {
                    view.kev_lists(
                        std::iter::once(id.as_str()).chain(aliases.iter().map(String::as_str)),
                    )
                })
                .count();
            u32::try_from(n).unwrap_or(u32::MAX)
        } else {
            0
        };
        let counts = store::counts(pool, entry.key.0).await.map_err(sql)?;
        let by_severity = store::open_by_severity(pool, entry.key.0)
            .await
            .map_err(sql)?;
        let last = store::last_scan(pool, entry.key.0).await.map_err(sql)?;
        Ok(wire::ListDepFindingsResponse {
            findings: page,
            next_page_token,
            counts_by_state: counts.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
            last_scan: last.as_ref().map_or(0, |l| l.at),
            last_scanned: last
                .as_ref()
                .map(|l| Bytes::from(l.rev.clone()))
                .map(|b| b.to_vec())
                .map(Bytes::from)
                .unwrap_or_default(),
            packages_checked: last
                .as_ref()
                .map_or(0, |l| u32::try_from(l.packages).unwrap_or(u32::MAX)),
            packages_by_ecosystem: last
                .as_ref()
                .map(|l| l.by_ecosystem.clone().into_iter().collect())
                .unwrap_or_default(),
            packages_unknown: last
                .as_ref()
                .map_or(0, |l| u32::try_from(l.unknown).unwrap_or(u32::MAX)),
            open_by_severity: by_severity
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            last_lookup: last.map_or(0, |l| i32::try_from(l.lookup).unwrap_or(0)),
            kev_feed: Some(feed_wire(&view.kev)),
            epss_feed: Some(feed_wire(&view.epss)),
            open_exploited,
            truncated,
        })
    }

    /// The security status of the repository: the trunk scan's freshness and completeness, the open
    /// findings and the moves that close them (docs/DEPENDENCIES.md "Status"). Reads only.
    pub async fn status(&self, ctx: &CallCtx) -> Result<super::status::RepoStatus, Status> {
        let entry = self.entry(ctx).await?;
        let trunk = self.trunk(&entry).await?;
        let loaded = self.repo_config(&entry, trunk).await?;
        let pool = self.registry.pool();
        let filter = store::Filter {
            states: vec![State::Open],
            ..store::Filter::default()
        };
        let mut open = store::list(pool, entry.key.0, &filter, 0, MAX_SORTED)
            .await
            .map_err(sql)?;
        cut(&mut open, MAX_SORTED);
        let by_severity = store::open_by_severity(pool, entry.key.0)
            .await
            .map_err(sql)?
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        let last = store::last_scan(pool, entry.key.0).await.map_err(sql)?;
        Ok(super::status::repo_status(
            &open,
            by_severity,
            last.as_ref(),
            self.now(),
            super::status::stale_after(loaded.config.schedule.as_deref()),
        ))
    }

    pub async fn dismiss(
        &self,
        ctx: &CallCtx,
        req: wire::DismissDepFindingRequest,
    ) -> Result<wire::DismissDepFindingResponse, Status> {
        if req.note.len() > MAX_NOTE || req.note.contains('\0') {
            return Err(Status::invalid_argument("the note is too long"));
        }
        let id = i64::try_from(req.id)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Status::invalid_argument("invalid finding id"))?;
        let reason = wire::DepDismissReason::try_from(req.reason)
            .map_err(|_| Status::invalid_argument("unknown reason"))?;
        let state = if reason == wire::DepDismissReason::Unspecified {
            State::Open
        } else {
            State::Dismissed
        };
        let entry = self.entry(ctx).await?;
        let pool = self.registry.pool();
        let Some(current) = store::get(pool, entry.key.0, id).await.map_err(sql)? else {
            return Err(Status::not_found("no such finding"));
        };
        let event = NewEvent::new("deps.finding.updated", actor_of(&ctx.principal.id))
            .repo(ctx.repo.as_str())
            .target(&id.to_string())
            .detail("finding", id.to_string())
            .detail("state", state.name())
            .detail("advisory", &current.advisory.id);
        let row = store::set_state(
            pool,
            entry.key.0,
            id,
            state,
            reason as i64,
            &req.note,
            &ctx.principal.id,
            self.now(),
            Some(&event),
        )
        .await
        .map_err(sql)?
        .ok_or_else(|| Status::not_found("no such finding"))?;
        Ok(wire::DismissDepFindingResponse {
            finding: Some(finding_wire(&row, &self.intel_view())),
        })
    }
}

/// The findings of `deps` under the answers, and the packages that could not be judged.
pub fn findings_of(
    deps: &[Dep],
    assessed: &Assessed,
    cfg: &RepoConfig,
    today: &str,
) -> (Vec<NewFinding>, HashSet<PackageKey>) {
    let mut found = Vec::new();
    let mut unjudged: HashSet<PackageKey> = HashSet::new();
    let mut seen: HashSet<(String, String, String, String, String)> = HashSet::new();
    for d in deps.iter().filter(|d| !d.version.is_empty()) {
        let key: PackageKey = (
            d.ecosystem.osv_name().to_owned(),
            d.name.clone(),
            d.version.clone(),
            d.path.clone(),
        );
        match assessed.answer(d) {
            Some(Lookup::Known(advs)) => {
                for a in advs {
                    let mut names: Vec<&str> = vec![a.id.as_str()];
                    names.extend(a.aliases.iter().map(String::as_str));
                    if cfg.advisory_ignored(d.ecosystem, &d.name, &names, today) {
                        continue;
                    }
                    let uniq = (
                        key.0.clone(),
                        key.1.clone(),
                        key.2.clone(),
                        key.3.clone(),
                        a.id.clone(),
                    );
                    if !seen.insert(uniq) {
                        continue;
                    }
                    found.push(NewFinding {
                        ecosystem: key.0.clone(),
                        name: d.name.clone(),
                        version: d.version.clone(),
                        path: d.path.clone(),
                        direct: d.direct,
                        advisory: a.clone(),
                        fixed_in: a
                            .fixed_after(d.ecosystem.scheme(), &d.version)
                            .unwrap_or_default(),
                    });
                }
            }
            _ => {
                unjudged.insert(key);
            }
        }
    }
    (found, unjudged)
}

// ------------------------------------------------------------------------------ the landing check

/// `severity` and `name` of an advisory for a refusal line.
fn line(eco: Ecosystem, name: &str, version: &str, id: &str, sev: DepSeverity) -> String {
    format!("{} {name} {version} {id} ({})", eco.osv_name(), sev.name())
}

#[async_trait]
impl DepsCheck for DepsHub {
    async fn check(&self, req: &CheckRequest<'_>) -> Result<Vec<String>, Status> {
        let block = req.policy.block_severity;
        let needs_intel = req.policy.needs_intel();
        if block.is_none() && !needs_intel {
            return Ok(Vec::new());
        }
        let files = dependency_files(req.touched);
        if files.is_empty() {
            return Ok(Vec::new());
        }
        if files.len() > MAX_GATE_FILES {
            return Ok(vec![format!(
                "the update changes more than {MAX_GATE_FILES} dependency files and cannot be checked; push it in parts"
            )]);
        }
        let new_root = self.view.root_of(req.entry, req.new).await?;
        let old_root = match req.base {
            Some(b) => Some(self.view.root_of(req.entry, b).await?),
            None => None,
        };
        let mut unreadable: Vec<String> = Vec::new();
        let mut introduced: Vec<Dep> = Vec::new();
        for path in &files {
            let parse = |bytes: &Bytes| match parse_file(path, bytes) {
                Some(Ok(p)) => Ok(p.deps),
                Some(Err(e)) => Err(e),
                None => Ok(Vec::new()),
            };
            let new = match self
                .view
                .read(req.entry, new_root, path, MAX_FILE_BYTES as u64)
                .await
            {
                Ok(Some(b)) => parse(&b),
                Ok(None) => continue,
                Err(s) if s.code() == tonic::Code::FailedPrecondition => {
                    Err(s.message().to_owned())
                }
                Err(s) => return Err(s),
            };
            let new = match new {
                Ok(d) => d,
                Err(e) => {
                    unreadable.push(format!("{path}: {e}"));
                    continue;
                }
            };
            let old: Vec<Dep> = match old_root {
                Some(root) => match self
                    .view
                    .read(req.entry, root, path, MAX_FILE_BYTES as u64)
                    .await
                {
                    Ok(Some(b)) => parse(&b).unwrap_or_default(),
                    _ => Vec::new(),
                },
                None => Vec::new(),
            };
            let had: HashSet<_> = old.iter().map(Dep::key).collect();
            introduced.extend(
                new.into_iter()
                    .filter(|d| !d.version.is_empty() && !had.contains(&d.key())),
            );
        }
        let mut lines: Vec<String> = Vec::new();
        // The exploitation keys always fail closed on an incomplete scan: an advisory that was never
        // looked up cannot be judged. The severity gate alone keeps `block_unknown`.
        let strict = req.policy.block_unknown || needs_intel;
        // A file that cannot be read says nothing about what it adds.
        if strict && !unreadable.is_empty() {
            lines.push(
                "a dependency file of the update cannot be read, so it cannot be checked".into(),
            );
            lines.extend(unreadable);
            lines.truncate(MAX_LINES);
            return Ok(lines);
        }
        let (pkgs, capped) = distinct(&introduced, MAX_GATE_PACKAGES);
        if capped && strict {
            return Ok(vec![format!(
                "the update adds more than {MAX_GATE_PACKAGES} package versions and cannot be checked; push it in parts"
            )]);
        }
        let queries: Vec<Query> = pkgs
            .iter()
            .map(|(eco, name, version)| Query {
                eco: *eco,
                name: name.clone(),
                version: version.clone(),
            })
            .collect();
        let timeout = std::time::Duration::from_secs(self.cfg.gate_timeout_secs);
        let answers = match tokio::time::timeout(timeout, self.osv.lookup(&queries, false)).await {
            Ok(a) => a.lookups,
            Err(_) => queries
                .iter()
                .map(|_| Lookup::Unknown("the lookup timed out".into()))
                .collect(),
        };
        let dismissed = store::dismissed(self.registry.pool(), req.entry.key.0)
            .await
            .map_err(sql)?;
        let view = self.intel_view();
        let mut vulnerable: Vec<String> = Vec::new();
        let mut exploited: Vec<String> = Vec::new();
        let mut judged_by_feed = false;
        let mut failed: Vec<String> = Vec::new();
        for (q, a) in queries.iter().zip(&answers) {
            match a {
                Lookup::Known(advs) => {
                    for adv in advs {
                        let sev = effective(adv);
                        let dis = dismissed.contains(&(
                            q.eco.osv_name().to_owned(),
                            q.name.clone(),
                            q.version.clone(),
                            adv.id.clone(),
                        ));
                        if dis {
                            continue;
                        }
                        if block.is_some_and(|b| sev >= b) {
                            vulnerable.push(line(q.eco, &q.name, &q.version, &adv.id, sev));
                        }
                        if needs_intel {
                            judged_by_feed = true;
                            let hit = view.intel.lookup(
                                std::iter::once(adv.id.as_str())
                                    .chain(adv.aliases.iter().map(String::as_str)),
                            );
                            let why = match (&hit.kev, &hit.epss) {
                                (Feed::Listed(_), _) if req.policy.block_exploited => {
                                    Some("known exploited".to_owned())
                                }
                                (_, Feed::Listed(e))
                                    if req.policy.epss_threshold > 0.0
                                        && e.score >= req.policy.epss_threshold =>
                                {
                                    Some(format!(
                                        "EPSS {:.3} at or above {}",
                                        e.score, req.policy.epss_threshold
                                    ))
                                }
                                _ => None,
                            };
                            if let Some(why) = why {
                                exploited.push(format!(
                                    "{} ({why})",
                                    line(q.eco, &q.name, &q.version, &adv.id, sev)
                                ));
                            }
                        }
                    }
                }
                Lookup::Unknown(_) => {
                    failed.push(format!(
                        "{} {} {}: no answer",
                        q.eco.osv_name(),
                        q.name,
                        q.version
                    ));
                }
                Lookup::Uncovered => {}
            }
        }
        // A feed the policy needs but cannot read leaves the added advisories unjudged: fail closed,
        // whatever `block_unknown` says.
        let mut missing: Vec<String> = Vec::new();
        if judged_by_feed {
            let need = |on: bool, name: &str, s: &FeedStatus| {
                (on && s.state != FeedState::Current)
                    .then(|| format!("the {name} feed is {}", IntelView::feed_problem(s)))
            };
            missing.extend(need(req.policy.block_exploited, "KEV", &view.kev));
            missing.extend(need(req.policy.epss_threshold > 0.0, "EPSS", &view.epss));
        }
        let refused = !vulnerable.is_empty() || !exploited.is_empty() || !missing.is_empty();
        if let Some(b) = block
            && !vulnerable.is_empty()
        {
            lines.push(format!(
                "the update adds dependencies with advisories at or above {}",
                b.name()
            ));
            lines.extend(vulnerable);
        }
        if !exploited.is_empty() {
            lines.push(
                "the update adds dependencies with advisories the policy treats as exploited"
                    .into(),
            );
            lines.extend(exploited);
        }
        if !missing.is_empty() {
            lines.push(
                "the policy needs exploitation data that is not available, so the update cannot be judged"
                    .into(),
            );
            lines.extend(missing);
        }
        if !refused && strict && !failed.is_empty() {
            lines.push(
                "the advisory lookup failed for dependencies this update adds, and the policy blocks on an unknown answer"
                    .into(),
            );
            lines.extend(failed);
        }
        lines.truncate(MAX_LINES);
        Ok(lines)
    }
}
