//! `asc cache rustc` (docs/designs/build-cache.md §4.1): asc as cargo's `RUSTC_WRAPPER`. Cargo
//! runs `asc <rustc> <args>…` for each crate; a cacheable compile is looked up in two steps:
//!
//! 1. the *unit key* (compiler version, arguments with every path made relative, the extern
//!    crates by content, the native libraries, cargo's environment) names the list of sources
//!    the crate read last time (its dep-info);
//! 2. the *full key* adds the identity of those sources (tree and file ids from asc where they
//!    are clean, so nothing is read or hydrated) and of the environment variables they read.
//!
//! A hit writes the outputs into `--out-dir` and replays rustc's output; a miss runs rustc and
//! stores both entries. Paths in the stored dep-info and output name the work tree, target
//! directory and cargo home by placeholder, so an entry written in one work tree is valid in all.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

use asc_buildcache::{Action, ActionKey, Event, EventKind, Output, OutputKind, Store, hex, unhex};
use serde::{Deserialize, Serialize};

use super::remote::{self, Mode};
use super::run::{origin, program, run_teed, spawn_push};
use super::{io_err, max_size, open_store};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::inputs;

/// The tool name of compile entries.
pub const TOOL: &str = "rustc";
/// The tool name of the entries that list a unit's sources.
pub const DEPS_TOOL: &str = "rustc-deps";
/// Crate types whose compile is cached.
const CACHEABLE: &[&str] = &["lib", "rlib", "staticlib", "proc-macro", "bin"];
/// rustc options that take a value, as the next argument or joined (`--x=v`, `-Cv`).
const VALUED: &[&str] = &[
    "--crate-name",
    "--crate-type",
    "--edition",
    "--emit",
    "--out-dir",
    "-o",
    "--extern",
    "-L",
    "-l",
    "--cfg",
    "--check-cfg",
    "--cap-lints",
    "-C",
    "--codegen",
    "-Z",
    "--target",
    "--error-format",
    "--json",
    "--color",
    "--print",
    "-W",
    "-A",
    "-D",
    "-F",
    "--warn",
    "--allow",
    "--deny",
    "--forbid",
    "--force-warn",
    "--explain",
    "--sysroot",
    "--remap-path-prefix",
    "--diagnostic-width",
    "--env-set",
];
/// Variables rustc itself reads, besides the ones cargo sets and the ones the crate reads.
const RUSTC_ENV: &[&str] = &[
    "RUSTC_BOOTSTRAP",
    "SOURCE_DATE_EPOCH",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
];
/// Cargo variables that do not reach the compile's result.
const CARGO_ENV_SKIPPED: &[&str] = &[
    "CARGO_MAKEFLAGS",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_INCREMENTAL",
    "CARGO_TERM_COLOR",
    "CARGO_TERM_PROGRESS_WHEN",
    "CARGO_TERM_VERBOSE",
    "CARGO_TERM_QUIET",
];

#[derive(clap::Args, Debug)]
pub struct RustcArgs {
    /// rustc (or `clippy-driver rustc`) and its arguments, as cargo passes them.
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

fn stem_is(arg: &OsString, names: &[&str]) -> bool {
    Path::new(arg)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| names.iter().any(|n| s.eq_ignore_ascii_case(n)))
}

/// Cargo's call of a `RUSTC_WRAPPER`, `asc <path>/rustc <args>…`, as `asc cache rustc -- …`.
pub fn as_wrapper(args: Vec<OsString>) -> Vec<OsString> {
    if !args
        .get(1)
        .is_some_and(|a| stem_is(a, &["rustc", "clippy-driver"]))
    {
        return args;
    }
    let mut out = Vec::with_capacity(args.len() + 3);
    out.push(args[0].clone());
    out.extend(["cache", "rustc", "--"].map(OsString::from));
    out.extend(args.into_iter().skip(1));
    out
}

/// One option or positional argument.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Opt {
    /// The option as spelled (`--crate-name`, `-C`); empty for a positional argument.
    name: String,
    value: Option<String>,
}

/// The arguments as options. `None` for an argument file (`@path`).
fn split(args: &[String]) -> Option<Vec<Opt>> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.starts_with('@') {
            return None;
        }
        let opt = if VALUED.contains(&a.as_str()) {
            Opt {
                name: a.clone(),
                value: Some(it.next()?.clone()),
            }
        } else if let Some((n, v)) = a
            .split_once('=')
            .filter(|(n, _)| n.starts_with("--") && VALUED.contains(n))
        {
            Opt {
                name: n.to_owned(),
                value: Some(v.to_owned()),
            }
        } else if let Some(short) = a
            .get(..2)
            .filter(|s| a.len() > 2 && !a.starts_with("--") && VALUED.contains(s))
        {
            Opt {
                name: short.to_owned(),
                value: Some(a[2..].to_owned()),
            }
        } else if a.starts_with('-') && a != "-" {
            Opt {
                name: a.clone(),
                value: None,
            }
        } else {
            Opt {
                name: String::new(),
                value: Some(a.clone()),
            }
        };
        out.push(opt);
    }
    Some(out)
}

/// A cacheable compile.
#[derive(Debug)]
struct Unit {
    crate_name: String,
    /// `-C extra-filename`: cargo's suffix of every output name.
    extra: String,
    out_dir: PathBuf,
    opts: Vec<Opt>,
}

impl Unit {
    /// The output names start with one of these, then `.`.
    fn stems(&self) -> [String; 2] {
        let s = format!("{}{}", self.crate_name, self.extra);
        [format!("lib{s}"), s]
    }

    fn dep_info(&self) -> PathBuf {
        self.out_dir
            .join(format!("{}{}.d", self.crate_name, self.extra))
    }
}

/// The compile `opts` describe, or `None` when it is not one asc caches: no crate name, input
/// from stdin, `--print`, an explicit `-o`, incremental compilation, no dep-info, or a crate type
/// outside [`CACHEABLE`].
fn unit(opts: Vec<Opt>, cwd: &Path) -> Option<Unit> {
    let (mut name, mut extra, mut out_dir) = (None, String::new(), None);
    let (mut types, mut inputs, mut dep_info) = (Vec::new(), 0, false);
    for o in &opts {
        let v = o.value.as_deref().unwrap_or("");
        match o.name.as_str() {
            "" => {
                if v == "-" {
                    return None;
                }
                inputs += 1;
            }
            "--crate-name" => name = Some(v.to_owned()),
            "--crate-type" => types.extend(v.split(',').map(str::to_owned)),
            "--out-dir" => out_dir = Some(cwd.join(v)),
            "--emit" => {
                for e in v.split(',') {
                    if e.contains('=') {
                        return None;
                    }
                    dep_info |= e == "dep-info";
                }
            }
            "-C" | "--codegen" => {
                if v.starts_with("incremental") {
                    return None;
                }
                if let Some(x) = v.strip_prefix("extra-filename=") {
                    extra = x.to_owned();
                }
            }
            "--print" | "-o" | "--explain" => return None,
            _ => {}
        }
    }
    if inputs != 1 || !dep_info || types.is_empty() {
        return None;
    }
    if !types.iter().all(|t| CACHEABLE.contains(&t.as_str())) {
        return None;
    }
    Some(Unit {
        crate_name: name?,
        extra,
        out_dir: out_dir?,
        opts,
    })
}

/// The machine-specific directories in arguments, dep-info and output, each written as a
/// placeholder in what is stored.
struct Places {
    /// (as written, placeholder), longest first.
    forms: Vec<(String, String)>,
}

impl Places {
    fn new(dirs: &[(char, &Path)]) -> Self {
        let mut forms: Vec<(String, String)> = Vec::new();
        for (tag, dir) in dirs {
            let native = dir.to_string_lossy();
            let native = native.trim_end_matches(['/', '\\']);
            if native.is_empty() {
                continue;
            }
            let written = [
                native.to_owned(),
                native.replace('\\', "/"),
                native.replace('\\', "\\\\"),
            ];
            for (i, w) in written.into_iter().enumerate() {
                if !forms.iter().any(|(f, _)| *f == w) {
                    forms.push((w, format!("\u{1}{tag}{i}\u{1}")));
                }
            }
        }
        forms.sort_by_key(|(f, _)| std::cmp::Reverse(f.len()));
        Places { forms }
    }

    fn norm(&self, text: &str) -> String {
        let mut s = text.to_owned();
        for (lit, ph) in &self.forms {
            s = replace_dir(&s, lit, ph);
        }
        s
    }

    fn denorm(&self, text: &str) -> String {
        let mut s = text.to_owned();
        for (lit, ph) in &self.forms {
            s = s.replace(ph.as_str(), lit);
        }
        s
    }

    /// Whether the normalized `text` starts with the placeholder of `tag`; the rest after it.
    fn strip<'a>(&self, text: &'a str, tag: char) -> Option<&'a str> {
        self.forms
            .iter()
            .filter(|(_, ph)| ph.chars().nth(1) == Some(tag))
            .find_map(|(_, ph)| text.strip_prefix(ph.as_str()))
    }
}

/// `text` with each occurrence of the directory `dir` (case-insensitive on Windows) that ends at
/// a path boundary replaced by `with`.
fn replace_dir(text: &str, dir: &str, with: &str) -> String {
    let (hay, needle) = if cfg!(windows) {
        (text.to_ascii_lowercase(), dir.to_ascii_lowercase())
    } else {
        (text.to_owned(), dir.to_owned())
    };
    let mut out = String::with_capacity(text.len());
    let (mut last, mut from) = (0, 0);
    while let Some(i) = hay[from..].find(&needle) {
        let (at, end) = (from + i, from + i + needle.len());
        let next = text[end..].chars().next();
        if next.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))) {
            out.push_str(&text[last..at]);
            out.push_str(with);
            last = end;
        }
        from = end;
    }
    out.push_str(&text[last..]);
    out
}

/// The sources a compile read and the variables it read, from rustc's dep-info: each source is
/// also its own empty rule (`path:`), and each variable a `# env-dep:NAME[=value]` comment.
fn parse_dep_info(text: &str, cwd: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let (mut deps, mut env) = (BTreeSet::new(), BTreeSet::new());
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            let name = rest.split_once('=').map_or(rest, |(n, _)| n);
            env.insert(name.to_owned());
        } else if !line.starts_with('#')
            && !line.contains(": ")
            && let Some(path) = line.strip_suffix(':')
            && !path.is_empty()
        {
            deps.insert(cwd.join(path.replace("\\ ", " ")));
        }
    }
    (deps.into_iter().collect(), env.into_iter().collect())
}

/// What a unit key names: the sources and variables the compile read, normalized.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Sources {
    deps: Vec<String>,
    env: Vec<String>,
}

/// Content id of a file outside asc's view (the target directory, cargo home, an ignored file),
/// remembered by path, size and modification time in `<store>/rustc/ids`.
fn file_id(store: &Store, path: &Path) -> Option<[u8; 32]> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let memo = store
        .root()
        .join("rustc")
        .join("ids")
        .join(hex(
            blake3::hash(path.as_os_str().as_encoded_bytes()).as_bytes()
        ));
    let stamp = format!("{} {mtime} ", meta.len());
    if let Ok(text) = std::fs::read_to_string(&memo)
        && let Some(id) = text.strip_prefix(&stamp).and_then(unhex)
    {
        return Some(id);
    }
    let mut h = blake3::Hasher::new();
    h.update_reader(std::fs::File::open(path).ok()?).ok()?;
    let id = *h.finalize().as_bytes();
    // A file written in the last seconds may change again within the same timestamp.
    let settled = meta
        .modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(2));
    if settled && let Some(dir) = memo.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&memo, format!("{stamp}{}", hex(&id)));
    }
    Some(id)
}

/// `<compiler> -vV` (and `--version` for clippy-driver), remembered for a compiler file that a
/// toolchain override cannot redirect.
fn version(store: &Store, command: &[OsString]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (i, prog) in command.iter().take(2).enumerate() {
        if i == 1 && !stem_is(prog, &["rustc"]) {
            break;
        }
        let flags: &[&str] = if stem_is(prog, &["clippy-driver"]) {
            &["-vV", "--version"]
        } else {
            &["-vV"]
        };
        for flag in flags {
            out.extend(version_of(store, prog, flag)?);
        }
    }
    Ok(out)
}

fn version_of(store: &Store, prog: &OsString, flag: &str) -> Result<Vec<u8>> {
    let path = PathBuf::from(program(prog));
    let toolchain = std::env::var_os("RUSTUP_TOOLCHAIN");
    // A rustup proxy picks the toolchain by directory unless RUSTUP_TOOLCHAIN names it.
    let proxy = path
        .parent()
        .is_some_and(|d| d.ends_with("bin") && d.parent() == Some(cargo_home().as_path()));
    let memo = std::fs::metadata(&path)
        .ok()
        .filter(|_| path.is_absolute() && (toolchain.is_some() || !proxy))
        .and_then(|m| {
            let t = m
                .modified()
                .ok()?
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()?;
            let mut h = blake3::Hasher::new();
            h.update(path.as_os_str().as_encoded_bytes());
            h.update(format!("\0{}\0{}\0{flag}\0", m.len(), t.as_nanos()).as_bytes());
            h.update(toolchain.as_deref().unwrap_or_default().as_encoded_bytes());
            h.update(b"\0");
            h.update(
                std::env::var_os("RUSTUP_HOME")
                    .unwrap_or_default()
                    .as_encoded_bytes(),
            );
            Some(
                store
                    .root()
                    .join("rustc")
                    .join("versions")
                    .join(hex(h.finalize().as_bytes())),
            )
        });
    if let Some(m) = &memo
        && let Ok(v) = std::fs::read(m)
    {
        return Ok(v);
    }
    let out = Command::new(&path).arg(flag).output()?;
    if !out.status.success() {
        return Err(CliError::Fatal(format!(
            "'{} {flag}' failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if let Some(m) = &memo
        && let Some(dir) = m.parent()
    {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(m, &out.stdout);
    }
    Ok(out.stdout)
}

fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".cargo")))
        .unwrap_or_default()
}

/// The target directory: the ancestor of `out_dir` that cargo marked with `CACHEDIR.TAG`, else
/// the profile directory above `deps`.
fn target_dir(out_dir: &Path) -> PathBuf {
    out_dir
        .ancestors()
        .find(|d| d.join("CACHEDIR.TAG").is_file())
        .or_else(|| out_dir.parent().filter(|_| out_dir.ends_with("deps")))
        .unwrap_or(out_dir)
        .to_owned()
}

/// Everything the lookup needs.
struct Plan {
    unit: Unit,
    places: Places,
    root: Option<PathBuf>,
    /// The compiler and the arguments as cargo passed them.
    command: Vec<OsString>,
    unit_key: ActionKey,
}

/// The plan of a cacheable compile; `None` runs rustc unchanged.
fn plan(ctx: &Ctx, store: &Store, command: Vec<OsString>) -> Result<Option<Plan>> {
    let skip = if stem_is(&command[0], &["clippy-driver"])
        && command.get(1).is_some_and(|c| stem_is(c, &["rustc"]))
    {
        2
    } else {
        1
    };
    let Some(args) = command[skip.min(command.len())..]
        .iter()
        .map(|a| a.to_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()
    else {
        return Ok(None);
    };
    let Some(unit) = split(&args).and_then(|o| unit(o, &ctx.cwd)) else {
        return Ok(None);
    };
    let root = ctx.ws_opt().map(|ws| ws.projection.toplevel().to_owned());
    let target = target_dir(&unit.out_dir);
    let home = cargo_home();
    let mut dirs = vec![('T', target.as_path()), ('H', home.as_path())];
    if let Some(r) = &root {
        dirs.push(('R', r.as_path()));
    }
    let places = Places::new(&dirs);
    let key = unit_key(store, &unit, &places, &ctx.cwd, &command, root.is_some())?;
    Ok(Some(Plan {
        unit,
        places,
        root,
        command,
        unit_key: key,
    }))
}

/// Key material of the arguments: paths relative, externs and native libraries by content.
fn unit_key(
    store: &Store,
    unit: &Unit,
    places: &Places,
    cwd: &Path,
    command: &[OsString],
    remapped: bool,
) -> Result<ActionKey> {
    let mut b = ActionKey::builder(TOOL)
        .part("version", &version(store, command)?)
        .part("cwd", places.norm(&cwd.to_string_lossy()).as_bytes())
        .part("remap", &[u8::from(remapped)]);
    for o in &unit.opts {
        let v = o.value.as_deref().unwrap_or("");
        match o.name.as_str() {
            "--extern" => {
                let (name, path) = v.split_once('=').unwrap_or((v, ""));
                b = b.part("extern", name.as_bytes());
                if !path.is_empty() {
                    let file = cwd.join(path);
                    b = b
                        .part("extern-path", places.norm(path).as_bytes())
                        .part("extern-id", &file_id(store, &file).unwrap_or_default());
                }
            }
            "-L" => {
                let (kind, dir) = match v.split_once('=') {
                    Some((k, d))
                        if matches!(k, "dependency" | "crate" | "native" | "framework" | "all") =>
                    {
                        (k, d)
                    }
                    _ => ("all", v),
                };
                // Crates are found through their externs, which are keyed by content.
                if matches!(kind, "dependency" | "crate") {
                    continue;
                }
                let norm = places.norm(dir);
                b = b.part("lib-dir", format!("{kind}={norm}").as_bytes());
                // A build script's native libraries (in the target directory or the work tree)
                // are keyed by content; system directories by name.
                if places.strip(&norm, 'T').is_some() || places.strip(&norm, 'R').is_some() {
                    let mut files: Vec<PathBuf> = std::fs::read_dir(cwd.join(dir))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_file())
                        .collect();
                    files.sort();
                    for f in files {
                        let name = f.file_name().unwrap_or_default().to_string_lossy();
                        b = b
                            .part("lib-file", name.as_bytes())
                            .part("id", &file_id(store, &f).unwrap_or_default());
                    }
                }
            }
            _ => {
                b = b
                    .part("opt", o.name.as_bytes())
                    .part("value", places.norm(v).as_bytes());
            }
        }
    }
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            (k.starts_with("CARGO") && !CARGO_ENV_SKIPPED.contains(&k.as_str()))
                || k == "OUT_DIR"
                || RUSTC_ENV.contains(&k.as_str())
        })
        .collect();
    env.sort();
    for (k, v) in env {
        b = b
            .part("env", k.as_bytes())
            .part("value", places.norm(&v).as_bytes());
    }
    Ok(b.finish())
}

/// The full key: the unit key and the identity of the sources and variables it read.
async fn full_key(ctx: &Ctx, store: &Store, plan: &Plan, sources: &Sources) -> Result<ActionKey> {
    let mut b = ActionKey::builder(TOOL).part("unit", &plan.unit_key.0);
    let mut in_repo: Vec<(String, String)> = Vec::new();
    let mut outside: Vec<&String> = Vec::new();
    for d in &sources.deps {
        match plan.places.strip(d, 'R') {
            Some(rest) if plan.root.is_some() => {
                let rel = rest.trim_start_matches(['/', '\\']).replace('\\', "/");
                in_repo.push((rel, d.clone()));
            }
            _ => outside.push(d),
        }
    }
    let mut ids: BTreeMap<Vec<u8>, [u8; 32]> = BTreeMap::new();
    if let (Some(ws), Some(root)) = (ctx.ws_opt(), &plan.root)
        && !in_repo.is_empty()
    {
        for chunk in in_repo.chunks(4000) {
            let paths: Vec<OsString> = chunk
                .iter()
                .map(|(rel, _)| root.join(rel).into_os_string())
                .collect();
            for e in inputs::compute(ctx, ws, &paths, true).await?.entries {
                ids.insert(e.path, e.id);
            }
        }
    }
    for (rel, d) in &in_repo {
        match ids.get(rel.as_bytes()) {
            Some(id) => b = b.part("src", rel.as_bytes()).part("id", id),
            // Ignored, or in a nested repository: hashed from its bytes.
            None => outside.push(d),
        }
    }
    outside.sort();
    for d in outside {
        let path = PathBuf::from(plan.places.denorm(d));
        b = b
            .part("file", d.as_bytes())
            .part("id", &file_id(store, &path).unwrap_or_default());
    }
    for name in &sources.env {
        b = match std::env::var_os(name) {
            Some(v) => b
                .part("env", name.as_bytes())
                .part("value", plan.places.norm(&v.to_string_lossy()).as_bytes()),
            None => b.part("unset", name.as_bytes()),
        };
    }
    Ok(b.finish())
}

fn passthrough(command: &[OsString], cwd: &Path) -> Result<ExitCode> {
    let status = Command::new(program(&command[0]))
        .args(&command[1..])
        .current_dir(cwd)
        .status()
        .map_err(|e| {
            CliError::Fatal(format!(
                "cannot run '{}': {e}",
                command[0].to_string_lossy()
            ))
        })?;
    Ok(exit(status.code()))
}

fn exit(code: Option<i32>) -> ExitCode {
    match code {
        Some(0) => ExitCode::SUCCESS,
        Some(c) => ExitCode::from(u8::try_from(c).unwrap_or(1)),
        None => ExitCode::FAILURE,
    }
}

pub async fn run(ctx: &Ctx, a: RustcArgs) -> Result<ExitCode> {
    let store = match open_store(ctx) {
        Ok(s) => s,
        Err(e) => return warn_and_run(&e, &a.command, &ctx.cwd),
    };
    let plan = match plan(ctx, &store, a.command.clone()) {
        Ok(Some(p)) => p,
        Ok(None) => return passthrough(&a.command, &ctx.cwd),
        Err(e) => return warn_and_run(&e, &a.command, &ctx.cwd),
    };
    let mode = remote::mode(ctx).unwrap_or(Mode::Off);
    let origin = origin(plan.root.as_deref().unwrap_or(&ctx.cwd));
    match lookup(ctx, &store, &plan, mode, &origin).await {
        Ok(Some(code)) => return Ok(code),
        Ok(None) => {}
        Err(e) => eprintln!("warning: asc cache rustc: {e}"),
    }
    compile(ctx, &store, &plan, mode, &origin).await
}

fn warn_and_run(e: &CliError, command: &[OsString], cwd: &Path) -> Result<ExitCode> {
    eprintln!("warning: asc cache rustc: {e}");
    passthrough(command, cwd)
}

/// A hit: the outputs written and the output replayed. `None` on a miss.
async fn lookup(
    ctx: &Ctx,
    store: &Store,
    plan: &Plan,
    mode: Mode,
    origin: &asc_buildcache::Origin,
) -> Result<Option<ExitCode>> {
    let mut listed = store.get_action(&plan.unit_key).map_err(io_err)?;
    if listed.is_none() && mode != Mode::Off {
        listed = remote::fetch_entry(ctx, store, &plan.unit_key).await;
    }
    let Some(sources) = listed.as_ref().and_then(|l| read_sources(store, l)) else {
        store.record(&Event::new(
            EventKind::Miss,
            TOOL,
            &plan.unit_key,
            origin,
            0,
        ));
        return Ok(None);
    };
    let key = full_key(ctx, store, plan, &sources).await?;
    let mut hit = store.lookup(TOOL, &key, origin).map_err(io_err)?;
    if hit.is_none() && mode != Mode::Off {
        hit = remote::fetch_entry(ctx, store, &key).await;
    }
    let Some(action) = hit else { return Ok(None) };
    if !store.restore(&action, &plan.unit.out_dir).map_err(io_err)? {
        return Ok(None);
    }
    for o in action.outputs.iter().filter(|o| o.path.ends_with(".d")) {
        let path = plan.unit.out_dir.join(&o.path);
        let text = std::fs::read_to_string(&path)?;
        std::fs::write(&path, plan.places.denorm(&text))?;
    }
    replay(
        store,
        plan,
        action.stdout.as_deref(),
        &mut std::io::stdout(),
    )?;
    replay(
        store,
        plan,
        action.stderr.as_deref(),
        &mut std::io::stderr(),
    )?;
    if mode == Mode::ReadWrite {
        store.queue_push(&plan.unit_key).map_err(io_err)?;
        store.queue_push(&key).map_err(io_err)?;
        spawn_push(store, &ctx.cwd);
    }
    Ok(Some(exit(Some(action.exit))))
}

fn read_sources(store: &Store, listed: &Action) -> Option<Sources> {
    let blob = unhex(listed.stdout.as_deref()?)?;
    serde_json::from_slice(&store.read_blob(&blob).ok()?).ok()
}

fn replay(store: &Store, plan: &Plan, blob: Option<&str>, to: &mut dyn Write) -> Result<()> {
    let Some(id) = blob.and_then(unhex) else {
        return Ok(());
    };
    let bytes = store.read_blob(&id).map_err(io_err)?;
    to.write_all(
        plan.places
            .denorm(&String::from_utf8_lossy(&bytes))
            .as_bytes(),
    )?;
    to.flush()?;
    Ok(())
}

/// A miss: rustc runs (with the work tree's paths remapped so the outputs hold none), and a
/// success is stored under both keys.
async fn compile(
    ctx: &Ctx,
    store: &Store,
    plan: &Plan,
    mode: Mode,
    origin: &asc_buildcache::Origin,
) -> Result<ExitCode> {
    let mut command = plan.command.clone();
    if let Some(root) = &plan.root {
        let mut remap = OsString::from("--remap-path-prefix=");
        remap.push(root);
        remap.push("=.");
        command.push(remap);
    }
    let start = SystemTime::now();
    let ran = run_teed(&command, &ctx.cwd)?;
    if ran.code != Some(0) {
        return Ok(exit(ran.code));
    }
    if let Err(e) = store_compile(ctx, store, plan, mode, origin, start, ran).await {
        eprintln!("warning: asc cache rustc: not stored: {e}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn store_compile(
    ctx: &Ctx,
    store: &Store,
    plan: &Plan,
    mode: Mode,
    origin: &asc_buildcache::Origin,
    start: SystemTime,
    ran: super::run::Ran,
) -> Result<()> {
    let (Some(stdout), Some(stderr)) = (ran.stdout, ran.stderr) else {
        return Ok(());
    };
    let (Ok(stdout), Ok(stderr)) = (String::from_utf8(stdout), String::from_utf8(stderr)) else {
        return Ok(());
    };
    let unit = &plan.unit;
    let dep_info = std::fs::read_to_string(unit.dep_info())?;
    let (deps, env) = parse_dep_info(&dep_info, &ctx.cwd);
    // A source changed while rustc ran: the outputs may be of its old content.
    let since = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t > start)
    };
    if deps.iter().any(|d| since(d)) {
        return Ok(());
    }
    let sources = Sources {
        deps: deps
            .iter()
            .map(|d| plan.places.norm(&d.to_string_lossy()))
            .collect(),
        env,
    };
    let key = full_key(ctx, store, plan, &sources).await?;

    // The outputs: the files rustc wrote under this unit's names.
    let stems = unit.stems();
    let fresh = start.checked_sub(Duration::from_secs(2)).unwrap_or(start);
    let mut names: Vec<String> = std::fs::read_dir(&unit.out_dir)?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let ours = stems.iter().any(|s| {
                name.strip_prefix(s.as_str())
                    .is_some_and(|r| r.starts_with('.'))
            });
            let modified = e.metadata().ok()?.modified().ok()?;
            (ours && modified >= fresh).then_some(name)
        })
        .collect();
    names.sort();
    let (infos, others): (Vec<String>, Vec<String>) =
        names.into_iter().partition(|n| n.ends_with(".d"));
    let mut outputs = store.put_outputs(&unit.out_dir, &others).map_err(io_err)?;
    for n in infos {
        let text = std::fs::read_to_string(unit.out_dir.join(&n))?;
        let norm = plan.places.norm(&text);
        let (id, size) = store.put_blob(norm.as_bytes()).map_err(io_err)?;
        outputs.push(Output {
            path: n,
            kind: OutputKind::File,
            size,
            blob: hex(&id),
        });
    }
    outputs.sort_by(|a, b| a.path.cmp(&b.path));

    let mut action = Action::new(TOOL, &unit.crate_name, origin.clone());
    action.outputs = outputs;
    for (text, slot) in [(&stdout, &mut action.stdout), (&stderr, &mut action.stderr)] {
        if !text.is_empty() {
            let norm = plan.places.norm(text);
            *slot = Some(hex(&store.put_blob(norm.as_bytes()).map_err(io_err)?.0));
        }
    }
    store.put_action(&key, &action).map_err(io_err)?;

    let mut listing = Action::new(DEPS_TOOL, &unit.crate_name, origin.clone());
    let body = serde_json::to_vec(&sources).map_err(|e| CliError::Fatal(e.to_string()))?;
    listing.stdout = Some(hex(&store.put_blob(&body).map_err(io_err)?.0));
    store.put_action(&plan.unit_key, &listing).map_err(io_err)?;

    if mode == Mode::ReadWrite {
        store.queue_push(&plan.unit_key).map_err(io_err)?;
        store.queue_push(&key).map_err(io_err)?;
        spawn_push(store, &ctx.cwd);
    }
    if let Ok(max) = max_size(ctx) {
        let _ = store.maybe_gc(max);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn cargo_calls_are_rewritten_to_the_wrapper_command() {
        let got = as_wrapper(os(&["asc", "/x/bin/rustc", "--crate-name", "a"]));
        assert_eq!(
            got,
            os(&[
                "asc",
                "cache",
                "rustc",
                "--",
                "/x/bin/rustc",
                "--crate-name",
                "a"
            ])
        );
        let clippy = as_wrapper(os(&["asc", "clippy-driver", "rustc", "-vV"]));
        assert_eq!(clippy[1..4], os(&["cache", "rustc", "--"])[..]);
        for other in [os(&["asc", "status"]), os(&["asc"]), os(&["asc", "rustcx"])] {
            assert_eq!(as_wrapper(other.clone()), other);
        }
    }

    fn cargo_args(extra: &[&str]) -> Vec<String> {
        let mut v = strings(&[
            "--crate-name",
            "util",
            "--edition=2021",
            "src/lib.rs",
            "--error-format=json",
            "--crate-type",
            "lib",
            "--emit=dep-info,metadata,link",
            "-C",
            "embed-bitcode=no",
            "-Cextra-filename=-abc",
            "--out-dir",
            "/t/debug/deps",
            "-L",
            "dependency=/t/debug/deps",
        ]);
        v.extend(strings(extra));
        v
    }

    #[test]
    fn cacheable_compiles_are_recognised() {
        let cwd = Path::new("/w");
        let u = unit(split(&cargo_args(&[])).unwrap(), cwd).unwrap();
        assert_eq!((u.crate_name.as_str(), u.extra.as_str()), ("util", "-abc"));
        assert_eq!(u.out_dir, Path::new("/t/debug/deps"));
        assert_eq!(u.stems(), ["libutil-abc".to_owned(), "util-abc".to_owned()]);
        for extra in [
            &["-C", "incremental=/t/inc"][..],
            &["--print", "cfg"],
            &["-o", "x"],
            &["--crate-type", "cdylib"],
            &["-"],
        ] {
            assert!(
                unit(split(&cargo_args(extra)).unwrap(), cwd).is_none(),
                "{extra:?}"
            );
        }
        assert!(split(&strings(&["@args.txt"])).is_none());
        assert!(unit(split(&strings(&["-vV"])).unwrap(), cwd).is_none());
        let no_dep_info = strings(&[
            "--crate-name",
            "a",
            "a.rs",
            "--crate-type",
            "lib",
            "--out-dir",
            "o",
            "--emit=link",
        ]);
        assert!(unit(split(&no_dep_info).unwrap(), cwd).is_none());
    }

    #[test]
    fn places_round_trip_and_respect_path_boundaries() {
        let p = Places::new(&[('T', Path::new("/w/target")), ('R', Path::new("/w"))]);
        let text = "/w/src/lib.rs /w/target/debug/x.rlib /w2/other /w";
        let n = p.norm(text);
        assert!(!n.contains("/w/"), "{n}");
        assert!(n.contains("/w2/other"), "{n}");
        assert_eq!(p.denorm(&n), text);
        assert_eq!(p.strip(&p.norm("/w/target/a"), 'T'), Some("/a"));
        assert_eq!(p.strip(&p.norm("/w/src/a"), 'R'), Some("/src/a"));
        assert!(p.strip(&p.norm("/w/target/a"), 'R').is_none());
        // Another work tree reads the same text with its own directories.
        let q = Places::new(&[('T', Path::new("/v/target")), ('R', Path::new("/v"))]);
        assert_eq!(
            q.denorm(&n),
            "/v/src/lib.rs /v/target/debug/x.rlib /w2/other /v"
        );
    }

    #[test]
    fn json_escaped_windows_paths_are_placeholders_too() {
        let p = Places::new(&[('R', Path::new(r"C:\w"))]);
        let n = p.norm(r#"{"artifact":"C:\\w\\t\\x.rmeta"} C:\w\src C:/w/src"#);
        assert!(!n.contains("C:"), "{n}");
        let q = Places::new(&[('R', Path::new(r"D:\v"))]);
        assert_eq!(
            q.denorm(&n),
            r#"{"artifact":"D:\\v\\t\\x.rmeta"} D:\v\src D:/v/src"#
        );
    }

    #[test]
    fn dep_info_lists_sources_and_variables() {
        let text = "/t/deps/util-abc.d: src/lib.rs src/a\\ b.rs\n\nsrc/lib.rs:\nsrc/a\\ b.rs:\n\n# env-dep:CARGO_PKG_NAME=util\n# env-dep:UNSET_ONE\n";
        let (deps, env) = parse_dep_info(text, Path::new("/w"));
        assert_eq!(
            deps,
            vec![
                PathBuf::from("/w/src/a b.rs"),
                PathBuf::from("/w/src/lib.rs")
            ]
        );
        assert_eq!(env, strings(&["CARGO_PKG_NAME", "UNSET_ONE"]));
    }
}
