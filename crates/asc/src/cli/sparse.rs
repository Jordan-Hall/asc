//! `sparse-checkout set|add|list|disable` and `asc sparse set|add|list|disable|dehydrate`
//! (SPEC §9.3, §10.2, §11.2).
//!
//! The cone is stored in `workspaces.sparse`, one directory per line. A cone always holds the
//! files at the root and directly in the ancestors of its directories, and everything inside
//! them (git's cone mode; non-cone patterns are not served).
//!
//! * **Plain workspace.** `set`/`add`/`disable` change the cone and then reconcile the work tree
//!   with HEAD: files that enter the cone are written (an existing untracked file is left alone),
//!   files that leave it are removed only when they still equal HEAD (a modified one is kept with
//!   a warning), emptied directories go, and the synthetic index re-marks skip-worktree.
//! * **Virtual workspace.** The cone is the prefetch profile: nothing is removed (a virtual tree
//!   shows every path), and the files that enter the cone are fetched into the cache.
//! * `sparse dehydrate [<path>…]` asks the daemon to re-virtualise clean hydrated files of a
//!   virtual workspace; a plain workspace has no placeholders and refuses.

use std::ffi::OsString;
use std::io::Read;
use std::process::ExitCode;

use asc_core::git::BlobOidHasher;
use asc_core::id::FileId;
use asc_core::object::{Entry, Kind};
use asc_core::path::RepoPath;
use asc_rpc::ipc::{WorkspaceClient, connect};
use asc_rpc::server::wire;
use asc_state::state::workspaces::Mode;
use asc_wc::SafeRoot;
use asc_wc::plain::Sparse;
use futures::TryStreamExt;

use super::common::{CliError, Ctx, Result, WsCtx};
use super::refs_plumbing::args::{Cursor, Word};
use super::restore::sync_worktree;
use crate::daemon::lifecycle::{self, Config, Probe};
use crate::ext;
use crate::native::NativeSpec;

const USAGE: &str =
    "usage: git sparse-checkout (set|add|list|disable) [--cone] [--stdin] [<directory>...]
   or: asc sparse (set|add|list|disable|dehydrate) [--stdin] [<path>...]";

/// Most directories in one cone.
const MAX_DIRS: usize = 10_000;

/// Most bytes read from `--stdin`.
const MAX_STDIN: u64 = 4 * 1024 * 1024;

/// Most leaves the reconcile walk collects.
const MAX_LEAVES: usize = 5_000_000;

/// Paths handed to one forced sync.
const SYNC_BATCH: usize = 1_000;

#[derive(clap::Parser, Debug)]
#[command(name = "sparse", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[
    NativeSpec {
        cmd: "sparse-checkout",
        flags: &["--cone", "--stdin"],
        placeholders: &[],
    },
    NativeSpec {
        cmd: "sparse",
        flags: &["--stdin"],
        placeholders: &[],
    },
];

#[derive(Debug, PartialEq, Eq)]
enum Sub {
    Set,
    Add,
    List,
    Disable,
    Dehydrate,
}

#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    sub: Sub,
    stdin: bool,
    items: Vec<String>,
}

fn parse(rest: &[OsString]) -> Result<Parsed> {
    let mut cur = Cursor::new(rest, USAGE);
    let Some(Word::Operand(name)) = cur.next() else {
        return Err(cur.usage_only());
    };
    let sub = match name.as_str() {
        "set" => Sub::Set,
        "add" => Sub::Add,
        "list" => Sub::List,
        "disable" => Sub::Disable,
        "dehydrate" => Sub::Dehydrate,
        other => {
            return Err(cur.usage_error(&format!("unknown subcommand '{other}'")));
        }
    };
    let (mut stdin, mut items) = (false, Vec::new());
    let mut literal = false;
    while let Some(word) = cur.next() {
        match word {
            Word::End => {
                literal = true;
                items.extend(cur.rest());
            }
            Word::Long(n, None) if !literal && n == "stdin" => stdin = true,
            Word::Long(n, None) if !literal && n == "cone" && sub != Sub::Dehydrate => {}
            Word::Long(n, _) => return Err(cur.usage_error(&format!("unknown option `{n}'"))),
            Word::Short(s) => return Err(cur.usage_error(&format!("unknown switch `{s}'"))),
            Word::Operand(o) => items.push(o),
        }
    }
    if matches!(sub, Sub::List | Sub::Disable) && (stdin || !items.is_empty()) {
        return Err(cur.usage_only());
    }
    Ok(Parsed { sub, stdin, items })
}

/// Cone directories from user text: slashes trimmed, validated as repository paths, sorted,
/// without duplicates and without a directory already covered by an ancestor in the list.
/// `.` and the empty string name the root, which every cone holds anyway.
fn cone_dirs(items: &[String]) -> Result<Vec<RepoPath>> {
    let mut dirs: Vec<RepoPath> = Vec::new();
    for raw in items {
        let t = raw.trim().replace('\\', "/");
        let t = t.trim_matches('/');
        let t = t.strip_prefix("./").unwrap_or(t);
        if t.is_empty() || t == "." {
            continue;
        }
        let p = RepoPath::new(t.as_bytes().to_vec())
            .map_err(|e| CliError::Fatal(format!("asc: '{raw}' is not a usable directory: {e}")))?;
        dirs.push(p);
    }
    dirs.sort();
    dirs.dedup();
    let all = dirs.clone();
    dirs.retain(|d| !all.iter().any(|a| a != d && d.starts_with_dir(a)));
    if dirs.len() > MAX_DIRS {
        return Err(CliError::Fatal(format!(
            "asc: a cone holds at most {MAX_DIRS} directories"
        )));
    }
    Ok(dirs)
}

fn render(dirs: &[RepoPath]) -> String {
    dirs.iter()
        .map(|d| format!("{}\n", String::from_utf8_lossy(d.as_bytes())))
        .collect()
}

fn read_stdin() -> Result<Vec<String>> {
    let mut text = String::new();
    std::io::stdin()
        .take(MAX_STDIN + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_STDIN {
        return Err(CliError::Fatal("asc: --stdin is too large".into()));
    }
    Ok(text.lines().map(str::to_owned).collect())
}

async fn store_cone(ws: &WsCtx, text: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE workspaces SET sparse = ? WHERE id = ?")
        .bind(text)
        .bind(ws.workspace.ws.id.to_string())
        .execute(ws.db.pool())
        .await
        .map_err(asc_state::Error::from)?;
    Ok(())
}

/// What a reconcile did.
#[derive(Debug, Default, PartialEq, Eq)]
struct Reconciled {
    written: usize,
    removed: usize,
    kept: Vec<RepoPath>,
}

/// Whether the file on disk still is the tracked content of `entry`.
async fn unchanged(ws: &WsCtx, abs: &std::path::Path, entry: &Entry) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(abs) else {
        return false;
    };
    match entry.kind {
        Kind::Symlink => {
            let Ok(want) = ws
                .engine
                .read_file(FileId::from_bytes(entry.id), entry.size)
                .await
            else {
                return false;
            };
            if meta.file_type().is_symlink() {
                std::fs::read_link(abs)
                    .is_ok_and(|t| t.to_string_lossy().replace('\\', "/").as_bytes() == &want[..])
            } else {
                meta.is_file() && std::fs::read(abs).is_ok_and(|b| b == want[..])
            }
        }
        Kind::File | Kind::Exec => {
            if !meta.is_file() || meta.len() != entry.size {
                return false;
            }
            let Ok(mut file) = std::fs::File::open(abs) else {
                return false;
            };
            let mut hasher = BlobOidHasher::new(entry.size);
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        hasher.update(&buf[..n]);
                    }
                    Err(_) => return false,
                }
            }
            hasher.finalize().ok() == Some(entry.git)
        }
        Kind::Dir | Kind::Gitlink => false,
    }
}

/// Brings the work tree of `ctx` (opened after the cone was stored) in line with the change
/// from `old` to `new`.
async fn reconcile(ctx: &Ctx, old: Option<&Sparse>, new: Option<&Sparse>) -> Result<Reconciled> {
    let ws = ctx.ws()?;
    let mut out = Reconciled::default();
    let Some(head) = ws.head_commit().await? else {
        return Ok(out);
    };
    let root = ws.engine.get_changeset(head).await?.root;
    let mut enter: Vec<RepoPath> = Vec::new();
    let mut leave: Vec<(RepoPath, Entry)> = Vec::new();
    let mut walk = ws.engine.walk(root, None);
    let mut seen = 0usize;
    while let Some((path, entry)) = walk.try_next().await? {
        seen += 1;
        if seen > MAX_LEAVES {
            return Err(CliError::Fatal(
                "asc: the tree is too large to reconcile".into(),
            ));
        }
        if matches!(entry.kind, Kind::Dir | Kind::Gitlink) {
            continue;
        }
        let was = old.is_none_or(|s| s.contains(&path));
        let will = new.is_none_or(|s| s.contains(&path));
        match (was, will) {
            (false, true) => enter.push(path),
            (true, false) => leave.push((path, entry)),
            _ => {}
        }
    }
    drop(walk);
    let gate = SafeRoot::new(ws.projection.toplevel());

    // Entering: write, but never over something that is already there.
    let mut to_write = Vec::new();
    for path in enter {
        match gate.resolve(&path) {
            Some(abs) if std::fs::symlink_metadata(&abs).is_err() => to_write.push(path),
            _ => out.kept.push(path),
        }
    }
    for batch in to_write.chunks(SYNC_BATCH) {
        sync_worktree(ctx, root, batch, true).await?;
        out.written += batch.len();
    }

    // Leaving: remove only what still equals HEAD.
    for (path, entry) in leave {
        let Some(abs) = gate.resolve(&path) else {
            continue;
        };
        if std::fs::symlink_metadata(&abs).is_err() {
            continue;
        }
        if unchanged(ws, &abs, &entry).await {
            if gate.remove_file(&path)? {
                out.removed += 1;
            }
            gate.remove_empty_parents(&path);
        } else {
            out.kept.push(path);
        }
    }
    Ok(out)
}

/// The current directory below the work tree root, `/`-separated; empty at the root.
fn cwd_prefix(ctx: &Ctx, ws: &WsCtx) -> String {
    let norm = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
    let top = norm(ws.projection.toplevel());
    let cwd = norm(&ctx.cwd);
    cwd.strip_prefix(top.trim_end_matches('/'))
        .map(|r| r.trim_matches('/').to_owned())
        .unwrap_or_default()
}

async fn change_cone(ctx: &Ctx, p: &Parsed) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let old_text = ws.workspace.ws.sparse.clone();
    let new_text: Option<String> = if p.sub == Sub::Disable {
        None
    } else {
        let mut items = p.items.clone();
        if p.stdin {
            items.extend(read_stdin()?);
        }
        let mut dirs = cone_dirs(&items)?;
        if p.sub == Sub::Add
            && let Some(old) = &old_text
        {
            let mut all: Vec<String> = old.lines().map(str::to_owned).collect();
            all.extend(
                dirs.iter()
                    .map(|d| String::from_utf8_lossy(d.as_bytes()).into_owned()),
            );
            dirs = cone_dirs(&all)?;
        }
        Some(render(&dirs))
    };
    if new_text == old_text {
        return Ok(ExitCode::SUCCESS);
    }
    store_cone(ws, new_text.as_deref()).await?;

    // A context opened now sees the new cone, which the work tree writers read.
    let fresh = super::clone::open_at(ctx, ws.projection.toplevel().to_owned()).await?;
    let fws = fresh.ws()?;
    let old = old_text.as_deref().map(Sparse::parse);
    let new = new_text.as_deref().map(Sparse::parse);
    if fws.workspace.ws.mode == Mode::Plain {
        let done = reconcile(&fresh, old.as_ref(), new.as_ref()).await?;
        for path in &done.kept {
            eprintln!(
                "warning: asc: left {} in place (it differs from HEAD or is in the way)",
                String::from_utf8_lossy(path.as_bytes())
            );
        }
    } else if let Some(text) = &new_text {
        // The cone is the prefetch profile of a virtual workspace: fetch what it names.
        let rest: Vec<OsString> = text
            .lines()
            .map(|d| OsString::from(format!(":(top){d}")))
            .collect();
        if !rest.is_empty() {
            super::prefetch::run(&fresh, super::prefetch::Args { rest }).await?;
        }
    }
    if let Some(head) = fws.head_commit().await? {
        ext::index_sink()
            .on_head_moved(fws.workspace.ws.id, Some(head), Some(head))
            .await
            .map_err(|e| CliError::Fatal(e.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

async fn dehydrate(ctx: &Ctx, p: &Parsed) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    if ws.workspace.ws.mode == Mode::Plain {
        return Err(CliError::Fatal(
            "asc: sparse dehydrate applies to virtual workspaces; this one is plain".into(),
        ));
    }
    let mut items = p.items.clone();
    if p.stdin {
        items.extend(read_stdin()?);
    }
    let prefix = cwd_prefix(ctx, ws);
    let mut paths: Vec<bytes::Bytes> = Vec::new();
    for item in &items {
        let t = item.trim().replace('\\', "/");
        let t = t.trim_matches('/');
        let joined = match (prefix.is_empty(), t) {
            (true, "" | ".") => continue,
            (true, t) => t.to_owned(),
            (false, "" | ".") => prefix.clone(),
            (false, t) => format!("{prefix}/{t}"),
        };
        let path = RepoPath::new(joined.into_bytes())
            .map_err(|e| CliError::Fatal(format!("asc: '{item}' is not a usable path: {e}")))?;
        paths.push(bytes::Bytes::copy_from_slice(path.as_bytes()));
    }
    if paths.is_empty() {
        return Err(CliError::Fatal("asc: name the paths to dehydrate".into()));
    }
    let cfg = Config::for_home(&ws.home).map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
    if lifecycle::disabled(&|k: &str| std::env::var_os(k))
        || !matches!(lifecycle::probe(&cfg).await, Probe::Up(_))
    {
        return Err(CliError::Fatal("asc: the daemon is not running".into()));
    }
    let mut client = WorkspaceClient::new(connect(&cfg.endpoint));
    let done = client
        .dehydrate(wire::DehydrateRequest {
            ws_id: ws.workspace.ws.id.to_string(),
            pathspec: paths,
        })
        .await
        .map_err(|s| CliError::Fatal(format!("asc: {}", s.message())))?
        .into_inner();
    println!(
        "dehydrated {} files, {} bytes freed",
        done.files, done.bytes_freed
    );
    Ok(ExitCode::SUCCESS)
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let p = parse(&args.rest)?;
    let ws = ctx.ws()?;
    match p.sub {
        Sub::List => {
            let Some(text) = ws.workspace.ws.sparse.as_deref() else {
                return Err(CliError::Fatal("this worktree is not sparse".into()));
            };
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                println!("{}", line.trim());
            }
            Ok(ExitCode::SUCCESS)
        }
        Sub::Dehydrate => dehydrate(ctx, &p).await,
        Sub::Set | Sub::Add | Sub::Disable => change_cone(ctx, &p).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn subcommands() {
        let p = parse(&os(&["set", "--cone", "a", "b/c"])).unwrap();
        assert_eq!(p.sub, Sub::Set);
        assert_eq!(p.items, strs(&["a", "b/c"]));
        assert!(parse(&os(&["set", "--stdin"])).unwrap().stdin);
        assert!(parse(&os(&["list", "a"])).is_err());
        assert!(parse(&os(&["disable", "--stdin"])).is_err());
        assert!(parse(&os(&["dehydrate", "--cone"])).is_err());
        assert!(parse(&os(&["init"])).is_err());
        assert!(parse(&os(&[])).is_err());
        assert_eq!(
            parse(&os(&["add", "--", "-odd"])).unwrap().items,
            strs(&["-odd"])
        );
    }

    #[test]
    fn cones_are_normalised() {
        let dirs = cone_dirs(&strs(&["b/", "/a", "./a", "a/x", ".", "", "b"])).unwrap();
        assert_eq!(render(&dirs), "a\nb\n");
        assert!(cone_dirs(&strs(&["../out"])).is_err());
        assert!(cone_dirs(&strs(&["a//b"])).is_err());
    }
}
