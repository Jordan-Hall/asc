//! The command sandbox of workflow command steps and agent tools (`.asc/workflows/*.toml`,
//! `docs/WORKFLOWS.md`).
//!
//! A [`Sandbox`] runs one allow-listed command over a working copy. What it does itself, with no
//! operating-system feature that needs `unsafe` code:
//!
//! * **No shell.** The command is an argument list. The program is a bare name, resolved against
//!   the configured absolute directories only (never the working copy, never the current
//!   directory), and the whole list must start with one of the allowed prefixes.
//! * **A scrubbed environment.** The child inherits nothing from the server. It gets `PATH`
//!   (the configured directories), `HOME` and the temporary directories (a scratch directory
//!   next to the working copy), a few fixed variables, the names the operator passes through,
//!   and the variables the caller sets (event values, secrets).
//! * **Bounds.** A wall-clock timeout, a cap on the captured output (the rest is read and
//!   dropped so the child never blocks), the working directory must lie inside the root the
//!   caller names, and on Unix the child leads its own process group, which is killed as a whole
//!   when the command ends, times out or is cancelled.
//! * **Secrets stay out of the output.** The caller passes the secret values it set; every
//!   occurrence in the captured output is replaced before the text leaves this module.
//!
//! What it cannot do without help: isolate the file system, the network and the CPU and memory
//! the child uses. An operator who runs commands for users puts a wrapper in front
//! ([`Config::wrapper`], for example `bwrap`, `firejail`, `docker run` or `prlimit`; `{cwd}` in
//! an argument is replaced by the working directory). With no wrapper the sandbox refuses to
//! run anything unless the operator set [`Config::allow_unisolated`], and says so.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken as Cancel;

/// Why a command did not run.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    #[error("command execution is not enabled on this server")]
    Disabled,
    #[error("this server has no isolation wrapper configured and does not allow unisolated commands")]
    NotIsolated,
    #[error("the command is empty or holds a NUL")]
    Empty,
    #[error("the program must be a bare name")]
    NotBare,
    #[error("the command is not on the allow list")]
    NotAllowed,
    #[error("the program {0:?} was not found in the sandbox path")]
    NotFound(String),
    #[error("the working directory is outside the sandbox root")]
    Outside,
    #[error("environment variable {0:?} is not allowed")]
    BadEnv(String),
    #[error("could not start the command: {0}")]
    Spawn(String),
}

/// The operator's settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Allowed argument-list prefixes: `["cargo", "test"]` allows `cargo test -p x`.
    pub allow: Vec<Vec<String>>,
    /// Absolute directories programs are found in (and `PATH` of the child).
    pub path_dirs: Vec<PathBuf>,
    /// Environment variable names copied from the server (for example `CARGO_HOME`).
    pub pass_env: Vec<String>,
    /// The argument list put in front of the command; `{cwd}` is replaced by the working
    /// directory. Empty means no isolation.
    pub wrapper: Vec<String>,
    /// Run commands with no wrapper. Off by default.
    pub allow_unisolated: bool,
    /// Longest the captured output grows, in bytes.
    pub max_output: usize,
    /// Longest any command runs; a caller's timeout is capped to it.
    pub max_wall: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            path_dirs: Vec::new(),
            pass_env: Vec::new(),
            wrapper: Vec::new(),
            allow_unisolated: false,
            max_output: 256 * 1024,
            max_wall: Duration::from_secs(3600),
        }
    }
}

/// One command to run.
#[derive(Clone, Debug)]
pub struct Spec {
    pub argv: Vec<String>,
    /// The working directory; it and its canonical form must lie under `root`.
    pub cwd: PathBuf,
    /// The directory the command may be rooted in (the working copy).
    pub root: PathBuf,
    /// Variables to set. Names must be upper-case `[A-Z0-9_]`; `PATH`, `HOME` and the
    /// temporary-directory names are the sandbox's own and cannot be set here.
    pub env: BTreeMap<String, String>,
    /// Secret values to hide in the output.
    pub secrets: Vec<String>,
    pub timeout: Duration,
}

/// What a command did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The exit code; `None` when the command was killed (timeout, cancel or a signal).
    pub code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    /// The output was cut at the cap.
    pub truncated: bool,
    /// Standard output and standard error in arrival order, secrets masked, lossy UTF-8.
    pub output: String,
    pub elapsed: Duration,
}

impl Outcome {
    /// The command ran to the end and exited with 0.
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.cancelled
    }
}

/// The text that replaces a secret in the output.
pub const MASK: &str = "***";

const OWN_ENV: [&str; 8] = ["PATH", "HOME", "TMPDIR", "TEMP", "TMP", "USERPROFILE", "CI", "LANG"];

/// A command runner.
#[derive(Clone, Debug)]
pub struct Sandbox {
    cfg: Arc<Config>,
}

impl Sandbox {
    pub fn new(cfg: Config) -> Self {
        Self { cfg: Arc::new(cfg) }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Whether any command can run at all.
    pub fn enabled(&self) -> bool {
        !self.cfg.allow.is_empty()
    }

    /// The allowed prefixes, rendered `cargo test`, for the model's tool description.
    pub fn allowed_text(&self) -> Vec<String> {
        self.cfg.allow.iter().map(|a| a.join(" ")).collect()
    }

    /// Whether `argv` starts with an allowed prefix and names a bare program.
    pub fn check(&self, argv: &[String]) -> Result<(), SandboxError> {
        if self.cfg.allow.is_empty() {
            return Err(SandboxError::Disabled);
        }
        let Some(program) = argv.first() else {
            return Err(SandboxError::Empty);
        };
        if program.is_empty() || argv.iter().any(|a| a.contains('\0')) {
            return Err(SandboxError::Empty);
        }
        if program.contains(['/', '\\']) || program.contains(':') || program == "." || program == ".." {
            return Err(SandboxError::NotBare);
        }
        if self
            .cfg
            .allow
            .iter()
            .any(|prefix| !prefix.is_empty() && argv.len() >= prefix.len() && argv[..prefix.len()] == prefix[..])
        {
            Ok(())
        } else {
            Err(SandboxError::NotAllowed)
        }
    }

    /// Resolves the program in the configured directories.
    fn resolve(&self, program: &str) -> Result<PathBuf, SandboxError> {
        for dir in &self.cfg.path_dirs {
            if !dir.is_absolute() {
                continue;
            }
            for name in candidates(program) {
                let p = dir.join(&name);
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
        Err(SandboxError::NotFound(program.to_owned()))
    }

    /// Runs `spec`. `cancel` ends it early.
    pub async fn run(&self, spec: &Spec, cancel: &Cancel) -> Result<Outcome, SandboxError> {
        self.check(&spec.argv)?;
        if self.cfg.wrapper.is_empty() && !self.cfg.allow_unisolated {
            return Err(SandboxError::NotIsolated);
        }
        let cwd = confined(&spec.root, &spec.cwd)?;
        for k in spec.env.keys() {
            let ok = !k.is_empty()
                && k.len() <= 64
                && k.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                && !OWN_ENV.contains(&k.as_str());
            if !ok {
                return Err(SandboxError::BadEnv(k.clone()));
            }
        }
        let program = self.resolve(&spec.argv[0])?;
        let scratch = scratch_dir(&cwd)?;
        let mut argv: Vec<String> = Vec::new();
        let cwd_text = cwd.to_string_lossy().into_owned();
        for w in &self.cfg.wrapper {
            argv.push(w.replace("{cwd}", &cwd_text));
        }
        argv.push(program.to_string_lossy().into_owned());
        argv.extend(spec.argv[1..].iter().cloned());

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(&cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let path = std::env::join_paths(self.cfg.path_dirs.iter().filter(|d| d.is_absolute()))
            .map_err(|e| SandboxError::Spawn(e.to_string()))?;
        cmd.env("PATH", path)
            .env("HOME", &scratch)
            .env("USERPROFILE", &scratch)
            .env("TMPDIR", &scratch)
            .env("TEMP", &scratch)
            .env("TMP", &scratch)
            .env("CI", "1")
            .env("LANG", "C.UTF-8");
        if cfg!(windows) {
            for name in ["SystemRoot", "WINDIR"] {
                if let Ok(v) = std::env::var(name) {
                    cmd.env(name, v);
                }
            }
        }
        for name in &self.cfg.pass_env {
            if let Ok(v) = std::env::var(name) {
                cmd.env(name, v);
            }
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        cmd.process_group(0);

        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|e| SandboxError::Spawn(e.kind().to_string()))?;
        #[cfg(unix)]
        let pgid = child.id();
        let buf = Arc::new(Mutex::new(Capture::new(self.cfg.max_output)));
        let out = child.stdout.take().map(|s| tokio::spawn(pump(s, buf.clone())));
        let err = child.stderr.take().map(|s| tokio::spawn(pump(s, buf.clone())));

        let wall = spec.timeout.min(self.cfg.max_wall);
        let (mut timed_out, mut cancelled) = (false, false);
        let status = tokio::select! {
            s = child.wait() => s.ok(),
            () = tokio::time::sleep(wall) => { timed_out = true; None }
            () = cancel.cancelled() => { cancelled = true; None }
        };
        if status.is_none() {
            let _ = child.start_kill();
        }
        #[cfg(unix)]
        kill_group(pgid);
        let code = match status {
            Some(s) => s.code(),
            None => {
                let _ = child.wait().await;
                None
            }
        };
        // The pumps end when the pipes close; a grandchild that kept them open must not hold us.
        for t in [out, err].into_iter().flatten() {
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
        let _ = std::fs::remove_dir_all(&scratch);
        let cap = std::mem::replace(
            &mut *buf.lock().unwrap_or_else(PoisonError::into_inner),
            Capture::new(0),
        );
        let mut output = String::from_utf8_lossy(&cap.bytes).into_owned();
        for s in spec.secrets.iter().filter(|s| !s.is_empty()) {
            output = output.replace(s.as_str(), MASK);
        }
        Ok(Outcome {
            code,
            timed_out,
            cancelled,
            truncated: cap.truncated,
            output,
            elapsed: started.elapsed(),
        })
    }
}

struct Capture {
    bytes: Vec<u8>,
    max: usize,
    truncated: bool,
}

impl Capture {
    fn new(max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let room = self.max.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
}

async fn pump<R: AsyncRead + Unpin>(mut r: R, buf: Arc<Mutex<Capture>>) {
    let mut chunk = vec![0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(&chunk[..n]),
        }
    }
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    use rustix::process::{Pid, Signal, kill_process_group};
    if let Some(pid) = pid
        && let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw)
    {
        let _ = kill_process_group(pid, Signal::KILL);
    }
}

fn candidates(program: &str) -> Vec<String> {
    if cfg!(windows) {
        let mut v = vec![program.to_owned()];
        for ext in ["exe", "cmd", "bat", "com"] {
            v.push(format!("{program}.{ext}"));
        }
        v
    } else {
        vec![program.to_owned()]
    }
}

/// `cwd` canonicalised, if it lies under the canonical `root`.
fn confined(root: &Path, cwd: &Path) -> Result<PathBuf, SandboxError> {
    if cwd.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(SandboxError::Outside);
    }
    let root = root.canonicalize().map_err(|_| SandboxError::Outside)?;
    let cwd = cwd.canonicalize().map_err(|_| SandboxError::Outside)?;
    if cwd.starts_with(&root) {
        Ok(cwd)
    } else {
        Err(SandboxError::Outside)
    }
}

/// A fresh scratch directory beside the working copy, for `HOME` and the temporary directories.
fn scratch_dir(cwd: &Path) -> Result<PathBuf, SandboxError> {
    let parent = cwd.parent().unwrap_or(cwd);
    let mut raw = [0u8; 8];
    getrandom::fill(&mut raw).map_err(|e| SandboxError::Spawn(e.to_string()))?;
    let name: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let dir = parent.join(format!(".asc-scratch-{name}"));
    std::fs::create_dir(&dir).map_err(|e| SandboxError::Spawn(e.kind().to_string()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test binary doubles as the child program: `--ignored --exact` runs one of the
    /// `child_*` tests below.
    fn exe() -> (PathBuf, String) {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().to_path_buf();
        let name = exe.file_stem().unwrap().to_string_lossy().into_owned();
        (dir, name)
    }

    fn sandbox(extra: &str) -> (Sandbox, String) {
        let (dir, name) = exe();
        let sb = Sandbox::new(Config {
            allow: vec![vec![name.clone(), "--ignored".into(), "--exact".into(), extra.into()]],
            path_dirs: vec![dir],
            allow_unisolated: true,
            max_output: 1000,
            ..Config::default()
        });
        (sb, name)
    }

    fn spec(name: &str, test: &str, root: &Path) -> Spec {
        Spec {
            argv: vec![name.into(), "--ignored".into(), "--exact".into(), test.into(), "--nocapture".into()],
            cwd: root.to_path_buf(),
            root: root.to_path_buf(),
            env: BTreeMap::new(),
            secrets: vec![],
            timeout: Duration::from_secs(60),
        }
    }

    #[test]
    #[ignore = "child of the sandbox tests"]
    fn child_echo() {
        println!("hello-from-child");
        eprintln!("err-from-child");
        let mut keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        keys.sort();
        println!("ENV:{}", keys.join(","));
        println!("TOKEN={}", std::env::var("TOKEN").unwrap_or_default());
        println!("HOME_EXISTS={}", std::env::var("HOME").is_ok_and(|h| Path::new(&h).is_dir()));
        std::process::exit(7);
    }

    #[test]
    #[ignore = "child of the sandbox tests"]
    fn child_sleep() {
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    #[ignore = "child of the sandbox tests"]
    fn child_flood() {
        let line = "x".repeat(1000);
        for _ in 0..2000 {
            println!("{line}");
        }
    }

    #[tokio::test]
    async fn runs_with_a_scrubbed_environment_and_masks_secrets() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let (sb, name) = sandbox("sandbox::tests::child_echo");
        let mut s = spec(&name, "sandbox::tests::child_echo", &work);
        s.argv = vec![name.clone(), "--ignored".into(), "--exact".into(), "sandbox::tests::child_echo".into(), "--nocapture".into()];
        // The allow prefix is the first four entries.
        s.env.insert("TOKEN".into(), "s3cret-value".into());
        s.secrets.push("s3cret-value".into());
        let o = sb.run(&s, &Cancel::new()).await.unwrap();
        assert_eq!(o.code, Some(7), "{}", o.output);
        assert!(!o.success());
        assert!(o.output.contains("hello-from-child"), "{}", o.output);
        assert!(o.output.contains("err-from-child"), "stderr is captured too");
        assert!(o.output.contains(&format!("TOKEN={MASK}")), "{}", o.output);
        assert!(!o.output.contains("s3cret-value"));
        assert!(o.output.contains("HOME_EXISTS=true"));
        let env_line = o.output.lines().find(|l| l.starts_with("ENV:")).unwrap();
        for k in env_line.trim_start_matches("ENV:").split(',') {
            let k = k.to_ascii_uppercase();
            assert!(
                ["PATH", "HOME", "USERPROFILE", "TMPDIR", "TEMP", "TMP", "CI", "LANG", "TOKEN", "SYSTEMROOT", "RUST_BACKTRACE", "WINDIR"].contains(&k.as_str()),
                "unexpected variable {k} in {env_line}"
            );
        }
        // The scratch directory is gone.
        let left: Vec<_> = std::fs::read_dir(root.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "{left:?}");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_command() {
        let root = tempfile::tempdir().unwrap();
        let (sb, name) = sandbox("sandbox::tests::child_sleep");
        let mut s = spec(&name, "sandbox::tests::child_sleep", root.path());
        s.timeout = Duration::from_millis(400);
        let o = sb.run(&s, &Cancel::new()).await.unwrap();
        assert!(o.timed_out);
        assert_eq!(o.code, None);
        assert!(!o.success());
        assert!(o.elapsed < Duration::from_secs(20), "{:?}", o.elapsed);
    }

    #[tokio::test]
    async fn cancelling_stops_the_command() {
        let root = tempfile::tempdir().unwrap();
        let (sb, name) = sandbox("sandbox::tests::child_sleep");
        let s = spec(&name, "sandbox::tests::child_sleep", root.path());
        let cancel = Cancel::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c2.cancel();
        });
        let o = sb.run(&s, &cancel).await.unwrap();
        assert!(o.cancelled && !o.timed_out);
        assert_eq!(o.code, None);
    }

    #[tokio::test]
    async fn output_is_capped_and_the_child_does_not_block() {
        let root = tempfile::tempdir().unwrap();
        let (sb, name) = sandbox("sandbox::tests::child_flood");
        let s = spec(&name, "sandbox::tests::child_flood", root.path());
        let o = sb.run(&s, &Cancel::new()).await.unwrap();
        assert!(o.truncated);
        assert!(o.output.len() <= 1000);
        assert_eq!(o.code, Some(0));
    }

    #[tokio::test]
    async fn refusals() {
        let root = tempfile::tempdir().unwrap();
        let (dir, name) = exe();
        let cfg = Config {
            allow: vec![vec![name.clone(), "--ignored".into()]],
            path_dirs: vec![dir.clone()],
            allow_unisolated: true,
            ..Config::default()
        };
        let sb = Sandbox::new(cfg.clone());
        let base = spec(&name, "sandbox::tests::child_echo", root.path());
        let run = |s: Spec| {
            let sb = sb.clone();
            async move { sb.run(&s, &Cancel::new()).await }
        };
        // Not on the allow list.
        let mut s = base.clone();
        s.argv = vec![name.clone(), "--list".into()];
        assert_eq!(run(s).await.unwrap_err(), SandboxError::NotAllowed);
        // Paths and shells are not bare programs.
        for p in ["/bin/sh", "sh/../x", "C:\\x", "..", "a\\b"] {
            let mut s = base.clone();
            s.argv = vec![p.into(), "--ignored".into()];
            assert_eq!(run(s).await.unwrap_err(), SandboxError::NotBare, "{p}");
        }
        // NUL and empty.
        let mut s = base.clone();
        s.argv = vec![];
        assert_eq!(run(s).await.unwrap_err(), SandboxError::Empty);
        let mut s = base.clone();
        s.argv = vec![name.clone(), "--ignored\0".into()];
        assert_eq!(run(s).await.unwrap_err(), SandboxError::Empty);
        // A working directory outside the root, or climbing out of it.
        let other = tempfile::tempdir().unwrap();
        let mut s = base.clone();
        s.cwd = other.path().to_path_buf();
        assert_eq!(run(s).await.unwrap_err(), SandboxError::Outside);
        let mut s = base.clone();
        s.cwd = root.path().join("..");
        assert_eq!(run(s).await.unwrap_err(), SandboxError::Outside);
        // Environment names the sandbox owns, or that are not shaped like variables.
        for k in ["PATH", "HOME", "lower", "A=B", ""] {
            let mut s = base.clone();
            s.env.insert(k.into(), "x".into());
            assert!(matches!(run(s).await.unwrap_err(), SandboxError::BadEnv(_)), "{k}");
        }
        // A program that is not in the sandbox path.
        let sb2 = Sandbox::new(Config {
            allow: vec![vec!["definitely-not-installed".into()]],
            path_dirs: vec![dir],
            allow_unisolated: true,
            ..Config::default()
        });
        let mut s = base.clone();
        s.argv = vec!["definitely-not-installed".into()];
        assert!(matches!(sb2.run(&s, &Cancel::new()).await.unwrap_err(), SandboxError::NotFound(_)));
        // Without a wrapper and without the opt-in nothing runs.
        let strict = Sandbox::new(Config { allow_unisolated: false, ..cfg });
        assert_eq!(strict.run(&base, &Cancel::new()).await.unwrap_err(), SandboxError::NotIsolated);
        // Nothing allowed at all.
        let off = Sandbox::new(Config::default());
        assert!(!off.enabled());
        assert_eq!(off.run(&base, &Cancel::new()).await.unwrap_err(), SandboxError::Disabled);
    }

    #[tokio::test]
    async fn a_program_in_the_working_copy_is_never_found() {
        let root = tempfile::tempdir().unwrap();
        let (dir, _) = exe();
        std::fs::write(root.path().join("evil"), b"#!/bin/sh\n").unwrap();
        let sb = Sandbox::new(Config {
            allow: vec![vec!["evil".into()]],
            path_dirs: vec![dir],
            allow_unisolated: true,
            ..Config::default()
        });
        let mut s = spec("evil", "x", root.path());
        s.argv = vec!["evil".into()];
        assert!(matches!(sb.run(&s, &Cancel::new()).await.unwrap_err(), SandboxError::NotFound(_)));
    }

    #[test]
    fn allow_prefixes_match_whole_arguments() {
        let sb = Sandbox::new(Config {
            allow: vec![vec!["cargo".into(), "test".into()], vec!["npm".into(), "run".into(), "lint".into()]],
            ..Config::default()
        });
        let a = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        assert!(sb.check(&a(&["cargo", "test"])).is_ok());
        assert!(sb.check(&a(&["cargo", "test", "-p", "x"])).is_ok());
        assert!(sb.check(&a(&["cargo", "testing"])).is_err());
        assert!(sb.check(&a(&["cargo"])).is_err());
        assert!(sb.check(&a(&["cargo", "install", "x"])).is_err());
        assert!(sb.check(&a(&["npm", "run", "lint"])).is_ok());
        assert!(sb.check(&a(&["npm", "run", "build"])).is_err());
        assert_eq!(sb.allowed_text(), ["cargo test", "npm run lint"]);
    }
}
