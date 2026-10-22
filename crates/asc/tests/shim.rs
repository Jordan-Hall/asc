//! U20a acceptance: the git shim (argv[0] = `git`) and `asc install-shim`, driven through the
//! built binary. Real git comes from `RealGit`, never a raw `git` child.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Instant;

use asc_git::realgit::RealGit;
use support::{Sandbox, asc_binary, text};

const EXE: &str = if cfg!(windows) { "asc.exe" } else { "asc" };
const GIT: &str = if cfg!(windows) { "git.exe" } else { "git" };

fn bin() -> &'static Path {
    Path::new(asc_binary().expect("integration test"))
}

/// Links `bin()` to `target` afresh (a rebuilt binary is a new file), copying when the
/// filesystem refuses a link.
fn link_binary(target: &Path) {
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let _ = fs::remove_file(target);
    if fs::hard_link(bin(), target).is_err() && !target.exists() {
        fs::copy(bin(), target).unwrap();
    }
}

/// The directory holding only the shim: a link to the binary named `git`.
fn shim_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = bin().parent().unwrap().join("asc-test-shim").join(std::process::id().to_string());
        link_binary(&dir.join(GIT));
        dir
    })
}

fn real_git() -> &'static Path {
    static REAL: OnceLock<PathBuf> = OnceLock::new();
    REAL.get_or_init(|| RealGit::resolve(None, None).expect("real git").path().to_owned())
}

#[derive(Debug, PartialEq, Eq)]
struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command) -> Out {
    let out = cmd.output().expect("spawn");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

/// A deterministic identity and clock, so commits hash the same in two repositories.
fn pin_identity(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
}

/// Runs the shim as `git` in `cwd`.
fn shim(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Out {
    let mut cmd = sb.command(shim_dir().join(GIT), cwd);
    cmd.env("ASC_REAL_GIT", real_git()).args(args);
    run(pin_identity(&mut cmd))
}

/// Runs real git directly in `cwd`.
fn real(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Out {
    let mut cmd = sb.command(real_git(), cwd);
    cmd.args(args);
    run(pin_identity(&mut cmd))
}

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Out {
    run(sb.asc(cwd).args(args))
}

fn ok(out: Out) -> Out {
    assert_eq!(out.code, 0, "{}", out.stderr);
    out
}

fn append_global(sb: &Sandbox, text: &str) {
    let path = sb.user.join(".gitconfig");
    let mut cur = fs::read_to_string(&path).unwrap_or_default();
    cur.push_str(text);
    fs::write(path, cur).unwrap();
}

/// The synthetic gitdir of the workspace at `root`, from its gitfile.
fn gitdir(root: &Path) -> PathBuf {
    let gitfile = fs::read_to_string(root.join(".git")).expect("gitfile");
    PathBuf::from(gitfile.trim().strip_prefix("gitdir:").expect("gitdir:").trim())
}

fn workspace(sb: &Sandbox, name: &str) -> PathBuf {
    let root = sb.dir(name);
    ok(asc(sb, &root, &["init"]));
    root
}

fn trace_lines(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .map(|t| t.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

// ---- fall-through ------------------------------------------------------------------------

#[test]
fn a_plain_repository_is_byte_identical_to_real_git() {
    let sb = Sandbox::new();
    let (a, b) = (sb.dir("a"), sb.dir("b"));
    for dir in [&a, &b] {
        ok(real(&sb, dir, &["init", "-b", "master"]));
    }
    let steps: &[&[&str]] = &[
        &["status"],
        &["commit", "--allow-empty", "-m", "x"],
        &["log", "-1"],
        &["status", "--porcelain=v2", "--branch"],
        &["commit", "-m", "nothing to commit"],
        &["frobnicate"],
    ];
    for step in steps {
        let (via_shim, direct) = (shim(&sb, &a, step), real(&sb, &b, step));
        assert_eq!(via_shim, direct, "git {step:?}");
    }
}

#[test]
fn fall_through_is_traced_and_a_workspace_is_not() {
    let sb = Sandbox::new();
    let trace = sb.tmp.path().join("trace.log");
    let plain = sb.dir("plain");
    ok(real(&sb, &plain, &["init", "-b", "master"]));
    let traced = |cwd: &Path, args: &[&str]| {
        let mut cmd = sb.command(shim_dir().join(GIT), cwd);
        cmd.env("ASC_REAL_GIT", real_git()).env("ASC_SHIM_TRACE", &trace).args(args);
        run(&mut cmd)
    };
    ok(traced(&plain, &["rev-parse", "--git-dir"]));
    assert_eq!(trace_lines(&trace), ["rev-parse --git-dir"]);

    let ws = workspace(&sb, "ws");
    ok(traced(&ws, &["config", "demo.key", "value"]));
    assert_eq!(ok(traced(&ws, &["config", "demo.key"])).stdout, "value\n");
    let unknown = traced(&ws, &["frobnicate"]);
    assert_eq!(unknown.code, 1);
    assert!(unknown.stderr.contains("'frobnicate' is not a git command"), "{}", unknown.stderr);
    assert_eq!(trace_lines(&trace).len(), 1, "a workspace never reaches real git");
}

#[test]
fn version_names_real_git_and_asc() {
    let sb = Sandbox::new();
    let cwd = sb.dir("none");
    let out = ok(shim(&sb, &cwd, &["--version"]));
    let re = regex::Regex::new(r"^git version \d+\.\d+\.\d+\S*\.asc\.\d+\.\d+\.\d+\S*\n$").unwrap();
    assert!(re.is_match(&out.stdout), "{}", out.stdout);
    assert_eq!(ok(shim(&sb, &cwd, &["version"])).stdout, out.stdout);
}

/// Whether Git for Windows sits at its default place, where resolution falls back to it.
fn default_git_exists() -> bool {
    cfg!(windows)
        && std::env::var_os("ProgramFiles")
            .is_some_and(|p| Path::new(&p).join("Git").join("cmd").join(GIT).is_file())
}

#[test]
fn a_path_with_only_the_shim_fails_cleanly_without_recursion() {
    let sb = Sandbox::new();
    let cwd = sb.dir("none");
    let mut cmd = sb.command(shim_dir().join(GIT), &cwd);
    cmd.env("PATH", shim_dir()).args(["status"]);
    let out = run(&mut cmd);
    assert_eq!(out.code, 128, "{}", out.stderr);
    if !default_git_exists() {
        assert!(out.stderr.contains("real git not found"), "{}", out.stderr);
    }
}

#[test]
fn a_real_git_that_is_the_shim_stops_at_the_depth_guard() {
    let sb = Sandbox::new();
    let cwd = sb.dir("none");
    let mut cmd = sb.command(shim_dir().join(GIT), &cwd);
    cmd.env("ASC_REAL_GIT", shim_dir().join(GIT)).args(["status"]);
    let out = run(&mut cmd);
    assert_eq!(out.code, 128);
    assert!(out.stderr.contains("loop"), "{}", out.stderr);
}

#[test]
fn the_depth_guard_fires() {
    let sb = Sandbox::new();
    let cwd = sb.dir("none");
    let mut cmd = sb.command(shim_dir().join(GIT), &cwd);
    cmd.env("ASC_REAL_GIT", real_git()).env("ASC_SHIM_DEPTH", "5").args(["status"]);
    let out = run(&mut cmd);
    assert_eq!(out.code, 128);
    assert!(out.stderr.contains("loop"), "{}", out.stderr);
}

#[test]
fn fall_through_overhead_is_small() {
    let sb = Sandbox::new();
    let repo = sb.dir("repo");
    ok(real(&sb, &repo, &["init", "-b", "master"]));
    let median = |f: &dyn Fn()| {
        let mut times: Vec<u128> = (0..15)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_micros()
            })
            .collect();
        times.sort_unstable();
        times[times.len() / 2]
    };
    let direct = median(&|| {
        real(&sb, &repo, &["rev-parse", "--git-dir"]);
    });
    let shimmed = median(&|| {
        shim(&sb, &repo, &["rev-parse", "--git-dir"]);
    });
    // The SPEC §11.4 budget (p95 5 ms) is measured by O05; this guards against a gross slowdown.
    let extra_ms = shimmed.saturating_sub(direct) / 1000;
    assert!(extra_ms < 100, "shim adds {extra_ms} ms to a {direct} us git call");
}

// ---- routing -----------------------------------------------------------------------------

#[test]
fn init_gets_the_master_default_unless_configured() {
    let sb = Sandbox::new();
    let cwd = sb.dir("none");
    ok(shim(&sb, &cwd, &["init", "x"]));
    assert_eq!(fs::read_to_string(cwd.join("x/.git/HEAD")).unwrap().trim(), "ref: refs/heads/master");
    append_global(&sb, "[init]\n\tdefaultBranch = trunk\n");
    ok(shim(&sb, &cwd, &["init", "y"]));
    assert_eq!(fs::read_to_string(cwd.join("y/.git/HEAD")).unwrap().trim(), "ref: refs/heads/trunk");
}

#[test]
fn asc_shim_init_routes_init_to_a_asc_workspace_without_a_ascignore() {
    let sb = Sandbox::new();
    append_global(&sb, "[asc \"shim\"]\n\tinit = asc\n");
    let cwd = sb.dir("none");
    ok(shim(&sb, &cwd, &["init", "x"]));
    let root = cwd.join("x");
    assert!(root.join(".git").is_file(), "a gitfile, not a git directory");
    assert!(gitdir(&root).join("asc-workspace").is_file());
    assert!(!root.join(".ascignore").exists(), "argv[0] = git never adds asc-only files");
}

#[test]
fn real_git_clone_and_init_inside_a_workspace_make_nested_repositories() {
    let sb = Sandbox::new();
    let src = sb.dir("src");
    ok(real(&sb, &src, &["init", "-b", "master"]));
    ok(real(&sb, &src, &["commit", "--allow-empty", "-m", "x"]));
    let bare = sb.tmp.path().join("bare.git");
    ok(real(&sb, sb.tmp.path(), &["clone", "--bare", src.to_str().unwrap(), bare.to_str().unwrap()]));

    let ws = workspace(&sb, "ws");
    ok(shim(&sb, &ws, &["clone", bare.to_str().unwrap(), "sub"]));
    assert!(ws.join("sub/.git").is_dir(), "a real git directory");
    assert!(!ws.join("sub/.git/asc-workspace").exists());
    assert_eq!(ok(shim(&sb, &ws, &["-C", "sub", "log", "-1", "--format=%s"])).stdout, "x\n");

    let tmp_bare = sb.tmp.path().join("scratch/x");
    ok(shim(&sb, &ws, &["clone", "--bare", bare.to_str().unwrap(), tmp_bare.to_str().unwrap()]));
    assert!(tmp_bare.join("HEAD").is_file());

    ok(shim(&sb, &ws, &["init", "fixtures/tmp"]));
    ok(shim(&sb, &ws, &["-C", "fixtures/tmp", "commit", "--allow-empty", "-m", "x"]));
    assert!(ws.join("fixtures/tmp/.git").is_dir());
    // The parent sees the nested repositories as untracked directories (SPEC 9.3).
    let status = ok(shim(&sb, &ws, &["status", "--porcelain"]));
    assert!(status.stdout.contains("?? fixtures/"), "{}", status.stdout);
    assert!(status.stdout.contains("?? sub/"), "{}", status.stdout);
}

#[test]
fn repository_independent_commands_ignore_the_workspace() {
    let sb = Sandbox::new();
    let ws = workspace(&sb, "ws");
    let trace = sb.tmp.path().join("trace.log");
    let traced = |args: &[&str]| {
        let mut cmd = sb.command(shim_dir().join(GIT), &ws);
        cmd.env("ASC_REAL_GIT", real_git()).env("ASC_SHIM_TRACE", &trace).args(args);
        run(&mut cmd)
    };
    let global = sb.user.join(".gitconfig");
    ok(traced(&["config", "--file", global.to_str().unwrap(), "demo.k", "v"]));
    ok(traced(&["config", "--global", "demo.k2", "v2"]));
    let lines = trace_lines(&trace);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(ok(traced(&["config", "--global", "demo.k"])).stdout, "v\n");
    assert!(fs::read_to_string(global).unwrap().contains("k2 = v2"));
}

// ---- install-shim ------------------------------------------------------------------------

/// A directory link: a junction on Windows, a symlink elsewhere.
fn link_dir(link: &Path, target: &Path) -> bool {
    let _ = fs::remove_dir(link);
    if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|o| o.status.success())
    } else {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link).is_ok();
        #[allow(unreachable_code)]
        false
    }
}

#[test]
fn install_shim_writes_git_into_a_chosen_directory() {
    let sb = Sandbox::new();
    let dir = bin().parent().unwrap().join("asc-test-install").join(format!("d{}", std::process::id()));
    let out = ok(asc(&sb, sb.tmp.path(), &["install-shim", "--dir", dir.to_str().unwrap()]));
    assert!(out.stdout.contains(dir.to_str().unwrap()), "the PATH line: {}", out.stdout);
    let git = dir.join(GIT);
    assert_eq!(fs::metadata(&git).unwrap().len(), fs::metadata(bin()).unwrap().len());
    // Running it again relinks without error.
    ok(asc(&sb, sb.tmp.path(), &["install-shim", "--dir", dir.to_str().unwrap()]));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn install_shim_relinks_through_the_current_junction() {
    let sb = Sandbox::new();
    let prefix = bin().parent().unwrap().join("asc-test-prefix").join(format!("p{}", std::process::id()));
    let _ = fs::remove_dir_all(&prefix);
    for ver in ["v1", "v2"] {
        link_binary(&prefix.join(ver).join(EXE));
    }
    let current = prefix.join("current");
    if !link_dir(&current, &prefix.join("v1")) {
        eprintln!("skipped: cannot create a directory link here");
        return;
    }
    let run_install = |sb: &Sandbox| {
        let mut cmd = sb.command(current.join(EXE), sb.tmp.path());
        ok(run(cmd.arg("install-shim")))
    };
    run_install(&sb);
    assert!(prefix.join("v1/shim").join(GIT).is_file());
    assert!(!prefix.join("v2/shim").exists());
    assert!(link_dir(&current, &prefix.join("v2")));
    let line = run_install(&sb).stdout;
    assert!(prefix.join("v2/shim").join(GIT).is_file());
    assert!(line.contains("current"), "the PATH line uses the stable path: {line}");
    let _ = fs::remove_dir(&current);
    let _ = fs::remove_dir_all(&prefix);
}
