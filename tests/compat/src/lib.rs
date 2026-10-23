//! The git compatibility runner (SPEC §17.1, PLAN U25).
//!
//! A case is a shell script (`scenarios/*.t`) that runs `git` through `PATH`. The runner runs it
//! twice on identical fixtures with a fixed environment: once with real git, once with the asc
//! shim installed as `git` first on `PATH` in an asc workspace (the asc side's home config sets
//! `asc.shim.init = asc`, so the scenario's `git init` makes the workspace). It then compares
//! stdout, stderr and the exit code of the script, followed by the post-state: `for-each-ref`,
//! `rev-parse HEAD`, `status --porcelain=v2 -z`, `ls-files -s`, the gitdir state files and the
//! work tree bytes.
//!
//! Normalisation is limited to temp paths and progress lines. Ids are never normalised.
//! Every other difference must be listed in `allowed.toml` with a reason and a SPEC section.
//! The asc side fails the case if it fell through to real git (`ASC_SHIM_TRACE`).
//!
//! Scenario files may start with `# compat: asc-only`; such a scenario runs on the asc side only
//! with `asc` on `PATH`, and passes when its script exits 0 (it asserts for itself).
#![allow(dead_code)] // The harness is included by the test crate; not every item is used.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;

/// Wall-clock limit for one scenario script on one side.
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest work tree file the runner reads for the post-state comparison.
pub const MAX_TREE_FILE: u64 = 8 * 1024 * 1024;
/// Most work tree files the runner reads for the post-state comparison.
pub const MAX_TREE_FILES: usize = 20_000;
/// Most diff lines shown per differing section.
pub const MAX_DIFF_LINES: usize = 60;
/// Largest stream the runner keeps from one command.
pub const MAX_STREAM: usize = 64 * 1024 * 1024;

const GIT_EXE: &str = if cfg!(windows) { "git.exe" } else { "git" };
const ASC_EXE: &str = if cfg!(windows) { "asc.exe" } else { "asc" };

/// Which program is `git` for a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Git,
    Asc,
}

impl Side {
    fn dir(self) -> &'static str {
        match self {
            Side::Git => "git",
            Side::Asc => "asc",
        }
    }
}

/// One allowed difference (`allowed.toml`).
#[derive(Debug, Deserialize)]
pub struct Allow {
    /// A scenario name, or `*`.
    pub scenario: String,
    /// A section name (`stdout`, `stderr`, `exit`, `post:...`), `*`, or a prefix ending in `*`.
    pub section: String,
    /// Lines matching this regex are dropped from both sides before comparing.
    pub drop: String,
    pub reason: String,
    pub spec: String,
}

#[derive(Debug, Deserialize)]
struct AllowFile {
    #[serde(default)]
    allow: Vec<Allow>,
}

/// What one side produced.
#[derive(Debug, Default)]
pub struct Capture {
    /// Section name to normalised bytes, in comparison order.
    pub sections: BTreeMap<String, Vec<u8>>,
    /// The `ASC_SHIM_TRACE` lines: fall-throughs to real git.
    pub fall_through: Vec<String>,
    /// The script's own exit code.
    pub exit: i32,
}

/// Shared, resolved tools.
pub struct Tools {
    pub asc: PathBuf,
    pub real_git: PathBuf,
    pub sh: PathBuf,
    /// A directory holding `git` and `asc` links to the asc binary.
    pub shim_dir: PathBuf,
    /// `tests/compat`.
    pub suite: PathBuf,
}

/// Locates `sh`: `ASC_COMPAT_SH`, then `PATH`, then Git for Windows' `usr/bin` above real git.
fn locate_sh(real_git: &Path) -> Result<PathBuf, String> {
    let name = if cfg!(windows) { "sh.exe" } else { "sh" };
    if let Some(p) = std::env::var_os("ASC_COMPAT_SH").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join(name);
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    for dir in real_git.ancestors().skip(1) {
        let cand = dir.join("usr").join("bin").join(name);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err("no `sh` found (set ASC_COMPAT_SH)".into())
}

fn link_binary(from: &Path, to: &Path) -> Result<(), String> {
    let _ = fs::remove_file(to);
    if fs::hard_link(from, to).is_err() && !to.exists() {
        fs::copy(from, to).map_err(|e| format!("cannot install {}: {e}", to.display()))?;
    }
    Ok(())
}

impl Tools {
    /// The tools for this test process; `asc` is the built binary.
    pub fn get(asc: &'static str) -> &'static Result<Tools, String> {
        static TOOLS: OnceLock<Result<Tools, String>> = OnceLock::new();
        TOOLS.get_or_init(|| {
            let asc = PathBuf::from(asc);
            let real_git = asc_git::realgit::RealGit::resolve(None, None)
                .map_err(|e| format!("real git: {e}"))?
                .path()
                .to_owned();
            let sh = locate_sh(&real_git)?;
            let shim_dir = asc
                .parent()
                .ok_or("asc binary has no parent")?
                .join("asc-compat-shim")
                .join(std::process::id().to_string());
            fs::create_dir_all(&shim_dir).map_err(|e| e.to_string())?;
            link_binary(&asc, &shim_dir.join(GIT_EXE))?;
            link_binary(&asc, &shim_dir.join(ASC_EXE))?;
            let suite = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/compat");
            Ok(Tools {
                asc,
                real_git,
                sh,
                shim_dir,
                suite,
            })
        })
    }

    /// The scenario files, by name without `.t`.
    pub fn scenario_names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.suite.join("scenarios"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".t").map(str::to_owned)
            })
            .collect();
        names.sort();
        names
    }

    /// Reads `allowed.toml`, rejecting an entry that lacks a reason or a SPEC section.
    pub fn allowed(&self) -> Result<Vec<Allow>, String> {
        let text = fs::read_to_string(self.suite.join("allowed.toml"))
            .map_err(|e| format!("allowed.toml: {e}"))?;
        let file: AllowFile = toml::from_str(&text).map_err(|e| format!("allowed.toml: {e}"))?;
        for a in &file.allow {
            if a.reason.trim().is_empty()
                || !a.spec.contains('§')
                || !a.spec.chars().any(|c| c.is_ascii_digit())
            {
                return Err(format!(
                    "allowed.toml: entry {a:?} needs a reason and a SPEC section (§N.N)"
                ));
            }
            Regex::new(&a.drop)
                .map_err(|e| format!("allowed.toml: bad regex {:?}: {e}", a.drop))?;
        }
        Ok(file.allow)
    }

    /// The script text of a scenario, with LF line ends.
    pub fn scenario(&self, name: &str) -> Result<String, String> {
        let path = self.suite.join("scenarios").join(format!("{name}.t"));
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(text.replace("\r\n", "\n"))
    }

    /// Runs a scenario on both sides and compares them.
    pub fn check_scenario(&self, name: &str) -> Result<(), String> {
        let script = self.scenario(name)?;
        self.check_script(name, &script)
    }

    /// Runs `script` as case `name` on both sides (or the asc side, for `asc-only`) and compares.
    pub fn check_script(&self, name: &str, script: &str) -> Result<(), String> {
        let asc_only = script
            .lines()
            .take(5)
            .any(|l| l.trim() == "# compat: asc-only");
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let base = tmp.path().to_owned();
        // ASC_COMPAT_KEEP=1 keeps the fixtures for inspection.
        let _tmp = if std::env::var_os("ASC_COMPAT_KEEP").is_some() {
            eprintln!("compat fixtures kept in {}", base.display());
            let _ = tmp.keep();
            None
        } else {
            Some(tmp)
        };
        let asc = self.run_side(Side::Asc, script, &base, asc_only)?;
        if !asc.fall_through.is_empty() {
            return Err(format!(
                "{name}: the asc side fell through to real git inside the scenario:\n  {}",
                asc.fall_through.join("\n  ")
            ));
        }
        if asc_only {
            return if asc.exit == 0 {
                Ok(())
            } else {
                Err(format!(
                    "{name}: asc-only scenario exited {}\n{}",
                    asc.exit,
                    text(asc.sections.get("stderr"))
                ))
            };
        }
        let git = self.run_side(Side::Git, script, &base, false)?;
        compare(name, &git, &asc, &self.allowed()?)
    }

    /// Runs the script on one side in `<base>/<side>/work` and captures the result.
    pub fn run_side(
        &self,
        side: Side,
        script: &str,
        base: &Path,
        asc_only: bool,
    ) -> Result<Capture, String> {
        let root = base.join(side.dir());
        let (work, home, asc_home) = (root.join("work"), root.join("home"), root.join("asc-home"));
        for d in [&work, &home] {
            fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let cfg_dir = match side {
            Side::Git => "home",
            Side::Asc => "home-asc",
        };
        let cfg = fs::read(self.suite.join(cfg_dir).join(".gitconfig"))
            .map_err(|e| format!("{cfg_dir}/.gitconfig: {e}"))?;
        fs::write(home.join(".gitconfig"), cfg).map_err(|e| e.to_string())?;
        let trace = root.join("shim-trace.log");

        let git_path: PathBuf = match side {
            Side::Git => self.real_git.clone(),
            Side::Asc => self.shim_dir.join(GIT_EXE),
        };
        let mut path_dirs: Vec<PathBuf> = Vec::new();
        match side {
            Side::Asc => path_dirs.push(self.shim_dir.clone()),
            Side::Git => path_dirs.push(
                self.real_git
                    .parent()
                    .map(Path::to_owned)
                    .unwrap_or_default(),
            ),
        }
        if let Some(sh_dir) = self.sh.parent() {
            path_dirs.push(sh_dir.to_owned());
        }
        if let Some(rest) = std::env::var_os("PATH") {
            path_dirs.extend(std::env::split_paths(&rest));
        }
        let path = std::env::join_paths(&path_dirs).map_err(|e| e.to_string())?;

        let mut cmd = Command::new(&self.sh);
        scrub(&mut cmd);
        fixed_env(&mut cmd, &home, &path);
        if side == Side::Asc {
            cmd.env("ASC_HOME", &asc_home)
                .env("ASC_REAL_GIT", &self.real_git)
                .env("ASC_SHIM_TRACE", &trace);
        }
        let script_path = root.join("scenario.sh");
        fs::write(&script_path, format!("{PRELUDE}\n{script}\n")).map_err(|e| e.to_string())?;
        cmd.arg(&script_path).current_dir(&work);
        let out = run_limited(&mut cmd)?;

        let norm = Normaliser::new(&root);
        let mut cap = Capture {
            exit: out.code,
            ..Capture::default()
        };
        cap.sections
            .insert("stdout".into(), norm.apply(&out.stdout));
        cap.sections
            .insert("stderr".into(), norm.apply(&out.stderr));
        cap.sections
            .insert("exit".into(), format!("{}\n", out.code).into_bytes());
        cap.fall_through = fs::read_to_string(&trace)
            .map(|t| t.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        if asc_only {
            return Ok(cap);
        }

        // The post-state, each command run with the same tool as the case.
        let post: &[(&str, &[&str])] = &[
            ("post:for-each-ref", &["for-each-ref"]),
            ("post:rev-parse HEAD", &["rev-parse", "HEAD"]),
            ("post:status", &["status", "--porcelain=v2", "-z"]),
            ("post:ls-files", &["ls-files", "-s"]),
        ];
        let tool = |args: &[&str]| -> Result<Output, String> {
            let mut c = Command::new(&git_path);
            scrub(&mut c);
            fixed_env(&mut c, &home, &path);
            if side == Side::Asc {
                c.env("ASC_HOME", &asc_home)
                    .env("ASC_REAL_GIT", &self.real_git)
                    .env("ASC_SHIM_TRACE", &trace);
            }
            c.args(args).current_dir(&work);
            run_limited(&mut c)
        };
        for (name, args) in post {
            let o = tool(args)?;
            let mut bytes = o.stdout;
            if !o.stderr.is_empty() {
                bytes.extend_from_slice(b"\n[stderr]\n");
                bytes.extend_from_slice(&o.stderr);
            }
            bytes.extend_from_slice(format!("\n[exit {}]\n", o.code).as_bytes());
            cap.sections.insert((*name).into(), norm.apply(&bytes));
        }
        // The gitdir state files.
        let gd = tool(&["rev-parse", "--git-dir"])?;
        let gitdir = String::from_utf8_lossy(&gd.stdout).trim().to_owned();
        if !gitdir.is_empty() {
            let gitdir = work.join(gitdir);
            for (rel, bytes) in state_files(&gitdir)? {
                cap.sections
                    .insert(format!("post:gitdir/{rel}"), norm.apply(&bytes));
            }
        }
        // The work tree bytes.
        for (rel, bytes) in tree_files(&work)? {
            cap.sections.insert(format!("post:tree/{rel}"), bytes);
        }
        Ok(cap)
    }
}

type Output = RunOutput;

/// The gitdir files that hold in-progress state (SPEC §4.4).
const STATE_NAMES: &[&str] = &[
    "HEAD",
    "MERGE_HEAD",
    "MERGE_MSG",
    "MERGE_MODE",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "ORIG_HEAD",
    "COMMIT_EDITMSG",
    "SQUASH_MSG",
    "BISECT_LOG",
    "BISECT_START",
    "BISECT_TERMS",
    "BISECT_EXPECTED_REV",
    "BISECT_NAMES",
    "REBASE_HEAD",
    "AUTO_MERGE",
];
const STATE_DIRS: &[&str] = &["rebase-merge", "rebase-apply", "sequencer"];

fn state_files(gitdir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    for name in STATE_NAMES {
        if let Ok(b) = fs::read(gitdir.join(name)) {
            out.push(((*name).to_owned(), b));
        }
    }
    for dir in STATE_DIRS {
        collect(&gitdir.join(dir), dir, &mut out, 0)?;
    }
    Ok(out)
}

fn collect(
    dir: &Path,
    rel: &str,
    out: &mut Vec<(String, Vec<u8>)>,
    depth: usize,
) -> Result<(), String> {
    if depth > 8 {
        return Err(format!("{}: nested too deep", dir.display()));
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(());
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let rel = format!("{rel}/{name}");
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            collect(&path, &rel, out, depth + 1)?;
        } else if ft.is_file() {
            out.push((
                rel,
                fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?,
            ));
        }
    }
    Ok(())
}

/// Every file under `work` except the top-level `.git`, by `/`-separated relative path.
fn tree_files(work: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    walk(work, "", &mut out, 0)?;
    Ok(out)
}

fn walk(
    dir: &Path,
    rel: &str,
    out: &mut Vec<(String, Vec<u8>)>,
    depth: usize,
) -> Result<(), String> {
    if depth > 64 {
        return Err(format!("{}: nested too deep", dir.display()));
    }
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if depth == 0 && name == ".git" {
            continue;
        }
        let rel = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let path = e.path();
        let ft = e.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            walk(&path, &rel, out, depth + 1)?;
        } else {
            if out.len() >= MAX_TREE_FILES {
                return Err(format!("more than {MAX_TREE_FILES} work tree files"));
            }
            let len = fs::metadata(&path).map_err(|e| e.to_string())?.len();
            if len > MAX_TREE_FILE {
                return Err(format!(
                    "{rel}: {len} bytes exceeds the {MAX_TREE_FILE} byte limit"
                ));
            }
            let bytes = fs::read(&path).map_err(|e| format!("{rel}: {e}"))?;
            out.push((rel, bytes));
        }
    }
    Ok(())
}

// ---- process plumbing ---------------------------------------------------------------------

pub struct RunOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
}

/// Removes every inherited `GIT_*` and `ASC_*` variable.
fn scrub(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy().to_ascii_uppercase();
        if name.starts_with("GIT_") || name.starts_with("ASC_") {
            cmd.env_remove(&key);
        }
    }
}

/// The fixed environment of SPEC §17.1.
fn fixed_env(cmd: &mut Command, home: &Path, path: &OsString) {
    cmd.env("PATH", path)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "C O Mitter")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env("NO_COLOR", "1");
}

/// Runs `cmd` with stdin closed and a wall-clock limit; output is capped at [`MAX_STREAM`].
fn run_limited(cmd: &mut Command) -> Result<RunOutput, String> {
    use std::io::Read;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn {:?}: {e}", cmd.get_program()))?;
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.by_ref().take(MAX_STREAM as u64).read_to_end(&mut buf);
            let _ = std::io::copy(&mut r, &mut std::io::sink());
            buf
        })
    };
    let out = drain(Box::new(child.stdout.take().ok_or("no stdout")?));
    let err = drain(Box::new(child.stderr.take().ok_or("no stderr")?));
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if start.elapsed() > SCRIPT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{:?} timed out after {SCRIPT_TIMEOUT:?}",
                cmd.get_program()
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Ok(RunOutput {
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        code: status.code().unwrap_or(-1),
    })
}

/// Runs inside each script before the scenario text. `g` runs git and marks the command and its
/// exit code on both streams; `at N` moves the commit clock to N minutes after the epoch.
const PRELUDE: &str = r#"
wpwd() { pwd -W 2>/dev/null || pwd; }
g() {
  printf '$ git %s\n' "$*"
  printf '$ git %s\n' "$*" >&2
  git "$@"
  rc=$?
  printf '[exit %s]\n' "$rc"
  printf '[exit %s]\n' "$rc" >&2
  return 0
}
at() {
  GIT_AUTHOR_DATE="$((1700000000 + $1 * 60)) +0000"
  GIT_COMMITTER_DATE="$GIT_AUTHOR_DATE"
  export GIT_AUTHOR_DATE GIT_COMMITTER_DATE
}
"#;

// ---- normalisation --------------------------------------------------------------------------

/// Replaces the side's root path in its native, slash and MSYS spellings, and drops progress lines.
struct Normaliser {
    paths: Regex,
    progress: Regex,
}

impl Normaliser {
    fn new(root: &Path) -> Self {
        let native = root.to_string_lossy().into_owned();
        let slash = native.replace('\\', "/");
        let mut forms = vec![native, slash.clone()];
        let b = slash.as_bytes();
        if b.len() > 2 && b[1] == b':' && b[2] == b'/' {
            forms.push(format!(
                "/{}{}",
                (b[0] as char).to_ascii_lowercase(),
                &slash[2..]
            ));
        }
        forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
        forms.dedup();
        let alt: Vec<String> = forms.iter().map(|f| regex::escape(f)).collect();
        let flags = if cfg!(windows) { "(?i-u)" } else { "(?-u)" };
        let paths = Regex::new(&format!("{flags}{}", alt.join("|"))).expect("path regex");
        let progress = Regex::new(
            r"(?m)^(remote: )?(Enumerating|Counting|Compressing|Writing|Receiving|Resolving|Unpacking|Checking out|Updating) (objects|files|deltas|reference)[^\n]*\n",
        )
        .expect("progress regex");
        Self { paths, progress }
    }

    fn apply(&self, bytes: &[u8]) -> Vec<u8> {
        let s = String::from_utf8_lossy(bytes);
        let s = self.paths.replace_all(&s, "<ROOT>");
        let s = self.progress.replace_all(&s, "");
        s.into_owned().into_bytes()
    }
}

// ---- comparison ----------------------------------------------------------------------------

fn text(bytes: Option<&Vec<u8>>) -> String {
    bytes
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

fn section_matches(pattern: &str, section: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => section.starts_with(prefix),
        None => pattern == section,
    }
}

/// `bytes` with NUL shown as a newline and the lines the allowed entries drop removed.
fn filtered(case: &str, section: &str, bytes: &[u8], allowed: &[Allow]) -> String {
    let mut lines: Vec<String> = String::from_utf8_lossy(bytes)
        .replace('\0', "\n\u{2400}\n")
        .split('\n')
        .map(str::to_owned)
        .collect();
    for a in allowed {
        if (a.scenario == "*" || a.scenario == case)
            && section_matches(&a.section, section)
            && let Ok(re) = Regex::new(&a.drop)
        {
            lines.retain(|l| !re.is_match(l));
        }
    }
    lines.join("\n")
}

/// Compares the two captures section by section; the error names each differing section and
/// shows the first lines that differ.
pub fn compare(case: &str, git: &Capture, asc: &Capture, allowed: &[Allow]) -> Result<(), String> {
    let mut names: Vec<&String> = git.sections.keys().chain(asc.sections.keys()).collect();
    names.sort();
    names.dedup();
    let mut report = String::new();
    for name in names {
        let (a, b) = (git.sections.get(name), asc.sections.get(name));
        let fa = a.map(|x| filtered(case, name, x, allowed));
        let fb = b.map(|x| filtered(case, name, x, allowed));
        if fa == fb {
            continue;
        }
        let _ = writeln!(report, "--- {name}");
        match (&fa, &fb) {
            (Some(_), None) => report.push_str("  present with git only\n"),
            (None, Some(_)) => report.push_str("  present with asc only\n"),
            (Some(x), Some(y)) => report.push_str(&first_diff(x, y)),
            (None, None) => {}
        }
    }
    if report.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{case}: asc differs from git (git = '-', asc = '+')\n{report}"
        ))
    }
}

fn first_diff(git: &str, asc: &str) -> String {
    if git.lines().eq(asc.lines()) {
        return format!("  -{git:?}\n  +{asc:?}\n");
    }
    let diff = similar::TextDiff::from_lines(git, asc);
    let unified = diff.unified_diff().context_radius(3).to_string();
    let mut out: String = unified
        .lines()
        .take(MAX_DIFF_LINES)
        .map(|l| format!("  {l}\n"))
        .collect();
    if unified.lines().count() > MAX_DIFF_LINES {
        out.push_str("  ...\n");
    }
    out
}
