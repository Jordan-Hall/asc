//! `--json` forms of `restore`, `switch`, `cherry-pick`, `revert`, `clone`, `worktree
//! add|remove|list`, `stash`, `fetch`, `pull` and `push` (SPEC §11.2, §24.8.2 A8, PLAN P45).
//!
//! Each verb keeps its one implementation. With `--json`, [`run`] runs that same command as a
//! child of this binary (stdout captured, stderr passed through), compares the refs, `HEAD` and
//! the verb's own state before and after, and prints one typed `OpResult` (`cli.proto`) in the
//! asc JSON profile as the only line on stdout. The exit code is the verb's. A verb that fails
//! prints no result: the host reads the exit code and stderr. A `cherry-pick` or `revert` that
//! stops on a conflict prints a `paused` result and exits 1; a rejected `push` prints its
//! refs with status `rejected`.
//!
//! No secret reaches the output: a URL loses its user information before it is printed, and
//! the child's argv is never printed.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;

use super::common::{CliError, Ctx, Result};

/// The most items one list in a result carries; the result says so in `hints` when it cuts.
const MAX_ITEMS: usize = 1000;
/// The most bytes of the child's stdout and stderr read.
const MAX_CAPTURE: u64 = 8 * 1024 * 1024;

/// Whether `argv` (`[verb, args…]`) is one of the `--json` forms of this unit.
pub fn wants(argv: &[OsString]) -> bool {
    let Some(verb) = argv.first().and_then(|a| a.to_str()) else {
        return false;
    };
    let json_before_end = argv[1..]
        .iter()
        .take_while(|a| *a != "--" && *a != "--end-of-options")
        .any(|a| a == "--json");
    if !json_before_end {
        return false;
    }
    match verb {
        "restore" | "switch" | "cherry-pick" | "revert" | "clone" | "stash" | "fetch" | "pull"
        | "push" => true,
        "worktree" => matches!(
            argv.get(1).and_then(|a| a.to_str()),
            Some("add" | "remove" | "list")
        ),
        _ => false,
    }
}

fn fatal(msg: impl Into<String>) -> CliError {
    CliError::Fatal(msg.into())
}

// ---------------------------------------------------------------------------- argument scan

/// What a verb's arguments hold: flags with their values and the operands.
#[derive(Default, Debug)]
struct Scan {
    flags: Vec<(String, Option<String>)>,
    words: Vec<String>,
}

impl Scan {
    fn has(&self, names: &[&str]) -> bool {
        self.flags.iter().any(|(f, _)| names.contains(&f.as_str()))
    }

    fn value(&self, names: &[&str]) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| names.contains(&f.as_str()))
            .and_then(|(_, v)| v.as_deref())
    }
}

/// Splits `args`; `value_short` and `value_long` name the flags that take a value.
fn scan(args: &[OsString], value_short: &[char], value_long: &[&str]) -> Scan {
    let mut out = Scan::default();
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(arg) = it.next() {
        if arg == "--" {
            out.words.extend(it.by_ref());
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some((n, v)) => out.flags.push((format!("--{n}"), Some(v.to_owned()))),
                None if value_long.contains(&long) => {
                    out.flags.push((arg.clone(), it.next()));
                }
                None => out.flags.push((arg.clone(), None)),
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            let body: Vec<char> = arg[1..].chars().collect();
            for (i, c) in body.iter().enumerate() {
                if value_short.contains(c) {
                    let attached: String = body[i + 1..].iter().collect();
                    let v = if attached.is_empty() {
                        it.next()
                    } else {
                        Some(attached)
                    };
                    out.flags.push((format!("-{c}"), v));
                    break;
                }
                out.flags.push((format!("-{c}"), None));
            }
        } else {
            out.words.push(arg);
        }
    }
    out
}

/// The argv a verb's child runs: `--json` dropped, `--end-of-options` as `--`. `switch` reads
/// `--` as a pathspec separator, so there the marker is dropped and an operand after it that
/// looks like an option is refused.
fn child_argv(argv: &[OsString]) -> Result<Vec<OsString>> {
    let bare = argv.first().is_some_and(|v| v == "switch");
    let mut out = Vec::with_capacity(argv.len());
    let mut ended = false;
    let mut dropped = false;
    for a in argv {
        if !ended && !dropped && a == "--json" {
            dropped = true;
            continue;
        }
        if !ended && a == "--end-of-options" {
            ended = true;
            if !bare {
                out.push(OsString::from("--"));
            }
            continue;
        }
        if a == "--" {
            ended = true;
        }
        if ended && bare && a.to_string_lossy().starts_with('-') && a != "--" {
            return Err(fatal(
                "an operand after --end-of-options cannot start with '-'",
            ));
        }
        out.push(a.clone());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------- child runs

struct Run {
    code: u8,
    err: Vec<u8>,
    out: Vec<u8>,
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = (&mut r).take(MAX_CAPTURE).read_to_end(&mut buf).await;
    let mut sink = [0u8; 8192];
    while matches!(r.read(&mut sink).await, Ok(n) if n > 0) {}
    buf
}

/// Runs this binary with `args` in `dir`: null stdin, stdout and stderr captured.
async fn spawn(dir: &Path, args: &[OsString]) -> Result<Run> {
    let exe = std::env::current_exe().map_err(|e| fatal(format!("cannot find asc itself: {e}")))?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| fatal(format!("cannot start the verb: {e}")))?;
    let out = child.stdout.take().map(read_capped);
    let err = child.stderr.take().map(read_capped);
    let (out, err) = tokio::join!(
        async {
            match out {
                Some(f) => f.await,
                None => Vec::new(),
            }
        },
        async {
            match err {
                Some(f) => f.await,
                None => Vec::new(),
            }
        }
    );
    let status = child
        .wait()
        .await
        .map_err(|e| fatal(format!("the verb did not finish: {e}")))?;
    let code = status.code().map_or(128, |c| c.clamp(0, 255) as u8);
    Ok(Run { code, err, out })
}

/// A read-only helper command; `None` when it fails.
async fn probe(dir: &Path, args: &[&str]) -> Option<String> {
    let argv: Vec<OsString> = args.iter().map(OsString::from).collect();
    let r = spawn(dir, &argv).await.ok()?;
    (r.code == 0).then(|| String::from_utf8_lossy(&r.out).trim().to_owned())
}

// ---------------------------------------------------------------------------- state

#[derive(Default, Clone)]
struct Snap {
    refs: BTreeMap<String, String>,
    head: Option<String>,
    branch: Option<String>,
}

fn is_hex_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn snap(dir: &Path) -> Snap {
    let mut s = Snap::default();
    if let Some(text) = probe(dir, &["show-ref", "--head"]).await {
        for line in text.lines() {
            let Some((oid, name)) = line.split_once(' ') else {
                continue;
            };
            if !is_hex_oid(oid) {
                continue;
            }
            if name == "HEAD" {
                s.head = Some(oid.to_ascii_lowercase());
            } else {
                s.refs.insert(name.to_owned(), oid.to_ascii_lowercase());
            }
        }
    }
    s.branch = probe(dir, &["symbolic-ref", "-q", "--short", "HEAD"])
        .await
        .filter(|b| !b.is_empty());
    s
}

/// The newest op-log id, when the repository keeps an op log.
async fn last_op(dir: &Path) -> Option<String> {
    let text = probe(dir, &["op", "log", "--json", "-n", "1"]).await?;
    let v: Value = serde_json::from_str(text.lines().last()?).ok()?;
    v.get("ops")?.get(0)?.get("op")?.as_str().map(str::to_owned)
}

/// The ref moves between two snapshots, sorted by name, and whether the list was cut.
fn ref_moves(before: &Snap, after: &Snap) -> (Vec<Value>, bool) {
    let names: BTreeSet<&String> = before.refs.keys().chain(after.refs.keys()).collect();
    let mut out = Vec::new();
    let mut cut = false;
    for n in names {
        let (o, w) = (before.refs.get(n), after.refs.get(n));
        if o == w {
            continue;
        }
        if out.len() == MAX_ITEMS {
            cut = true;
            break;
        }
        let mut m = json!({ "name": n });
        if let Some(o) = o {
            m["old"] = json!(o);
        }
        if let Some(w) = w {
            m["new"] = json!(w);
        }
        out.push(m);
    }
    (out, cut)
}

/// A URL without its user information; text that is not a URL is returned unchanged.
fn strip_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{scheme}://{host}{tail}")
}

fn slashes(p: &Path) -> String {
    let t = p.to_string_lossy();
    let t = t.strip_prefix(r"\\?\").unwrap_or(&t);
    t.replace('\\', "/")
}

fn norm(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = s
        .strip_prefix("//?/")
        .unwrap_or(&s)
        .trim_end_matches('/')
        .to_owned();
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

fn absolute(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

/// The `OpResult` every verb fills; `detail` is the oneof member and its message.
struct Res {
    status: &'static str,
    refs: Vec<Value>,
    hints: Vec<String>,
    detail: (&'static str, Value),
}

impl Res {
    fn new(status: &'static str, key: &'static str, detail: Value) -> Self {
        Self {
            status,
            refs: Vec::new(),
            hints: Vec::new(),
            detail: (key, detail),
        }
    }

    fn value(self, op: Option<String>) -> Value {
        let mut v = json!({ "version": 1, "status": self.status });
        if let Some(op) = op {
            v["op"] = json!(op);
        }
        if !self.refs.is_empty() {
            v["refs"] = Value::Array(self.refs);
        }
        if !self.hints.is_empty() {
            v["hints"] = json!(self.hints);
        }
        v[self.detail.0] = self.detail.1;
        v
    }
}

// ---------------------------------------------------------------------------- the entry point

/// The stack of the thread that runs a `--json` verb. The command frames above it are deep in a
/// debug build; the work needs room of its own.
const STACK: usize = 64 * 1024 * 1024;

/// Runs `argv` (`[verb, args…]`, `--json` present) and prints its `OpResult`.
///
/// The work runs on a thread of its own with a large stack and a runtime of its own, so the
/// depth of the caller's frames does not limit it.
///
/// # Errors
/// A verb that cannot be started or whose state cannot be read.
pub async fn run(ctx: &Ctx, argv: Vec<OsString>) -> Result<ExitCode> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn_scoped(scope, || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| fatal(format!("cannot start a runtime: {e}")))?;
                rt.block_on(run_here(ctx, argv))
            })
            .map_err(|e| fatal(format!("cannot start a thread: {e}")))?
            .join()
            .unwrap_or_else(|_| Err(fatal("the --json run panicked")))
    })
}

async fn run_here(ctx: &Ctx, argv: Vec<OsString>) -> Result<ExitCode> {
    let verb = argv[0].to_string_lossy().into_owned();
    let args = child_argv(&argv)?;
    let rest = &args[1..];
    let dir = ctx.cwd.clone();
    let sub = rest
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cloning = verb == "clone";

    // Clone has no repository to look at before it runs.
    let before = if cloning {
        Snap::default()
    } else {
        snap(&dir).await
    };
    let op_before = if cloning { None } else { last_op(&dir).await };
    let picked = match verb.as_str() {
        "cherry-pick" | "revert" => resolve_commits(&dir, &verb, rest).await,
        _ => Vec::new(),
    };

    let ran = spawn(&dir, &args).await?;
    // The verb's diagnostics belong to the user; its human stdout is not the result.
    let _ = std::io::stderr().write_all(&ran.err);

    let target = if cloning {
        clone_target(&dir, rest)
    } else {
        dir.clone()
    };
    let after = if cloning && ran.code != 0 {
        Snap::default()
    } else {
        snap(&target).await
    };
    let op_after = if cloning { None } else { last_op(&dir).await };
    let op = if op_after != op_before {
        op_after
    } else {
        None
    };

    let built: Option<(Res, u8)> = match verb.as_str() {
        "restore" => ok_only(&ran, restore(rest)),
        "switch" => ok_only(&ran, switch(rest, &after)),
        "cherry-pick" | "revert" => {
            Box::pin(pick(&dir, &verb, rest, &ran, &before, &after, picked)).await
        }
        "clone" => ok_only(&ran, clone(&target, rest, &after)),
        "worktree" => Box::pin(worktree(ctx, &sub, rest, &ran)).await?,
        "stash" => ok_only(&ran, Box::pin(stash(&dir, &sub, &before, &after)).await),
        "fetch" => ok_only(&ran, Box::pin(fetch(&dir, rest, &before, &after)).await),
        "pull" => ok_only(&ran, Box::pin(pull(&dir, rest, &before, &after)).await),
        "push" => Box::pin(push(&dir, rest, &ran, &before, &after)).await,
        _ => None,
    };
    let Some((mut res, code)) = built else {
        return Ok(ExitCode::from(ran.code.max(1)));
    };
    if res.refs.is_empty() && verb != "worktree" {
        let (moves, cut) = ref_moves(&before, &after);
        res.refs = moves;
        if cut {
            res.hints
                .push(format!("more than {MAX_ITEMS} refs moved; the list is cut"));
        }
    }
    let mut line = serde_json::to_string(&res.value(op)).map_err(|e| fatal(e.to_string()))?;
    line.push('\n');
    let mut out = std::io::stdout().lock();
    out.write_all(line.as_bytes())?;
    out.flush()?;
    Ok(ExitCode::from(code))
}

/// A result only when the verb succeeded.
fn ok_only(ran: &Run, r: Res) -> Option<(Res, u8)> {
    (ran.code == 0).then_some((r, 0))
}

fn status_of(changed: bool) -> &'static str {
    if changed { "ok" } else { "nothing" }
}

// ---------------------------------------------------------------------------- restore, switch

fn restore(rest: &[OsString]) -> Res {
    let s = scan(rest, &['s'], &["source", "pathspec-from-file", "conflict"]);
    let staged = s.has(&["-S", "--staged"]);
    let worktree = s.has(&["-W", "--worktree"]) || !staged;
    Res::new(
        "ok",
        "restore",
        json!({ "paths": s.words, "staged": staged, "worktree": worktree }),
    )
}

const CREATE_FLAGS: &[&str] = &["-c", "-C", "--create", "--force-create", "--orphan"];

fn switch(rest: &[OsString], after: &Snap) -> Res {
    let s = scan(
        rest,
        &['c', 'C'],
        &["create", "force-create", "orphan", "conflict"],
    );
    let created = s.has(CREATE_FLAGS);
    let target = s
        .value(CREATE_FLAGS)
        .map(str::to_owned)
        .or_else(|| s.words.first().cloned())
        .unwrap_or_default();
    let mut d = json!({ "target": target, "created": created, "detached": after.branch.is_none() });
    if let Some(h) = &after.head {
        d["head"] = json!(h);
    }
    Res::new("ok", "switch", d)
}

// ---------------------------------------------------------------------------- cherry-pick, revert

const PICK_SHORT: &[char] = &['m', 'X', 's', 'S'];
const PICK_LONG: &[&str] = &["mainline", "strategy", "strategy-option"];

/// The commits the operands name, in the order the verb applies them.
async fn resolve_commits(dir: &Path, verb: &str, rest: &[OsString]) -> Vec<String> {
    let s = scan(rest, PICK_SHORT, PICK_LONG);
    if s.has(&["--continue", "--skip", "--abort", "--quit"]) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for w in &s.words {
        if out.len() >= MAX_ITEMS {
            break;
        }
        if w.contains("..") {
            let max = format!("--max-count={MAX_ITEMS}");
            let mut a = vec!["rev-list", &max];
            if verb == "cherry-pick" {
                a.push("--reverse");
            }
            a.push(w);
            if let Some(t) = probe(dir, &a).await {
                out.extend(t.lines().map(str::to_owned));
            }
        } else {
            let spec = format!("{w}^{{commit}}");
            if let Some(t) = probe(dir, &["rev-parse", "--verify", "-q", &spec]).await {
                out.push(t);
            }
        }
    }
    out.truncate(MAX_ITEMS);
    out
}

async fn pick(
    dir: &Path,
    verb: &str,
    rest: &[OsString],
    ran: &Run,
    before: &Snap,
    after: &Snap,
    picked: Vec<String>,
) -> Option<(Res, u8)> {
    let s = scan(rest, PICK_SHORT, PICK_LONG);
    let moved = before.head != after.head;
    let mut made: Vec<String> = Vec::new();
    if moved && let Some(new) = &after.head {
        let max = format!("--max-count={MAX_ITEMS}");
        let range = match &before.head {
            Some(old) => format!("{old}..{new}"),
            None => new.clone(),
        };
        if let Some(t) = probe(dir, &["rev-list", "--reverse", &max, &range]).await {
            made = t.lines().map(str::to_owned).collect();
        }
    }
    let (key, field) = if verb == "revert" {
        ("revert", "reverted")
    } else {
        ("cherry_pick", "picked")
    };
    let mut d = json!({ field: picked, "made": made });
    if let Some(h) = &after.head {
        d["head"] = json!(h);
    }
    if ran.code != 0 {
        let status = probe(dir, &["status", "--porcelain"])
            .await
            .unwrap_or_default();
        let paths: BTreeSet<&str> = status
            .lines()
            .filter(|l| {
                matches!(
                    l.get(..2),
                    Some("DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU")
                )
            })
            .filter_map(|l| l.get(3..))
            .map(|p| p.trim_matches('"'))
            .collect();
        if paths.is_empty() {
            return None; // a failure that stopped nothing half-way: the exit code says it
        }
        let mut res = Res::new("paused", key, d);
        res.hints = paths
            .iter()
            .take(100)
            .map(|p| format!("conflict in {p}"))
            .collect();
        res.hints.push(format!(
            "resolve the paths, then run `{verb} --continue` or `{verb} --abort`"
        ));
        return Some((res, 1));
    }
    let nothing = !moved && !s.has(&["-n", "--no-commit", "--abort", "--quit"]);
    Some((Res::new(status_of(!nothing), key, d), 0))
}

// ---------------------------------------------------------------------------- clone

const CLONE_SHORT: &[char] = &['b', 'o', 'j'];
const CLONE_LONG: &[&str] = &[
    "branch",
    "origin",
    "depth",
    "filter",
    "mode",
    "refs",
    "reference",
    "seed",
    "jobs",
];

fn clone_target(cwd: &Path, rest: &[OsString]) -> PathBuf {
    let s = scan(rest, CLONE_SHORT, CLONE_LONG);
    let dest = match s.words.get(1) {
        Some(d) => d.clone(),
        None => {
            let name =
                crate::remote::url::default_dir_name(s.words.first().map_or("", String::as_str));
            if s.has(&["--bare", "--mirror"]) {
                format!("{name}.git")
            } else {
                name
            }
        }
    };
    absolute(cwd, &dest)
}

fn clone(target: &Path, rest: &[OsString], after: &Snap) -> Res {
    let s = scan(rest, CLONE_SHORT, CLONE_LONG);
    let mut d = json!({
        "url": strip_userinfo(s.words.first().map_or("", String::as_str)),
        "path": slashes(target),
        "mode": s.value(&["--mode"]).unwrap_or("plain"),
        "branch": after.branch.clone().unwrap_or_default(),
    });
    if let Some(h) = &after.head {
        d["head"] = json!(h);
    }
    Res::new("ok", "clone", d)
}

// ---------------------------------------------------------------------------- worktree

struct WtRow {
    path: String,
    head: String,
    branch: String,
    locked: bool,
    ephemeral: bool,
    ws: String,
    mode: &'static str,
    expires_at: i64,
}

/// The registered worktrees with their `HEAD` and branch.
async fn wt_rows(ctx: &Ctx) -> Result<Vec<WtRow>> {
    use asc_state::state::workspaces::{self, Mode};
    let ws = ctx.ws()?;
    let mut rows = Vec::new();
    for w in workspaces::list(&ws.db, Some(ws.repo.key)).await? {
        let s = snap(Path::new(&w.ws.path)).await;
        rows.push(WtRow {
            path: slashes(Path::new(&w.ws.path)),
            head: s.head.unwrap_or_default(),
            branch: s.branch.unwrap_or_default(),
            locked: w.locked.is_some(),
            ephemeral: w.ws.ephemeral_until.is_some(),
            ws: w.ws.id.to_string(),
            mode: match w.ws.mode {
                Mode::Virtual => "virtual",
                Mode::Plain => "plain",
            },
            expires_at: w.ws.ephemeral_until.unwrap_or(0),
        });
    }
    Ok(rows)
}

async fn worktree(ctx: &Ctx, sub: &str, rest: &[OsString], ran: &Run) -> Result<Option<(Res, u8)>> {
    if ran.code != 0 {
        return Ok(None);
    }
    let s = scan(
        rest.get(1..).unwrap_or_default(),
        &['b', 'B'],
        &["reason", "ttl", "mode"],
    );
    let cwd = &ctx.cwd;
    let mut hints = Vec::new();
    let (key, detail) = match sub {
        "add" => {
            let Some(p) = s.words.first() else {
                return Ok(None);
            };
            let want = norm(&slashes(&absolute(cwd, p)));
            let rows = wt_rows(ctx).await?;
            let Some(r) = rows.iter().find(|r| norm(&r.path) == want) else {
                return Err(fatal("the new worktree is not registered"));
            };
            (
                "worktree_add",
                json!({
                    "path": r.path, "rev": r.head, "ws": r.ws, "branch": r.branch,
                    "mode": r.mode, "ephemeral": r.ephemeral, "expires_at": r.expires_at,
                }),
            )
        }
        "remove" => {
            let Some(p) = s.words.first() else {
                return Ok(None);
            };
            (
                "worktree_remove",
                json!({ "path": slashes(&absolute(cwd, p)), "forced": s.has(&["-f", "--force"]) }),
            )
        }
        _ => {
            let rows = wt_rows(ctx).await?;
            if rows.len() > MAX_ITEMS {
                hints.push(format!("more than {MAX_ITEMS} worktrees; the list is cut"));
            }
            let list: Vec<Value> = rows
                .iter()
                .take(MAX_ITEMS)
                .map(|r| {
                    json!({
                        "path": r.path, "head": r.head, "branch": r.branch,
                        "locked": r.locked, "ephemeral": r.ephemeral,
                    })
                })
                .collect();
            ("worktree_list", json!({ "worktrees": list }))
        }
    };
    let mut res = Res::new("ok", key, detail);
    res.hints = hints;
    Ok(Some((res, 0)))
}

// ---------------------------------------------------------------------------- stash

async fn stash_entries(dir: &Path) -> Vec<Value> {
    let max = format!("--max-count={MAX_ITEMS}");
    let Some(t) = probe(
        dir,
        &["log", "-g", &max, "--format=%H%x1f%gs", "refs/stash"],
    )
    .await
    else {
        return Vec::new();
    };
    t.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let (oid, msg) = l.split_once('\u{1f}')?;
            Some(json!({ "index": i, "commit": oid.to_ascii_lowercase(), "message": msg }))
        })
        .collect()
}

async fn stash(dir: &Path, sub: &str, before: &Snap, after: &Snap) -> Res {
    let action = match sub {
        "push" | "pop" | "apply" | "drop" | "list" | "clear" | "show" | "branch" | "create"
        | "store" => sub,
        "save" => "push",
        _ => "push",
    };
    let changed = before.refs.get("refs/stash") != after.refs.get("refs/stash");
    let status = match action {
        "push" | "pop" | "drop" | "clear" => status_of(changed),
        _ => "ok",
    };
    Res::new(
        status,
        "stash",
        json!({ "action": action, "entries": stash_entries(dir).await }),
    )
}

// ---------------------------------------------------------------------------- fetch, pull, push

async fn default_remote(dir: &Path, after: &Snap) -> String {
    if let Some(b) = &after.branch
        && let Some(r) = probe(dir, &["config", "--get", &format!("branch.{b}.remote")]).await
        && !r.is_empty()
    {
        return r;
    }
    "origin".to_owned()
}

const NET_SHORT: &[char] = &['j', 'o', 's', 'X'];
const NET_LONG: &[&str] = &[
    "depth",
    "jobs",
    "upstream",
    "repo",
    "push-option",
    "receive-pack",
    "exec",
    "shallow-since",
    "shallow-exclude",
    "deepen",
    "strategy",
    "strategy-option",
    "server-option",
    "negotiation-tip",
    "refmap",
    "filter",
];

async fn fetch_detail(dir: &Path, s: &Scan, before: &Snap, after: &Snap) -> Value {
    let remote = if s.has(&["--all"]) {
        String::new()
    } else {
        match s.words.first() {
            Some(r) => strip_userinfo(r),
            None => default_remote(dir, after).await,
        }
    };
    let names: BTreeSet<&String> = before.refs.keys().chain(after.refs.keys()).collect();
    let mut updated = Vec::new();
    let mut pruned = Vec::new();
    for n in names {
        if !(n.starts_with("refs/remotes/") || n.starts_with("refs/tags/")) {
            continue;
        }
        let (o, w) = (before.refs.get(n), after.refs.get(n));
        if o == w {
            continue;
        }
        if w.is_none() {
            if pruned.len() < MAX_ITEMS {
                pruned.push(json!(n));
            }
        } else if updated.len() < MAX_ITEMS {
            let mut r = json!({ "name": n });
            if let Some(o) = o {
                r["old"] = json!(o);
            }
            if let Some(w) = w {
                r["new"] = json!(w);
            }
            updated.push(r);
        }
    }
    json!({ "remote": remote, "updated": updated, "pruned": pruned })
}

async fn fetch(dir: &Path, rest: &[OsString], before: &Snap, after: &Snap) -> Res {
    let s = scan(rest, NET_SHORT, NET_LONG);
    let d = fetch_detail(dir, &s, before, after).await;
    let changed = d["updated"].as_array().is_some_and(|a| !a.is_empty())
        || d["pruned"].as_array().is_some_and(|a| !a.is_empty());
    Res::new(status_of(changed), "fetch", d)
}

async fn pull(dir: &Path, rest: &[OsString], before: &Snap, after: &Snap) -> Res {
    let s = scan(rest, NET_SHORT, NET_LONG);
    let fetch = fetch_detail(dir, &s, before, after).await;
    let explicit = s
        .flags
        .iter()
        .rev()
        .find(|(f, _)| f == "-r" || f == "--rebase" || f == "--no-rebase")
        .map(|(f, v)| f != "--no-rebase" && !matches!(v.as_deref(), Some("false" | "no")));
    let rebase = match explicit {
        Some(r) => r,
        None => matches!(
            probe(dir, &["config", "--get", "pull.rebase"])
                .await
                .as_deref(),
            Some("true" | "merges" | "interactive")
        ),
    };
    let moved = before.head != after.head;
    let integrate = if rebase {
        "rebase"
    } else if moved {
        let merge = match &after.head {
            Some(h) => probe(dir, &["rev-list", "--parents", "-n", "1", h])
                .await
                .is_some_and(|t| t.split_whitespace().count() > 2),
            None => false,
        };
        if merge { "merge" } else { "fast-forward" }
    } else {
        "fast-forward"
    };
    let mut d = json!({ "fetch": fetch, "integrate": integrate });
    if let Some(h) = &after.head {
        d["head"] = json!(h);
    }
    Res::new(status_of(moved || before.refs != after.refs), "pull", d)
}

async fn push(
    dir: &Path,
    rest: &[OsString],
    ran: &Run,
    before: &Snap,
    after: &Snap,
) -> Option<(Res, u8)> {
    let s = scan(rest, NET_SHORT, NET_LONG);
    let remote = match s.words.first() {
        Some(r) => r.clone(),
        None => default_remote(dir, after).await,
    };
    let track = format!("refs/remotes/{remote}/");
    // The remote-side names the operands name.
    let mut asked: Vec<String> = Vec::new();
    for spec in s.words.iter().skip(1) {
        let spec = spec.strip_prefix('+').unwrap_or(spec);
        let dst = spec.rsplit_once(':').map_or(spec, |(_, d)| d);
        let dst = if dst == "HEAD" {
            after.branch.clone().unwrap_or_default()
        } else {
            dst.to_owned()
        };
        if !dst.is_empty() {
            asked.push(if dst.starts_with("refs/") {
                dst
            } else {
                format!("refs/heads/{dst}")
            });
        }
    }
    if asked.is_empty()
        && !s.has(&["--all", "--mirror", "--tags"])
        && let Some(b) = &after.branch
    {
        asked.push(format!("refs/heads/{b}"));
    }
    let mut refs: BTreeMap<String, Value> = BTreeMap::new();
    let names: BTreeSet<&String> = before.refs.keys().chain(after.refs.keys()).collect();
    for n in names {
        let Some(short) = n.strip_prefix(&track) else {
            continue;
        };
        let (o, w) = (before.refs.get(n), after.refs.get(n));
        if o == w || refs.len() >= MAX_ITEMS {
            continue;
        }
        let name = format!("refs/heads/{short}");
        let mut r = json!({ "name": name, "status": "ok" });
        if let Some(o) = o {
            r["old"] = json!(o);
        }
        if let Some(w) = w {
            r["new"] = json!(w);
        }
        refs.insert(name, r);
    }
    let failed = ran.code != 0;
    for name in asked {
        let row = if failed {
            json!({
                "name": name, "status": "rejected",
                "reason": "the push failed; see the diagnostics on stderr",
            })
        } else {
            let mut r = json!({ "name": name, "status": "up-to-date" });
            if let Some(c) = name
                .strip_prefix("refs/heads/")
                .and_then(|b| after.refs.get(&format!("{track}{b}")))
            {
                r["old"] = json!(c);
                r["new"] = json!(c);
            }
            r
        };
        refs.entry(name).or_insert(row);
    }
    if failed && refs.is_empty() {
        return None; // a failure before any ref was offered: the exit code says it
    }
    let any_moved = refs.values().any(|r| r["status"] == "ok");
    let mut res = Res::new(
        status_of(any_moved),
        "push",
        json!({ "remote": strip_userinfo(&remote), "refs": refs.into_values().collect::<Vec<_>>() }),
    );
    if failed {
        res.hints
            .push("the push was rejected; nothing was changed on the remote".to_owned());
    }
    Some((res, ran.code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn wants_only_the_verbs_of_this_unit() {
        assert!(wants(&os(&["restore", "--json", "a"])));
        assert!(wants(&os(&["worktree", "add", "--json", "p"])));
        assert!(wants(&os(&["worktree", "list", "--json"])));
        assert!(!wants(&os(&["worktree", "lock", "--json", "p"])));
        assert!(!wants(&os(&["status", "--json"])));
        assert!(!wants(&os(&["restore", "--", "--json"])));
        assert!(!wants(&os(&["restore", "--end-of-options", "--json"])));
    }

    #[test]
    fn child_argv_drops_json_and_maps_end_of_options() {
        assert_eq!(
            child_argv(&os(&[
                "restore",
                "--json",
                "-S",
                "--end-of-options",
                "--json"
            ]))
            .unwrap(),
            os(&["restore", "-S", "--", "--json"])
        );
        assert_eq!(
            child_argv(&os(&["switch", "--json", "--end-of-options", "topic"])).unwrap(),
            os(&["switch", "topic"])
        );
        assert!(child_argv(&os(&["switch", "--json", "--end-of-options", "-x"])).is_err());
    }

    #[test]
    fn userinfo_never_reaches_the_output() {
        assert_eq!(
            strip_userinfo("https://user:tok@example.com/r.git"),
            "https://example.com/r.git"
        );
        assert_eq!(
            strip_userinfo("https://example.com/a@b"),
            "https://example.com/a@b"
        );
        assert_eq!(strip_userinfo("origin"), "origin");
    }

    #[test]
    fn scan_reads_attached_and_separate_values() {
        let s = scan(
            &os(&["-cname", "--mode", "plain", "-q", "x"]),
            &['c'],
            &["mode"],
        );
        assert_eq!(s.value(&["-c"]), Some("name"));
        assert_eq!(s.value(&["--mode"]), Some("plain"));
        assert_eq!(s.words, ["x"]);
        let s = scan(&os(&["--", "-c"]), &['c'], &[]);
        assert_eq!(s.words, ["-c"]);
    }
}
