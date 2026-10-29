//! Real git resolution (SPEC §11.4): finds the genuine `git` that is not asc's own shim,
//! guards against shim loops and builds the hook child `PATH` (SPEC §9.4.1).

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Environment variable that overrides every other resolution step.
pub const ENV_REAL_GIT: &str = "ASC_REAL_GIT";
/// Environment variable carrying the shim nesting depth.
pub const ENV_SHIM_DEPTH: &str = "ASC_SHIM_DEPTH";
/// Highest `ASC_SHIM_DEPTH` a process may start with before it refuses to spawn git.
pub const MAX_SHIM_DEPTH: u32 = 4;
/// Oldest supported real git.
pub const MIN_VERSION: GitVersion = GitVersion {
    major: 2,
    minor: 40,
};

const GIT_FILE: &str = if cfg!(windows) { "git.exe" } else { "git" };

/// Failures of real git resolution. [`RealGitError::exit_code`] is always 128.
#[derive(Debug, thiserror::Error)]
pub enum RealGitError {
    /// No real git on any search step.
    #[error("fatal: asc: real git not found; set asc.realGit")]
    NotFound,
    /// `ASC_REAL_GIT` or `asc.realGit` names something that is not a file.
    #[error("fatal: asc: configured real git '{}' is not a file", .0.display())]
    NotAFile(PathBuf),
    /// `ASC_REAL_GIT` or `asc.realGit` is a relative path, which would resolve against the cwd.
    #[error("fatal: asc: configured real git '{}' must be an absolute path", .0.display())]
    NotAbsolute(PathBuf),
    /// The resolved git is older than [`MIN_VERSION`] or did not report a version.
    #[error("fatal: asc: real git '{}' must be version 2.40 or newer (found '{found}')", path.display())]
    TooOld { path: PathBuf, found: String },
    /// `ASC_SHIM_DEPTH` is above [`MAX_SHIM_DEPTH`].
    #[error("fatal: asc: git shim loop detected (ASC_SHIM_DEPTH={0})")]
    DepthExceeded(u32),
    /// A child PATH entry contains the platform separator.
    #[error("fatal: asc: cannot build child PATH: {0}")]
    JoinPaths(#[from] std::env::JoinPathsError),
    /// Running `git --version` failed.
    #[error("fatal: asc: cannot run '{}': {source}", path.display())]
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl RealGitError {
    /// Process exit code for this failure.
    pub fn exit_code(&self) -> i32 {
        128
    }
}

/// A git `major.minor` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
}

impl GitVersion {
    /// Parses the output of `git --version` (`git version 2.43.0.windows.1`).
    pub fn parse(output: &str) -> Option<Self> {
        let rest = output.trim().strip_prefix("git version ")?;
        let mut parts = rest.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some(Self { major, minor })
    }
}

/// Inputs of real git resolution; [`Resolver::from_process`] fills them from the process.
#[derive(Debug, Clone)]
pub struct Resolver {
    /// `ASC_REAL_GIT`.
    pub env_override: Option<PathBuf>,
    /// Global config `asc.realGit`.
    pub config: Option<PathBuf>,
    /// The `PATH` to search.
    pub path: OsString,
    /// The running binary, which must never be returned.
    pub current_exe: PathBuf,
    /// `%ProgramFiles%` (Windows fallback only).
    pub program_files: Option<PathBuf>,
    /// `$ASC_HOME/run`, holding the `real-git` cache.
    pub run_dir: Option<PathBuf>,
    /// The inherited `ASC_SHIM_DEPTH`.
    pub depth: u32,
    /// Check the git >= 2.40 requirement on a fresh (uncached) resolution.
    pub verify_version: bool,
}

impl Resolver {
    /// Reads the process environment. `config` is `asc.realGit`; `asc_home` is `$ASC_HOME`.
    pub fn from_process(config: Option<PathBuf>, asc_home: Option<&Path>) -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        Self {
            env_override: var(ENV_REAL_GIT).map(PathBuf::from),
            config,
            path: std::env::var_os("PATH").unwrap_or_default(),
            current_exe: std::env::current_exe().unwrap_or_default(),
            program_files: var("ProgramFiles").map(PathBuf::from),
            run_dir: asc_home.map(|h| h.join("run")),
            depth: var(ENV_SHIM_DEPTH)
                .and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
                .unwrap_or(0),
            verify_version: true,
        }
    }

    /// Resolves real git: env, config, cache, `PATH`, then the Windows default.
    pub fn resolve(&self) -> Result<RealGit, RealGitError> {
        if let Some(p) = self.env_override.as_ref().or(self.config.as_ref()) {
            if !p.is_absolute() {
                return Err(RealGitError::NotAbsolute(p.clone()));
            }
            if !p.is_file() {
                return Err(RealGitError::NotAFile(p.clone()));
            }
            return Ok(self.finish(p.clone()));
        }
        if let Some(p) = self.read_cache() {
            return Ok(self.finish(p));
        }
        let found = self
            .search_path()
            .or_else(|| self.windows_default())
            .ok_or(RealGitError::NotFound)?;
        if self.verify_version {
            check_version(&found)?;
        }
        self.write_cache(&found);
        Ok(self.finish(found))
    }

    fn finish(&self, path: PathBuf) -> RealGit {
        RealGit {
            path,
            depth: self.depth,
        }
    }

    fn is_self(&self, candidate: &Path) -> bool {
        same_file::is_same_file(candidate, &self.current_exe).unwrap_or(false)
    }

    /// Empty and relative `PATH` entries are skipped: they resolve against the cwd, so a
    /// repository could plant its own `git` there.
    fn search_path(&self) -> Option<PathBuf> {
        std::env::split_paths(&self.path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(GIT_FILE))
            .find(|c| c.is_file() && !self.is_self(c))
    }

    fn windows_default(&self) -> Option<PathBuf> {
        if !cfg!(windows) {
            return None;
        }
        let c = self
            .program_files
            .as_ref()?
            .join("Git")
            .join("cmd")
            .join(GIT_FILE);
        (c.is_file() && !self.is_self(&c)).then_some(c)
    }

    fn cache_file(&self) -> Option<PathBuf> {
        self.run_dir.as_ref().map(|d| d.join("real-git"))
    }

    /// Cache format: line 1 the git path, line 2 its mtime as `secs.nanos`.
    fn read_cache(&self) -> Option<PathBuf> {
        let text = fs::read_to_string(self.cache_file()?).ok()?;
        let (path, stamp) = text.split_once('\n')?;
        let path = PathBuf::from(path);
        (path.is_absolute() && stamp.trim() == mtime_stamp(&path)? && !self.is_self(&path))
            .then_some(path)
    }

    /// Best effort: a cache that cannot be written only costs a re-resolve.
    fn write_cache(&self, path: &Path) {
        let (Some(file), Some(stamp), Some(path_str)) =
            (self.cache_file(), mtime_stamp(path), path.to_str())
        else {
            return;
        };
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        let ok = file.parent().is_some_and(|d| fs::create_dir_all(d).is_ok())
            && fs::write(&tmp, format!("{path_str}\n{stamp}")).is_ok()
            && fs::rename(&tmp, &file).is_ok();
        if !ok {
            let _ = fs::remove_file(&tmp);
        }
    }
}

fn mtime_stamp(path: &Path) -> Option<String> {
    let d = fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    Some(format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
}

fn check_version(path: &Path) -> Result<(), RealGitError> {
    let out = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|source| RealGitError::Spawn {
            path: path.to_owned(),
            source,
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    match GitVersion::parse(&text) {
        Some(v) if v >= MIN_VERSION => Ok(()),
        _ => Err(RealGitError::TooOld {
            path: path.to_owned(),
            found: text.trim().to_owned(),
        }),
    }
}

/// The resolved real git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealGit {
    path: PathBuf,
    depth: u32,
}

/// A command for the git that tests and tools compare asc with: the one `ASC_REAL_GIT` names
/// (the pinned oracle, docs/git-fidelity.md), else the first `git` on `PATH`. Panics when there is
/// none, so a test never runs against nothing. Test code spawns git through this, never through
/// `Command::new("git")` (`build/lint/no-raw-git.sh`): a runner's own git moves on its own.
pub fn pinned_command() -> Command {
    RealGit::resolve(None, None)
        .expect("a real git (ASC_REAL_GIT, else PATH)")
        .command()
        .expect("a git command")
}

impl RealGit {
    /// Resolves from the process environment (see [`Resolver::from_process`]).
    pub fn resolve(config: Option<PathBuf>, asc_home: Option<&Path>) -> Result<Self, RealGitError> {
        Resolver::from_process(config, asc_home).resolve()
    }

    /// The git executable.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A command running real git with `ASC_SHIM_DEPTH` raised by one. The PATH is inherited.
    pub fn command(&self) -> Result<Command, RealGitError> {
        if self.depth > MAX_SHIM_DEPTH {
            return Err(RealGitError::DepthExceeded(self.depth));
        }
        let mut cmd = Command::new(&self.path);
        cmd.env(ENV_SHIM_DEPTH, (self.depth + 1).to_string());
        Ok(cmd)
    }

    /// The Git for Windows root (the directory holding `usr\bin\sh.exe`); `None` off Windows.
    pub fn git_for_windows_root(&self) -> Option<PathBuf> {
        if !cfg!(windows) {
            return None;
        }
        self.path
            .ancestors()
            .skip(1)
            .take(4)
            .find(|d| d.join("usr").join("bin").join("sh.exe").is_file())
            .map(Path::to_owned)
    }

    /// The POSIX shell for hooks: Git for Windows' `usr\bin\sh.exe` (never the `bin` wrappers),
    /// `/bin/sh` elsewhere. `None` on Windows without Git for Windows.
    pub fn sh(&self) -> Option<PathBuf> {
        if cfg!(windows) {
            self.git_for_windows_root()
                .map(|r| r.join("usr").join("bin").join("sh.exe"))
        } else {
            Some(PathBuf::from("/bin/sh"))
        }
    }

    /// The hook child `PATH` of SPEC §9.4.1, built on the current `PATH`.
    pub fn child_path(&self, shim_dir: &Path) -> Result<OsString, RealGitError> {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        child_path_from(shim_dir, self.git_for_windows_root().as_deref(), &inherited)
    }
}

/// `<shim>[;<GfW>\mingw64\bin;<GfW>\usr\bin];<inherited>`.
fn child_path_from(
    shim_dir: &Path,
    gfw: Option<&Path>,
    inherited: &OsStr,
) -> Result<OsString, RealGitError> {
    let mut dirs = vec![shim_dir.to_owned()];
    if let Some(root) = gfw {
        dirs.push(root.join("mingw64").join("bin"));
        dirs.push(root.join("usr").join("bin"));
    }
    dirs.extend(std::env::split_paths(inherited));
    Ok(std::env::join_paths(dirs)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    fn touch(dir: &Path, name: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, b"fake").unwrap();
        p
    }

    fn resolver(path: Vec<PathBuf>, exe: PathBuf, run: Option<PathBuf>) -> Resolver {
        Resolver {
            env_override: None,
            config: None,
            path: std::env::join_paths(path).unwrap(),
            current_exe: exe,
            program_files: None,
            run_dir: run,
            depth: 0,
            verify_version: false,
        }
    }

    /// A temp tree with a fake asc binary and a shim dir holding a hardlink to it named `git`.
    fn shim_fixture(t: &Path) -> (PathBuf, PathBuf) {
        let exe = touch(&t.join("asc"), "asc.bin");
        let shim = t.join("shim");
        fs::create_dir_all(&shim).unwrap();
        fs::hard_link(&exe, shim.join(GIT_FILE)).unwrap();
        (exe, shim)
    }

    #[test]
    fn skips_hardlink_of_current_exe() {
        let t = tempfile::tempdir().unwrap();
        let (exe, shim) = shim_fixture(t.path());
        let real = touch(&t.path().join("real"), GIT_FILE);
        let r = resolver(vec![shim, t.path().join("real")], exe, None)
            .resolve()
            .unwrap();
        assert_eq!(r.path(), real);
    }

    #[test]
    fn skips_relative_and_empty_path_entries() {
        let t = tempfile::tempdir().unwrap();
        let cwd = std::env::current_dir().unwrap();
        let rel = PathBuf::from(format!("asc-realgit-test-{}", std::process::id()));
        touch(&cwd.join(&rel), GIT_FILE);
        let real = touch(&t.path().join("real"), GIT_FILE);
        let r = resolver(
            vec![PathBuf::new(), rel.clone(), t.path().join("real")],
            PathBuf::new(),
            None,
        )
        .resolve();
        fs::remove_dir_all(cwd.join(&rel)).unwrap();
        assert_eq!(r.unwrap().path(), real);
    }

    #[test]
    fn relative_override_is_rejected() {
        let mut r = resolver(vec![], PathBuf::new(), None);
        r.env_override = Some(PathBuf::from("git"));
        assert!(matches!(
            r.resolve().unwrap_err(),
            RealGitError::NotAbsolute(_)
        ));
    }

    #[test]
    fn only_shim_is_not_found() {
        let t = tempfile::tempdir().unwrap();
        let (exe, shim) = shim_fixture(t.path());
        let err = resolver(vec![shim], exe, None).resolve().unwrap_err();
        assert!(matches!(err, RealGitError::NotFound));
        assert_eq!(
            err.to_string(),
            "fatal: asc: real git not found; set asc.realGit"
        );
        assert_eq!(err.exit_code(), 128);
    }

    #[test]
    fn env_beats_config_and_must_be_a_file() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "a-git");
        let b = touch(t.path(), "b-git");
        let mut r = resolver(vec![], PathBuf::new(), None);
        r.env_override = Some(a.clone());
        r.config = Some(b.clone());
        assert_eq!(r.resolve().unwrap().path(), a);
        r.env_override = None;
        assert_eq!(r.resolve().unwrap().path(), b);
        r.config = Some(t.path().join("missing"));
        assert!(matches!(
            r.resolve().unwrap_err(),
            RealGitError::NotAFile(_)
        ));
    }

    #[test]
    fn cache_is_invalidated_by_mtime() {
        let t = tempfile::tempdir().unwrap();
        let exe = touch(&t.path().join("asc"), "asc.bin");
        let a = touch(&t.path().join("a"), GIT_FILE);
        let b = touch(&t.path().join("b"), GIT_FILE);
        let run = t.path().join("home").join("run");
        let first = resolver(vec![t.path().join("a")], exe.clone(), Some(run.clone()))
            .resolve()
            .unwrap();
        assert_eq!(first.path(), a);
        assert!(run.join("real-git").is_file());
        let later = vec![t.path().join("b")];
        let cached = resolver(later.clone(), exe.clone(), Some(run.clone()))
            .resolve()
            .unwrap();
        assert_eq!(cached.path(), a, "a valid cache wins over PATH");
        let f = File::options().write(true).open(&a).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        drop(f);
        let fresh = resolver(later, exe, Some(run)).resolve().unwrap();
        assert_eq!(fresh.path(), b);
    }

    #[test]
    fn depth_guard() {
        let g = RealGit {
            path: PathBuf::from("git"),
            depth: MAX_SHIM_DEPTH,
        };
        let cmd = g.command().unwrap();
        let depth = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new(ENV_SHIM_DEPTH))
            .and_then(|(_, v)| v);
        assert_eq!(depth, Some(OsStr::new("5")));
        let over = RealGit {
            path: PathBuf::from("git"),
            depth: MAX_SHIM_DEPTH + 1,
        };
        let err = over.command().unwrap_err();
        assert!(matches!(err, RealGitError::DepthExceeded(5)));
        assert_eq!(err.exit_code(), 128);
    }

    #[test]
    fn version_parsing() {
        assert_eq!(
            GitVersion::parse("git version 2.43.0.windows.1\n"),
            Some(GitVersion {
                major: 2,
                minor: 43
            })
        );
        assert!(GitVersion::parse("git version 2.39.5").unwrap() < MIN_VERSION);
        assert!(GitVersion::parse("garbage").is_none());
    }

    #[test]
    fn child_path_order() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let shim = Path::new("S");
        let plain = child_path_from(shim, None, OsStr::new("A")).unwrap();
        assert_eq!(plain, OsString::from(format!("S{sep}A")));
        let gfw = child_path_from(shim, Some(Path::new("G")), OsStr::new("A")).unwrap();
        let expect = format!(
            "S{sep}{}{sep}{}{sep}A",
            Path::new("G").join("mingw64").join("bin").display(),
            Path::new("G").join("usr").join("bin").display()
        );
        assert_eq!(gfw, OsString::from(expect));
    }

    #[cfg(windows)]
    #[test]
    fn sh_is_usr_bin_sh() {
        let t = tempfile::tempdir().unwrap();
        touch(&t.path().join("usr").join("bin"), "sh.exe");
        let git = touch(&t.path().join("cmd"), "git.exe");
        let g = RealGit {
            path: git,
            depth: 0,
        };
        assert_eq!(
            g.sh().unwrap(),
            t.path().join("usr").join("bin").join("sh.exe")
        );
    }
}
