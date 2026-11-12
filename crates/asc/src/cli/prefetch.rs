//! `asc prefetch` (SPEC §10.2 items 5a and 5b, §11.2): fills the machine cache with what HEAD
//! holds, so later reads (a build, a language server, an offline stretch) hit local disk. Files
//! are read through the object stack, which verifies each object against its key and caches what
//! the remote tier supplies; nothing is written to the work tree.
//!
//! What is warmed:
//! - `<pathspec>…`: the files under the pathspecs;
//! - `--history <n>`: the changesets and root trees of the `n` newest commits reachable from
//!   HEAD, so `log` and `diff` work offline over that window;
//! - `--profile <name>`: the paths a learned profile holds (item 5b); `--learned`: every profile
//!   learned in the last `asc.prefetch.learnedDays` days;
//! - `--auto`: the `.asc/prefetch` globs of HEAD (default: the source globs of item 5a, outside
//!   redirections) and the learned profiles. `asc clone` and `asc worktree add` start it in the
//!   background; a branch switch starts `--learned`.
//!
//! Profiles are learned from what a build read: `--learn <name>` takes the paths on standard
//! input (newline or NUL separated, absolute inside the work tree or repository-relative; the
//! build plugins send their file dependencies this way), and `--record <name> -- <command>…` runs
//! a command in a virtual workspace and keeps the paths the daemon hydrated while it ran. Only
//! files HEAD tracks are kept. `--list`, `--show` and `--forget` manage the profiles.
//!
//! Work is bounded: a fixed read fan-out, no file is held whole (a chunked file is read chunk by
//! chunk), `--history` stops at [`MAX_HISTORY`], a profile holds at most
//! [`prefetch::MAX_PATHS`] paths and standard input is read up to [`MAX_STDIN`] bytes.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asc_core::codec::decode_envelope;
use asc_core::content::CHUNK_THRESHOLD;
use asc_core::id::{ChangesetId, FileId, TreeId};
use asc_core::object::{ChunkList, Entry, Kind};
use asc_core::path::RepoPath;
use asc_engine::tree::Pathspec;
use asc_rpc::ipc::{WorkspaceClient, connect};
use asc_rpc::server::wire;
use asc_state::prefetch;
use asc_state::state::workspaces::Mode;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_wc::redirect::Redirections;
use futures::StreamExt;
use serde_json::json;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::restore::Spec;
use crate::daemon::lifecycle::{self, Config, Probe};
use crate::dispatch::Program;
use crate::env::GitEnv;
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc prefetch <pathspec>... [--history <n>]
   or: asc prefetch [--profile <name>]... [--learned] [--auto] [-q]
   or: asc prefetch --learn <name> [--replace] < paths
   or: asc prefetch --record <name> [--replace] -- <command>...
   or: asc prefetch --list [--json]
   or: asc prefetch --show <name>
   or: asc prefetch --forget <name>";

/// Files read at once.
const READS: usize = 8;

/// Most commits `--history` fetches.
const MAX_HISTORY: u64 = 1_000_000;

/// Most failures listed before the rest are only counted.
const MAX_LISTED: usize = 20;

/// Most bytes `--learn` reads from standard input.
const MAX_STDIN: u64 = 64 << 20;

/// Default `asc.prefetch.learnedDays`.
const LEARNED_DAYS: i64 = 14;

/// The globs `--auto` warms when HEAD has no `.asc/prefetch` (SPEC §10.2 item 5a).
const DEFAULT_GLOBS: &[&str] = &[":(glob)**/*.{ts,tsx,rs,py,cs,toml,json}"];

/// Longest `.asc/prefetch` read, in bytes.
const MAX_GLOB_FILE: u64 = 64 << 10;

#[derive(clap::Parser, Debug)]
#[command(
    name = "prefetch",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "prefetch",
    flags: &[
        "--history",
        "--profile",
        "--learned",
        "--auto",
        "--learn",
        "--record",
        "--replace",
        "--list",
        "--json",
        "--show",
        "--forget",
        "-q",
        "--quiet",
        "--in",
    ],
    placeholders: &[],
}];

/// What one run does besides warming.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Warm,
    Learn { name: String },
    Record { name: String, cmd: Vec<OsString> },
    List,
    Show { name: String },
    Forget { name: String },
}

#[derive(Debug, PartialEq, Eq)]
struct Opts {
    paths: Vec<OsString>,
    history: u64,
    profiles: Vec<String>,
    learned: bool,
    auto: bool,
    replace: bool,
    json: bool,
    quiet: bool,
    /// `--in <dir>`: the workspace to warm, for a background child started outside it (a
    /// process whose current directory is inside a work tree keeps Windows from deleting it).
    within: Option<std::path::PathBuf>,
    action: Action,
}

impl Opts {
    fn warms(&self) -> bool {
        !self.paths.is_empty()
            || self.history > 0
            || !self.profiles.is_empty()
            || self.learned
            || self.auto
    }
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut o = Opts {
        paths: Vec::new(),
        history: 0,
        profiles: Vec::new(),
        learned: false,
        auto: false,
        replace: false,
        json: false,
        quiet: false,
        within: None,
        action: Action::Warm,
    };
    let mut it = rest.iter();
    let mut literal = false;
    let set = |o: &mut Opts, a: Action| {
        if o.action != Action::Warm {
            return Err(usage(
                "--learn, --record, --list, --show and --forget exclude each other",
            ));
        }
        o.action = a;
        Ok(())
    };
    while let Some(a) = it.next() {
        let text = a.to_string_lossy();
        if literal || !text.starts_with('-') || text == "-" {
            o.paths.push(a.clone());
            continue;
        }
        if text == "--in" {
            let dir = it.next().ok_or_else(|| usage("--in needs a directory"))?;
            o.within = Some(dir.into());
            continue;
        }
        let (name, inline) = match text.split_once('=') {
            Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
            None => (text.to_string(), None),
        };
        let mut value = |what: &str| -> Result<String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => it
                    .next()
                    .map(|v| v.to_string_lossy().into_owned())
                    .ok_or_else(|| usage(&format!("{name} needs {what}"))),
            }
        };
        match name.as_str() {
            "--" => {
                if let Action::Record { cmd, .. } = &mut o.action {
                    cmd.extend(it.by_ref().cloned());
                } else {
                    literal = true;
                }
            }
            "--history" => {
                let v = value("a number")?;
                o.history = v
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n <= MAX_HISTORY)
                    .ok_or_else(|| {
                        usage(&format!("--history takes 0 to {MAX_HISTORY}, not '{v}'"))
                    })?;
            }
            "--profile" => {
                let n = profile_name(value("a profile name")?)?;
                o.profiles.push(n);
            }
            "--learned" => o.learned = true,
            "--auto" => o.auto = true,
            "--replace" => o.replace = true,
            "--json" => o.json = true,
            "-q" | "--quiet" => o.quiet = true,
            "--learn" => {
                let name = profile_name(value("a profile name")?)?;
                set(&mut o, Action::Learn { name })?;
            }
            "--record" => {
                let name = profile_name(value("a profile name")?)?;
                let cmd = Vec::new();
                set(&mut o, Action::Record { name, cmd })?;
            }
            "--list" => set(&mut o, Action::List)?,
            "--show" => {
                let name = profile_name(value("a profile name")?)?;
                set(&mut o, Action::Show { name })?;
            }
            "--forget" => {
                let name = profile_name(value("a profile name")?)?;
                set(&mut o, Action::Forget { name })?;
            }
            other => return Err(usage(&format!("unknown option `{other}'"))),
        }
    }
    let only_with = "--replace goes with --learn or --record, --json with --list";
    match &o.action {
        Action::Warm if !o.warms() => return Err(CliError::Usage(USAGE.to_owned())),
        Action::Warm if o.replace || o.json => return Err(usage(only_with)),
        Action::Warm => {}
        _ if o.within.is_some() => return Err(usage("--in goes with a warm")),
        _ if o.warms() => return Err(usage("a learn or a listing warms nothing")),
        Action::Record { cmd, .. } if cmd.is_empty() => {
            return Err(usage("--record needs a command after `--'"));
        }
        Action::Learn { .. } | Action::Record { .. } if o.json => return Err(usage(only_with)),
        Action::List if o.replace => return Err(usage(only_with)),
        Action::Show { .. } | Action::Forget { .. } if o.replace || o.json => {
            return Err(usage(only_with));
        }
        _ => {}
    }
    Ok(o)
}

fn profile_name(name: String) -> Result<String> {
    if prefetch::valid_name(&name) {
        Ok(name)
    } else {
        Err(usage(&format!(
            "'{name}' is not a profile name: 1 to {} bytes, no spaces",
            prefetch::MAX_NAME_LEN
        )))
    }
}

fn usage(msg: &str) -> CliError {
    CliError::Usage(format!("error: {msg}\n{USAGE}"))
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {e}"))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Reads every object of one file through the stack, never holding more than one chunk.
async fn warm_file(
    objects: &dyn Objects,
    file: FileId,
    size: u64,
) -> std::result::Result<u64, String> {
    let miss = |what: &str| format!("{what} of {file} is not available");
    if size <= CHUNK_THRESHOLD {
        let data = objects
            .get(&ObjKey::File(file))
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| miss("the content"))?;
        return Ok(data.len() as u64);
    }
    let list = objects
        .get(&ObjKey::ChunkList(file))
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| miss("the chunk list"))?;
    let list = decode_envelope::<ChunkList>(&list).map_err(|e| e.to_string())?;
    if list.size != size {
        return Err(format!("the chunk list of {file} disagrees with its size"));
    }
    let mut total = 0u64;
    for (id, len) in &list.chunks {
        let chunk = objects
            .get(&ObjKey::Chunk(*id))
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| miss("a chunk"))?;
        if chunk.len() != *len as usize {
            return Err(format!("a chunk of {file} has the wrong length"));
        }
        total += chunk.len() as u64;
    }
    Ok(total)
}

fn is_content(e: &Entry) -> bool {
    matches!(e.kind, Kind::File | Kind::Exec | Kind::Symlink)
}

/// What one warm read and what it could not.
#[derive(Default)]
struct Tally {
    files: u64,
    bytes: u64,
    failures: Vec<String>,
    /// Paths already warmed in this run, so a file named twice is read once.
    seen: HashSet<RepoPath>,
}

impl Tally {
    async fn warm(&mut self, objects: &dyn Objects, entries: Vec<(RepoPath, Entry)>) {
        let fresh: Vec<_> = entries
            .into_iter()
            .filter(|(p, _)| self.seen.insert(p.clone()))
            .collect();
        let mut work = Box::pin(
            futures::stream::iter(fresh)
                .map(|(path, e)| async move {
                    let read = warm_file(objects, FileId::from_bytes(e.id), e.size).await;
                    (path, read)
                })
                .buffer_unordered(READS),
        );
        while let Some((path, read)) = work.next().await {
            match read {
                Ok(n) => {
                    self.files += 1;
                    self.bytes = self.bytes.saturating_add(n);
                }
                Err(why) => self.failures.push(format!("{path}: {why}")),
            }
        }
    }
}

/// The files of `root` a pathspec selects, outside redirections.
async fn select(
    ws: &WsCtx,
    root: TreeId,
    spec: &Pathspec,
    skip: Option<&Redirections>,
) -> Result<Vec<(RepoPath, Entry)>> {
    let mut walk = Box::pin(ws.engine.walk(root, Some(spec)));
    let mut out = Vec::new();
    while let Some(next) = walk.next().await {
        let (path, e) = next.map_err(fatal)?;
        if is_content(&e) && !skip.is_some_and(|r| r.covers(&path)) {
            out.push((path, e));
        }
    }
    Ok(out)
}

/// The entries of `paths` that `root` holds as files.
async fn lookup_all(
    ws: &WsCtx,
    root: TreeId,
    paths: &[RepoPath],
) -> Result<Vec<(RepoPath, Entry)>> {
    let mut out = Vec::new();
    for p in paths {
        if let Some(e) = ws.engine.lookup(root, p).await.map_err(fatal)?
            && is_content(&e)
        {
            out.push((p.clone(), e));
        }
    }
    Ok(out)
}

/// The globs of `.asc/prefetch` in `root` (one per line, `#` comments, `!` excludes), or the
/// defaults when the file is missing, empty, not a regular file or above [`MAX_GLOB_FILE`]
/// (SPEC §12.3 reads it the same way for seeds).
async fn auto_globs(ws: &WsCtx, root: TreeId) -> Result<Vec<String>> {
    let defaults = || DEFAULT_GLOBS.iter().map(|g| (*g).to_owned()).collect();
    let file = RepoPath::new(".asc/prefetch").map_err(fatal)?;
    let Some(e) = ws.engine.lookup(root, &file).await.map_err(fatal)? else {
        return Ok(defaults());
    };
    if !matches!(e.kind, Kind::File | Kind::Exec) || e.size > MAX_GLOB_FILE {
        return Ok(defaults());
    }
    let body = ws
        .objects
        .get(&ObjKey::File(FileId::from_bytes(e.id)))
        .await
        .map_err(fatal)?
        .ok_or_else(|| fatal("the content of .asc/prefetch is not available"))?;
    let globs: Vec<String> = String::from_utf8_lossy(&body)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.strip_prefix('!') {
            Some(neg) => format!(":(exclude,glob){neg}"),
            None => format!(":(glob){l}"),
        })
        .collect();
    if globs.iter().all(|g| g.starts_with(":(exclude")) {
        return Ok(defaults().into_iter().chain(globs).collect());
    }
    Ok(globs)
}

/// The profiles `--learned` (and `--auto`) warm: those learned in the window.
async fn learned_names(ctx: &Ctx, ws: &WsCtx) -> Result<Vec<String>> {
    let days = ctx
        .config
        .get("asc.prefetch.learnedDays")
        .map(|v| {
            v.trim()
                .parse::<i64>()
                .ok()
                .filter(|d| *d >= 0)
                .ok_or_else(|| {
                    fatal(format!(
                        "bad numeric config value '{v}' for 'asc.prefetch.learnedDays'"
                    ))
                })
        })
        .transpose()?
        .unwrap_or(LEARNED_DAYS);
    let since = now_secs().saturating_sub(days.saturating_mul(86_400));
    prefetch::recent(&ws.db, ws.repo.key, since)
        .await
        .map_err(fatal)
}

async fn warm(ctx: &Ctx, o: &Opts) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let head = ws.head_commit().await?;
    let mut t = Tally::default();
    let objects: &dyn Objects = &*ws.objects;
    let root = match head {
        Some(h) => Some(ws.engine.get_changeset(h).await?.root),
        None => None,
    };

    if let Some(root) = root {
        if !o.paths.is_empty() {
            let spec = Spec::new(ctx, &o.paths)?;
            let entries = select(ws, root, &spec.spec, None).await?;
            t.warm(objects, entries).await;
        }
        if o.auto {
            let globs = auto_globs(ws, root).await?;
            let spec = Pathspec::parse(&globs).map_err(fatal)?;
            let redirections = Redirections::load(ws.projection.toplevel()).ok();
            let entries = select(ws, root, &spec, redirections.as_ref()).await?;
            t.warm(objects, entries).await;
        }
        let mut names = o.profiles.clone();
        if o.learned || o.auto {
            names.extend(learned_names(ctx, ws).await?);
        }
        let mut done = HashSet::new();
        for name in names.into_iter().filter(|n| done.insert(n.clone())) {
            let paths = prefetch::paths(&ws.db, ws.repo.key, &name)
                .await
                .map_err(fatal)?;
            if paths.is_empty() && o.profiles.contains(&name) {
                t.failures
                    .push(format!("profile {name}: there is no such profile"));
                continue;
            }
            let entries = lookup_all(ws, root, &paths).await?;
            t.warm(objects, entries).await;
        }
    }

    let mut commits = 0u64;
    if o.history > 0
        && let Some(head) = head
    {
        let mut queue: VecDeque<ChangesetId> = VecDeque::from([head]);
        let mut seen: HashSet<ChangesetId> = HashSet::from([head]);
        while commits < o.history {
            let Some(cs) = queue.pop_front() else { break };
            match ws.engine.get_changeset(cs).await {
                Ok(c) => {
                    commits += 1;
                    if let Err(e) = ws.engine.get_tree(c.root).await {
                        t.failures.push(format!("tree of {cs}: {e}"));
                    }
                    for p in &c.parents {
                        if seen.insert(*p) {
                            queue.push_back(*p);
                        }
                    }
                }
                Err(e) => t.failures.push(format!("changeset {cs}: {e}")),
            }
        }
    }

    for f in t.failures.iter().take(MAX_LISTED) {
        eprintln!("error: asc: could not prefetch {f}");
    }
    if t.failures.len() > MAX_LISTED {
        eprintln!("error: asc: … and {} more", t.failures.len() - MAX_LISTED);
    }
    if !o.quiet {
        println!(
            "prefetched {} files ({} bytes) and {commits} commits",
            t.files, t.bytes
        );
    }
    Ok(if t.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

// ---------------------------------------------------------------------------- learning

/// `line` as a repository path: absolute inside one of `roots`, else repository-relative.
/// `None` for a path outside the work tree, into `.git`, or not a usable path.
fn to_repo_path(roots: &[&Path], line: &str) -> Option<RepoPath> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() {
        return None;
    }
    let rel = if Path::new(line).is_absolute() {
        let p = Path::new(line);
        roots
            .iter()
            .find_map(|r| p.strip_prefix(r).ok())
            .or_else(|| {
                // Windows paths compare without case; a build tool may spell the drive lower.
                let low = line.replace('\\', "/").to_lowercase();
                roots.iter().find_map(|r| {
                    let root = r.to_string_lossy().replace('\\', "/").to_lowercase();
                    let root = root.trim_end_matches('/');
                    (cfg!(windows) && low.starts_with(root) && low[root.len()..].starts_with('/'))
                        .then(|| Path::new(&line[root.len() + 1..]))
                })
            })?
            .to_string_lossy()
            .into_owned()
    } else {
        line.to_owned()
    };
    let mut parts: Vec<&str> = Vec::new();
    let rel = rel.replace('\\', "/");
    for comp in rel.split('/') {
        match comp {
            "" | "." => {}
            ".." => return None,
            c => parts.push(c),
        }
    }
    if parts.is_empty() || parts[0].eq_ignore_ascii_case(".git") {
        return None;
    }
    RepoPath::new(parts.join("/").into_bytes()).ok()
}

/// The tracked files among `lines`, deduplicated, at most [`prefetch::MAX_PATHS`]; answers them
/// and how many lines were dropped.
async fn tracked(ws: &WsCtx, lines: &[String]) -> Result<(Vec<RepoPath>, usize)> {
    let top = ws.projection.toplevel().to_owned();
    let shown = ws.root_shown();
    let canon = top.canonicalize().unwrap_or_else(|_| top.clone());
    let roots = [top.as_path(), shown.as_path(), canon.as_path()];
    let mut wanted = BTreeSet::new();
    for l in lines {
        if let Some(p) = to_repo_path(&roots, l) {
            wanted.insert(p);
        }
    }
    let Some(head) = ws.head_commit().await? else {
        return Ok((Vec::new(), lines.len()));
    };
    let root = ws.engine.get_changeset(head).await?.root;
    let wanted: Vec<RepoPath> = wanted.into_iter().collect();
    let mut kept: Vec<RepoPath> = lookup_all(ws, root, &wanted)
        .await?
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    kept.truncate(prefetch::MAX_PATHS);
    let dropped =
        lines.iter().filter(|l| !l.trim().is_empty()).count() - kept.len().min(lines.len());
    Ok((kept, dropped))
}

/// Splits standard input on NUL when it has one, else on newlines.
fn split_input(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body);
    let sep = if body.contains(&0) { '\0' } else { '\n' };
    text.split(sep)
        .map(|l| l.trim_end_matches('\r').to_owned())
        .filter(|l| !l.is_empty())
        .collect()
}

async fn save(
    ws: &WsCtx,
    name: &str,
    paths: &[RepoPath],
    replace: bool,
    quiet: bool,
) -> Result<()> {
    let held = prefetch::learn(&ws.db, ws.repo.key, name, paths, replace, now_secs())
        .await
        .map_err(fatal)?;
    if !quiet {
        println!("learned {} paths into {name} ({held} held)", paths.len());
    }
    Ok(())
}

async fn learn(ctx: &Ctx, o: &Opts, name: &str) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let mut body = Vec::new();
    std::io::stdin()
        .take(MAX_STDIN + 1)
        .read_to_end(&mut body)
        .map_err(fatal)?;
    if body.len() as u64 > MAX_STDIN {
        return Err(fatal(format!(
            "standard input is longer than {MAX_STDIN} bytes"
        )));
    }
    let lines = split_input(&body);
    let (paths, _) = tracked(ws, &lines).await?;
    save(ws, name, &paths, o.replace, o.quiet).await?;
    Ok(ExitCode::SUCCESS)
}

/// The daemon serving this workspace's mount, when it runs.
async fn daemon(ws: &WsCtx) -> Option<Config> {
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
        return None;
    }
    let cfg = Config::for_home(&ws.home).ok()?;
    matches!(lifecycle::probe(&cfg).await, Probe::Up(_)).then_some(cfg)
}

async fn record(ctx: &Ctx, o: &Opts, name: &str, cmd: &[OsString]) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let need = "--record needs a virtual workspace with the daemon running; \
                send the paths to `asc prefetch --learn` instead";
    if ws.workspace.ws.mode != Mode::Virtual {
        return Err(fatal(need));
    }
    let Some(cfg) = daemon(ws).await else {
        return Err(fatal(need));
    };
    // Millisecond stamps: a read in the same millisecond as the start counts.
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .max(1) as u64;
    let status = Command::new(super::cache::run::program(&cmd[0]))
        .args(&cmd[1..])
        .current_dir(&ctx.cwd)
        .status()
        .map_err(|e| fatal(format!("cannot run {}: {e}", cmd[0].to_string_lossy())))?;
    if !status.success() {
        eprintln!("asc: the command failed; nothing was learned");
        return Ok(ExitCode::from(
            status
                .code()
                .and_then(|c| u8::try_from(c).ok())
                .unwrap_or(1),
        ));
    }
    let mut client = WorkspaceClient::new(connect(&cfg.endpoint));
    let got = client
        .stats(wire::StatsRequest {
            ws_id: ws.workspace.ws.id.to_string(),
            read_since_ms: start,
        })
        .await
        .map_err(|s| fatal(s.message()))?
        .into_inner();
    let lines: Vec<String> = got
        .read_since
        .iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();
    let (paths, _) = tracked(ws, &lines).await?;
    save(ws, name, &paths, o.replace, o.quiet).await?;
    Ok(ExitCode::SUCCESS)
}

async fn list(ctx: &Ctx, o: &Opts) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let profiles = prefetch::list(&ws.db, ws.repo.key).await.map_err(fatal)?;
    if o.json {
        let rows: Vec<_> = profiles
            .iter()
            .map(|p| json!({"name": p.name, "paths": p.paths, "lastUsed": p.last_used}))
            .collect();
        println!("{}", serde_json::Value::Array(rows));
    } else {
        for p in &profiles {
            println!("{}\t{}\t{}", p.name, p.paths, p.last_used);
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn show(ctx: &Ctx, name: &str) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let paths = prefetch::paths(&ws.db, ws.repo.key, name)
        .await
        .map_err(fatal)?;
    if paths.is_empty() {
        return Err(fatal(format!("there is no profile {name}")));
    }
    for p in paths {
        println!("{p}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn forget(ctx: &Ctx, o: &Opts, name: &str) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if !prefetch::forget(&ws.db, ws.repo.key, name)
        .await
        .map_err(fatal)?
    {
        return Err(fatal(format!("there is no profile {name}")));
    }
    if !o.quiet {
        println!("forgot {name}");
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let o = parse(&args.rest)?;
    if let Some(dir) = &o.within {
        let dir = ctx.cwd.join(dir);
        let inner = Ctx::open(Program::Asc, GitEnv::from_process(), dir).await?;
        return warm(&inner, &o).await;
    }
    match &o.action {
        Action::Warm => warm(ctx, &o).await,
        Action::Learn { name } => learn(ctx, &o, name).await,
        Action::Record { name, cmd } => record(ctx, &o, name, cmd).await,
        Action::List => list(ctx, &o).await,
        Action::Show { name } => show(ctx, name).await,
        Action::Forget { name } => forget(ctx, &o, name).await,
    }
}

// ------------------------------------------------------------------- background warms

/// Starts `asc prefetch <args> --in <top>` detached, with no input or output, from asc's home
/// directory rather than the work tree, so the work tree can still be deleted while it runs.
/// Best effort; nothing starts without a daemon (`ASC_NO_DAEMON`, SPEC §9.1) or under
/// `ASC_NO_BACKGROUND`.
fn spawn_detached(ws: &WsCtx, args: &[&str]) {
    let get = |k: &str| std::env::var_os(k);
    if lifecycle::disabled(&get) || background_off(&get) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.args([crate::dispatch::ASC_ENTRY, "prefetch"])
        .args(args)
        .arg("--in")
        .arg(ws.projection.toplevel())
        .current_dir(ws.home.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let _ = cmd.spawn();
}

/// `ASC_NO_BACKGROUND` set to anything but empty, `0` or `false`.
fn background_off(get: &dyn Fn(&str) -> Option<OsString>) -> bool {
    get("ASC_NO_BACKGROUND").is_some_and(|v| {
        let v = v.to_string_lossy();
        !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
    })
}

/// After a branch switch: warms the learned profiles in the background, when there are any and
/// `asc.prefetch.learned` is not false.
pub(crate) async fn after_switch(ctx: &Ctx, ws: &WsCtx) {
    if !ctx.config.bool("asc.prefetch.learned").unwrap_or(true) {
        return;
    }
    if learned_names(ctx, ws).await.is_ok_and(|n| !n.is_empty()) {
        spawn_detached(ws, &["--learned", "--quiet"]);
    }
}

/// After `asc clone` or `asc worktree add` of a plain workspace (a virtual one is warmed by the
/// daemon when it mounts): adopts the profiles of other clones of the same remote and warms the
/// `.asc/prefetch` globs and the profiles in the background, unless `asc.prefetch.auto` is false.
pub(crate) async fn after_clone(ctx: &Ctx, ws: &WsCtx) {
    if let Some(url) = ws.repo.repo.remote_url.as_deref() {
        let _ = prefetch::adopt(&ws.db, ws.repo.key, url).await;
    }
    if !ctx.config.bool("asc.prefetch.auto").unwrap_or(true)
        || ws.workspace.ws.mode == Mode::Virtual
    {
        return;
    }
    spawn_detached(ws, &["--auto", "--quiet"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn arguments() {
        assert!(parse(&os(&[])).is_err());
        let o = parse(&os(&["src", "--history", "5", "docs"])).unwrap();
        assert_eq!(o.paths, os(&["src", "docs"]));
        assert_eq!(o.history, 5);
        assert_eq!(parse(&os(&["--history=3"])).unwrap().history, 3);
        assert!(parse(&os(&["--history", "x", "a"])).is_err());
        assert!(parse(&os(&["--history", "9999999999", "a"])).is_err());
        assert!(parse(&os(&["--bogus", "a"])).is_err());
        assert_eq!(parse(&os(&["--", "-odd"])).unwrap().paths, os(&["-odd"]));
    }

    #[test]
    fn a_background_warm_names_its_work_tree() {
        let o = parse(&os(&["--auto", "--quiet", "--in", "/w/repo"])).unwrap();
        assert_eq!(o.within, Some(std::path::PathBuf::from("/w/repo")));
        assert!(o.auto && o.quiet);
        assert!(parse(&os(&["--auto", "--in"])).is_err());
        assert!(parse(&os(&["--list", "--in", "/w/repo"])).is_err());
    }

    #[test]
    fn profile_arguments() {
        let o = parse(&os(&["--profile", "vite:app", "--profile=b", "-q"])).unwrap();
        assert_eq!(o.profiles, vec!["vite:app".to_owned(), "b".to_owned()]);
        assert!(o.quiet);
        assert!(parse(&os(&["--learned"])).unwrap().learned);
        assert!(parse(&os(&["--auto"])).unwrap().auto);
        assert!(parse(&os(&["--profile", "a b"])).is_err());
        assert_eq!(
            parse(&os(&["--learn", "x", "--replace"])).unwrap().action,
            Action::Learn { name: "x".into() }
        );
        assert_eq!(
            parse(&os(&["--record", "x", "--", "npm", "run", "--x"]))
                .unwrap()
                .action,
            Action::Record {
                name: "x".into(),
                cmd: os(&["npm", "run", "--x"])
            }
        );
        assert!(parse(&os(&["--record", "x"])).is_err());
        assert_eq!(
            parse(&os(&["--list", "--json"])).unwrap().action,
            Action::List
        );
        assert!(parse(&os(&["--list", "--show", "x"])).is_err());
        assert!(parse(&os(&["--learn", "x", "src"])).is_err());
        assert!(parse(&os(&["--learn", "x", "--json"])).is_err());
        assert!(parse(&os(&["src", "--replace"])).is_err());
        assert!(parse(&os(&["--forget", "x", "--replace"])).is_err());
    }

    #[test]
    fn learned_lines_become_repository_paths() {
        let root = if cfg!(windows) {
            Path::new(r"C:\work\repo")
        } else {
            Path::new("/work/repo")
        };
        let roots = [root];
        let abs = root.join("src").join("a.ts");
        assert_eq!(
            to_repo_path(&roots, &abs.to_string_lossy()),
            Some(RepoPath::new("src/a.ts").unwrap())
        );
        assert_eq!(
            to_repo_path(&roots, "./src/b.ts\r"),
            Some(RepoPath::new("src/b.ts").unwrap())
        );
        assert_eq!(
            to_repo_path(&roots, "src\\c.ts"),
            Some(RepoPath::new("src/c.ts").unwrap())
        );
        assert_eq!(to_repo_path(&roots, "../x"), None);
        assert_eq!(to_repo_path(&roots, ".git/config"), None);
        assert_eq!(to_repo_path(&roots, ""), None);
        let outside = if cfg!(windows) {
            r"C:\other\a"
        } else {
            "/other/a"
        };
        assert_eq!(to_repo_path(&roots, outside), None);
        if cfg!(windows) {
            assert_eq!(
                to_repo_path(&roots, r"c:\WORK\repo\src\d.ts"),
                Some(RepoPath::new("src/d.ts").unwrap())
            );
        }
    }

    #[test]
    fn background_switch() {
        let env = |v: &'static str| move |k: &str| (k == "ASC_NO_BACKGROUND").then(|| v.into());
        assert!(background_off(&env("1")));
        assert!(!background_off(&env("0")));
        assert!(!background_off(&env("false")));
        assert!(!background_off(&|_: &str| None));
    }

    #[test]
    fn input_splits_on_nul_or_newlines() {
        assert_eq!(split_input(b"a\nb\r\n\nc"), vec!["a", "b", "c"]);
        assert_eq!(split_input(b"a b\0c\nd\0"), vec!["a b", "c\nd"]);
    }
}
