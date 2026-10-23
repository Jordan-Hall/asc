//! U61p: helpers for the turbo contract suite (SPEC section 17, "Turbo contract"). A fixture
//! monorepo is built twice, as a real git clone and as a plain asc workspace whose `git` is the
//! shim; turbo 2.10.11 runs in both and the results are compared.
//!
//! Every git process goes through `asc_git::realgit`. Nothing here reads or writes the developer's
//! own `ASC_HOME`, git configuration or repositories.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use asc_git::realgit::RealGit;

/// The pinned turbo, as `bunx` names it.
pub const TURBO: &str = "turbo@2.10.11";
/// How many packages the fixture has.
pub const PACKAGES: usize = 20;
/// The bound on one turbo or git run, in output bytes kept for assertions.
const MAX_OUTPUT: usize = 64 << 20;

const GIT_EXE: &str = if cfg!(windows) { "git.exe" } else { "git" };
const ASC_EXE: &str = if cfg!(windows) { "asc.exe" } else { "asc" };

const TURBO_JSON: &str = include_str!("../fixture/turbo.json");
const ROOT_PACKAGE_JSON: &str = include_str!("../fixture/package.json");
const GITIGNORE: &str = include_str!("../fixture/gitignore");

/// Which tool a command runs under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// A real git clone: the PATH is untouched and `git` is real git.
    Git,
    /// A plain asc workspace: the shim directory is first on the PATH.
    Asc,
}

/// The name of package `i`.
pub fn package_name(i: usize) -> String {
    format!("@fx/p{i:02}")
}

/// The directory of package `i`, relative to the repository root.
pub fn package_dir(i: usize) -> String {
    format!("packages/p{i:02}")
}

/// Package `i` depends on package `i / 2`, so edits fan out through a tree of dependents.
fn parent_of(i: usize) -> Option<usize> {
    (i > 0).then_some(i / 2)
}

/// The `bun.lock` of the fixture: workspaces only, no registry packages, so no install is needed.
fn bun_lock() -> String {
    let mut workspaces = String::from("    \"\": { \"name\": \"fx-root\" },\n");
    let mut packages = String::new();
    for i in 0..PACKAGES {
        let name = package_name(i);
        let deps = parent_of(i)
            .map(|p| {
                format!(
                    ", \"dependencies\": {{ \"{}\": \"workspace:*\" }}",
                    package_name(p)
                )
            })
            .unwrap_or_default();
        let sep = if i + 1 == PACKAGES { "" } else { "," };
        workspaces.push_str(&format!(
            "    \"{}\": {{ \"name\": \"{name}\"{deps} }}{sep}\n",
            package_dir(i)
        ));
        packages.push_str(&format!(
            "    \"{name}\": [\"{name}@workspace:{}\"]{sep}\n",
            package_dir(i)
        ));
    }
    format!(
        "{{\n  \"lockfileVersion\": 1,\n  \"workspaces\": {{\n{workspaces}  }},\n  \"packages\": {{\n{packages}  }}\n}}\n"
    )
}

/// Writes the fixture monorepo into `root` (which must exist and be empty of tracked files).
pub fn write_fixture(root: &Path) {
    let put = |rel: &str, text: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, text).expect("write fixture file");
    };
    put("package.json", ROOT_PACKAGE_JSON);
    put("turbo.json", TURBO_JSON);
    put("bun.lock", &bun_lock());
    put(".gitignore", GITIGNORE);
    for i in 0..PACKAGES {
        let deps = parent_of(i)
            .map(|p| {
                format!(
                    ",\n  \"dependencies\": {{ \"{}\": \"workspace:*\" }}",
                    package_name(p)
                )
            })
            .unwrap_or_default();
        put(
            &format!("{}/package.json", package_dir(i)),
            &format!(
                "{{\n  \"name\": \"{}\",\n  \"version\": \"1.0.0\",\n  \"scripts\": {{ \"build\": \"mkdir -p dist && echo built > dist/out.txt\" }}{deps}\n}}\n",
                package_name(i)
            ),
        );
        put(
            &format!("{}/src/index.js", package_dir(i)),
            &format!("export const id = {i};\n"),
        );
    }
}

/// The captured result of one process.
#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// Panics with both streams unless the process succeeded.
    pub fn ok(self, what: &str) -> Self {
        assert_eq!(
            self.code, 0,
            "{what} failed\nstdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }
}

fn capture(out: Output) -> Run {
    let cut = |bytes: &[u8]| {
        let kept = &bytes[..bytes.len().min(MAX_OUTPUT)];
        String::from_utf8_lossy(kept).replace("\r\n", "\n")
    };
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: cut(&out.stdout),
        stderr: cut(&out.stderr),
    }
}

/// The `asc` binary under test: `ASC_BIN`, else `asc` next to this test's own `deps` directory,
/// built with `cargo build -p asc` into the same target directory when it is missing.
fn asc_binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(path) = std::env::var_os("ASC_BIN") {
            return PathBuf::from(path);
        }
        let exe = std::env::current_exe().expect("test exe");
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("target profile directory")
            .to_owned();
        let bin = profile_dir.join(ASC_EXE);
        if bin.is_file() {
            return bin;
        }
        let mut build = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        build.args(["build", "-j", "6", "-p", "asc"]);
        if profile_dir.file_name().is_some_and(|n| n == "release") {
            build.arg("--release");
        }
        if let Some(target) = profile_dir.parent() {
            build.env("CARGO_TARGET_DIR", target);
        }
        let status = build.status().expect("run cargo build -p asc");
        assert!(status.success(), "cargo build -p asc failed");
        assert!(bin.is_file(), "no asc binary at {}", bin.display());
        bin
    })
}

/// The `bun` launcher: `BUN`, else the first of `bun.exe`, `bun.cmd`, `bun` on the PATH
/// (Windows `CreateProcess` only finds `.exe` by itself, and node-managed installs ship `.cmd`).
fn bun() -> &'static Path {
    static BUN: OnceLock<PathBuf> = OnceLock::new();
    BUN.get_or_init(|| {
        if let Some(path) = std::env::var_os("BUN") {
            return PathBuf::from(path);
        }
        let paths = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&paths) {
            for name in ["bun.exe", "bun.cmd", "bun"] {
                if dir.join(name).is_file() {
                    return dir.join(name);
                }
            }
        }
        panic!("bun not found on PATH; install bun or set BUN");
    })
}

fn real_git() -> &'static RealGit {
    static REAL: OnceLock<RealGit> = OnceLock::new();
    REAL.get_or_init(|| RealGit::resolve(None, None).expect("real git"))
}

/// The bun package cache, shared by every lab so `bunx` downloads turbo once.
fn bun_cache() -> Option<OsString> {
    if let Some(dir) = std::env::var_os("BUN_INSTALL_CACHE_DIR") {
        return Some(dir);
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(
        PathBuf::from(home)
            .join(".bun/install/cache")
            .into_os_string(),
    )
}

/// A hermetic sandbox: a temporary `ASC_HOME`, user home and git configuration, and a directory
/// holding only the shim (the `asc` binary named `git`).
pub struct Lab {
    pub tmp: tempfile::TempDir,
    pub asc_home: PathBuf,
    pub user: PathBuf,
    pub shim_dir: PathBuf,
}

impl Lab {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let asc_home = tmp.path().join("asc-home");
        let user = tmp.path().join("user");
        let shim_dir = tmp.path().join("shim");
        fs::create_dir_all(&user).expect("user dir");
        fs::create_dir_all(&shim_dir).expect("shim dir");
        for dir in [".config", "AppData/Roaming", "AppData/Local"] {
            fs::create_dir_all(user.join(dir)).expect("config dir");
        }
        // `git init` through the shim makes an asc workspace, as `asc init` does, with no `.ascignore`.
        fs::write(user.join(".gitconfig"), "[asc \"shim\"]\n\tinit = asc\n").expect("gitconfig");
        let link = shim_dir.join(GIT_EXE);
        if fs::hard_link(asc_binary(), &link).is_err() {
            fs::copy(asc_binary(), &link).expect("copy shim");
        }
        Self {
            tmp,
            asc_home,
            user,
            shim_dir,
        }
    }

    /// A directory under the sandbox.
    pub fn dir(&self, name: &str) -> PathBuf {
        let path = self.tmp.path().join(name);
        fs::create_dir_all(&path).expect("mkdir");
        path
    }

    /// The sandbox environment applied to `cmd`: no inherited `GIT_*` or `ASC_*`, a pinned
    /// identity and clock, and the shim first on the PATH for [`Side::Asc`].
    fn envelop(&self, cmd: &mut Command, cwd: &Path, side: Side) {
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("GIT_") || name.starts_with("ASC_") || name.starts_with("TURBO_") {
                cmd.env_remove(&key);
            }
        }
        cmd.current_dir(cwd)
            .env("ASC_HOME", &self.asc_home)
            .env("HOME", &self.user)
            .env("USERPROFILE", &self.user)
            .env("XDG_CONFIG_HOME", self.user.join(".config"))
            .env("APPDATA", self.user.join("AppData/Roaming"))
            .env("LOCALAPPDATA", self.user.join("AppData/Local"))
            .env("GIT_CONFIG_GLOBAL", self.user.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .env("NO_COLOR", "1")
            .env("TURBO_TELEMETRY_DISABLED", "1")
            .env("DO_NOT_TRACK", "1")
            .env("TURBO_UI", "false")
            .env("TURBO_NO_UPDATE_NOTIFIER", "1");
        if let Some(cache) = bun_cache() {
            cmd.env("BUN_INSTALL_CACHE_DIR", cache);
        }
        if side == Side::Asc {
            cmd.env("ASC_REAL_GIT", real_git().path());
            let mut paths = vec![self.shim_dir.clone()];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            cmd.env("PATH", std::env::join_paths(paths).expect("PATH"));
        }
    }

    /// Runs real git in `cwd` (never the shim).
    pub fn real_git(&self, cwd: &Path, args: &[&str]) -> Run {
        let mut cmd = real_git().command().expect("real git command");
        self.envelop(&mut cmd, cwd, Side::Git);
        capture(cmd.args(args).output().expect("spawn git"))
    }

    /// Runs the shim as `git` in `cwd`.
    pub fn shim(&self, cwd: &Path, args: &[&str]) -> Run {
        let mut cmd = Command::new(self.shim_dir.join(GIT_EXE));
        self.envelop(&mut cmd, cwd, Side::Asc);
        capture(cmd.args(args).output().expect("spawn shim"))
    }

    /// `git <args>` on `side`.
    pub fn git(&self, side: Side, cwd: &Path, args: &[&str]) -> Run {
        match side {
            Side::Git => self.real_git(cwd, args),
            Side::Asc => self.shim(cwd, args),
        }
    }

    /// Runs `bunx turbo@2.10.11 <args>` in `cwd` on `side`, with extra environment.
    pub fn turbo(&self, side: Side, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new(bun());
        self.envelop(&mut cmd, cwd, side);
        cmd.args(["x", TURBO]).args(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        capture(cmd.output().expect("spawn bun x"))
    }

    /// A repository holding the fixture in one commit on `master`: a real git clone of an origin
    /// for [`Side::Git`], a plain asc workspace for [`Side::Asc`]. Returns its root.
    pub fn repo(&self, side: Side, name: &str) -> PathBuf {
        let root = self.dir(name);
        match side {
            Side::Git => {
                self.real_git(&root, &["init", "-b", "master"])
                    .ok("git init");
            }
            Side::Asc => {
                self.shim(&root, &["init"]).ok("shim init");
                assert!(root.join(".git").is_file(), "an asc workspace has a gitfile");
            }
        }
        write_fixture(&root);
        self.git(side, &root, &["add", "-A"]).ok("add");
        self.git(side, &root, &["commit", "-m", "base"])
            .ok("commit base");
        root
    }

    /// Commits the working tree as `message`.
    pub fn commit_all(&self, side: Side, root: &Path, message: &str) {
        self.git(side, root, &["add", "-A"]).ok("add");
        self.git(side, root, &["commit", "-m", message])
            .ok("commit");
    }
}

impl Default for Lab {
    fn default() -> Self {
        Self::new()
    }
}

/// Edits the source of package `i`.
pub fn edit_package(root: &Path, i: usize, text: &str) {
    let path = root.join(package_dir(i)).join("src/index.js");
    fs::write(path, text).expect("edit package");
}

/// `turbo run build --dry=json`, reduced to `taskId -> hash`.
pub fn dry_hashes(lab: &Lab, side: Side, root: &Path) -> BTreeMap<String, String> {
    let run = lab
        .turbo(side, root, &["run", "build", "--dry=json"], &[])
        .ok("turbo --dry=json");
    parse_hashes(&run.stdout)
}

/// The task hashes of a `--dry=json` document.
pub fn parse_hashes(stdout: &str) -> BTreeMap<String, String> {
    let start = stdout.find('{').expect("dry-run JSON");
    let doc: serde_json::Value =
        serde_json::from_str(&stdout[start..]).expect("valid dry-run JSON");
    doc["tasks"]
        .as_array()
        .expect("tasks array")
        .iter()
        .map(|task| {
            (
                task["taskId"].as_str().expect("taskId").to_owned(),
                task["hash"].as_str().expect("hash").to_owned(),
            )
        })
        .collect()
}

/// `turbo ls --affected` against `base`, as the sorted package names.
pub fn affected(lab: &Lab, side: Side, root: &Path, base: &str) -> Vec<String> {
    let run = lab
        .turbo(
            side,
            root,
            &["ls", "--affected", "--output=json"],
            &[("TURBO_SCM_BASE", base)],
        )
        .ok("turbo ls --affected");
    let start = run.stdout.find('{').expect("ls JSON");
    let doc: serde_json::Value = serde_json::from_str(&run.stdout[start..]).expect("valid ls JSON");
    let mut names: Vec<String> = doc["packages"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}
