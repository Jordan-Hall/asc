//! `asc stack exec [--revset <r>] -- <cmd>` (SPEC §20.7, v1.x): runs a command on each change
//! of the current stack, bottom to top, in one ephemeral plain worktree. A result is cached by
//! the change's `TreeId` and the command line, so a change whose tree did not move (a restack
//! over disjoint paths, a message edit) is not run again.
//!
//! The command is run directly (no shell) with a null stdin. Its output is captured up to
//! [`OUT_CAP`] bytes per stream and it is killed after [`TIMEOUT`]. A killed or signalled run
//! is reported and never cached. The worktree lives under `$ASC_HOME/tmp` and is removed on every
//! path out. Exit 0 when every command exits 0, 1 when any fails, asc never prompts.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use asc_core::id::{ChangesetId, TreeId};
use asc_engine::change::ChangeRows;
use clap::Parser;
use serde::{Deserialize, Serialize};

use super::sl::{Local, clean, fatal, stack_err};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::output::Output;
use crate::cli::pager::PagerWant;
use crate::stack::json::{VERSION, line};

/// Most changes one call runs the command on.
const MAX_CHANGES: usize = 1_000;
/// Most bytes kept per output stream of one run.
const OUT_CAP: usize = 64 * 1024;
/// Longest one run may take.
const TIMEOUT: Duration = Duration::from_secs(3600);
/// Largest cache file read back.
const CACHE_MAX: u64 = 4 * OUT_CAP as u64;

#[derive(Parser, Debug)]
#[command(name = "exec", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Only the changes in this revset (SPEC §20.3 subset).
    #[arg(long = "revset")]
    pub revset: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, num_args = 1..)]
    pub cmd: Vec<OsString>,
}

super::info::sub!(EXEC, "exec", Args, run);

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Cached {
    exit: i32,
    stdout: String,
    stderr: String,
    truncated: bool,
}

#[derive(Serialize)]
struct Row {
    change: String,
    commit: String,
    tree: String,
    exit: Option<i32>,
    cached: bool,
    timed_out: bool,
    truncated: bool,
    stdout: String,
    stderr: String,
}

#[derive(Serialize)]
struct Report {
    version: u32,
    ok: bool,
    results: Vec<Row>,
}

/// Cache key of a command line: the hash of its length-prefixed words.
fn cmd_key(cmd: &[OsString]) -> String {
    let mut buf = b"asc stack exec v1\0".to_vec();
    for w in cmd {
        let b = w.to_string_lossy();
        buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
        buf.extend_from_slice(b.as_bytes());
    }
    TreeId::of(&buf).to_string()
}

fn cache_file(dir: &Path, tree: TreeId) -> PathBuf {
    dir.join(format!("{tree}.json"))
}

fn read_cache(dir: &Path, tree: TreeId) -> Option<Cached> {
    let p = cache_file(dir, tree);
    let meta = std::fs::symlink_metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > CACHE_MAX {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(&p).ok()?.take(CACHE_MAX + 1).read_to_end(&mut buf).ok()?;
    let c: Cached = serde_json::from_slice(&buf).ok()?;
    (c.stdout.len() <= OUT_CAP * 4 && c.stderr.len() <= OUT_CAP * 4).then_some(c)
}

fn write_cache(dir: &Path, tree: TreeId, c: &Cached) {
    // Best effort: a cache that cannot be written only costs a rerun.
    let Ok(bytes) = serde_json::to_vec(c) else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, cache_file(dir, tree)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Reads at most [`OUT_CAP`] bytes and drains the rest, so the child never blocks on a pipe.
fn capture<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut over = false;
        let mut chunk = [0u8; 8192];
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let room = OUT_CAP - kept.len();
            if n > room {
                over = true;
            }
            kept.extend_from_slice(&chunk[..n.min(room)]);
        }
        (kept, over)
    })
}

struct Run {
    exit: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
    truncated: bool,
}

fn run_command(ctx: &Ctx, cmd: &[OsString], cwd: &Path) -> Result<Run> {
    let mut child = ctx
        .child(&cmd[0])?
        .args(&cmd[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CliError::Fatal(format!("asc: cannot run '{}': {e}", clean(&cmd[0].to_string_lossy()))))?;
    let out = capture(child.stdout.take().ok_or_else(|| fatal("no stdout pipe"))?);
    let err = capture(child.stderr.take().ok_or_else(|| fatal("no stderr pipe"))?);
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > TIMEOUT {
            timed_out = true;
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (o, o_over) = out.join().unwrap_or_default();
    let (e, e_over) = err.join().unwrap_or_default();
    Ok(Run {
        exit: status.code(),
        timed_out,
        stdout: String::from_utf8_lossy(&o).into_owned(),
        stderr: String::from_utf8_lossy(&e).into_owned(),
        truncated: o_over || e_over,
    })
}

/// Runs `asc <args>` in `cwd`, failing with its stderr.
fn asc_child(ctx: &Ctx, args: &[&OsString], cwd: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| CliError::Fatal(format!("asc: cannot find asc: {e}")))?;
    let mut c = ctx.child(exe)?;
    c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let out = c.output()?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr);
    Err(CliError::Fatal(format!("asc: stack exec: worktree step failed: {}", clean(msg.trim()))))
}

/// The changes to run on: the current stack bottom to top, narrowed by `--revset`.
async fn select(l: &Local<'_>, revset: Option<&str>) -> Result<Vec<ChangesetId>> {
    let st = l.stacks().await?;
    let head = st
        .current_head()
        .ok_or_else(|| CliError::Raw("asc: stack exec: HEAD is not on a change".into(), 1))?;
    let stack = st.stack_of(head).await.map_err(stack_err)?;
    let filter: Option<BTreeSet<ChangesetId>> = match revset {
        Some(r) => Some(asc_engine::stack::revset::eval_str(&st, r).await.map_err(stack_err)?),
        None => None,
    };
    let picked: Vec<ChangesetId> =
        stack.changes.iter().copied().filter(|c| filter.as_ref().is_none_or(|f| f.contains(c))).collect();
    if picked.len() > MAX_CHANGES {
        return Err(fatal(format!("stack exec: more than {MAX_CHANGES} changes")));
    }
    Ok(picked)
}

async fn go(ctx: &Ctx, l: &Local<'_>, a: &Args, picked: &[ChangesetId], wt: &Path) -> Result<Vec<Row>> {
    let cache = l.ws.home.cache().join("stack-exec").join(cmd_key(&a.cmd));
    let mut rows = Vec::new();
    let mut ready = false;
    for &cs in picked {
        let c = l.ws.engine.get_changeset(cs).await.map_err(fatal)?;
        let tree = c.root;
        let commit = l.git_id(cs).await?;
        let change = match l.rows.get(cs).await.map_err(fatal)? {
            Some(r) => r.change.to_string(),
            None => String::new(),
        };
        let (cached, r) = match read_cache(&cache, tree) {
            Some(c) => (true, Run { exit: Some(c.exit), timed_out: false, stdout: c.stdout, stderr: c.stderr, truncated: c.truncated }),
            None => {
                let sha = OsString::from(&commit);
                if ready {
                    asc_child(ctx, &[&"switch".into(), &"--detach".into(), &sha], Some(wt))?;
                } else {
                    let path = wt.as_os_str().to_owned();
                    asc_child(
                        ctx,
                        &[
                            &"worktree".into(), &"add".into(), &"--detach".into(), &"--ephemeral".into(),
                            &"--ttl".into(), &"1h".into(), &path, &sha,
                        ],
                        None,
                    )?;
                    ready = true;
                }
                let r = run_command(ctx, &a.cmd, wt)?;
                if let (Some(code), false) = (r.exit, r.timed_out) {
                    write_cache(
                        &cache,
                        tree,
                        &Cached { exit: code, stdout: r.stdout.clone(), stderr: r.stderr.clone(), truncated: r.truncated },
                    );
                }
                (false, r)
            }
        };
        rows.push(Row {
            change,
            commit,
            tree: tree.to_string(),
            exit: r.exit,
            cached,
            timed_out: r.timed_out,
            truncated: r.truncated,
            stdout: r.stdout,
            stderr: r.stderr,
        });
    }
    Ok(rows)
}

fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\n' && c != '\t' { '?' } else { c }).collect()
}

fn render(rows: &[Row], subjects: &[String]) -> String {
    let mut out = String::new();
    for (r, subject) in rows.iter().zip(subjects) {
        let state = match (r.exit, r.timed_out) {
            (_, true) => "timeout".to_owned(),
            (Some(0), _) => "ok".to_owned(),
            (Some(n), _) => format!("fail({n})"),
            (None, _) => "signalled".to_owned(),
        };
        let short: String = r.change.chars().take(12).collect();
        let short = if short.is_empty() { r.commit.chars().take(7).collect() } else { short };
        out.push_str(&format!(
            "{state:<10} {short} {}{}\n",
            clean(subject),
            if r.cached { " (cached)" } else { "" }
        ));
        if r.exit != Some(0) {
            for s in [&r.stdout, &r.stderr] {
                for l in printable(s).lines() {
                    out.push_str("    ");
                    out.push_str(l);
                    out.push('\n');
                }
            }
        }
    }
    out
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let l = Local::open(ctx).await?;
    let picked = select(&l, a.revset.as_deref()).await?;
    let mut subjects = Vec::new();
    for &cs in &picked {
        let c = l.ws.engine.get_changeset(cs).await.map_err(fatal)?;
        let first = c.message.split(|&b| b == b'\n').next().unwrap_or_default();
        subjects.push(String::from_utf8_lossy(first).chars().take(200).collect::<String>());
    }
    // Outside the asc home: a worktree cannot live inside it (SPEC §9.3).
    let holder = std::env::temp_dir().join(format!("asc-stack-exec-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&holder)?;
    let wt = holder.join("wt");
    let result = go(ctx, &l, &a, &picked, &wt).await;
    if wt.exists() {
        let path = wt.as_os_str().to_owned();
        let _ = asc_child(ctx, &[&"worktree".into(), &"remove".into(), &"-f".into(), &path], None);
    }
    let _ = std::fs::remove_dir_all(&holder);
    let rows = result?;
    let ok = rows.iter().all(|r| r.exit == Some(0) && !r.timed_out);
    let mut out = Output::open(ctx, "stack", PagerWant::Never)?;
    if a.json {
        out.write_all(line(&Report { version: VERSION, ok, results: rows }).map_err(fatal)?.as_bytes())?;
    } else {
        out.write_all(render(&rows, &subjects).as_bytes())?;
    }
    out.finish()?;
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_depends_on_every_word_and_its_boundaries() {
        let k = |w: &[&str]| cmd_key(&w.iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(k(&["a", "b"]), k(&["a", "b"]));
        assert_ne!(k(&["a", "b"]), k(&["ab"]));
        assert_ne!(k(&["a"]), k(&["a", ""]));
    }

    #[test]
    fn cache_round_trips_and_rejects_oversize_and_garbage() {
        let d = tempfile::tempdir().unwrap();
        let t = TreeId::of(b"x");
        assert!(read_cache(d.path(), t).is_none());
        let c = Cached { exit: 3, stdout: "o".into(), stderr: "e".into(), truncated: false };
        write_cache(d.path(), t, &c);
        assert_eq!(read_cache(d.path(), t).unwrap().exit, 3);
        std::fs::write(cache_file(d.path(), t), b"not json").unwrap();
        assert!(read_cache(d.path(), t).is_none());
        std::fs::write(cache_file(d.path(), t), vec![b' '; CACHE_MAX as usize + 1]).unwrap();
        assert!(read_cache(d.path(), t).is_none());
    }
}
