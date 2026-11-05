//! `asc cache status | ls | show | gc | clear | push | warm | run | rustc | go | restore | save | serve | env | setup`: the machine-wide build cache
//! (docs/designs/build-cache.md §2, §3, §5). The store is `asc.buildcache.dir`, else
//! `<ASC_HOME>/buildcache`; it is kept under `asc.buildcache.maxSize` (20 GiB when unset).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use asc_buildcache::{EntryInfo, GcReport, Store, ToolStats, human_size, parse_size};
use asc_state::AscHome;
use serde_json::{Value, json};

use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

pub(crate) mod bundle;
pub(crate) mod front;
pub(crate) mod go;
pub(crate) mod remote;
pub(crate) mod run;
pub(crate) mod rustc;
pub(crate) mod setup;

#[derive(clap::Parser, Debug)]
#[command(
    name = "cache",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Option<Sub>,
}

#[derive(clap::Subcommand, Debug)]
pub enum Sub {
    /// Size, entries and hit rate per tool over the last day and week.
    Status(Json),
    /// Entries, most recently used first.
    Ls(LsArgs),
    /// One entry: its outputs and the work trees and workflows that wrote and used it.
    Show(ShowArgs),
    /// Evicts least recently used entries down to the size limit.
    Gc(GcArgs),
    /// Removes every entry, or those of one tool.
    Clear(ClearArgs),
    /// Uploads the entries waiting in the outbox and lists them under a revision.
    Push(RemoteArgs),
    /// Fetches the entries a revision's builds used that this machine lacks.
    Warm(WarmArgs),
    /// Runs a command as a cached action: a hit restores its outputs and replays its output.
    Run(run::RunArgs),
    /// Cargo's `RUSTC_WRAPPER`: a compile asc caches is restored, any other runs unchanged.
    Rustc(rustc::RustcArgs),
    /// The go command's `GOCACHEPROG`: its build results kept in the shared store.
    Go(go::GoArgs),
    /// Restores a build's outputs from the cache by its inputs, and prints its key (for build
    /// tool plugins: on a miss the tool builds and calls `save`).
    Restore(bundle::RestoreArgs),
    /// Stores a build's outputs under the key `restore` printed.
    Save(bundle::SaveArgs),
    /// Serves the cache fronts (turbo, Nx, Bazel, Gradle, Maven, ccache) on the loopback interface
    /// in the foreground, for a machine without the daemon (CI).
    Serve,
    /// Prints the variables that point turbo, Nx and Maven at the cache fronts.
    Env(EnvArgs),
    /// Writes a build tool's configuration so its builds use the cache (`--undo` takes it out).
    Setup(setup::SetupArgs),
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Sh,
    Pwsh,
    Cmd,
    Json,
}

#[derive(clap::Args, Debug)]
pub struct EnvArgs {
    /// The syntax (default: pwsh on Windows, sh elsewhere).
    #[arg(long, value_enum)]
    shell: Option<Shell>,
}

#[derive(clap::Args, Debug)]
pub struct RemoteArgs {
    /// The revision to list the keys under (default: the work tree's HEAD commit).
    #[arg(long)]
    rev: Option<String>,
    /// The server remote (default: origin).
    #[arg(long)]
    remote: Option<String>,
    #[command(flatten)]
    json: Json,
}

#[derive(clap::Args, Debug)]
pub struct WarmArgs {
    /// The revision whose entries to fetch (default: the work tree's HEAD commit).
    rev: Option<String>,
    /// The server remote (default: origin).
    #[arg(long)]
    remote: Option<String>,
    #[command(flatten)]
    json: Json,
}

#[derive(clap::Args, Debug, Default)]
pub struct Json {
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct LsArgs {
    #[arg(long)]
    tool: Option<String>,
    /// At most this many entries.
    #[arg(long, short = 'n')]
    limit: Option<usize>,
    #[command(flatten)]
    json: Json,
}

#[derive(clap::Args, Debug)]
pub struct ShowArgs {
    /// The key, or a unique prefix of at least 4 hex digits.
    key: String,
    #[command(flatten)]
    json: Json,
}

#[derive(clap::Args, Debug)]
pub struct GcArgs {
    /// Overrides `asc.buildcache.maxSize` (`500M`, `20G`).
    #[arg(long = "max-size")]
    max_size: Option<String>,
    #[command(flatten)]
    json: Json,
}

#[derive(clap::Args, Debug)]
pub struct ClearArgs {
    #[arg(long)]
    tool: Option<String>,
    #[command(flatten)]
    json: Json,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "cache",
    flags: &[
        "--json",
        "--tool",
        "--limit",
        "-n",
        "--max-size",
        "--rev",
        "--remote",
        "--name",
        "--inputs",
        "--outputs",
        "--env",
        "--no-remote",
        "--quiet",
        "-q",
        "--user",
        "--repo",
        "--no-incremental",
        "--trimpath",
        "--shell",
        "--part",
        "--key",
        "--undo",
    ],
    placeholders: &[],
}];

const DAY: u64 = 86_400;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `~/` expanded.
fn expand(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(p),
    }
}

/// The store directory: `asc.buildcache.dir`, else `<home>/buildcache`.
pub(crate) fn store_dir(ctx: &Ctx) -> Result<PathBuf> {
    if let Some(p) = ctx
        .config
        .get("asc.buildcache.dir")
        .filter(|p| !p.is_empty())
    {
        return Ok(expand(&p));
    }
    let home = match ctx.ws_opt() {
        Some(ws) => ws.home.clone(),
        None => AscHome::locate()?,
    };
    Ok(home.root().join("buildcache"))
}

pub(crate) fn open_store(ctx: &Ctx) -> Result<Store> {
    let dir = store_dir(ctx)?;
    Store::open(&dir).map_err(|e| {
        CliError::Fatal(format!(
            "cannot open the build cache {}: {e}",
            dir.display()
        ))
    })
}

/// `asc.buildcache.maxSize`, else 20 GiB.
pub(crate) fn max_size(ctx: &Ctx) -> Result<u64> {
    let text = ctx
        .config
        .get("asc.buildcache.maxsize")
        .or_else(|| ctx.config.get("asc.buildcache.maxSize"));
    match text {
        Some(t) => parse_size(&t).ok_or_else(|| {
            CliError::Fatal(format!(
                "bad size '{t}' for 'asc.buildcache.maxSize' (use bytes or K, M, G, T)"
            ))
        }),
        None => Ok(asc_buildcache::DEFAULT_MAX_SIZE),
    }
}

pub(crate) fn io_err(e: std::io::Error) -> CliError {
    CliError::Fatal(format!("build cache: {e}"))
}

/// `5s ago`, `3m ago`, `2h ago`, `4d ago`.
fn ago(t: u64, now: u64) -> String {
    let s = now.saturating_sub(t);
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn stats_json(s: &ToolStats) -> Value {
    json!({
        "hits": s.hits,
        "misses": s.misses,
        "puts": s.puts,
        "restored": s.restored,
        "hit_rate": s.hit_rate(),
    })
}

fn entry_json(e: &EntryInfo) -> Value {
    let a = &e.action;
    json!({
        "key": e.key.to_string(),
        "tool": a.tool,
        "label": a.label,
        "size": a.size(),
        "outputs": a.outputs.len(),
        "exit": a.exit,
        "created": a.created,
        "last_use": e.last_use,
        "worktree": a.origin.worktree,
        "workflow": a.origin.workflow,
    })
}

fn report_json(r: &GcReport) -> Value {
    json!({
        "entries": r.entries,
        "blobs": r.blobs,
        "freed": r.freed,
        "before": r.before,
        "after": r.after,
    })
}

fn print_json(out: &mut impl Write, v: &Value) -> Result<()> {
    writeln!(out, "{v}")?;
    Ok(())
}

/// The front file of this user, created when absent.
pub(crate) fn front_file(ctx: &Ctx) -> Result<front::FrontFile> {
    let home = AscHome::locate()?;
    front::FrontFile::ensure(&home, &store_dir(ctx)?).map_err(io_err)
}

async fn serve(ctx: &Ctx) -> Result<ExitCode> {
    let f = front_file(ctx)?;
    let store = open_store(ctx)?;
    let queue = remote::mode(ctx)? == remote::Mode::ReadWrite;
    let listener = front::bind(&f).await.map_err(|e| {
        CliError::Fatal(format!(
            "cannot listen on {}: {e} (does the daemon serve the fronts already?)",
            f.addr()
        ))
    })?;
    eprintln!("asc cache: serving the build cache fronts on {}", f.url());
    front::serve(listener, front::router(store, &f.token, queue), async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(io_err)?;
    Ok(ExitCode::SUCCESS)
}

/// The variables of the tools that read their cache from the environment.
fn env_vars(f: &front::FrontFile) -> Vec<(&'static str, String)> {
    let url = f.url();
    vec![
        ("TURBO_API", url.clone()),
        ("TURBO_TOKEN", f.token.clone()),
        ("TURBO_TEAM", "asc".into()),
        ("NX_SELF_HOSTED_REMOTE_CACHE_SERVER", url.clone()),
        ("NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN", f.token.clone()),
        (
            "MAVEN_ARGS",
            format!(
                "-Dmaven.build.cache.remote.enabled=true -Dmaven.build.cache.remote.save.enabled=true \
                 -Dmaven.build.cache.remote.url={url}/maven \
                 -Dmaven.build.cache.remote.server.id={}",
                setup::fronts::MAVEN_SERVER
            ),
        ),
        ("ASC_BUILDCACHE_URL", url),
        ("ASC_BUILDCACHE_TOKEN", f.token.clone()),
    ]
}

fn env(ctx: &Ctx, a: &EnvArgs, out: &mut impl Write) -> Result<ExitCode> {
    let f = front_file(ctx)?;
    let vars = env_vars(&f);
    let shell = a.shell.unwrap_or(if cfg!(windows) {
        Shell::Pwsh
    } else {
        Shell::Sh
    });
    match shell {
        Shell::Json => {
            let map: serde_json::Map<String, Value> = vars
                .into_iter()
                .map(|(k, v)| (k.to_owned(), Value::String(v)))
                .collect();
            writeln!(out, "{}", Value::Object(map))?;
        }
        // The values are a URL, hex and a word: nothing to quote beyond the delimiters.
        Shell::Sh => vars
            .iter()
            .try_for_each(|(k, v)| writeln!(out, "export {k}='{v}'"))?,
        Shell::Pwsh => vars
            .iter()
            .try_for_each(|(k, v)| writeln!(out, "$env:{k} = '{v}'"))?,
        Shell::Cmd => vars
            .iter()
            .try_for_each(|(k, v)| writeln!(out, "set \"{k}={v}\""))?,
    }
    Ok(ExitCode::SUCCESS)
}
fn status(ctx: &Ctx, a: &Json, out: &mut impl Write) -> Result<ExitCode> {
    let store = open_store(ctx)?;
    let max = max_size(ctx)?;
    let usage = store.usage().map_err(io_err)?;
    let t = now();
    let day = store.stats(t.saturating_sub(DAY)).map_err(io_err)?;
    let week = store.stats(t.saturating_sub(7 * DAY)).map_err(io_err)?;
    if a.json {
        let tools: serde_json::Map<String, Value> = week
            .iter()
            .map(|(tool, w)| {
                let d = day.get(tool).cloned().unwrap_or_default();
                (
                    tool.clone(),
                    json!({ "day": stats_json(&d), "week": stats_json(w) }),
                )
            })
            .collect();
        print_json(
            out,
            &json!({
                "root": store.root().to_string_lossy(),
                "bytes": usage.bytes,
                "max_size": max,
                "entries": usage.entries,
                "blobs": usage.blobs,
                "tools": tools,
            }),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(out, "store    {}", store.root().display())?;
    writeln!(
        out,
        "size     {} of {} ({} entries, {} blobs)",
        human_size(usage.bytes),
        human_size(max),
        usage.entries,
        usage.blobs
    )?;
    if week.is_empty() {
        writeln!(out, "no lookups in the last 7 days")?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(
        out,
        "\n{:<16} {:>12} {:>12} {:>12}",
        "tool", "hits 24h", "hits 7d", "restored 7d"
    )?;
    for (tool, w) in &week {
        let d = day.get(tool).cloned().unwrap_or_default();
        let rate = |s: &ToolStats| {
            if s.hits + s.misses == 0 {
                "-".to_owned()
            } else {
                format!("{:.0}%", s.hit_rate() * 100.0)
            }
        };
        writeln!(
            out,
            "{:<16} {:>12} {:>12} {:>12}",
            tool,
            rate(&d),
            rate(w),
            human_size(w.restored)
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

fn ls(ctx: &Ctx, a: &LsArgs, out: &mut impl Write) -> Result<ExitCode> {
    let store = open_store(ctx)?;
    let entries: Vec<EntryInfo> = store
        .list()
        .map_err(io_err)?
        .into_iter()
        .filter(|e| a.tool.as_deref().is_none_or(|t| e.action.tool == t))
        .take(a.limit.unwrap_or(usize::MAX))
        .collect();
    if a.json.json {
        let v: Vec<Value> = entries.iter().map(entry_json).collect();
        print_json(out, &Value::Array(v))?;
        return Ok(ExitCode::SUCCESS);
    }
    let t = now();
    for e in &entries {
        let key = e.key.to_string();
        writeln!(
            out,
            "{}  {:<12} {:>10}  {:<8}  {}",
            &key[..12],
            e.action.tool,
            human_size(e.action.size()),
            ago(e.last_use, t),
            e.action.label
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

fn show(ctx: &Ctx, a: &ShowArgs, out: &mut impl Write) -> Result<ExitCode> {
    let store = open_store(ctx)?;
    let Some(e) = store
        .find(&a.key)
        .map_err(|err| CliError::Raw(format!("asc: {err}\n"), 1))?
    else {
        return Err(CliError::Raw(
            format!("asc: no build cache entry '{}'\n", a.key),
            1,
        ));
    };
    let events = store.events_for(&e.key).map_err(io_err)?;
    let worktrees: BTreeSet<&str> = events
        .iter()
        .filter_map(|v| v.worktree.as_deref())
        .collect();
    let workflows: BTreeSet<&str> = events
        .iter()
        .filter_map(|v| v.workflow.as_deref())
        .collect();
    let hits = events
        .iter()
        .filter(|v| v.kind == asc_buildcache::EventKind::Hit)
        .count();
    if a.json.json {
        let mut v = entry_json(&e);
        v["outputs"] = serde_json::to_value(&e.action.outputs)
            .map_err(|err| CliError::Fatal(err.to_string()))?;
        v["hits"] = json!(hits);
        v["used_by"] = json!({ "worktrees": worktrees, "workflows": workflows });
        print_json(out, &v)?;
        return Ok(ExitCode::SUCCESS);
    }
    let t = now();
    let ac = &e.action;
    writeln!(out, "key       {}", e.key)?;
    writeln!(out, "tool      {}", ac.tool)?;
    if !ac.label.is_empty() {
        writeln!(out, "label     {}", ac.label)?;
    }
    writeln!(
        out,
        "size      {} in {} outputs",
        human_size(ac.size()),
        ac.outputs.len()
    )?;
    writeln!(out, "created   {}", ago(ac.created, t))?;
    writeln!(
        out,
        "last use  {} ({hits} hits in 14 days)",
        ago(e.last_use, t)
    )?;
    if let Some(w) = &ac.origin.worktree {
        writeln!(out, "written   from {w}")?;
    }
    if let Some(w) = &ac.origin.workflow {
        writeln!(out, "workflow  {w}")?;
    }
    for w in &worktrees {
        writeln!(out, "used by   {w}")?;
    }
    for w in &workflows {
        writeln!(out, "used by   workflow {w}")?;
    }
    writeln!(out)?;
    for o in &ac.outputs {
        writeln!(
            out,
            "{:<4} {:>10}  {}",
            match o.kind {
                asc_buildcache::OutputKind::File => "file",
                asc_buildcache::OutputKind::Exec => "exec",
                asc_buildcache::OutputKind::Link => "link",
            },
            human_size(o.size),
            o.path
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_report(out: &mut impl Write, verb: &str, r: &GcReport, json_out: bool) -> Result<()> {
    if json_out {
        return print_json(out, &report_json(r));
    }
    writeln!(
        out,
        "{verb} {} entries and {} blobs, freed {}; {} left ({} entries)",
        r.entries,
        r.blobs,
        human_size(r.freed),
        human_size(r.after.bytes),
        r.after.entries
    )?;
    Ok(())
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let cmd = args.cmd.unwrap_or(Sub::Status(Json::default()));
    match cmd {
        Sub::Run(a) => return run::run(ctx, a).await,
        Sub::Rustc(a) => return rustc::run(ctx, a).await,
        Sub::Go(a) => return go::run(ctx, a).await,
        Sub::Serve => return serve(ctx).await,
        Sub::Restore(a) => return bundle::restore(ctx, a, &mut std::io::stdout().lock()).await,
        Sub::Save(a) => return bundle::save(ctx, a),
        _ => {}
    }
    let mut out = std::io::stdout().lock();
    match cmd {
        Sub::Status(a) => status(ctx, &a, &mut out),
        Sub::Ls(a) => ls(ctx, &a, &mut out),
        Sub::Show(a) => show(ctx, &a, &mut out),
        Sub::Gc(a) => {
            let max = match &a.max_size {
                Some(t) => parse_size(t).ok_or_else(|| {
                    CliError::Usage(format!(
                        "error: bad --max-size '{t}' (use bytes or K, M, G, T)"
                    ))
                })?,
                None => max_size(ctx)?,
            };
            let r = open_store(ctx)?.gc(max).map_err(io_err)?;
            print_report(&mut out, "removed", &r, a.json.json)?;
            Ok(ExitCode::SUCCESS)
        }
        Sub::Clear(a) => {
            let r = open_store(ctx)?.clear(a.tool.as_deref()).map_err(io_err)?;
            print_report(&mut out, "cleared", &r, a.json.json)?;
            Ok(ExitCode::SUCCESS)
        }
        Sub::Push(a) => {
            remote::push(
                ctx,
                a.remote.as_deref(),
                a.rev.as_deref(),
                a.json.json,
                &mut out,
            )
            .await
        }
        Sub::Warm(a) => {
            remote::warm(
                ctx,
                a.remote.as_deref(),
                a.rev.as_deref(),
                a.json.json,
                &mut out,
            )
            .await
        }
        Sub::Setup(a) => setup::run(ctx, &a, &mut out),
        Sub::Env(a) => env(ctx, &a, &mut out),
        Sub::Run(_) | Sub::Rustc(_) | Sub::Go(_) | Sub::Serve | Sub::Restore(_) | Sub::Save(_) => {
            unreachable!("dispatched above")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(ago(100, 105), "5s ago");
        assert_eq!(ago(0, 3 * 60), "3m ago");
        assert_eq!(ago(0, 2 * 3600 + 5), "2h ago");
        assert_eq!(ago(0, 4 * DAY), "4d ago");
        assert_eq!(ago(10, 5), "0s ago", "a clock step back");
    }
}
