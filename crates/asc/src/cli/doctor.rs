//! `asc doctor [--fix] [--store <url>] [--report-reclaimable [<path>…]]` (SPEC §2.4, §9.4, §10.5,
//! §11.4, §12, unit U40).
//!
//! Every check prints one line, `ok|info|warn|fail <name>: <detail>`, and a `warn` or `fail` may
//! add an indented `fix:` line. Only `fail` changes the exit code (1): a missing optional
//! feature or an offline remote is a warning. `--fix` repairs what it can (the private shim dir,
//! a stopped daemon), then checks again. `--report-reclaimable` only measures: it reads, sums
//! and prints, and deletes nothing, ever.
//!
//! Safety: every probe is a child with null stdin, a 10 s limit and an absolute system path
//! (never a `PATH` lookup, never the current directory). Directory walks never follow a
//! symbolic link or reparse point and stop at a fixed entry count. Text that comes from a
//! file or a child process is stripped of control characters and cut short before it is
//! printed. Credentials are reported by source name only, never by value.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{DirEntry, FileType};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use asc_state::AscHome;
use asc_state::state::repos::RemoteKind;

use super::common::{Ctx, Result};
use crate::child;
use crate::daemon::lifecycle::{self, Probe};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "doctor", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Repair what can be repaired without administrator rights.
    #[arg(long)]
    pub fix: bool,
    /// Run the create-only probe against a bucket (SPEC §3.4).
    #[arg(long, value_name = "URL")]
    pub store: Option<String>,
    /// Report the sizes of an old `.git`, `worktrees/`, `modules/`, `lfs/` and cruft packs.
    #[arg(long = "report-reclaimable", value_name = "PATH", num_args = 0..)]
    pub report_reclaimable: Option<Vec<PathBuf>>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "doctor",
    flags: &["--fix", "--store", "--report-reclaimable"],
    placeholders: &[],
}];

// ------------------------------------------------------------------ results

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// What `--fix` can do for a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fix {
    Shim,
    Daemon,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub level: Level,
    pub name: String,
    pub detail: String,
    pub hint: Option<String>,
    pub fix: Option<Fix>,
}

impl Check {
    fn new(level: Level, name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            level,
            name: name.into(),
            detail: clean(&detail.into()),
            hint: None,
            fix: None,
        }
    }
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Level::Ok, name, detail)
    }
    fn info(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Level::Info, name, detail)
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Level::Warn, name, detail)
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Level::Fail, name, detail)
    }
    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(clean(&hint.into()));
        self
    }
    fn fixable(mut self, fix: Fix) -> Self {
        self.fix = Some(fix);
        self
    }
}

/// `text` without control characters and at most 400 characters, for printing.
pub(crate) fn clean(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(400)
        .collect();
    if text.chars().count() > 400 {
        out.push_str("...");
    }
    out
}

/// `bytes` with a binary unit: `1536` is `1.5 KiB`.
pub(crate) fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for u in UNITS {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = u;
    }
    format!("{value:.1} {unit}")
}

// ------------------------------------------------------------------ bounded walks

/// The most directory entries one walk visits.
const WALK_LIMIT: usize = 20_000_000;

fn list_dir(dir: &Path) -> Option<std::fs::ReadDir> {
    std::fs::read_dir(dir).ok() // lint:allow-worktree-fs: bounded by walk_dirs, links never entered
}

/// The visitor of [`walk_dirs`]: a directory and its entries with their types.
pub(crate) type Visit<'a> = dyn FnMut(&Path, &[(DirEntry, FileType)]) + 'a;

/// Visits every directory below `root` (and `root`) with its entries. A symbolic link or
/// reparse point is listed but never entered; `skip` names directories that are not entered
/// either. Returns whether the entry limit stopped the walk. Unreadable directories are
/// skipped.
// lint:allow-worktree-fs: a doctor walk lists names and sizes, never follows a link, and is bounded.
pub(crate) fn walk_dirs(
    root: &Path,
    skip: &dyn Fn(&OsStr) -> bool,
    limit: usize,
    visit: &mut Visit<'_>,
) -> bool {
    let mut stack = vec![root.to_owned()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Some(read) = list_dir(&dir) else {
            continue;
        };
        let mut entries = Vec::new();
        for entry in read.flatten() {
            seen += 1;
            if seen > limit {
                return true;
            }
            if let Ok(kind) = entry.file_type() {
                entries.push((entry, kind));
            }
        }
        for (entry, kind) in &entries {
            if kind.is_dir() && !skip(&entry.file_name()) {
                stack.push(entry.path());
            }
        }
        visit(&dir, &entries);
    }
    false
}

/// Apparent size of a tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Size {
    pub bytes: u64,
    pub files: u64,
    /// The entry limit stopped the count; the numbers are lower bounds.
    pub truncated: bool,
}

/// Sum of the file sizes below `root`, links not followed (a link counts as zero bytes).
pub(crate) fn dir_size(root: &Path) -> Size {
    let mut size = Size::default();
    size.truncated = walk_dirs(root, &|_| false, WALK_LIMIT, &mut |_, entries| {
        for (entry, kind) in entries {
            if kind.is_file() {
                size.files += 1;
                size.bytes = size
                    .bytes
                    .saturating_add(entry.metadata().map_or(0, |m| m.len()));
            }
        }
    });
    size
}

fn size_text(s: Size) -> String {
    format!(
        "{} bytes ({}){}",
        s.bytes,
        human(s.bytes),
        if s.truncated { ", at least" } else { "" }
    )
}

// ------------------------------------------------------------------ child probes

const PROBE_LIMIT: Duration = Duration::from_secs(10);
const PROBE_OUTPUT: usize = 64 * 1024;

fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
}

/// Runs `cmd` with null stdin and stderr and returns its stdout when it exits 0 in time.
async fn probe_cmd(mut cmd: tokio::process::Command) -> Option<String> {
    cmd.stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = tokio::time::timeout(PROBE_LIMIT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = &out.stdout[..out.stdout.len().min(PROBE_OUTPUT)];
    Some(String::from_utf8_lossy(body).replace("\r\n", "\n"))
}

#[cfg(windows)]
fn windows_tool(rel: &str) -> tokio::process::Command {
    let root = system_root();
    let mut cmd = tokio::process::Command::new(root.join(rel)); // lint:allow-worktree-fs: a system directory, not a work tree
    cmd.current_dir(root);
    cmd
}

#[cfg(windows)]
async fn powershell(script: &str) -> Option<String> {
    let mut cmd = windows_tool(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    probe_cmd(cmd).await
}

// ------------------------------------------------------------------ paths

/// Directory entries of a `PATH`-style value; empty and relative entries are dropped.
fn path_dirs(path: &OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|p| p.is_absolute())
        .collect()
}

/// The first `git` executable in `path`, as the surface would find it.
pub(crate) fn find_git(path: &OsStr) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["git.exe", "git.cmd", "git.bat", "git.com"]
    } else {
        &["git"]
    };
    path_dirs(path).into_iter().find_map(|dir| {
        names
            .iter()
            .map(|n| dir.join(n))
            .find(|c| std::fs::metadata(c).is_ok_and(|m| m.is_file()))
    })
}

/// A comparison key for a directory: canonical when it exists, lowercase on Windows.
fn norm_dir(p: &Path) -> String {
    let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let mut s = real.to_string_lossy().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_owned();
    }
    let s = s.trim_end_matches('/').to_owned();
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

fn in_dir(file: &Path, dir: &Path) -> bool {
    file.parent().is_some_and(|p| norm_dir(p) == norm_dir(dir))
}

/// `/c/Users/me/git` as `C:/Users/me/git`; any other path as it is.
pub(crate) fn msys_to_win(p: &str) -> String {
    let b = p.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/' {
        return format!("{}:{}", b[1].to_ascii_uppercase() as char, &p[2..]);
    }
    p.to_owned()
}

/// What one `git` surface resolved to.
fn surface(name: &str, found: Option<PathBuf>, shim_dir: &Path) -> Check {
    let name = format!("git@{name}");
    match found {
        Some(p) if in_dir(&p, shim_dir) => {
            Check::ok(name, format!("{} (the asc shim)", p.display()))
        }
        Some(p) => Check::warn(
            name,
            format!("resolves to {}, not the asc shim", p.display()),
        )
        .hint(format!(
            "put {} first on PATH (`asc install-shim` prints the line)",
            shim_dir.display()
        )),
        None => Check::warn(name, "git is not found").hint(format!(
            "put {} on PATH (`asc install-shim` prints the line)",
            shim_dir.display()
        )),
    }
}

fn shim_dir() -> PathBuf {
    child::shim_dir_of(&std::env::current_exe().unwrap_or_default())
}

// ------------------------------------------------------------------ checks

fn check_shim(dir: &Path) -> Check {
    let file = dir.join(if cfg!(windows) { "git.exe" } else { "git" });
    match std::fs::symlink_metadata(&file) {
        Ok(_) if std::fs::metadata(&file).is_ok() => {
            Check::ok("shim-dir", format!("{} holds git", dir.display()))
        }
        Ok(_) => Check::warn(
            "shim-dir",
            format!("{} holds a dangling git", dir.display()),
        )
        .hint("asc doctor --fix, or asc install-shim")
        .fixable(Fix::Shim),
        Err(_) => Check::warn(
            "shim-dir",
            format!("{} does not exist or holds no git", dir.display()),
        )
        .hint("asc doctor --fix, or asc install-shim")
        .fixable(Fix::Shim),
    }
}

async fn check_real_git(ctx: &Ctx) -> Vec<Check> {
    use asc_git::realgit::{GitVersion, MIN_VERSION};
    let real = match ctx.real_git() {
        Ok(r) => r,
        Err(e) => {
            return vec![
                Check::fail("real-git", e.to_string().replace("fatal: ", ""))
                    .hint("install git 2.40 or newer, or set asc.realGit to its absolute path"),
            ];
        }
    };
    let Ok(std_cmd) = real.command() else {
        return vec![Check::fail(
            "real-git",
            "the shim nesting limit is exceeded",
        )];
    };
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.arg("--version");
    let out = probe_cmd(cmd).await;
    let version = out.as_deref().and_then(GitVersion::parse);
    let shown = out.as_deref().map_or("", str::trim).to_owned();
    vec![match version {
        Some(v) if v >= MIN_VERSION => {
            Check::ok("real-git", format!("{shown} at {}", real.path().display()))
        }
        Some(_) => Check::fail(
            "real-git",
            format!("{shown} at {} is older than 2.40", real.path().display()),
        )
        .hint("install git 2.40 or newer"),
        None => Check::fail(
            "real-git",
            format!("cannot read the version of {}", real.path().display()),
        ),
    }]
}

async fn check_daemon(home: &AscHome) -> Check {
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
        return Check::ok("daemon", "disabled by ASC_NO_DAEMON");
    }
    let cfg = match lifecycle::Config::for_home(home) {
        Ok(c) => c,
        Err(e) => return Check::warn("daemon", format!("no endpoint for this user: {e}")),
    };
    match lifecycle::probe(&cfg).await {
        Probe::Up(i) if i.version == cfg.version => Check::ok(
            "daemon",
            format!("running (pid {}, version {})", i.pid, i.version),
        ),
        Probe::Up(i) => Check::warn(
            "daemon",
            format!("version {} runs, this asc is {}", i.version, cfg.version),
        )
        .hint("asc daemon start replaces it")
        .fixable(Fix::Daemon),
        Probe::Hung => Check::fail("daemon", "a daemon is connected but does not answer")
            .hint("asc daemon stop")
            .fixable(Fix::Daemon),
        Probe::Down => Check::info("daemon", "not running (asc starts it when needed)")
            .hint("asc daemon start")
            .fixable(Fix::Daemon),
    }
}

/// Free bytes on the volume of `path`.
#[cfg(unix)]
async fn free_bytes(path: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(path).ok()?;
    Some(s.f_bavail.saturating_mul(s.f_frsize))
}

#[cfg(not(any(unix, windows)))]
async fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(windows)]
async fn free_bytes_windows(path: &Path) -> Option<u64> {
    let letter = volume_key(path).filter(|k| k.len() == 1)?;
    if !letter.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let script = format!("[System.IO.DriveInfo]::new('{letter}').AvailableFreeSpace");
    powershell(&script).await?.trim().parse().ok()
}

async fn check_free_space(home: &AscHome) -> Check {
    #[cfg(windows)]
    let free = free_bytes_windows(home.root()).await;
    #[cfg(not(windows))]
    let free = free_bytes(home.root()).await;
    let place = home.root().display();
    match free {
        None => Check::info(
            "asc-home-space",
            format!("cannot read the free space for {place}"),
        ),
        Some(f) if f < (1 << 30) => {
            Check::fail("asc-home-space", format!("{} free at {place}", human(f)))
                .hint("free disk space; `asc gc` evicts the cache")
        }
        Some(f) if f < (10 << 30) => {
            Check::warn("asc-home-space", format!("{} free at {place}", human(f)))
                .hint("`asc gc` evicts the cache")
        }
        Some(f) => Check::ok("asc-home-space", format!("{} free at {place}", human(f))),
    }
}

/// Where the nearest existing ancestor of `path` lives: a drive letter or share on Windows, the
/// device number elsewhere.
pub(crate) fn volume_key(path: &Path) -> Option<String> {
    let mut at = path.to_owned();
    let real = loop {
        match std::fs::canonicalize(&at) {
            Ok(r) => break r,
            Err(_) => {
                if !at.pop() {
                    return None;
                }
            }
        }
    };
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let Some(Component::Prefix(p)) = real.components().next() else {
            return None;
        };
        Some(match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                char::from(d).to_ascii_uppercase().to_string()
            }
            _ => p.as_os_str().to_string_lossy().to_ascii_lowercase(),
        })
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(std::fs::metadata(&real).ok()?.dev().to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = real;
        None
    }
}

#[cfg(windows)]
async fn check_windows(home: &AscHome) -> Vec<Check> {
    let projfs = async {
        let dll = system_root().join(r"System32\ProjectedFSLib.dll");
        if dll.is_file() {
            Check::ok("projfs", "the Windows Projected File System is installed")
        } else {
            Check::warn("projfs", "the Windows Projected File System is not installed")
                .hint("virtual workspaces need it: Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart (administrator)")
        }
    };
    let long_paths = async {
        let mut cmd = windows_tool(r"System32\reg.exe");
        cmd.args([
            "query",
            r"HKLM\SYSTEM\CurrentControlSet\Control\FileSystem",
            "/v",
            "LongPathsEnabled",
        ]);
        let on = probe_cmd(cmd).await.is_some_and(|o| {
            o.lines()
                .any(|l| l.contains("LongPathsEnabled") && l.trim_end().ends_with("0x1"))
        });
        if on {
            Check::ok("long-paths", "LongPathsEnabled=1")
        } else {
            Check::warn("long-paths", "LongPathsEnabled is not 1")
                .hint(r"set HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled to 1 (administrator), then sign out")
        }
    };
    let defender = async {
        let out = powershell(
            "(Get-MpPreference -ErrorAction Stop).ExclusionPath | ForEach-Object { $_ }",
        )
        .await;
        let Some(out) = out else {
            return Check::info("defender", "Defender exclusions cannot be read");
        };
        let lines: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if lines.iter().any(|l| l.starts_with("N/A")) {
            return Check::info("defender", "exclusions are visible to administrators only");
        }
        let home_key = norm_dir(home.root());
        let covered = lines.iter().any(|l| {
            let e = l
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_ascii_lowercase();
            !e.is_empty() && (home_key == e || home_key.starts_with(&format!("{e}/")))
        });
        if covered {
            Check::ok("defender", format!("{} is excluded", home.root().display()))
        } else {
            Check::warn(
                "defender",
                format!("{} is not excluded from Defender", home.root().display()),
            )
            .hint(format!(
                "Add-MpPreference -ExclusionPath '{}' (administrator)",
                home.root().display()
            ))
        }
    };
    let devdrive = async {
        let letter = volume_key(home.root())
            .filter(|k| k.len() == 1 && k.bytes().all(|b| b.is_ascii_alphabetic()))?;
        let mut cmd = windows_tool(r"System32\fsutil.exe");
        cmd.args(["devdrv", "query", &format!("{letter}:")]);
        let out = probe_cmd(cmd).await?.to_lowercase();
        Some(
            if out.contains("developer volume") && !out.contains("not a developer") {
                Check::ok("dev-drive", format!("{letter}: is a Dev Drive"))
            } else {
                Check::info("dev-drive", format!("{letter}: is not a Dev Drive"))
            },
        )
    };
    let (a, b, c, d) = tokio::join!(projfs, long_paths, defender, devdrive);
    let mut v = vec![a, b, c];
    v.extend(d);
    v
}

#[cfg(target_os = "linux")]
async fn check_fuse() -> Vec<Check> {
    let node = Path::new("/dev/fuse").exists();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tool = path_dirs(&path).into_iter().any(|d| {
        ["fusermount3", "fusermount"]
            .iter()
            .any(|n| d.join(n).is_file())
    });
    vec![if node && tool {
        Check::ok("fuse", "/dev/fuse and fusermount are available")
    } else {
        Check::warn(
            "fuse",
            format!(
                "/dev/fuse {}, fusermount {}",
                if node { "present" } else { "missing" },
                if tool { "present" } else { "missing" }
            ),
        )
        .hint("virtual workspaces need FUSE: install fuse3 and allow your user to use it")
    }]
}

/// Where each surface finds `git` (SPEC §11.4).
async fn check_git_surfaces(shim: &Path) -> Vec<Check> {
    let current = std::env::var_os("PATH").unwrap_or_default();
    #[allow(unused_mut)]
    let mut out = vec![surface(
        "turbo (this environment)",
        find_git(&current),
        shim,
    )];
    #[cfg(windows)]
    {
        let cmd = async {
            let mut c = windows_tool(r"System32\cmd.exe");
            c.args(["/d", "/c", "where", "git"]);
            first_line(probe_cmd(c).await).map(PathBuf::from)
        };
        let ps = async {
            first_line(
                powershell(
                    "(Get-Command git -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1).Source",
                )
                .await,
            )
            .map(PathBuf::from)
        };
        let bash = async {
            let real = asc_git::realgit::RealGit::resolve(None, None).ok()?;
            let sh = real.sh()?;
            let mut c = tokio::process::Command::new(sh);
            c.current_dir(system_root()).args(["-lc", "command -v git"]);
            let line = first_line_last(probe_cmd(c).await)?;
            let win = msys_to_win(&line);
            let p = PathBuf::from(win);
            // `command -v git` names `git`, not `git.exe`; the directory is what counts.
            Some(p)
        };
        let registry = async { registry_git().await };
        let (cmd, ps, bash, registry) = tokio::join!(cmd, ps, bash, registry);
        out.push(surface("cmd", cmd, shim));
        out.push(surface("powershell", ps, shim));
        out.push(surface("git-bash", bash, shim));
        out.push(surface("machine+user PATH", registry, shim));
    }
    out
}

#[cfg(windows)]
fn first_line(out: Option<String>) -> Option<String> {
    out?.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

#[cfg(windows)]
fn first_line_last(out: Option<String>) -> Option<String> {
    out?.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(str::to_owned)
}

/// `git` on the machine `PATH` followed by the user `PATH`, as a new login would see it.
#[cfg(windows)]
async fn registry_git() -> Option<PathBuf> {
    async fn value(key: &str) -> Option<String> {
        let mut c = windows_tool(r"System32\reg.exe");
        c.args(["query", key, "/v", "Path"]);
        parse_reg_path(&probe_cmd(c).await?)
    }
    let machine = value(r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment").await;
    let user = value(r"HKCU\Environment").await;
    let joined = [machine, user]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(";");
    find_git(OsStr::new(&expand_env(&joined)))
}

/// The data of a `reg query ... /v Path` answer.
pub(crate) fn parse_reg_path(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let l = l.trim();
        if !l.to_ascii_lowercase().starts_with("path") {
            return None;
        }
        let at = l
            .find("REG_EXPAND_SZ")
            .map(|i| i + 13)
            .or_else(|| l.find("REG_SZ").map(|i| i + 6))?;
        Some(l[at..].trim().to_owned())
    })
}

/// Replaces `%NAME%` with the environment value; unknown names stay as they are.
pub(crate) fn expand_env(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ------------------------------------------------------------------ workspace checks

/// Findings of one walk of the work tree.
#[derive(Default)]
struct TreeScan {
    shadowed: Vec<String>,
    /// attribute -> (file, patterns)
    attrs: std::collections::BTreeMap<String, (String, Vec<String>)>,
    truncated: bool,
}

const ATTR_FILES: usize = 200;
const ATTR_BYTES: u64 = 64 * 1024;
const IGNORE_BYTES: u64 = 1024 * 1024;
const SHADOW_LIST: usize = 50;

fn read_capped(path: &Path, cap: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut body = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(cap + 1)
        .read_to_end(&mut body)
        .ok()?;
    Some(body)
}

fn scan_tree(root: &Path) -> TreeScan {
    let mut scan = TreeScan::default();
    let mut attr_files = 0usize;
    let skip = |n: &OsStr| n == ".git" || n == ".asc" || n == "node_modules";
    let top = root.to_owned();
    scan.truncated = walk_dirs(root, &skip, 2_000_000, &mut |dir, entries| {
        let rel = |d: &Path| {
            d.strip_prefix(&top)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default()
        };
        let regular = |name: &str| {
            entries
                .iter()
                .find(|(e, k)| k.is_file() && e.file_name() == name)
                .map(|(e, _)| e.path())
        };
        if let (Some(git), Some(asc)) = (regular(".gitignore"), regular(".ascignore")) {
            let a = read_capped(&git, IGNORE_BYTES);
            let b = read_capped(&asc, IGNORE_BYTES);
            if a != b && scan.shadowed.len() < SHADOW_LIST {
                let r = rel(dir);
                scan.shadowed
                    .push(if r.is_empty() { ".".into() } else { r });
            }
        }
        for name in [".gitattributes", ".ascattributes"] {
            let Some(file) = regular(name) else { continue };
            if attr_files >= ATTR_FILES {
                break;
            }
            attr_files += 1;
            let Some(body) = read_capped(&file, ATTR_BYTES) else {
                continue;
            };
            let shown = {
                let r = rel(dir);
                if r.is_empty() {
                    name.to_owned()
                } else {
                    format!("{r}/{name}")
                }
            };
            for line in String::from_utf8_lossy(&body).lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let Some(pattern) = parts.next() else {
                    continue;
                };
                for attr in parts {
                    if let Some(key) = unapplied_attribute(attr) {
                        let e = scan
                            .attrs
                            .entry(key)
                            .or_insert_with(|| (shown.clone(), Vec::new()));
                        if e.1.len() < 3 && !e.1.iter().any(|x| x == pattern) {
                            e.1.push(pattern.to_owned());
                        }
                    }
                }
            }
        }
    });
    scan
}

/// The attribute name when asc does not apply or support `attr` (SPEC §2.4.1).
pub(crate) fn unapplied_attribute(attr: &str) -> Option<String> {
    let key = attr
        .trim_start_matches(['-', '!'])
        .split('=')
        .next()
        .unwrap_or("");
    match key {
        "eol" | "crlf" | "ident" | "working-tree-encoding" => Some(key.to_owned()),
        "text" if attr.contains('=') || attr == "text" => Some("text".to_owned()),
        "filter" => {
            let value = attr.split_once('=').map_or("", |(_, v)| v);
            (value != "lfs").then(|| "filter".to_owned())
        }
        _ => None,
    }
}

/// Glob match for `gitdir:` conditions: `*` stops at `/`, `**` does not, `?` is one character.
pub(crate) fn glob_match(pattern: &str, text: &str, ignore_case: bool) -> bool {
    if pattern.len() > 512 || pattern.matches('*').count() > 16 {
        return false;
    }
    let fold = |s: &str| -> Vec<u8> {
        if ignore_case {
            s.to_ascii_lowercase().into_bytes()
        } else {
            s.as_bytes().to_vec()
        }
    };
    let (p, t) = (fold(pattern), fold(text));
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => {
                let deep = p.get(1) == Some(&b'*');
                let rest = &p[if deep { 2 } else { 1 }..];
                (0..=t.len()).any(|i| (deep || !t[..i].contains(&b'/')) && go(rest, &t[i..]))
            }
            Some(b'?') => t.first().is_some_and(|c| *c != b'/') && go(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && go(&p[1..], &t[1..]),
        }
    }
    go(&p, &t)
}

/// Whether `[includeIf "<cond>"]` applies to a repository at `gitdir`. `None` for conditions
/// that do not look at the gitdir.
pub(crate) fn gitdir_condition(cond: &str, gitdir: &str, home: Option<&Path>) -> Option<bool> {
    let (ci, pattern) = if let Some(p) = cond.strip_prefix("gitdir/i:") {
        (true, p)
    } else {
        (false, cond.strip_prefix("gitdir:")?)
    };
    let mut pat = match (pattern.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => format!("{}/{rest}", h.to_string_lossy().replace('\\', "/")),
        _ => pattern.replace('\\', "/"),
    };
    if pat.starts_with("./") {
        return None; // relative to the including file
    }
    if pat.ends_with('/') {
        pat.push_str("**");
    }
    if !pat.starts_with('/') && !pat.starts_with("**") && !pat.contains(':') {
        pat = format!("**/{pat}");
    }
    let target = gitdir.replace('\\', "/");
    Some(glob_match(&pat, &target, ci || cfg!(windows)))
}

fn check_include_if(ctx: &Ctx, root: &Path, real_gitdir: &Path) -> Check {
    let home = std::env::home_dir();
    let asc_view = format!("{}/.git", root.to_string_lossy().replace('\\', "/"));
    let real_view = real_gitdir.to_string_lossy().replace('\\', "/");
    let mut drift = Vec::new();
    let mut seen = 0usize;
    for section in ctx
        .config
        .file()
        .sections_by_name("includeIf")
        .into_iter()
        .flatten()
    {
        let Some(sub) = section.header().subsection_name() else {
            continue;
        };
        let cond = String::from_utf8_lossy(sub).into_owned();
        let (Some(asc), Some(real)) = (
            gitdir_condition(&cond, &asc_view, home.as_deref()),
            gitdir_condition(&cond, &real_view, home.as_deref()),
        ) else {
            continue;
        };
        seen += 1;
        if asc != real && drift.len() < 20 {
            drift.push(cond);
        }
    }
    if drift.is_empty() {
        return Check::ok(
            "include-if",
            format!("{seen} gitdir condition(s) agree for asc and for tools that read the gitfile"),
        );
    }
    Check::warn(
        "include-if",
        format!(
            "[includeIf \"{}\"] applies in asc (as {asc_view}) but not to tools that read {real_view}",
            drift.join("\", \"")
        ),
    )
    .hint("use hasconfig:remote.*.url: conditions, which do not depend on the gitdir location")
}

/// Whether the user's global (not repository) configuration sets `core.autocrlf` to true; a
/// repository's own value cannot silence the warning about the user's.
fn global_autocrlf() -> bool {
    gix_config::File::from_globals()
        .ok()
        .and_then(|f| f.boolean("core.autocrlf").ok().flatten())
        .unwrap_or(false)
}

fn check_bun_volume(root: &Path) -> Option<Check> {
    let cache = std::env::var_os("BUN_INSTALL_CACHE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let default = std::env::home_dir()?
                .join(".bun")
                .join("install")
                .join("cache");
            default.is_dir().then_some(default)
        })?;
    let (a, b) = (volume_key(root)?, volume_key(&cache)?);
    Some(if a == b {
        Check::ok(
            "bun-cache-volume",
            "the workspace and the bun cache share a volume",
        )
    } else {
        Check::warn(
            "bun-cache-volume",
            format!(
                "the workspace ({}) and BUN_INSTALL_CACHE_DIR ({}) are on different volumes",
                root.display(),
                cache.display()
            ),
        )
        .hint("bun cannot hardlink across volumes: point BUN_INSTALL_CACHE_DIR at the workspace volume")
    })
}

fn check_credentials(ctx: &Ctx) -> Check {
    let mut sources = BTreeSet::new();
    if std::env::var_os("ASC_TOKEN").is_some_and(|v| !v.is_empty()) {
        sources.insert("ASC_TOKEN");
    }
    if let Some(home) = std::env::home_dir() {
        for n in [".netrc", "_netrc"] {
            if home.join(n).is_file() {
                sources.insert("netrc");
            }
        }
    }
    if !ctx.config.get_all("credential.helper").is_empty() {
        sources.insert("credential.helper");
    }
    if sources.is_empty() {
        Check::info(
            "credentials",
            "none found (ASC_TOKEN, netrc, credential.helper): anonymous",
        )
    } else {
        Check::ok(
            "credentials",
            format!(
                "available from {}",
                sources.into_iter().collect::<Vec<_>>().join(", ")
            ),
        )
    }
}

/// Host and port of a remote URL that speaks over TCP.
pub(crate) fn remote_endpoint(url: &str) -> Option<(String, u16)> {
    if let Ok(u) = url::Url::parse(url) {
        let port = u.port_or_known_default().or(match u.scheme() {
            "ssh" => Some(22),
            "git" => Some(9418),
            _ => None,
        })?;
        return Some((u.host_str()?.to_owned(), port));
    }
    // scp-like `user@host:path`
    let (head, _) = url.split_once(':')?;
    let host = head.rsplit('@').next()?;
    (!host.is_empty() && !host.contains('/') && !host.contains('\\')).then(|| (host.to_owned(), 22))
}

async fn check_remote(ctx: &Ctx) -> Option<Check> {
    let ws = ctx.ws_opt()?;
    let repo = &ws.repo.repo;
    let Some(url) = repo.remote_url.as_deref() else {
        return Some(Check::info("remote", "none (local-only repository)"));
    };
    let shown = url::Url::parse(url)
        .map(|u| crate::credential::mask_url(&u))
        .unwrap_or_else(|_| url.to_owned());
    if repo.remote_kind == RemoteKind::Bucket {
        return Some(Check::info(
            "remote",
            format!("{shown} is a bucket: use `asc doctor --store {shown}`"),
        ));
    }
    if let Ok(u) = url::Url::parse(url)
        && u.scheme() == "file"
    {
        let ok = u.to_file_path().is_ok_and(|p| p.exists());
        return Some(if ok {
            Check::ok("remote", format!("{shown} exists"))
        } else {
            Check::warn("remote", format!("{shown} does not exist"))
        });
    }
    let Some((host, port)) = remote_endpoint(url) else {
        let ok = Path::new(url).exists();
        return Some(if ok {
            Check::ok("remote", format!("{shown} exists"))
        } else {
            Check::warn("remote", format!("{shown} is not a reachable address"))
        });
    };
    let connect = tokio::net::TcpStream::connect((host.as_str(), port));
    Some(
        match tokio::time::timeout(Duration::from_secs(5), connect).await {
            Ok(Ok(_)) => Check::ok("remote", format!("{shown} accepts connections")),
            Ok(Err(e)) => Check::warn("remote", format!("{shown} is not reachable: {e}")),
            Err(_) => Check::warn("remote", format!("{shown} did not answer within 5 s")),
        },
    )
}

async fn check_store(url: &str) -> Check {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return Check::fail("store", format!("bad store url: {e}")),
    };
    let opened = if parsed.scheme() == "s3" {
        asc_store::s3::open(&parsed).map_err(|e| e.to_string())
    } else {
        object_store::parse_url(&parsed)
            .map(|(s, p)| (std::sync::Arc::from(s), p))
            .map_err(|e| e.to_string())
    };
    let (store, root) = match opened {
        Ok(v) => v,
        Err(e) => return Check::fail("store", format!("cannot open the store: {e}")),
    };
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        asc_store::probe::probe(&*store, &root),
    )
    .await;
    match result {
        Ok(r) if r.create_exclusive => Check::ok("store", r.detail),
        Ok(r) => Check::fail("store", format!("{}: BucketRefs is refused", r.detail))
            .hint("refs for this bucket need a server or the single-writer mode"),
        Err(_) => Check::fail("store", "the probe did not finish within 60 s"),
    }
}

async fn workspace_checks(ctx: &Ctx) -> Vec<Check> {
    let Some(ws) = ctx.ws_opt() else {
        return vec![Check::info(
            "workspace",
            "not inside a workspace: workspace checks skipped",
        )];
    };
    let root = ws.projection.toplevel().to_owned();
    let gitdir = ws.projection.git_dir().to_owned();
    let scan_root = root.clone();
    let scan = tokio::task::spawn_blocking(move || scan_tree(&scan_root))
        .await
        .unwrap_or_default();
    let mut out = Vec::new();
    if scan.shadowed.is_empty() {
        out.push(Check::ok(
            "ascignore",
            "no .ascignore shadows a different .gitignore",
        ));
    } else {
        out.push(
            Check::warn(
                "ascignore",
                format!(
                    "{} director{} where .ascignore shadows a differing .gitignore: {}",
                    scan.shadowed.len(),
                    if scan.shadowed.len() == 1 { "y" } else { "ies" },
                    scan.shadowed.join(", ")
                ),
            )
            .hint("rename or merge them: asc migrate ignore"),
        );
    }
    if scan.attrs.is_empty() {
        out.push(Check::ok("gitattributes", "no unapplied attributes"));
    } else {
        for (attr, (file, patterns)) in &scan.attrs {
            let what = match attr.as_str() {
                "ident" | "working-tree-encoding" | "filter" => "is not supported",
                _ => "is not applied at checkout",
            };
            out.push(Check::warn(
                "gitattributes",
                format!(
                    "attribute '{attr}' in {file} {what} (affects {})",
                    patterns.join(", ")
                ),
            ));
        }
    }
    if global_autocrlf() {
        out.push(
            Check::warn(
                "autocrlf",
                "core.autocrlf=true is ignored in asc workspaces; files hold the committed bytes",
            )
            .hint("git config --global core.autocrlf false"),
        );
    } else {
        out.push(Check::ok("autocrlf", "core.autocrlf is not true"));
    }
    if scan.truncated {
        out.push(Check::info(
            "tree-scan",
            "the work tree is large; the ignore and attribute checks read the first 2,000,000 entries",
        ));
    }
    out.push(check_include_if(ctx, &root, &gitdir));
    out.extend(check_bun_volume(&root));
    out.push(check_credentials(ctx));
    out.extend(check_remote(ctx).await);
    out
}

// ------------------------------------------------------------------ gathering and fixing

async fn gather(ctx: &Ctx, home: &AscHome, store: Option<&str>) -> Vec<Check> {
    let shim = shim_dir();
    let mut out = Vec::new();
    #[cfg(windows)]
    let platform = check_windows(home);
    #[cfg(target_os = "linux")]
    let platform = check_fuse();
    #[cfg(not(any(windows, target_os = "linux")))]
    let platform = async { Vec::new() };
    let store_check = async {
        match store {
            Some(u) => Some(check_store(u).await),
            None => None,
        }
    };
    let (platform, git, surfaces, daemon, space, ws, store_check) = tokio::join!(
        platform,
        check_real_git(ctx),
        check_git_surfaces(&shim),
        check_daemon(home),
        check_free_space(home),
        workspace_checks(ctx),
        store_check,
    );
    out.extend(git);
    out.push(check_shim(&shim));
    out.extend(surfaces);
    out.push(daemon);
    out.push(space);
    out.extend(platform);
    out.extend(ws);
    out.extend(store_check);
    out
}

async fn apply_fix(fix: Fix, home: &AscHome) -> std::result::Result<String, String> {
    match fix {
        Fix::Shim => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let mut cmd = tokio::process::Command::new(exe);
            cmd.arg("install-shim");
            probe_cmd(cmd)
                .await
                .map(|o| format!("installed the shim; add to PATH: {}", o.trim()))
                .ok_or_else(|| "`asc install-shim` failed".to_owned())
        }
        Fix::Daemon => {
            if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
                return Err("the daemon is disabled by ASC_NO_DAEMON".to_owned());
            }
            let _ = home;
            let (cfg, proc) = super::daemon::system().map_err(|e| e.to_string())?;
            lifecycle::ensure_running(&cfg, &proc)
                .await
                .map(|i| format!("daemon running (pid {})", i.pid))
                .map_err(|e| e.to_string())
        }
    }
}

fn print_checks(checks: &[Check]) {
    for c in checks {
        println!("{:<5} {}: {}", c.level.label(), c.name, c.detail);
        if matches!(c.level, Level::Warn | Level::Fail)
            && let Some(h) = &c.hint
        {
            println!("      fix: {h}");
        }
    }
}

// ------------------------------------------------------------------ reclaimable report

const WORKTREE_LIMIT: usize = 1000;

/// A file name's stem and extension, for the pack file family.
fn pack_family(name: &str) -> Option<(&str, &str)> {
    let (stem, ext) = name.rsplit_once('.')?;
    (stem.starts_with("pack-") && matches!(ext, "pack" | "idx" | "rev" | "mtimes" | "bitmap"))
        .then_some((stem, ext))
}

/// Sizes of the packs that have a `.mtimes` file: cruft packs.
fn cruft_packs(pack_dir: &Path) -> (u64, u64) {
    let mut sizes: std::collections::BTreeMap<String, (u64, bool)> = Default::default();
    walk_dirs(pack_dir, &|_| true, 100_000, &mut |dir, entries| {
        if dir != pack_dir {
            return;
        }
        for (e, k) in entries {
            if !k.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some((stem, ext)) = pack_family(&name) {
                let slot = sizes.entry(stem.to_owned()).or_default();
                slot.0 += e.metadata().map_or(0, |m| m.len());
                slot.1 |= ext == "mtimes";
            }
        }
    });
    let cruft: Vec<u64> = sizes
        .values()
        .filter(|(_, c)| *c)
        .map(|(s, _)| *s)
        .collect();
    (cruft.iter().sum(), cruft.len() as u64)
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// The checkout directories that the linked worktrees of `git_dir` name.
fn worktree_checkouts(git_dir: &Path) -> (Vec<PathBuf>, u64) {
    let dir = git_dir.join("worktrees");
    let mut checkouts = Vec::new();
    let mut count = 0u64;
    walk_dirs(&dir, &|_| true, WORKTREE_LIMIT * 4, &mut |d, entries| {
        if d != dir {
            return;
        }
        for (e, k) in entries {
            if !k.is_dir() {
                continue;
            }
            count += 1;
            if checkouts.len() >= WORKTREE_LIMIT {
                continue;
            }
            let file = e.path().join("gitdir");
            if let Some(body) = read_capped(&file, 4096)
                && body.len() <= 4096
            {
                let line = String::from_utf8_lossy(&body);
                let first = line.lines().next().unwrap_or("").trim();
                if let Some(parent) = Path::new(first).parent()
                    && parent.is_absolute()
                {
                    checkouts.push(parent.to_owned());
                }
            }
        }
    });
    (checkouts, count)
}

fn report_one(path: &Path) -> std::result::Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{}: is a symbolic link and is not followed",
            path.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{}: is not a directory", path.display()));
    }
    let dot_git = path.join(".git"); // lint:allow-worktree-fs
    let git_dir = match std::fs::symlink_metadata(&dot_git) {
        Ok(m) if m.is_dir() => dot_git,
        Ok(m) if m.is_file() => {
            return Err(format!(
                "{}: .git is a file: a linked worktree owns no object store",
                path.display()
            ));
        }
        Ok(_) => {
            return Err(format!(
                "{}: .git is a link and is not followed",
                path.display()
            ));
        }
        Err(_) if path.join("HEAD").is_file() && path.join("objects").is_dir() => path.to_owned(),
        Err(_) => return Err(format!("{}: no .git directory", path.display())),
    };
    println!("reclaimable {}", clean(&path.display().to_string()));
    println!("  git dir: {}", clean(&git_dir.display().to_string()));
    println!("  .git: {}", size_text(dir_size(&git_dir)));
    for name in ["worktrees", "modules", "lfs"] {
        let sub = git_dir.join(name);
        if is_link(&sub) {
            println!("  {name}/: is a link and is not followed");
            continue;
        }
        if !sub.is_dir() {
            println!("  {name}/: absent");
            continue;
        }
        let mut entries = 0u64;
        walk_dirs(&sub, &|_| true, WALK_LIMIT, &mut |d, list| {
            if d == sub {
                entries = list.len() as u64;
            }
        });
        println!(
            "  {name}/: {} in {entries} entries",
            size_text(dir_size(&sub))
        );
    }
    let pack_dir = git_dir.join("objects").join("pack");
    let (cruft_bytes, cruft_count) = if is_link(&pack_dir) {
        (0, 0)
    } else {
        cruft_packs(&pack_dir)
    };
    println!(
        "  cruft packs: {cruft_bytes} bytes ({}) in {cruft_count} packs",
        human(cruft_bytes)
    );
    let (checkouts, linked) = worktree_checkouts(&git_dir);
    let mut total = Size::default();
    let mut seen = BTreeSet::new();
    for c in &checkouts {
        if is_link(c) || !c.is_dir() || !seen.insert(norm_dir(c)) {
            continue;
        }
        let s = dir_size(c);
        total.bytes = total.bytes.saturating_add(s.bytes);
        total.files += s.files;
        total.truncated |= s.truncated;
    }
    println!(
        "  linked worktree checkouts: {} in {} of {linked} worktrees",
        size_text(total),
        seen.len()
    );
    Ok(())
}

fn report_reclaimable(ctx: &Ctx, paths: &[PathBuf]) -> Result<ExitCode> {
    let cwd = [ctx.cwd.clone()];
    let paths = if paths.is_empty() { &cwd[..] } else { paths };
    let mut failed = false;
    for p in paths {
        let full = if p.is_absolute() {
            p.clone()
        } else {
            ctx.cwd.join(p)
        };
        if let Err(e) = report_one(&full) {
            eprintln!("error: {}", clean(&e));
            failed = true;
        }
    }
    println!("nothing was deleted");
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    if let Some(paths) = &args.report_reclaimable {
        return report_reclaimable(ctx, paths);
    }
    let home = match ctx.ws_opt() {
        Some(ws) => ws.home.clone(),
        None => AscHome::locate()?,
    };
    let mut checks = gather(ctx, &home, args.store.as_deref()).await;
    if args.fix {
        let mut fixed = false;
        let mut done = Vec::new();
        for fix in checks.iter().filter_map(|c| c.fix).collect::<BTreeSet<_>>() {
            match apply_fix(fix, &home).await {
                Ok(msg) => {
                    fixed = true;
                    done.push(format!("fixed {fix:?}: {msg}"));
                }
                Err(e) => done.push(format!("could not fix {fix:?}: {e}")),
            }
        }
        for d in &done {
            println!("{}", clean(d));
        }
        if fixed {
            checks = gather(ctx, &home, args.store.as_deref()).await;
        }
    }
    print_checks(&checks);
    let failed = checks.iter().any(|c| c.level == Level::Fail);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(58 << 30), "58.0 GiB");
    }

    #[test]
    fn clean_strips_control_characters_and_bounds_length() {
        assert_eq!(clean("a\x1b[31mb\n"), "a?[31mb?");
        let long = "x".repeat(1000);
        assert_eq!(clean(&long).len(), 403);
    }

    #[test]
    fn msys_paths_become_windows_paths() {
        assert_eq!(msys_to_win("/c/Users/me/shim/git"), "C:/Users/me/shim/git");
        assert_eq!(msys_to_win("/mingw64/bin/git"), "/mingw64/bin/git");
        assert_eq!(msys_to_win("/usr/bin/git"), "/usr/bin/git");
    }

    #[test]
    fn glob_rules() {
        assert!(glob_match(
            "/home/me/work/**",
            "/home/me/work/a/.git",
            false
        ));
        assert!(!glob_match(
            "/home/me/work/**",
            "/home/me/other/.git",
            false
        ));
        assert!(glob_match("/home/*/.git", "/home/me/.git", false));
        assert!(!glob_match("/home/*/.git", "/home/me/x/.git", false));
        assert!(glob_match("/HOME/**", "/home/x", true));
        assert!(!glob_match("/HOME/**", "/home/x", false));
        assert!(!glob_match(&"*".repeat(40), "a", false));
    }

    #[test]
    fn gitdir_conditions() {
        let home = Path::new("/home/me");
        // The asc view (`<root>/.git`) matches; the real gitdir under ASC_HOME does not.
        let cond = "gitdir:~/work/";
        assert_eq!(
            gitdir_condition(cond, "/home/me/work/app/.git", Some(home)),
            Some(true)
        );
        assert_eq!(
            gitdir_condition(cond, "/home/me/.asc/wc/1/git", Some(home)),
            Some(false)
        );
        assert_eq!(
            gitdir_condition("hasconfig:remote.*.url:x", "/a", None),
            None
        );
        assert_eq!(gitdir_condition("gitdir:./x/", "/a", None), None);
        assert_eq!(
            gitdir_condition("gitdir/i:/HOME/me/**", "/home/me/a", None),
            Some(true)
        );
    }

    #[test]
    fn reg_output_is_parsed() {
        let out = "\nHKEY_CURRENT_USER\\Environment\n    Path    REG_EXPAND_SZ    C:\\a;%SystemRoot%\\b\n";
        assert_eq!(
            parse_reg_path(out).as_deref(),
            Some("C:\\a;%SystemRoot%\\b")
        );
        assert_eq!(parse_reg_path("nothing"), None);
    }

    #[test]
    fn env_expansion() {
        // SAFETY-free: reads only. PATH exists on every platform the tests run on.
        let path = std::env::var("PATH").unwrap();
        assert_eq!(expand_env("%PATH%;x"), format!("{path};x"));
        assert_eq!(expand_env("%ASC_SURELY_UNSET_VAR%"), "%ASC_SURELY_UNSET_VAR%");
        assert_eq!(expand_env("100%"), "100%");
    }

    #[test]
    fn attributes_asc_does_not_apply() {
        assert_eq!(unapplied_attribute("eol=crlf").as_deref(), Some("eol"));
        assert_eq!(unapplied_attribute("-text").as_deref(), None);
        assert_eq!(unapplied_attribute("text").as_deref(), Some("text"));
        assert_eq!(unapplied_attribute("filter=lfs"), None);
        assert_eq!(unapplied_attribute("filter=x").as_deref(), Some("filter"));
        assert_eq!(unapplied_attribute("binary"), None);
    }

    #[test]
    fn remote_endpoints() {
        assert_eq!(
            remote_endpoint("https://forge.example/x.git"),
            Some(("forge.example".into(), 443))
        );
        assert_eq!(
            remote_endpoint("git@forge.example:asc/dds.git"),
            Some(("forge.example".into(), 22))
        );
        assert_eq!(
            remote_endpoint("ssh://git@h:2222/x"),
            Some(("h".into(), 2222))
        );
        assert_eq!(remote_endpoint("/srv/repo.git"), None);
    }

    #[test]
    fn pack_files_group_by_stem() {
        assert_eq!(pack_family("pack-ab.mtimes"), Some(("pack-ab", "mtimes")));
        assert_eq!(pack_family("multi-pack-index"), None);
        assert_eq!(pack_family("pack-ab.keep"), None);
    }

    #[test]
    fn find_git_ignores_relative_entries() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        std::fs::write(dir.path().join(name), b"").unwrap();
        let path = std::env::join_paths([Path::new("."), dir.path()]).unwrap();
        assert_eq!(find_git(&path), Some(dir.path().join(name)));
        assert_eq!(find_git(OsStr::new("")), None);
        assert!(in_dir(&dir.path().join(name), dir.path()));
    }

    #[test]
    fn walk_does_not_enter_links_and_counts_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/f"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("g"), vec![0u8; 5]).unwrap();
        let s = dir_size(dir.path());
        assert_eq!((s.bytes, s.files, s.truncated), (105, 2, false));
        let mut seen = 0;
        let cut = walk_dirs(dir.path(), &|_| false, 1, &mut |_, _| seen += 1);
        assert!(cut);
    }
}
