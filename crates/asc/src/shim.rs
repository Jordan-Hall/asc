//! The git shim (SPEC §11.4): the `asc` binary invoked as `git`. It classifies the invocation
//! with git-style discovery (no `state.db`, no daemon) and either hands it to `dispatch` (an asc
//! workspace, or a target asc serves) or runs real git with identical argv, env, cwd and stdio.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use asc_git::realgit::RealGit;
use gix_config::file::{Metadata, init};
use gix_config::{File, Source};

use crate::dispatch::{self, Program};
use crate::env::GitEnv;

/// A `GIT_DIR` that names no repository.
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// The marker file that makes a gitdir an asc workspace (SPEC §9.4).
const MARKER: &str = "asc-workspace";

/// Where an invocation goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// asc runs it through `dispatch`.
    Asc,
    /// Real git runs it; `default_branch` adds `-c init.defaultBranch=master` when unset.
    Real { default_branch: bool },
}

/// What the shim reads from configuration.
#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    /// Global `asc.shim.init = asc`.
    init_asc: bool,
    /// `init.defaultBranch` is set somewhere.
    branch_set: bool,
    /// `asc.realGit`.
    real_git: Option<PathBuf>,
}

impl Settings {
    /// Reads the global config and the `-c` overrides. This never goes through
    /// `ConfigSnapshot`: its installation lookup runs `git` from `PATH`, which is this shim.
    fn load(env: &GitEnv) -> Self {
        let file = global_config(env);
        let value = |key: &str| {
            let over = env.config_overrides().filter(|p| p.key.eq_ignore_ascii_case(key)).last();
            match over {
                Some(p) => p.value.clone(),
                None => file.as_ref().and_then(|f| f.string(key)).map(|v| v.to_string()),
            }
        };
        Self {
            init_asc: value("asc.shim.init").is_some_and(|v| v.eq_ignore_ascii_case("asc")),
            branch_set: value("init.defaultBranch").is_some(),
            real_git: value("asc.realGit").map(|v| expand_home(&v)),
        }
    }
}

/// The user-level config files, lowest precedence first.
fn global_config(env: &GitEnv) -> Option<File> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let paths: Vec<PathBuf> = match &env.config_global {
        Some(p) => vec![p.clone()],
        None => {
            let home = std::env::home_dir();
            let xdg = var("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config")));
            [xdg.map(|x| x.join("git").join("config")), home.map(|h| h.join(".gitconfig"))]
                .into_iter()
                .flatten()
                .collect()
        }
    };
    let mut merged: Option<File> = None;
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let meta = Metadata { path: Some(path), source: Source::User, level: 0, trust: gix_sec::Trust::Full };
        let Ok(layer) = File::from_bytes_no_includes(&bytes, meta, init::Options::default()) else {
            continue;
        };
        match merged.as_mut() {
            Some(m) => {
                let _ = m.append(layer);
            }
            None => merged = Some(layer),
        }
    }
    merged
}

/// `~/` expands to the home directory.
fn expand_home(value: &str) -> PathBuf {
    match (value.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(value),
    }
}

pub(crate) async fn run(args: Vec<OsString>) -> ExitCode {
    let get = |k: &str| std::env::var_os(k);
    let mut env = GitEnv::from_process();
    let tail = args.get(1..).unwrap_or_default();
    let parsed = env.parse_globals(tail, &get);
    let mut cwd = std::env::current_dir().unwrap_or_default();
    let (version, rest): (bool, &[OsString]) = match &parsed {
        Ok((opts, at)) => {
            for dir in &opts.chdir {
                cwd = cwd.join(dir);
            }
            (opts.version, &tail[*at..])
        }
        Err(_) => (false, &[]),
    };
    let settings = Settings::load(&env);
    let root = find_workspace(&env, &cwd);
    let route = if version {
        Route::Asc
    } else {
        route(rest, &cwd, root.as_deref(), &settings)
    };
    match route {
        Route::Asc => dispatch::run(Program::Git, args).await,
        Route::Real { default_branch } => {
            let add = default_branch && !settings.branch_set;
            // Real git cannot read a workspace's synthetic `.git`; a command that needs no
            // repository must not trip over it. `clone` and `init` ignore the parent anyway.
            let word = rest.first().and_then(|w| w.to_str());
            let hide = root.is_some() && !matches!(word, Some("clone" | "init"));
            exec_real(&args[1..], add, hide, settings.real_git)
        }
    }
}

/// Classifies `rest` (the command word and its arguments). `root` is the workspace root when
/// discovery found one.
pub(crate) fn route(rest: &[OsString], cwd: &Path, root: Option<&Path>, s: &Settings) -> Route {
    let Some(word) = rest.first().and_then(|w| w.to_str()) else {
        return if root.is_some() { Route::Asc } else { Route::Real { default_branch: false } };
    };
    let args = &rest[1..];
    let outside = match root {
        None => true,
        Some(root) => independent(word, args, cwd, root),
    };
    if !outside {
        return Route::Asc;
    }
    let asc_url = args.iter().filter_map(|a| a.to_str()).any(is_asc_url);
    match word {
        "version" if args.is_empty() => Route::Asc,
        "clone" if s.init_asc || asc_url => Route::Asc,
        "init" if s.init_asc => Route::Asc,
        "clone" | "init" => Route::Real { default_branch: true },
        _ => Route::Real { default_branch: false },
    }
}

/// Whether an `asc+…` or `s3://` URL asks for asc (SPEC §11.4).
fn is_asc_url(arg: &str) -> bool {
    arg.starts_with("asc+") || arg.starts_with("s3://")
}

/// Repository-independent invocations route by their target (SPEC §11.4), even inside a
/// workspace.
fn independent(word: &str, args: &[OsString], cwd: &Path, root: &Path) -> bool {
    let text = || args.iter().filter_map(|a| a.to_str());
    match word {
        "clone" | "help" | "version" => true,
        w if w.starts_with("credential") => true,
        "init" => init_target(args).is_some_and(|d| !same_path(&cwd.join(d), root)),
        "ls-remote" => text()
            .find(|a| !a.starts_with('-'))
            .is_some_and(looks_like_location),
        "archive" => text().any(|a| a == "--remote" || a.starts_with("--remote=")),
        "config" => config_is_external(args, cwd, root),
        _ => false,
    }
}

/// The directory argument of `init`, skipping the options that take a value.
fn init_target(args: &[OsString]) -> Option<&OsString> {
    const WITH_VALUE: &[&str] = &["-b", "--initial-branch", "--separate-git-dir", "--object-format", "--ref-format", "--template"];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some(s) if WITH_VALUE.contains(&s) => {
                it.next();
            }
            Some(s) if s.starts_with('-') => {}
            _ => return Some(a),
        }
    }
    None
}

/// A URL or path, as opposed to a configured remote name.
fn looks_like_location(arg: &str) -> bool {
    arg.contains("://") || arg.contains([':', '/', '\\']) || arg.starts_with('.')
}

/// `config --global|--system|--file <f>` with `<f>` outside the workspace.
fn config_is_external(args: &[OsString], cwd: &Path, root: &Path) -> bool {
    let mut it = args.iter().filter_map(|a| a.to_str());
    while let Some(a) = it.next() {
        let file = match a {
            "--global" | "--system" => return true,
            "-f" | "--file" => it.next(),
            _ => a.strip_prefix("--file="),
        };
        if let Some(f) = file {
            return !within(&cwd.join(f), root);
        }
    }
    false
}

fn normal(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_owned();
    if cfg!(windows) { s.to_ascii_lowercase() } else { s }
}

fn same_path(a: &Path, b: &Path) -> bool {
    normal(a) == normal(b)
}

fn within(path: &Path, root: &Path) -> bool {
    let (p, r) = (normal(path), normal(root));
    p == r || p.strip_prefix(&r).is_some_and(|rest| rest.starts_with('/'))
}

/// The workspace root of `cwd`, found as git does: `GIT_DIR`, else the first `.git` walking up
/// below `GIT_CEILING_DIRECTORIES`. It is an asc workspace if and only if the resolved gitdir
/// holds the `asc-workspace` marker. Reads no database.
pub(crate) fn find_workspace(env: &GitEnv, cwd: &Path) -> Option<PathBuf> {
    if let Some(git_dir) = &env.git_dir {
        let git_dir = cwd.join(git_dir);
        let tree = env.work_tree.as_ref().map_or_else(|| cwd.to_owned(), |t| cwd.join(t));
        return git_dir.join(MARKER).is_file().then_some(tree);
    }
    for dir in cwd.ancestors() {
        if dir != cwd && env.ceiling_dirs.iter().any(|c| same_path(c, dir)) {
            break;
        }
        if !env.discovery_across_filesystem && dir != cwd && crosses_filesystem(cwd, dir) {
            break;
        }
        let dot_git = dir.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
            continue;
        };
        let gitdir = if meta.is_file() {
            read_gitfile(&dot_git, dir)?
        } else {
            dot_git
        };
        return gitdir.join(MARKER).is_file().then(|| dir.to_owned());
    }
    None
}

/// The target of a `gitdir: <path>` file; a relative path is relative to `dir`.
fn read_gitfile(file: &Path, dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    Some(dir.join(target))
}

#[cfg(unix)]
fn crosses_filesystem(from: &Path, to: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(from), std::fs::metadata(to)) {
        (Ok(a), Ok(b)) => a.dev() != b.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn crosses_filesystem(_from: &Path, _to: &Path) -> bool {
    false
}

/// Appends one line for this fall-through to `$ASC_SHIM_TRACE` (best effort).
fn trace(args: &[OsString]) {
    let Some(file) = std::env::var_os("ASC_SHIM_TRACE").filter(|f| !f.is_empty()) else {
        return;
    };
    let line: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(f, "{}", line.join(" "));
    }
}

/// Runs real git with `args` (argv[1..]) and never returns on Unix.
fn exec_real(
    args: &[OsString],
    default_branch: bool,
    hide_repo: bool,
    configured: Option<PathBuf>,
) -> ExitCode {
    let asc_home = std::env::var_os("ASC_HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let command = RealGit::resolve(configured.clone(), asc_home.as_deref()).and_then(|g| g.command());
    let mut cmd = match command {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(e.exit_code().clamp(0, 255) as u8);
        }
    };
    if default_branch {
        cmd.args(["-c", "init.defaultBranch=master"]);
    }
    if hide_repo && std::env::var_os("GIT_DIR").is_none() {
        cmd.env("GIT_DIR", NULL_DEVICE);
    }
    cmd.args(args);
    trace(args);
    run_child(cmd)
}

#[cfg(unix)]
fn run_child(mut cmd: std::process::Command) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let e = cmd.exec();
    eprintln!("fatal: asc: cannot run git: {e}");
    ExitCode::from(128)
}

#[cfg(windows)]
fn run_child(mut cmd: std::process::Command) -> ExitCode {
    console::ignore_interrupts();
    match cmd.status() {
        // The exit code passes through unclamped (NTSTATUS codes exceed 255).
        Ok(status) => std::process::exit(status.code().unwrap_or(128)),
        Err(e) => {
            eprintln!("fatal: asc: cannot run git: {e}");
            ExitCode::from(128)
        }
    }
}

/// The Windows console control handler: Ctrl-C and Ctrl-Break reach the child through the
/// shared console, so the shim only has to outlive them and return the child's exit code.
#[cfg(windows)]
#[allow(unsafe_code)]
mod console {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;

    unsafe extern "system" fn handler(event: u32) -> i32 {
        i32::from(matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT))
    }

    pub fn ignore_interrupts() {
        // SAFETY: `handler` is a plain function that touches no state.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &[&str]) -> Vec<OsString> {
        w.iter().map(OsString::from).collect()
    }

    fn s() -> Settings {
        Settings::default()
    }

    #[test]
    fn outside_a_repository_everything_is_real_git() {
        let cwd = Path::new("/w");
        let real = Route::Real { default_branch: false };
        assert_eq!(route(&words(&["status"]), cwd, None, &s()), real);
        assert_eq!(route(&words(&["config", "--global", "a.b"]), cwd, None, &s()), real);
        assert_eq!(route(&words(&["version", "--build-options"]), cwd, None, &s()), real);
        assert_eq!(route(&words(&["version"]), cwd, None, &s()), Route::Asc);
        assert_eq!(route(&[], cwd, None, &s()), real);
    }

    #[test]
    fn init_and_clone_get_the_default_branch_unless_routed_to_asc() {
        let cwd = Path::new("/w");
        let branch = Route::Real { default_branch: true };
        assert_eq!(route(&words(&["init", "x"]), cwd, None, &s()), branch);
        assert_eq!(route(&words(&["clone", "https://h/r"]), cwd, None, &s()), branch);
        assert_eq!(route(&words(&["clone", "asc+https://h/r"]), cwd, None, &s()), Route::Asc);
        assert_eq!(route(&words(&["clone", "s3://b/r"]), cwd, None, &s()), Route::Asc);
        let asc = Settings { init_asc: true, ..Settings::default() };
        assert_eq!(route(&words(&["init", "x"]), cwd, None, &asc), Route::Asc);
        assert_eq!(route(&words(&["clone", "u"]), cwd, None, &asc), Route::Asc);
    }

    #[test]
    fn inside_a_workspace_git_commands_are_asc() {
        let (cwd, root) = (Path::new("/w/sub"), Path::new("/w"));
        for cmd in [&["status"][..], &["log", "-1"], &["init"], &["ls-remote", "origin"], &["config", "a.b", "c"], &["config", "--file", "/w/f", "a.b"]] {
            assert_eq!(route(&words(cmd), cwd, Some(root), &s()), Route::Asc, "{cmd:?}");
        }
    }

    #[test]
    fn repository_independent_commands_follow_their_target() {
        let (cwd, root) = (Path::new("/w"), Path::new("/w"));
        let branch = Route::Real { default_branch: true };
        let real = Route::Real { default_branch: false };
        let r = |c: &[&str]| route(&words(c), cwd, Some(root), &s());
        assert_eq!(r(&["clone", "/src", "sub"]), branch);
        assert_eq!(r(&["init", "fixtures/tmp"]), branch);
        assert_eq!(r(&["init", "-b", "dev", "fixtures/tmp"]), branch);
        assert_eq!(r(&["ls-remote", "https://h/r"]), real);
        assert_eq!(r(&["ls-remote", "/some/repo"]), real);
        assert_eq!(r(&["config", "--global", "a.b"]), real);
        assert_eq!(r(&["config", "--file", "/elsewhere/c", "a.b"]), real);
        assert_eq!(r(&["credential", "fill"]), real);
        assert_eq!(r(&["credential-store", "get"]), real);
        assert_eq!(r(&["help", "log"]), real);
        assert_eq!(r(&["archive", "--remote=u", "HEAD"]), real);
        assert_eq!(r(&["version"]), Route::Asc);
    }

    #[test]
    fn discovery_needs_the_marker_in_the_resolved_gitdir() {
        let t = tempfile::tempdir().unwrap();
        let env = GitEnv::from_vars(&|_| None);
        let (ws, gitdir) = (t.path().join("ws"), t.path().join("home/wc/1/git"));
        std::fs::create_dir_all(ws.join("a/b")).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(ws.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        assert_eq!(find_workspace(&env, &ws.join("a/b")), None, "no marker yet");
        std::fs::write(gitdir.join(MARKER), "1\n").unwrap();
        assert_eq!(find_workspace(&env, &ws.join("a/b")), Some(ws.clone()));
        // A nested plain repository ends the walk.
        std::fs::create_dir_all(ws.join("a/.git")).unwrap();
        assert_eq!(find_workspace(&env, &ws.join("a/b")), None);
        // Ceiling directories stop the walk before the workspace.
        std::fs::remove_dir_all(ws.join("a/.git")).unwrap();
        let ceil = GitEnv::from_vars(&|k| {
            (k == "GIT_CEILING_DIRECTORIES").then(|| ws.join("a").into_os_string())
        });
        assert_eq!(find_workspace(&ceil, &ws.join("a/b")), None);
    }
}
