//! Daemon observability (SPEC §16.1): the Prometheus endpoint behind `--metrics-listen`, the
//! daemon metrics catalogue and the rotating JSON-lines event log (50 MiB × 5).
//!
//! The endpoint is off by default. When on it binds loopback only (a non-loopback address is
//! refused at start), answers `GET /metrics` and nothing else, checks the `Host` header against
//! DNS rebinding, bounds the request it reads, the time it waits for one and the connections it
//! serves at once. Label values come from untrusted places (process image names), so they are
//! escaped and bounded.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

/// Log files are rotated at this size (SPEC §16.1).
pub const LOG_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// Files kept: the live log and four rotated ones.
pub const LOG_FILES: usize = 5;
/// Longest log line; longer ones are cut so one event cannot fill a file.
const MAX_LINE: usize = 16 * 1024;

/// Largest request head read from a metrics client.
const MAX_REQUEST: usize = 8 * 1024;
/// How long a metrics client may take to send its request head.
const REQUEST_WAIT: Duration = Duration::from_secs(2);
/// Metrics connections served at once.
const MAX_CONNECTIONS: usize = 8;
/// Process images listed in `asc_daemon_hydrations_total`; the rest are summed as `other`.
const MAX_PROCESS_LABELS: usize = 50;
/// Longest label value.
const MAX_LABEL: usize = 128;
/// Distinct `op` values kept for provider errors.
const MAX_OPS: usize = 32;

// ---------------------------------------------------------------------------------- listen

/// Parses `--metrics-listen`: `host:port` where the host is a loopback IP address. The daemon
/// has no authentication on this port, so nothing else may reach it.
pub fn parse_listen(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text
        .parse()
        .map_err(|_| format!("--metrics-listen '{text}' is not <ip>:<port>"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "--metrics-listen '{text}' is not a loopback address; the daemon's metrics are local only"
        ));
    }
    Ok(addr)
}

// ---------------------------------------------------------------------------------- state

/// What a scrape reads from the live daemon.
#[derive(Clone, Debug, Default)]
pub struct Collected {
    /// Hydrations by process image.
    pub hydrations: Vec<(String, u64)>,
    /// Bytes fetched to hydrate.
    pub fetch_bytes: u64,
    pub journal_entries: u64,
    pub pending_paths: u64,
}

/// The part of the daemon that answers a scrape.
#[async_trait]
pub trait Source: Send + Sync {
    async fn collect(&self) -> Collected;
}

/// A scheduler task's counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskCounters {
    pub runs: u64,
    pub failures: u64,
    pub last_ms: u64,
}

/// The daemon's counters. Cheap to update from anywhere; rendered by [`Metrics::render`].
pub struct Metrics {
    started: Instant,
    cache_bytes: AtomicU64,
    reconcile_paths: AtomicU64,
    provider_errors: Mutex<BTreeMap<&'static str, u64>>,
    tasks: Mutex<BTreeMap<&'static str, TaskCounters>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            cache_bytes: AtomicU64::new(0),
            reconcile_paths: AtomicU64::new(0),
            provider_errors: Mutex::default(),
            tasks: Mutex::default(),
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn set_cache_bytes(&self, bytes: u64) {
        self.cache_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn add_reconciled(&self, paths: u64) {
        self.reconcile_paths.fetch_add(paths, Ordering::Relaxed);
    }

    /// Counts a failed provider operation (`op` is a fixed name such as `"dehydrate"`).
    pub fn provider_error(&self, op: &'static str) {
        let mut g = lock(&self.provider_errors);
        if g.len() < MAX_OPS || g.contains_key(op) {
            *g.entry(op).or_default() += 1;
        }
    }

    pub fn provider_errors(&self, op: &'static str) -> u64 {
        lock(&self.provider_errors).get(op).copied().unwrap_or(0)
    }

    /// Records one run of a scheduler task.
    pub fn task_run(&self, name: &'static str, ok: bool, took: Duration) {
        let mut g = lock(&self.tasks);
        if g.len() >= 64 && !g.contains_key(name) {
            return;
        }
        let t = g.entry(name).or_default();
        t.runs += 1;
        t.failures += u64::from(!ok);
        t.last_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
    }

    pub fn task(&self, name: &str) -> TaskCounters {
        lock(&self.tasks).get(name).copied().unwrap_or_default()
    }

    /// The Prometheus text exposition of the daemon catalogue (SPEC §16.1).
    pub fn render(&self, c: &Collected) -> String {
        let mut o = String::new();
        let family = |o: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };

        family(
            &mut o,
            "asc_daemon_hydrations_total",
            "counter",
            "Files hydrated, by requesting process.",
        );
        let mut by = c.hydrations.clone();
        by.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let rest: u64 = by.iter().skip(MAX_PROCESS_LABELS).map(|(_, n)| n).sum();
        for (image, n) in by.iter().take(MAX_PROCESS_LABELS) {
            let _ = writeln!(
                o,
                "asc_daemon_hydrations_total{{process=\"{}\"}} {n}",
                escape(image)
            );
        }
        if rest > 0 {
            let _ = writeln!(o, "asc_daemon_hydrations_total{{process=\"other\"}} {rest}");
        }

        family(
            &mut o,
            "asc_daemon_fetch_bytes_total",
            "counter",
            "Bytes fetched to hydrate files.",
        );
        let _ = writeln!(o, "asc_daemon_fetch_bytes_total {}", c.fetch_bytes);
        family(
            &mut o,
            "asc_daemon_cache_bytes",
            "gauge",
            "Size of a cache directory at the last cache GC.",
        );
        let _ = writeln!(
            o,
            "asc_daemon_cache_bytes{{dir=\"cache\"}} {}",
            self.cache_bytes.load(Ordering::Relaxed)
        );
        family(
            &mut o,
            "asc_daemon_journal_entries",
            "gauge",
            "Change journal entries across mounted workspaces.",
        );
        let _ = writeln!(o, "asc_daemon_journal_entries {}", c.journal_entries);
        family(
            &mut o,
            "asc_daemon_pending_paths",
            "gauge",
            "Paths a switch could not finish yet.",
        );
        let _ = writeln!(o, "asc_daemon_pending_paths {}", c.pending_paths);
        family(
            &mut o,
            "asc_daemon_reconcile_paths_total",
            "counter",
            "Paths examined by reconcile.",
        );
        let _ = writeln!(
            o,
            "asc_daemon_reconcile_paths_total {}",
            self.reconcile_paths.load(Ordering::Relaxed)
        );
        family(
            &mut o,
            "asc_daemon_provider_errors_total",
            "counter",
            "Failed provider operations.",
        );
        for (op, n) in lock(&self.provider_errors).iter() {
            let _ = writeln!(
                o,
                "asc_daemon_provider_errors_total{{op=\"{}\"}} {n}",
                escape(op)
            );
        }
        family(
            &mut o,
            "asc_daemon_uptime_seconds",
            "gauge",
            "Seconds since the daemon started.",
        );
        let _ = writeln!(o, "asc_daemon_uptime_seconds {}", self.uptime().as_secs());
        family(
            &mut o,
            "asc_rss_bytes",
            "gauge",
            "Resident set size of the daemon.",
        );
        let _ = writeln!(o, "asc_rss_bytes {}", rss_bytes().unwrap_or(0));

        family(
            &mut o,
            "asc_daemon_task_runs_total",
            "counter",
            "Scheduler task runs.",
        );
        let tasks = lock(&self.tasks).clone();
        for (name, t) in &tasks {
            let _ = writeln!(o, "asc_daemon_task_runs_total{{task=\"{name}\"}} {}", t.runs);
        }
        family(
            &mut o,
            "asc_daemon_task_failures_total",
            "counter",
            "Scheduler task runs that failed.",
        );
        for (name, t) in &tasks {
            let _ = writeln!(
                o,
                "asc_daemon_task_failures_total{{task=\"{name}\"}} {}",
                t.failures
            );
        }
        family(
            &mut o,
            "asc_daemon_task_last_run_ms",
            "gauge",
            "Duration of the last run of a scheduler task.",
        );
        for (name, t) in &tasks {
            let _ = writeln!(
                o,
                "asc_daemon_task_last_run_ms{{task=\"{name}\"}} {}",
                t.last_ms
            );
        }
        o
    }
}

/// A label value: `\`, `"` and line breaks escaped, control characters dropped, length bound.
fn escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if out.len() >= MAX_LABEL {
            break;
        }
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------------- rss

/// The resident set size of this process, when the platform can say.
pub fn rss_bytes() -> Option<u64> {
    imp::rss_bytes()
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn rss_bytes() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = text.lines().find_map(|l| l.strip_prefix("VmRSS:"))?;
        let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
        kib.checked_mul(1024)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // one Win32 call, with a zeroed out-struct and its true size.
mod imp {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn rss_bytes() -> Option<u64> {
        // SAFETY: `counters` is a plain-data struct that the call fills; `cb` is its size and
        // the pseudo handle of the current process needs no closing.
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            counters.cb = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).ok()?;
            if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) == 0 {
                return None;
            }
            Some(counters.WorkingSetSize as u64)
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn rss_bytes() -> Option<u64> {
        None
    }
}

// ---------------------------------------------------------------------------------- http

/// Serves `GET /metrics` on `listener` until `stop` fires.
pub async fn serve(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    source: Arc<dyn Source>,
    mut stop: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            _ = stop.changed() => return,
        };
        let Ok((stream, peer)) = accepted else {
            // A failed accept (file descriptors exhausted) must not spin.
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        // At the limit a new connection is dropped at once rather than queued.
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        let (metrics, source) = (metrics.clone(), source.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let _ =
                tokio::time::timeout(REQUEST_WAIT * 4, answer(stream, &metrics, source.as_ref()))
                    .await;
        });
    }
}

/// Binds the metrics listener.
pub async fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "metrics listen address is not loopback",
        ));
    }
    TcpListener::bind(addr).await
}

async fn answer(mut stream: TcpStream, metrics: &Metrics, source: &dyn Source) -> io::Result<()> {
    let head = match tokio::time::timeout(REQUEST_WAIT, read_head(&mut stream)).await {
        Ok(Ok(Some(head))) => head,
        _ => return respond(&mut stream, "400 Bad Request", "bad request\n").await,
    };
    match route(&head) {
        Route::Metrics => {
            let body = metrics.render(&source.collect().await);
            respond_typed(
                &mut stream,
                "200 OK",
                "text/plain; version=0.0.4; charset=utf-8",
                &body,
            )
            .await
        }
        Route::NotFound => respond(&mut stream, "404 Not Found", "not found\n").await,
        Route::Method => {
            respond(
                &mut stream,
                "405 Method Not Allowed",
                "method not allowed\n",
            )
            .await
        }
        Route::Host => respond(&mut stream, "403 Forbidden", "forbidden\n").await,
        Route::Bad => respond(&mut stream, "400 Bad Request", "bad request\n").await,
    }
}

/// Reads up to the end of the request head, at most [`MAX_REQUEST`] bytes.
async fn read_head(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        head.extend_from_slice(&chunk[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(Some(head));
        }
        if head.len() >= MAX_REQUEST {
            return Ok(None);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Metrics,
    NotFound,
    Method,
    Host,
    Bad,
}

fn route(head: &[u8]) -> Route {
    let Ok(text) = std::str::from_utf8(head) else {
        return Route::Bad;
    };
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Route::Bad;
    };
    if !version.starts_with("HTTP/1.") {
        return Route::Bad;
    }
    let mut hosts = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim());
    // One loopback Host: a page that rebinds a name to 127.0.0.1 carries its own name here.
    match (hosts.next(), hosts.next()) {
        (Some(h), None) if host_is_local(h) => {}
        _ => return Route::Host,
    }
    if method != "GET" {
        return Route::Method;
    }
    let path = target.split(['?', '#']).next().unwrap_or("");
    if path == "/metrics" {
        Route::Metrics
    } else {
        Route::NotFound
    }
}

fn host_is_local(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(name, port)| {
            if port.bytes().all(|b| b.is_ascii_digit()) {
                name
            } else {
                host
            }
        })
    };
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    respond_typed(stream, status, "text/plain; charset=utf-8", body).await
}

async fn respond_typed(
    stream: &mut TcpStream,
    status: &str,
    kind: &str,
    body: &str,
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

// ---------------------------------------------------------------------------------- log

/// An append-only file that rotates at a size: `name`, `name.1` … `name.<keep-1>`.
pub struct RotatingLog {
    path: PathBuf,
    max: u64,
    keep: usize,
    file: Option<fs::File>,
    size: u64,
}

fn rotated(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

impl RotatingLog {
    /// Opens (creating) the log. A path that is a link is refused: the daemon never appends
    /// through a link someone else could have planted.
    pub fn open(path: impl Into<PathBuf>, max: u64, keep: usize) -> io::Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut log = Self {
            path,
            max: max.max(1),
            keep: keep.max(1),
            file: None,
            size: 0,
        };
        log.reopen()?;
        Ok(log)
    }

    fn reopen(&mut self) -> io::Result<()> {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| asc_wc::safe_path::is_link(&m)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the log path is a link",
            ));
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        if self.keep > 1 {
            let _ = fs::remove_file(rotated(&self.path, self.keep - 1));
            for n in (1..self.keep - 1).rev() {
                let from = rotated(&self.path, n);
                if from.exists() {
                    fs::rename(&from, rotated(&self.path, n + 1))?;
                }
            }
            fs::rename(&self.path, rotated(&self.path, 1))?;
        } else {
            fs::remove_file(&self.path)?;
        }
        self.reopen()
    }

    /// Appends `line` and a line feed, rotating first when the file would pass the limit.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let mut bytes = line.as_bytes();
        if bytes.len() > MAX_LINE {
            let mut end = MAX_LINE;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            bytes = &bytes[..end];
        }
        let need = bytes.len() as u64 + 1;
        if self.file.is_none() {
            self.reopen()?;
        }
        if self.size > 0 && self.size + need > self.max {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log closed"))?;
        let mut buf = Vec::with_capacity(bytes.len() + 1);
        buf.extend_from_slice(bytes);
        buf.push(b'\n');
        file.write_all(&buf)?;
        self.size += need;
        Ok(())
    }
}

/// The daemon's JSON-lines event log with the stable fields of SPEC §16.1.
pub struct EventLog {
    log: Mutex<Option<RotatingLog>>,
}

impl EventLog {
    /// A log that drops every event (tests, or a log directory that cannot be opened).
    pub fn none() -> Self {
        Self {
            log: Mutex::new(None),
        }
    }

    /// `<dir>/daemon.log`, 50 MiB × 5.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let log = RotatingLog::open(dir.join("daemon.log"), LOG_MAX_BYTES, LOG_FILES)?;
        Ok(Self {
            log: Mutex::new(Some(log)),
        })
    }

    /// Writes one event. `fields` are extra string fields; a failed write is dropped (logging
    /// never stops the daemon).
    pub fn emit(&self, level: &str, event: &str, fields: &[(&str, &str)]) {
        let mut g = lock(&self.log);
        let Some(log) = g.as_mut() else { return };
        let mut obj = serde_json::Map::new();
        obj.insert("ts".into(), rfc3339(SystemTime::now()).into());
        obj.insert("level".into(), level.into());
        obj.insert("tier".into(), "daemon".into());
        obj.insert("event".into(), event.into());
        for (k, v) in fields {
            if !matches!(*k, "ts" | "level" | "tier" | "event") {
                obj.insert((*k).into(), (*v).into());
            }
        }
        if let Ok(line) = serde_json::to_string(&obj) {
            let _ = log.write_line(&line);
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (secs, ms) = (d.as_secs(), d.subsec_millis());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Collected);

    #[async_trait]
    impl Source for Fixed {
        async fn collect(&self) -> Collected {
            self.0.clone()
        }
    }

    #[test]
    fn listen_address_must_be_loopback() {
        assert!(parse_listen("127.0.0.1:9100").is_ok());
        assert!(parse_listen("[::1]:9100").is_ok());
        for bad in [
            "0.0.0.0:9100",
            "192.168.1.5:9100",
            "[::]:9100",
            "localhost:9100",
            "9100",
            "",
        ] {
            assert!(parse_listen(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn render_has_the_catalogue_and_escapes_labels() {
        let m = Metrics::new();
        m.provider_error("dehydrate");
        m.set_cache_bytes(7);
        m.task_run("cache_gc", true, Duration::from_millis(5));
        m.task_run("cache_gc", false, Duration::from_millis(6));
        let text = m.render(&Collected {
            hydrations: vec![("evil\"\nname\\".into(), 3), ("tsserver.exe".into(), 9)],
            fetch_bytes: 11,
            journal_entries: 2,
            pending_paths: 1,
        });
        for name in [
            "asc_daemon_hydrations_total",
            "asc_daemon_fetch_bytes_total",
            "asc_daemon_cache_bytes{dir=\"cache\"} 7",
            "asc_daemon_journal_entries 2",
            "asc_daemon_pending_paths 1",
            "asc_daemon_reconcile_paths_total 0",
            "asc_daemon_provider_errors_total{op=\"dehydrate\"} 1",
            "asc_daemon_uptime_seconds",
            "asc_rss_bytes",
            "asc_daemon_task_runs_total{task=\"cache_gc\"} 2",
            "asc_daemon_task_failures_total{task=\"cache_gc\"} 1",
        ] {
            assert!(text.contains(name), "{name} missing from\n{text}");
        }
        assert!(
            text.contains("process=\"evil\\\"\\nname\\\\\""),
            "label escaped:\n{text}"
        );
        assert!(
            text.find("tsserver.exe").unwrap() < text.find("evil").unwrap(),
            "busiest first"
        );
        assert!(
            text.lines().all(|l| !l.is_empty()),
            "no blank lines inside a label value leaked"
        );
    }

    #[test]
    fn many_processes_are_folded_into_other() {
        let m = Metrics::new();
        let hydrations = (0..MAX_PROCESS_LABELS + 10)
            .map(|i| (format!("p{i}"), 1))
            .collect();
        let text = m.render(&Collected {
            hydrations,
            ..Collected::default()
        });
        assert!(text.contains("process=\"other\"} 10"));
        assert_eq!(
            text.matches("asc_daemon_hydrations_total{").count(),
            MAX_PROCESS_LABELS + 1
        );
    }

    #[test]
    fn routing_checks_method_path_and_host() {
        let req = |line: &str, host: &str| format!("{line}\r\nHost: {host}\r\n\r\n").into_bytes();
        assert_eq!(
            route(&req("GET /metrics HTTP/1.1", "127.0.0.1:9100")),
            Route::Metrics
        );
        assert_eq!(
            route(&req("GET /metrics?x=1 HTTP/1.1", "localhost:9100")),
            Route::Metrics
        );
        assert_eq!(
            route(&req("GET /metrics HTTP/1.1", "[::1]:9100")),
            Route::Metrics
        );
        assert_eq!(
            route(&req("GET /metrics HTTP/1.1", "evil.example:9100")),
            Route::Host
        );
        assert_eq!(
            route(&req("GET /metrics HTTP/1.1", "127.0.0.1.evil.example")),
            Route::Host
        );
        assert_eq!(route(b"GET /metrics HTTP/1.1\r\n\r\n"), Route::Host);
        assert_eq!(
            route(&req("POST /metrics HTTP/1.1", "127.0.0.1:1")),
            Route::Method
        );
        assert_eq!(
            route(&req("GET /other HTTP/1.1", "127.0.0.1:1")),
            Route::NotFound
        );
        assert_eq!(route(&req("GET /metrics", "127.0.0.1:1")), Route::Bad);
        assert_eq!(route(&[0xff, 0xfe]), Route::Bad);
    }

    #[tokio::test]
    async fn the_endpoint_serves_metrics_and_refuses_the_rest() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = watch::channel(false);
        let source = Arc::new(Fixed(Collected {
            pending_paths: 4,
            ..Collected::default()
        }));
        let task = tokio::spawn(serve(listener, Arc::new(Metrics::new()), source, rx));

        let ask = |req: String| async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(req.as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        let host = format!("127.0.0.1:{}", addr.port());
        let ok = ask(format!("GET /metrics HTTP/1.1\r\nHost: {host}\r\n\r\n")).await;
        assert!(ok.starts_with("HTTP/1.1 200 OK"), "{ok}");
        assert!(ok.contains("asc_daemon_pending_paths 4"));
        let rebind = ask("GET /metrics HTTP/1.1\r\nHost: attacker.example\r\n\r\n".into()).await;
        assert!(rebind.starts_with("HTTP/1.1 403"), "{rebind}");
        let other = ask(format!("GET /x HTTP/1.1\r\nHost: {host}\r\n\r\n")).await;
        assert!(other.starts_with("HTTP/1.1 404"));
        let huge = ask(format!(
            "GET /metrics HTTP/1.1\r\nHost: {host}\r\nX: {}\r\n",
            "a".repeat(2 * MAX_REQUEST)
        ))
        .await;
        assert!(
            huge.starts_with("HTTP/1.1 400"),
            "an unterminated oversized head is refused"
        );

        tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[test]
    fn rotating_log_keeps_the_newest_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut log = RotatingLog::open(&path, 30, 3).unwrap();
        for i in 0..12 {
            log.write_line(&format!("line-{i:02}-xxxxxxxx")).unwrap();
        }
        assert!(path.exists() && rotated(&path, 1).exists() && rotated(&path, 2).exists());
        assert!(!rotated(&path, 3).exists(), "at most `keep` files");
        let newest = fs::read_to_string(&path).unwrap();
        assert!(newest.contains("line-11"), "{newest}");
        let oldest = fs::read_to_string(rotated(&path, 2)).unwrap();
        assert!(!oldest.contains("line-00"), "the oldest lines are gone");
        for p in [path.clone(), rotated(&path, 1), rotated(&path, 2)] {
            assert!(fs::metadata(p).unwrap().len() <= 30);
        }
    }

    #[test]
    fn rotating_log_cuts_an_oversized_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut log = RotatingLog::open(&path, LOG_MAX_BYTES, LOG_FILES).unwrap();
        log.write_line(&"é".repeat(MAX_LINE)).unwrap();
        assert!(fs::metadata(&path).unwrap().len() <= MAX_LINE as u64 + 1);
    }

    #[test]
    fn events_are_json_lines_with_the_stable_fields() {
        let dir = tempfile::tempdir().unwrap();
        let events = EventLog::open(dir.path()).unwrap();
        events.emit(
            "info",
            "daemon.restart",
            &[("repo", "r"), ("event", "forged")],
        );
        let line = fs::read_to_string(dir.path().join("daemon.log")).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["tier"], "daemon");
        assert_eq!(
            v["event"], "daemon.restart",
            "extra fields cannot replace the stable ones"
        );
        assert_eq!(v["repo"], "r");
        assert!(v["ts"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn timestamps_are_civil_utc() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400 + 86_399)),
            "2000-02-29T23:59:59.000Z"
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn rss_is_readable() {
        assert!(rss_bytes().is_some_and(|b| b > 0));
    }
}
