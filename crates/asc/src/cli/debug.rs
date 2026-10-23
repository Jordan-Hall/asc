//! `asc debug stats|hydrations|trace` (SPEC §10.2 item 5, §12.4 "Client diagnostics").
//!
//! * `stats [--json]`: the size of the cache and the store, the bytes each workspace holds, and
//!   (when the daemon runs) its version, uptime, hydrations and journal size per workspace.
//!   A virtual workspace holds about nothing until files are read.
//! * `hydrations [--by-process] [--json]`: the daemon's hydration counters per path, or per
//!   process image with `--by-process`, busiest first. Without a daemon nothing hydrates and
//!   there is nothing to list.
//! * `trace`: asks a running daemon for a diagnostic dump in `log/` and prints its path.
//!
//! Nothing here starts the daemon; "when present" means a `Ping` answered. Counters the daemon
//! does not keep (a cache hit ratio, its resident memory) are not reported.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use asc_rpc::ipc::{WorkspaceClient, connect};
use asc_rpc::server::wire;
use asc_state::AscHome;
use asc_state::state::workspaces::{self, Mode};
use serde_json::{Value, json};

use super::common::{CliError, Ctx, Result};
use super::doctor::{Size, dir_size, human};
use super::refs_plumbing::args::{Cursor, Word};
use crate::daemon::lifecycle::{self, Config, Info, Probe};
use crate::daemon::scheduler;
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc debug stats [--json]
   or: asc debug hydrations [--by-process] [--json]
   or: asc debug trace";

/// Most hydration rows printed.
const MAX_ROWS: usize = 1_000;

#[derive(clap::Parser, Debug)]
#[command(name = "debug", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "debug",
    flags: &["--json", "--by-process"],
    placeholders: &[],
}];

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Stats { json: bool },
    Hydrations { json: bool, by_process: bool },
    Trace,
}

fn parse(rest: &[OsString]) -> Result<Action> {
    let mut cur = Cursor::new(rest, USAGE);
    let Some(Word::Operand(sub)) = cur.next() else {
        return Err(cur.usage_only());
    };
    let (mut json, mut by_process) = (false, false);
    while let Some(word) = cur.next() {
        match word {
            Word::Long(name, None) if name == "json" => json = true,
            Word::Long(name, None) if name == "by-process" && sub == "hydrations" => {
                by_process = true;
            }
            Word::Long(name, _) => return Err(cur.usage_error(&format!("unknown option `{name}'"))),
            Word::End | Word::Short(_) | Word::Operand(_) => return Err(cur.usage_only()),
        }
    }
    match sub.as_str() {
        "stats" => Ok(Action::Stats { json }),
        "hydrations" => Ok(Action::Hydrations { json, by_process }),
        "trace" if !json => Ok(Action::Trace),
        _ => Err(cur.usage_only()),
    }
}

/// The daemon, when one answers.
struct Daemon {
    cfg: Config,
    info: Info,
}

async fn daemon_for(home: &AscHome) -> Option<Daemon> {
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
        return None;
    }
    let cfg = Config::for_home(home).ok()?;
    match lifecycle::probe(&cfg).await {
        Probe::Up(info) => Some(Daemon { cfg, info }),
        Probe::Hung | Probe::Down => None,
    }
}

impl Daemon {
    /// The counters of one workspace; `None` when the daemon does not host it.
    async fn stats(&self, ws_id: &str) -> Option<wire::StatsResponse> {
        let mut client = WorkspaceClient::new(connect(&self.cfg.endpoint));
        client
            .stats(wire::StatsRequest {
                ws_id: ws_id.to_owned(),
            })
            .await
            .ok()
            .map(tonic::Response::into_inner)
    }
}

fn cache_root(ctx: &Ctx, home: &AscHome) -> PathBuf {
    match ctx.config.get("asc.cache.dir") {
        Some(p) => match (p.strip_prefix("~/"), std::env::home_dir()) {
            (Some(rest), Some(h)) => h.join(rest),
            _ => PathBuf::from(p),
        },
        None => home.cache(),
    }
}

fn cache_limit(ctx: &Ctx) -> u64 {
    ctx.config
        .get("asc.cache.maxsize")
        .or_else(|| ctx.config.get("asc.cache.maxSize"))
        .and_then(|t| scheduler::parse_size(&t))
        .unwrap_or_else(|| scheduler::Settings::new(PathBuf::new()).cache_max_bytes)
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Plain => "plain",
        Mode::Virtual => "virtual",
    }
}

fn size_json(s: Size) -> Value {
    json!({ "bytes": s.bytes, "files": s.files, "truncated": s.truncated })
}

fn size_line(label: &str, s: Size) -> String {
    format!(
        "{label}: {} bytes ({}) in {} files{}",
        s.bytes,
        human(s.bytes),
        s.files,
        if s.truncated { ", at least" } else { "" }
    )
}

/// The size of a work tree: the files it holds, without `.git`/`.asc` and without entering links.
/// A virtual workspace is not walked: enumerating it would list placeholders, not disk use.
fn workspace_size(path: &str, mode: Mode) -> Size {
    if mode != Mode::Plain {
        return Size::default();
    }
    dir_size_without_meta(std::path::Path::new(path))
}

fn dir_size_without_meta(root: &std::path::Path) -> Size {
    let mut size = Size::default();
    size.truncated = super::doctor::walk_dirs(
        root,
        &|n| n == ".git" || n == ".asc",
        20_000_000,
        &mut |_, entries| {
            for (entry, kind) in entries {
                if kind.is_file() {
                    size.files += 1;
                    size.bytes = size
                        .bytes
                        .saturating_add(entry.metadata().map_or(0, |m| m.len()));
                }
            }
        },
    );
    size
}

async fn stats(ctx: &Ctx, json_out: bool) -> Result<ExitCode> {
    let home = match ctx.ws_opt() {
        Some(ws) => ws.home.clone(),
        None => AscHome::locate()?,
    };
    let cache = dir_size(&cache_root(ctx, &home));
    let store = dir_size(&home.store());
    let limit = cache_limit(ctx);
    let daemon = daemon_for(&home).await;

    let mut rows: Vec<Value> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    if let Some(ws) = ctx.ws_opt() {
        for w in workspaces::list(&ws.db, Some(ws.repo.key)).await? {
            let id = w.ws.id.to_string();
            let size = workspace_size(&w.ws.path, w.ws.mode);
            let live = match &daemon {
                Some(d) => d.stats(&id).await,
                None => None,
            };
            let mut row = json!({
                "path": w.ws.path,
                "mode": mode_name(w.ws.mode),
                "bytes": size.bytes,
                "files": size.files,
            });
            let mut line = format!(
                "workspace {}: {} bytes ({}) in {} files",
                w.ws.path,
                size.bytes,
                human(size.bytes),
                size.files
            );
            if let Some(s) = live {
                let hydrations: u64 = s.by_path.iter().map(|r| r.count).sum();
                row["hydrations"] = json!(hydrations);
                row["bytes_hydrated"] = json!(s.bytes_hydrated);
                row["journal_size"] = json!(s.journal_size);
                line.push_str(&format!(
                    "; {hydrations} hydrations ({} bytes), journal {}",
                    s.bytes_hydrated, s.journal_size
                ));
            }
            rows.push(row);
            lines.push(line);
        }
    }

    if json_out {
        let mut out = json!({
            "cache": size_json(cache),
            "cache_limit": limit,
            "store": size_json(store),
            "workspaces": rows,
            "daemon": Value::Null,
        });
        if let Some(d) = &daemon {
            out["daemon"] = json!({
                "version": d.info.version,
                "pid": d.info.pid,
                "uptime_ms": d.info.uptime_ms,
            });
        }
        println!("{out}");
    } else {
        println!(
            "{} (limit {} bytes, {})",
            size_line("cache", cache),
            limit,
            human(limit)
        );
        println!("{}", size_line("store", store));
        for l in &lines {
            println!("{l}");
        }
        match &daemon {
            Some(d) => println!(
                "daemon: running (pid {}, version {}, up {} s)",
                d.info.pid,
                d.info.version,
                d.info.uptime_ms / 1000
            ),
            None => println!("daemon: not running"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn hydrations(ctx: &Ctx, json_out: bool, by_process: bool) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let Some(daemon) = daemon_for(&ws.home).await else {
        if json_out {
            println!("[]");
        } else {
            println!("no hydrations recorded: the daemon is not running");
        }
        return Ok(ExitCode::SUCCESS);
    };
    let mut rows: Vec<Value> = Vec::new();
    for w in workspaces::list(&ws.db, Some(ws.repo.key)).await? {
        let Some(s) = daemon.stats(&w.ws.id.to_string()).await else {
            continue;
        };
        let counts = if by_process { s.by_process } else { s.by_path };
        for r in counts.into_iter().take(MAX_ROWS) {
            let mut row = json!({
                "workspace": w.ws.path,
                "count": r.count,
                "bytes": r.bytes,
            });
            row[if by_process { "process" } else { "path" }] = json!(r.key);
            rows.push(row);
        }
    }
    if json_out {
        println!("{}", Value::Array(rows));
    } else if rows.is_empty() {
        println!("no hydrations recorded");
    } else {
        for r in &rows {
            let key = r
                .get("process")
                .or_else(|| r.get("path"))
                .and_then(Value::as_str);
            println!(
                "{}\t{}\t{}\t{}",
                r["count"],
                r["bytes"],
                r["workspace"].as_str().unwrap_or_default(),
                key.unwrap_or_default()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn trace(ctx: &Ctx) -> Result<ExitCode> {
    let home = match ctx.ws_opt() {
        Some(ws) => ws.home.clone(),
        None => AscHome::locate()?,
    };
    let Some(daemon) = daemon_for(&home).await else {
        return Err(CliError::Fatal("asc: the daemon is not running".into()));
    };
    let path = lifecycle::write_dump(&daemon.cfg, daemon.info.pid)
        .map_err(|e| CliError::Fatal(format!("asc: cannot write the dump: {e}")))?;
    println!("{}", path.display());
    Ok(ExitCode::SUCCESS)
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    match parse(&args.rest)? {
        Action::Stats { json } => stats(ctx, json).await,
        Action::Hydrations { json, by_process } => hydrations(ctx, json, by_process).await,
        Action::Trace => trace(ctx).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn subcommands() {
        assert_eq!(
            parse(&os(&["stats"])).unwrap(),
            Action::Stats { json: false }
        );
        assert_eq!(
            parse(&os(&["stats", "--json"])).unwrap(),
            Action::Stats { json: true }
        );
        assert_eq!(
            parse(&os(&["hydrations", "--by-process"])).unwrap(),
            Action::Hydrations {
                json: false,
                by_process: true
            }
        );
        assert_eq!(parse(&os(&["trace"])).unwrap(), Action::Trace);
        assert!(parse(&os(&[])).is_err());
        assert!(parse(&os(&["stats", "--by-process"])).is_err());
        assert!(parse(&os(&["bogus"])).is_err());
        assert!(parse(&os(&["trace", "--json"])).is_err());
    }
}
