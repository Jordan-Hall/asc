//! `asc cache run` (docs/designs/build-cache.md §4.4): any command as a cached action. The key is
//! the command line, the named environment variables, the platform and the content of the inputs
//! (§1); a hit restores the outputs and replays the output and exit status without running
//! anything, a miss runs the command and stores what it wrote.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use asc_buildcache::{Action, ActionKey, Origin, Store, hex};

use super::remote::{self, Fetched, Mode};
use super::{io_err, max_size, open_store};
use crate::cli::common::{CliError, Ctx, Result};
use crate::cli::inputs;

/// The tool name of `asc cache run` entries.
pub const TOOL: &str = "run";
/// Captured output kept per stream; a command that writes more runs uncached.
const MAX_LOG: usize = 16 << 20;

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// A name for the action, part of its key and shown in `asc cache ls`.
    #[arg(long)]
    name: String,
    /// Paths whose content keys the action (default: the current directory). Ignored files
    /// are left out, so build outputs below an input do not change the key.
    #[arg(long = "inputs", num_args = 1..)]
    inputs: Vec<OsString>,
    /// Files or directories the command writes, restored on a hit.
    #[arg(long = "outputs", num_args = 1..)]
    outputs: Vec<OsString>,
    /// Environment variables whose values key the action.
    #[arg(long = "env")]
    env: Vec<String>,
    /// Neither fetch from nor queue for the server.
    #[arg(long = "no-remote")]
    no_remote: bool,
    /// No `cache hit` note on stderr.
    #[arg(long, short = 'q')]
    quiet: bool,
    /// The command and its arguments.
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

/// Where the entry was used: the work tree, and the CI workflow when there is one.
pub(crate) fn origin(root: &Path) -> Origin {
    let workflow = ["ASC_WORKFLOW", "GITHUB_WORKFLOW", "CI_JOB_NAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()));
    Origin {
        worktree: Some(root.to_string_lossy().replace('\\', "/")),
        workflow,
    }
}

/// The action key: the name, the directory it runs in, the command line, the named variables,
/// the platform and the inputs digest.
fn key(a: &RunArgs, prefix: &str, digest: &[u8; 32]) -> ActionKey {
    let mut b = ActionKey::builder(TOOL)
        .part("name", a.name.as_bytes())
        .part("cwd", prefix.as_bytes())
        .part(
            "platform",
            format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH).as_bytes(),
        );
    for arg in &a.command {
        b = b.part("arg", arg.as_encoded_bytes());
    }
    let mut names: Vec<&String> = a.env.iter().collect();
    names.sort();
    names.dedup();
    for name in names {
        b = match std::env::var_os(name) {
            Some(v) => b
                .part("env", name.as_bytes())
                .part("value", v.as_encoded_bytes()),
            None => b.part("unset", name.as_bytes()),
        };
    }
    b.inputs(digest).finish()
}

/// Starts `asc cache push` in the background, unless one started in the last minute.
pub(crate) fn spawn_push(store: &Store, cwd: &Path) {
    let stamp = store.root().join("outbox").join(".push");
    let fresh = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < 60);
    if fresh || std::fs::write(&stamp, b"").is_err() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.args(["cache", "push"])
        .current_dir(cwd)
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
    // Best effort: what is not pushed now waits in the outbox for the next push.
    let _ = cmd.spawn();
}

/// `program` as a path: on Windows a bare name is searched on `PATH` with `PATHEXT`, so `npm`
/// finds `npm.cmd` (whose arguments the standard library then quotes or refuses).
fn program(name: &OsString) -> OsString {
    #[cfg(windows)]
    {
        let p = Path::new(name);
        if p.components().count() == 1 && p.extension().is_none() {
            let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
                for ext in exts.split(';').filter(|e| !e.is_empty()) {
                    let mut file = name.clone();
                    file.push(ext);
                    let candidate = dir.join(&file);
                    if candidate.is_file() {
                        return candidate.into_os_string();
                    }
                }
            }
        }
    }
    name.clone()
}

/// Copies `from` to `to` as it arrives and keeps up to [`MAX_LOG`] bytes; `None` past that.
fn tee(mut from: impl Read, mut to: impl Write) -> Option<Vec<u8>> {
    let mut kept = Some(Vec::new());
    let mut buf = [0u8; 16 << 10];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let _ = to.write_all(&buf[..n]);
        let _ = to.flush();
        if let Some(k) = &mut kept {
            if k.len() + n > MAX_LOG {
                kept = None;
            } else {
                k.extend_from_slice(&buf[..n]);
            }
        }
    }
    kept
}

/// What the command did.
struct Ran {
    /// `None` when it was ended by a signal.
    code: Option<i32>,
    stdout: Option<Vec<u8>>,
    stderr: Option<Vec<u8>>,
}

fn run_command(a: &RunArgs, cwd: &Path) -> Result<Ran> {
    let mut child = Command::new(program(&a.command[0]))
        .args(&a.command[1..])
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            CliError::Fatal(format!(
                "cannot run '{}': {e}",
                a.command[0].to_string_lossy()
            ))
        })?;
    let out = child.stdout.take().expect("piped");
    let err = child.stderr.take().expect("piped");
    let (stdout, stderr) = std::thread::scope(|s| {
        let o = s.spawn(|| tee(out, std::io::stdout()));
        let e = s.spawn(|| tee(err, std::io::stderr()));
        (o.join().ok().flatten(), e.join().ok().flatten())
    });
    let status = child.wait()?;
    Ok(Ran {
        code: status.code(),
        stdout,
        stderr,
    })
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn replay(store: &Store, blob: Option<&str>, to: &mut dyn Write) -> Result<()> {
    if let Some(b) = blob {
        store.copy_blob_to(b, to).map_err(io_err)?;
        to.flush()?;
    }
    Ok(())
}

pub async fn run(ctx: &Ctx, a: RunArgs) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel().to_owned();
    let prefix = crate::cli::add::cwd_prefix(ctx, ws);
    let outputs: Vec<String> = a
        .outputs
        .iter()
        .map(|p| {
            let rel = inputs::normalize(&root, &prefix, p)?;
            if rel.is_empty() {
                return Err(CliError::Usage(
                    "error: an output cannot be the repository root".into(),
                ));
            }
            String::from_utf8(rel)
                .map_err(|_| CliError::Usage("error: an output path is not UTF-8".into()))
        })
        .collect::<Result<_>>()?;
    let digest = inputs::compute(ctx, ws, &a.inputs, false).await?.digest;
    let key = key(&a, &prefix, &digest);
    let store = open_store(ctx)?;
    let origin = origin(&root);
    let mode = if a.no_remote {
        Mode::Off
    } else {
        remote::mode(ctx)?
    };

    let mut hit = store.lookup(TOOL, &key, &origin).map_err(io_err)?;
    if hit.is_none() && mode != Mode::Off {
        hit = fetch(ctx, &store, &key).await;
    }
    if let Some(action) = hit
        && store.restore(&action, &root).map_err(io_err)?
    {
        if !a.quiet {
            eprintln!("asc cache: hit {} ({})", a.name, &key.to_string()[..12]);
        }
        replay(&store, action.stdout.as_deref(), &mut std::io::stdout())?;
        replay(&store, action.stderr.as_deref(), &mut std::io::stderr())?;
        if mode == Mode::ReadWrite {
            store.queue_push(&key).map_err(io_err)?;
            spawn_push(&store, &ctx.cwd);
        }
        return Ok(exit(action.exit));
    }

    let ran = run_command(&a, &ctx.cwd)?;
    let Some(code) = ran.code else {
        return Ok(ExitCode::FAILURE);
    };
    // Only a success is kept: a failure is worth running again.
    if code != 0 {
        return Ok(exit(code));
    }
    let (Some(stdout), Some(stderr)) = (ran.stdout, ran.stderr) else {
        return Ok(exit(code));
    };
    let present: Vec<String> = outputs
        .into_iter()
        .filter(|o| std::fs::symlink_metadata(root.join(o)).is_ok())
        .collect();
    let mut action = Action::new(TOOL, &a.name, origin);
    action.outputs = store.put_outputs(&root, &present).map_err(io_err)?;
    for (bytes, slot) in [(&stdout, &mut action.stdout), (&stderr, &mut action.stderr)] {
        if !bytes.is_empty() {
            *slot = Some(hex(&store.put_blob(bytes).map_err(io_err)?.0));
        }
    }
    store.put_action(&key, &action).map_err(io_err)?;
    if mode == Mode::ReadWrite {
        store.queue_push(&key).map_err(io_err)?;
        spawn_push(&store, &ctx.cwd);
    }
    if let Ok(max) = max_size(ctx) {
        let _ = store.maybe_gc(max);
    }
    Ok(ExitCode::SUCCESS)
}

/// A remote hit for `key`, or `None` on a miss, without a server, or when the server cannot be
/// reached (the build goes on as on a miss).
async fn fetch(ctx: &Ctx, store: &Store, key: &ActionKey) -> Option<Action> {
    let api = crate::cli::ci::cache::http_api(ctx, None).await.ok()?;
    match remote::fetch(&api, store, key).await {
        Ok(Fetched::Stored(_)) => store.get_action(key).ok().flatten(),
        Ok(Fetched::Refused(m)) => {
            eprintln!("warning: build cache artefact refused: {m}");
            None
        }
        Ok(Fetched::Missing | Fetched::TooLarge) => None,
        Err(e) => {
            eprintln!("warning: build cache server: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(clap::Parser)]
    struct T {
        #[command(flatten)]
        a: RunArgs,
    }

    fn args(v: &[&str]) -> RunArgs {
        T::try_parse_from(std::iter::once("t").chain(v.iter().copied()))
            .unwrap()
            .a
    }

    #[test]
    fn the_key_covers_name_cwd_command_and_named_variables() {
        let d = [1u8; 32];
        let base = key(&args(&["--name", "a", "--", "make", "x"]), "", &d);
        assert_eq!(
            base,
            key(&args(&["--name", "a", "--", "make", "x"]), "", &d)
        );
        for other in [
            key(&args(&["--name", "b", "--", "make", "x"]), "", &d),
            key(&args(&["--name", "a", "--", "make", "y"]), "", &d),
            key(&args(&["--name", "a", "--", "make", "x"]), "sub", &d),
            key(&args(&["--name", "a", "--", "make", "x"]), "", &[2u8; 32]),
            key(&args(&["--name", "a", "--", "makex"]), "", &d),
        ] {
            assert_ne!(base, other);
        }
        // An unset variable and an empty one differ.
        let unset = key(
            &args(&["--name", "a", "--env", "ASC_TEST_UNSET_VAR", "--", "x"]),
            "",
            &d,
        );
        assert_ne!(unset, key(&args(&["--name", "a", "--", "x"]), "", &d));
    }

    #[test]
    fn tee_keeps_up_to_the_limit() {
        let mut seen = Vec::new();
        assert_eq!(tee(&b"hello"[..], &mut seen).unwrap(), b"hello");
        assert_eq!(seen, b"hello");
        let big = vec![0u8; MAX_LOG + 1];
        let mut sink = Vec::new();
        assert!(tee(&big[..], &mut sink).is_none());
        assert_eq!(sink.len(), big.len(), "still copied through");
    }
}
