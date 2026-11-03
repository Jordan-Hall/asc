//! `asc ci status` and `asc ci log` (SPEC §12.9): the CI runs recorded against a change, read
//! from the `Ci` service whichever runner or forge reported them.
//!
//! ```text
//! asc ci status [<change>] [--all] [-w] [--limit N] [--remote R] [--json]
//! asc ci log <run> <job> [-f] [--bytes N] [--remote R]
//! ```
//!
//! `status` shows the newest run of each workflow for the change (the change of `HEAD` by
//! default) and ends 0 when they passed, 1 when one failed or was cancelled and 8 while one is
//! still queued or running; `-w` waits for them to end. A run for an older version of the change
//! is shown as stale and does not count. `log` prints the tail of one job's log; `-f` follows it
//! until the job ends. Everything the server sends is text of a repository, so it is escaped
//! before it reaches a terminal.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use asc_core::change::ChangeId;
use asc_rpc::server::wire;
use asc_rpc::services::ci_client::CiClient;
use bytes::Bytes;
use clap::Parser;
use tonic::transport::Channel;

use super::changed::parse;
use crate::cli::agent::conn::{Conn, MAX_MESSAGE, short};
use crate::cli::codeintel::{change_id, clean, clean_str, emit_json, lines, with_stdout};
use crate::cli::common::{CliError, Ctx, Result};
use crate::remote::server::Session;

/// Every flag of `ci status` and `ci log`, for the native table.
pub const FLAGS: &[&str] = &[
    "--all", "--watch", "-w", "--limit", "--remote", "--json", "--follow", "-f", "--bytes",
];

/// Runs listed when `--limit` is absent, and the most the server lists at once.
const LIMIT: u32 = 30;
const MAX_LIMIT: u32 = 100;
/// How often `-w` and `-f` ask again.
const POLL: Duration = Duration::from_secs(5);
const FOLLOW_POLL: Duration = Duration::from_secs(2);
/// How long `-w` waits for the first run of a change that has none yet.
const FIRST_RUN_WAIT: Duration = Duration::from_secs(120);
/// The exit code while a run is still queued or running.
const PENDING: u8 = 8;
/// The largest tail `ci log` asks for.
const MAX_LOG_BYTES: u32 = 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "ci status",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct StatusArgs {
    /// A revision or a 32-letter change id (default: the change of `HEAD`).
    change: Option<String>,
    /// The newest runs of every change and of the default branch.
    #[arg(long, conflicts_with = "change")]
    all: bool,
    /// Wait until no run is queued or running.
    #[arg(short = 'w', long)]
    watch: bool,
    #[arg(long)]
    limit: Option<u32>,
    /// A configured remote or a server URL (default `origin`).
    #[arg(long, env = "ASC_REMOTE")]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(name = "ci log", disable_help_flag = true, disable_version_flag = true)]
struct LogArgs {
    /// The run's number, as `ci status` shows it after `#`.
    run: u64,
    /// The job's number, as `ci status` shows it after `#`.
    job: u64,
    /// Keep printing what the job writes until it ends.
    #[arg(short = 'f', long)]
    follow: bool,
    /// The most bytes of the tail to print first (default 64 KiB, at most 1 MiB).
    #[arg(long)]
    bytes: Option<u32>,
    #[arg(long, env = "ASC_REMOTE")]
    remote: Option<String>,
}

fn client(s: &Session) -> CiClient<Channel> {
    CiClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

fn bad(text: impl Into<String>) -> CliError {
    CliError::Usage(format!("asc ci: {}", text.into()))
}

/// What the runs of a change say together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// No current run.
    None,
    Passed,
    Pending,
    Failed,
}

impl Verdict {
    fn code(self) -> ExitCode {
        match self {
            Self::None | Self::Passed => ExitCode::SUCCESS,
            Self::Failed => ExitCode::FAILURE,
            Self::Pending => ExitCode::from(PENDING),
        }
    }
}

fn status_of(n: i32) -> wire::CiStatus {
    wire::CiStatus::try_from(n).unwrap_or(wire::CiStatus::Unspecified)
}

fn status_name(n: i32) -> String {
    match status_of(n) {
        wire::CiStatus::Unspecified => "unknown".to_owned(),
        s => short(s.as_str_name(), "CI_STATUS_"),
    }
}

fn is_over(n: i32) -> bool {
    matches!(
        status_of(n),
        wire::CiStatus::Success
            | wire::CiStatus::Failure
            | wire::CiStatus::Cancelled
            | wire::CiStatus::Skipped
    )
}

/// The newest run of each workflow: a run reported again (a re-run) replaces the older one.
/// `runs` are newest first, as the server lists them.
fn newest(runs: Vec<wire::CiRun>) -> Vec<wire::CiRun> {
    let mut seen = HashSet::new();
    runs.into_iter()
        .filter(|r| seen.insert((r.source.clone(), r.name.clone(), r.change.clone())))
        .collect()
}

/// A failed or cancelled run fails the change; else one queued or running keeps it pending.
/// A stale run is for an older version and does not count.
fn verdict(runs: &[wire::CiRun]) -> Verdict {
    let current: Vec<_> = runs.iter().filter(|r| !r.stale).collect();
    if current.is_empty() {
        return Verdict::None;
    }
    let failed = current.iter().any(|r| {
        matches!(
            status_of(r.status),
            wire::CiStatus::Failure | wire::CiStatus::Cancelled
        )
    });
    if failed {
        Verdict::Failed
    } else if current.iter().all(|r| is_over(r.status)) {
        Verdict::Passed
    } else {
        Verdict::Pending
    }
}

/// `1h02m`, `2m10s` or `40s`; empty before a start. A running job counts to `now`.
fn duration(started: i64, finished: i64, now: i64) -> String {
    if started <= 0 {
        return String::new();
    }
    let end = if finished > 0 { finished } else { now };
    let s = (end - started).max(0);
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn change_text(change: &[u8]) -> String {
    match <[u8; 16]>::try_from(change) {
        Ok(b) => ChangeId::from_bytes(b).short(),
        Err(_) if change.is_empty() => "default branch".to_owned(),
        Err(_) => "?".to_owned(),
    }
}

fn run_line(r: &wire::CiRun, now: i64, with_change: bool) -> String {
    let mut line = format!(
        "{:<9} #{} {} ({})",
        status_name(r.status),
        r.id,
        clean_str(&r.name),
        clean_str(&r.source)
    );
    if with_change {
        line.push_str(&format!(" [{}]", change_text(&r.change)));
    }
    let d = duration(r.started, r.finished, now);
    if !d.is_empty() {
        line.push_str(&format!("  {d}"));
    }
    if r.stale {
        line.push_str("  stale");
    }
    line
}

fn job_line(j: &wire::CiJob, now: i64) -> String {
    let mut line = format!(
        "  {:<9} #{} {}",
        status_name(j.status),
        j.id,
        clean_str(&j.name)
    );
    let d = duration(j.started, j.finished, now);
    if !d.is_empty() {
        line.push_str(&format!("  {d}"));
    }
    line
}

fn render(out: &mut dyn Write, runs: &[wire::CiRun], now: i64, with_change: bool) -> Result<()> {
    for r in runs {
        writeln!(out, "{}", run_line(r, now, with_change))?;
        if !r.url.is_empty() {
            writeln!(out, "  {}", clean_str(&r.url))?;
        }
        for j in &r.jobs {
            writeln!(out, "{}", job_line(j, now))?;
        }
    }
    Ok(())
}

/// The lines `-w` prints for what changed since `before`: a run that is new or changed state,
/// and each job that is new or did.
fn changes(before: &HashMap<u64, Snapshot>, runs: &[wire::CiRun], now: i64) -> Vec<String> {
    let mut out = Vec::new();
    for r in runs {
        let old = before.get(&r.id);
        if old.is_none_or(|o| o.status != r.status) {
            out.push(run_line(r, now, false));
        }
        for j in &r.jobs {
            let was = old.and_then(|o| o.jobs.get(&j.id));
            if was != Some(&j.status) {
                out.push(format!("#{}{}", r.id, job_line(j, now)));
            }
        }
    }
    out
}

#[derive(Debug, Default)]
struct Snapshot {
    status: i32,
    jobs: HashMap<u64, i32>,
}

fn snapshot(runs: &[wire::CiRun]) -> HashMap<u64, Snapshot> {
    runs.iter()
        .map(|r| {
            let jobs = r.jobs.iter().map(|j| (j.id, j.status)).collect();
            (
                r.id,
                Snapshot {
                    status: r.status,
                    jobs,
                },
            )
        })
        .collect()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

async fn list(conn: &Conn, change: &[u8], limit: u32) -> Result<Vec<wire::CiRun>> {
    let msg = wire::ListRunsRequest {
        repo: Some(conn.repo_ref()),
        change: Bytes::copy_from_slice(change),
        status: 0,
        limit,
        page_token: String::new(),
    };
    let runs = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .list_runs(s.wire.request(m, None))
                    .await?
                    .into_inner()
                    .runs)
            }
        })
        .await?;
    Ok(newest(runs))
}

/// The change `status` is about: the one named, else the change of `HEAD`; `None` for every
/// change (`--all`, or no workspace, or a `HEAD` that is no change).
async fn target(ctx: &Ctx, a: &StatusArgs) -> Result<Option<ChangeId>> {
    if a.all {
        return Ok(None);
    }
    match &a.change {
        Some(word) => change_id(ctx, Some(word)).await.map(Some),
        None if ctx.ws_opt().is_none() => Ok(None),
        None => Ok(change_id(ctx, None).await.ok()),
    }
}

pub async fn status(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a: StatusArgs = parse("ci status", rest)?;
    let limit = a.limit.unwrap_or(LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(bad(format!("--limit is 1 to {MAX_LIMIT}")));
    }
    let change = target(ctx, &a).await?;
    let bytes: &[u8] = change.as_ref().map_or(&[], |c| c.as_bytes());
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let mut runs = list(&conn, bytes, limit).await?;
    if a.watch {
        let begun = Instant::now();
        let mut seen = HashMap::new();
        loop {
            if !a.json {
                let lines = changes(&seen, &runs, now());
                if !lines.is_empty() {
                    let mut err = std::io::stderr().lock();
                    for l in lines {
                        let _ = writeln!(err, "{l}");
                    }
                }
            }
            seen = snapshot(&runs);
            match verdict(&runs) {
                Verdict::Pending => {}
                Verdict::None if begun.elapsed() < FIRST_RUN_WAIT => {}
                _ => break,
            }
            tokio::time::sleep(POLL).await;
            runs = list(&conn, bytes, limit).await?;
        }
    }
    let v = verdict(&runs);
    with_stdout(|out| {
        if a.json {
            let mut l = lines(out);
            for r in &runs {
                emit_json(&mut l, "asc.v1.CiRun", r)?;
            }
        } else if runs.is_empty() {
            match &change {
                Some(c) => writeln!(out, "no CI runs for {}", c.short())?,
                None => writeln!(out, "no CI runs")?,
            }
        } else {
            render(out, &runs, now(), change.is_none())?;
        }
        Ok(v.code())
    })
}

/// Whether the job has ended; `None` when the run is not among the newest ones.
async fn job_over(conn: &Conn, run: u64, job: u64) -> Result<Option<bool>> {
    let msg = wire::ListRunsRequest {
        repo: Some(conn.repo_ref()),
        limit: MAX_LIMIT,
        ..Default::default()
    };
    let runs = conn
        .call(false, |s, _| {
            let m = msg.clone();
            async move {
                Ok(client(&s)
                    .list_runs(s.wire.request(m, None))
                    .await?
                    .into_inner()
                    .runs)
            }
        })
        .await?;
    Ok(runs.iter().find(|r| r.id == run).map(|r| {
        is_over(r.status)
            || r.jobs
                .iter()
                .find(|j| j.id == job)
                .is_none_or(|j| is_over(j.status))
    }))
}

/// Removes CSI sequences (colours, cursor moves) from a log: they are layout, not text, and the
/// rest is escaped by [`clean`].
fn strip_csi(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && data.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < data.len() && (0x20..=0x3f).contains(&data[j]) {
                j += 1;
            }
            if j < data.len() && (0x40..=0x7e).contains(&data[j]) {
                i = j + 1;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// A log as terminal text: CSI removed, each line escaped on its own.
fn log_text(data: &[u8]) -> String {
    let data = strip_csi(data);
    let mut out = String::new();
    for line in data.split_inclusive(|b| *b == b'\n') {
        let (body, nl) = match line.strip_suffix(b"\n") {
            Some(b) => (b.strip_suffix(b"\r").unwrap_or(b), true),
            None => (line, false),
        };
        out.push_str(&clean(body));
        if nl {
            out.push('\n');
        }
    }
    out
}

async fn log_part(conn: &Conn, a: &LogArgs, since: u64) -> Result<wire::JobLogResponse> {
    let msg = wire::JobLogRequest {
        repo: Some(conn.repo_ref()),
        run_id: a.run,
        job_id: a.job,
        max_bytes: a.bytes.unwrap_or(0),
        since_offset: since,
    };
    conn.call(false, |s, _| {
        let m = msg.clone();
        async move {
            Ok(client(&s)
                .job_log(s.wire.request(m, None))
                .await?
                .into_inner())
        }
    })
    .await
}

pub async fn log(ctx: &Ctx, rest: &[OsString]) -> Result<ExitCode> {
    let a: LogArgs = parse("ci log", rest)?;
    if a.bytes.is_some_and(|b| b == 0 || b > MAX_LOG_BYTES) {
        return Err(bad(format!("--bytes is 1 to {MAX_LOG_BYTES}")));
    }
    let conn = Conn::open(ctx, a.remote.as_deref()).await?;
    let first = log_part(&conn, &a, 0).await?;
    if first.truncated {
        eprintln!(
            "(the first {} bytes of the log are not shown)",
            first.offset
        );
    }
    let mut out = std::io::stdout().lock();
    let mut write = |data: &[u8]| -> Result<bool> {
        match out
            .write_all(log_text(data).as_bytes())
            .and_then(|()| out.flush())
        {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
            Err(e) => Err(CliError::Io(e)),
        }
    };
    if !write(&first.data)? || !a.follow {
        return Ok(ExitCode::SUCCESS);
    }
    let mut next = first.next_offset;
    loop {
        // Ask whether the job ended before reading, so the bytes it wrote last are read after.
        let over = job_over(&conn, a.run, a.job).await?.unwrap_or(false);
        let part = log_part(&conn, &a, next).await?;
        if part.next_offset > next {
            if !write(&part.data)? {
                return Ok(ExitCode::SUCCESS);
            }
            next = part.next_offset;
        }
        if over {
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(FOLLOW_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: u64, name: &str, status: wire::CiStatus, stale: bool) -> wire::CiRun {
        wire::CiRun {
            id,
            source: "hub".into(),
            name: name.into(),
            status: status as i32,
            stale,
            ..Default::default()
        }
    }

    #[test]
    fn a_rerun_replaces_the_older_run() {
        use wire::CiStatus as S;
        let runs = newest(vec![
            run(3, "ci", S::Success, false),
            run(2, "lint", S::Failure, false),
            run(1, "ci", S::Failure, false),
        ]);
        let ids: Vec<_> = runs.iter().map(|r| r.id).collect();
        assert_eq!(ids, [3, 2]);
    }

    #[test]
    fn the_verdict_counts_current_runs_only() {
        use wire::CiStatus as S;
        assert_eq!(verdict(&[]), Verdict::None);
        assert_eq!(verdict(&[run(1, "a", S::Failure, true)]), Verdict::None);
        assert_eq!(
            verdict(&[
                run(1, "a", S::Success, false),
                run(2, "b", S::Skipped, false)
            ]),
            Verdict::Passed
        );
        assert_eq!(
            verdict(&[
                run(1, "a", S::Success, false),
                run(2, "b", S::Running, false)
            ]),
            Verdict::Pending
        );
        assert_eq!(
            verdict(&[
                run(1, "a", S::Cancelled, false),
                run(2, "b", S::Queued, false)
            ]),
            Verdict::Failed
        );
        assert_eq!(
            verdict(&[run(1, "a", S::Unspecified, false)]),
            Verdict::Pending
        );
        assert_eq!(Verdict::Pending.code(), ExitCode::from(PENDING));
    }

    #[test]
    fn durations_read_plainly() {
        assert_eq!(duration(0, 0, 100), "");
        assert_eq!(duration(100, 140, 0), "40s");
        assert_eq!(duration(100, 0, 230), "2m10s");
        assert_eq!(duration(1, 3_722, 0), "1h02m");
        assert_eq!(duration(200, 100, 0), "0s");
    }

    #[test]
    fn lines_escape_server_text() {
        let mut r = run(7, "ci\u{1b}[2J", wire::CiStatus::Running, true);
        r.started = 100;
        r.change = Bytes::from_static(&[0; 16]);
        let line = run_line(&r, 160, true);
        assert!(line.starts_with("running   #7 ci\\x1b[2J (hub)"), "{line}");
        assert!(
            line.contains("[zzzzzzzzzzzz]") && line.ends_with("1m00s  stale"),
            "{line}"
        );
        assert_eq!(change_text(&[]), "default branch");
    }

    #[test]
    fn watch_prints_what_changed() {
        use wire::CiStatus as S;
        let mut a = run(1, "ci", S::Running, false);
        a.jobs = vec![
            wire::CiJob {
                id: 10,
                name: "build".into(),
                status: S::Running as i32,
                ..Default::default()
            },
            wire::CiJob {
                id: 11,
                name: "test".into(),
                status: S::Queued as i32,
                ..Default::default()
            },
        ];
        let first = changes(&HashMap::new(), std::slice::from_ref(&a), 0);
        assert_eq!(first.len(), 3, "{first:?}");
        let before = snapshot(std::slice::from_ref(&a));
        assert!(changes(&before, std::slice::from_ref(&a), 0).is_empty());
        a.jobs[0].status = S::Success as i32;
        let next = changes(&before, std::slice::from_ref(&a), 0);
        assert_eq!(next, ["#1  success   #10 build"]);
    }

    #[test]
    fn a_log_loses_its_colours_but_not_its_lines() {
        let log = b"\x1b[32mok\x1b[0m done\r\nnext \x1b]0;title\x07\npartial";
        assert_eq!(log_text(log), "ok done\nnext \\x1b]0;title\\x07\npartial");
        assert_eq!(strip_csi(b"\x1b["), b"\x1b[");
    }
}
