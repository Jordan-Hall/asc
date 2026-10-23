//! U20 acceptance: the CLI frame, `init`, `config`, dispatch and the child environment, driven
//! through the built binary. No sibling command is used (PLAN "Acceptance before U25").

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use support::{Sandbox, asc_binary, text};

fn bin() -> &'static Path {
    Path::new(asc_binary().expect("integration test"))
}

/// A copy (hardlink) of the binary named `git`, so argv[0] selects the git program.
fn git_binary() -> &'static Path {
    static GIT: OnceLock<PathBuf> = OnceLock::new();
    GIT.get_or_init(|| {
        let dir = bin().parent().unwrap().join("asc-test-git");
        fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        let target = dir.join(name);
        if !target.exists() && fs::hard_link(bin(), &target).is_err() && !target.exists() {
            fs::copy(bin(), &target).unwrap();
        }
        target
    })
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut std::process::Command) -> Out {
    let out = cmd.output().expect("spawn");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
    }
}

fn asc(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Out {
    run(sb.asc(cwd).args(args))
}

fn git(sb: &Sandbox, cwd: &Path, args: &[&str]) -> Out {
    run(sb.command(git_binary(), cwd).args(args))
}

/// The synthetic gitdir of the workspace at `root`, from its gitfile.
fn gitdir(root: &Path) -> PathBuf {
    let gitfile = fs::read_to_string(root.join(".git")).expect("gitfile");
    let target = gitfile.trim().strip_prefix("gitdir:").expect("gitdir:").trim();
    PathBuf::from(target)
}

fn head(root: &Path) -> String {
    fs::read_to_string(gitdir(root).join("HEAD")).unwrap().trim().to_owned()
}

fn append_global(sb: &Sandbox, text: &str) {
    let path = sb.user.join(".gitconfig");
    let mut cur = fs::read_to_string(&path).unwrap_or_default();
    cur.push_str(text);
    fs::write(path, cur).unwrap();
}

/// A workspace created by `asc init` in a fresh directory.
fn init_repo(sb: &Sandbox, name: &str) -> PathBuf {
    let root = sb.dir(name);
    let out = asc(sb, &root, &["init"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    root
}

// ---- init -------------------------------------------------------------------------------

#[test]
fn init_makes_a_master_workspace_with_an_empty_home() {
    let sb = Sandbox::new();
    let root = sb.dir("repo");
    let out = asc(&sb, &root, &[]);
    assert_eq!(out.code, 1, "no command prints usage and exits 1");
    let out = asc(&sb, &root, &["init"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("Initialized empty Git repository in "), "{}", out.stdout);
    assert!(out.stdout.trim_end().ends_with("/.git/"), "{}", out.stdout);
    assert_eq!(head(&root), "ref: refs/heads/master");
    assert!(gitdir(&root).join("asc-workspace").is_file(), "the shim marker");
    // The excludes carry the Claude Code worktree directory (SPEC §2.4.2).
    let exclude = fs::read_dir(sb.asc_home.join("repos"))
        .unwrap()
        .map(|e| e.unwrap().path().join(".git/info/exclude"))
        .next()
        .unwrap();
    assert!(fs::read_to_string(exclude).unwrap().contains(".claude/worktrees/"));
}

#[test]
fn init_honours_init_default_branch_and_dash_b() {
    let sb = Sandbox::new();
    let a = sb.dir("a");
    let out = asc(&sb, &a, &["-c", "init.defaultBranch=trunk", "init"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(head(&a), "ref: refs/heads/trunk");

    append_global(&sb, "[init]\n\tdefaultBranch = develop\n");
    let b = sb.dir("b");
    assert_eq!(asc(&sb, &b, &["init"]).code, 0);
    assert_eq!(head(&b), "ref: refs/heads/develop");

    let c = sb.dir("c");
    assert_eq!(asc(&sb, &c, &["init", "-b", "feature"]).code, 0);
    assert_eq!(head(&c), "ref: refs/heads/feature");

    let d = sb.dir("d");
    let bad = asc(&sb, &d, &["init", "-b", "a..b"]);
    assert_eq!(bad.code, 128);
    assert_eq!(bad.stderr, "fatal: invalid initial branch name: 'a..b'\n");
}

#[test]
fn init_dash_c_dir_and_a_dir_argument() {
    let sb = Sandbox::new();
    let parent = sb.dir("parent");
    let out = asc(&sb, &parent, &["-C", parent.to_str().unwrap(), "init", "child"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(parent.join("child/.git").is_file());
    let out = asc(&sb, &parent, &["-C", "child", "init", "--quiet"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, "", "reinitialising is quiet with -q");
    let again = asc(&sb, &parent.join("child"), &["init"]);
    assert!(again.stdout.starts_with("Reinitialized existing Git repository in "), "{}", again.stdout);
    let missing = asc(&sb, &parent, &["-C", "nowhere", "status"]);
    assert_eq!(missing.code, 128);
    assert!(missing.stderr.starts_with("fatal: cannot change to 'nowhere': "), "{}", missing.stderr);
}

#[test]
fn init_writes_a_ascignore_only_without_a_gitignore() {
    let sb = Sandbox::new();
    let plain = sb.dir("plain");
    assert_eq!(asc(&sb, &plain, &["init"]).code, 0);
    let ignore = fs::read_to_string(plain.join(".ascignore")).unwrap();
    assert_eq!(ignore.lines().next(), Some("# asc: replaces .gitignore in this directory"));

    let has = sb.dir("has");
    fs::write(has.join(".gitignore"), "target/\n").unwrap();
    assert_eq!(asc(&sb, &has, &["init"]).code, 0);
    assert!(!has.join(".ascignore").exists(), "a .gitignore is kept as the ignore file");

    // Invoked as `git` (argv[0]), init never adds asc-only files. `git init` is real git unless
    // the global config routes it to asc (SPEC §11.4).
    append_global(&sb, "[asc \"shim\"]\n\tinit = asc\n");
    let as_git = sb.dir("as-git");
    let out = git(&sb, &as_git, &["init"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(as_git.join(".git").is_file());
    assert!(!as_git.join(".ascignore").exists());
}

#[test]
fn init_refuses_a_real_git_directory() {
    let sb = Sandbox::new();
    let root = sb.dir("real");
    fs::create_dir_all(root.join(".git")).unwrap();
    let out = asc(&sb, &root, &["init"]);
    assert_eq!(out.code, 128);
    assert!(out.stderr.contains("real git directory"), "{}", out.stderr);
}

// ---- config -----------------------------------------------------------------------------

#[test]
fn config_set_get_list_unset() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    assert_eq!(asc(&sb, &root, &["config", "demo.key", "value"]).code, 0);
    let got = asc(&sb, &root, &["config", "demo.key"]);
    assert_eq!((got.code, got.stdout.as_str()), (0, "value\n"));
    let missing = asc(&sb, &root, &["config", "--get", "demo.missing"]);
    assert_eq!((missing.code, missing.stdout.as_str()), (1, ""));

    for v in ["a", "b"] {
        assert_eq!(asc(&sb, &root, &["config", "--add", "demo.multi", v]).code, 0);
    }
    let all = asc(&sb, &root, &["config", "--get-all", "demo.multi"]);
    assert_eq!(all.stdout, "a\nb\n");
    assert_eq!(asc(&sb, &root, &["config", "demo.multi", "c"]).code, 5, "would overwrite two");
    assert_eq!(asc(&sb, &root, &["config", "--replace-all", "demo.multi", "c"]).code, 0);
    assert_eq!(asc(&sb, &root, &["config", "--get-all", "demo.multi"]).stdout, "c\n");

    let list = asc(&sb, &root, &["config", "--list", "--local"]);
    assert!(list.stdout.contains("demo.key=value\n"), "{}", list.stdout);
    assert!(list.stdout.contains("core.bare=false\n"), "{}", list.stdout);
    let origin = asc(&sb, &root, &["config", "--list", "--show-origin", "--local"]);
    assert!(origin.stdout.lines().all(|l| l.starts_with("file:")), "{}", origin.stdout);

    assert_eq!(asc(&sb, &root, &["config", "--unset", "demo.key"]).code, 0);
    assert_eq!(asc(&sb, &root, &["config", "demo.key"]).code, 1);
    assert_eq!(asc(&sb, &root, &["config", "--unset", "demo.key"]).code, 5, "nothing to unset");
}

#[test]
fn config_types_regexp_and_subsections() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    for (k, v) in [("demo.flag", "yes"), ("demo.size", "2k"), ("remote.origin.url", "https://x/y")] {
        assert_eq!(asc(&sb, &root, &["config", k, v]).code, 0);
    }
    assert_eq!(asc(&sb, &root, &["config", "--bool", "demo.flag"]).stdout, "true\n");
    assert_eq!(asc(&sb, &root, &["config", "--int", "demo.size"]).stdout, "2048\n");
    let re = asc(&sb, &root, &["config", "--get-regexp", "^remote\\."]);
    assert_eq!(re.stdout, "remote.origin.url https://x/y\n");
    let names = asc(&sb, &root, &["config", "--get-regexp", "--name-only", "^demo\\."]);
    assert_eq!(names.stdout, "demo.flag\ndemo.size\n");
    let nul = asc(&sb, &root, &["config", "-z", "--get", "demo.flag"]);
    assert_eq!(nul.stdout, "yes\0");
    assert_eq!(asc(&sb, &root, &["config", "--remove-section", "demo"]).code, 0);
    assert_eq!(asc(&sb, &root, &["config", "demo.flag"]).code, 1);
    assert_eq!(asc(&sb, &root, &["config", "--rename-section", "remote.origin", "remote.up"]).code, 0);
    assert_eq!(asc(&sb, &root, &["config", "remote.up.url"]).stdout, "https://x/y\n");
}

#[test]
fn config_scopes_defaults_and_errors() {
    let sb = Sandbox::new();
    let outside = sb.dir("outside");
    // --global needs no workspace and no $ASC_HOME.
    assert_eq!(asc(&sb, &outside, &["config", "--global", "user.name", "Zed"]).code, 0);
    assert!(fs::read_to_string(sb.user.join(".gitconfig")).unwrap().contains("name = Zed"));
    assert_eq!(asc(&sb, &outside, &["config", "user.name"]).stdout, "Zed\n");
    assert!(!sb.asc_home.exists(), "config outside a workspace must not create $ASC_HOME");
    // A local write needs a repository.
    let local = asc(&sb, &outside, &["config", "demo.key", "v"]);
    assert_eq!((local.code, local.stderr.as_str()), (128, "fatal: not in a git directory\n"));
    // -f works anywhere.
    let file = outside.join("custom.cfg");
    assert_eq!(asc(&sb, &outside, &["config", "-f", "custom.cfg", "a.b", "c"]).code, 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), "[a]\n\tb = c\n");
    let unreadable = asc(&sb, &outside, &["config", "-f", "nope.cfg", "--get", "a.b"]);
    assert_eq!(unreadable.code, 128);
    assert!(unreadable.stderr.starts_with("fatal: unable to read config file 'nope.cfg': "), "{}", unreadable.stderr);
    // Key validation.
    for (key, msg) in [
        ("nokey", "error: key does not contain a section: nokey\n"),
        ("sec.", "error: key does not contain variable name: sec.\n"),
        ("1a.b c", "error: invalid key: 1a.b c\n"),
    ] {
        let out = asc(&sb, &outside, &["config", "--get", key]);
        assert_eq!((out.code, out.stderr.as_str()), (1, msg), "{key}");
    }
}

#[test]
fn config_dash_c_and_environment_layers() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    let out = asc(&sb, &root, &["-c", "demo.cli=from-c", "config", "demo.cli"]);
    assert_eq!(out.stdout, "from-c\n");
    let mut cmd = sb.asc(&root);
    cmd.env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "demo.env")
        .env("GIT_CONFIG_VALUE_0", "from-env")
        .args(["config", "demo.env"]);
    assert_eq!(run(&mut cmd).stdout, "from-env\n");
    let mut cmd = sb.asc(&root);
    cmd.env("TOKEN", "s3cret").args(["--config-env=demo.tok=TOKEN", "config", "demo.tok"]);
    assert_eq!(run(&mut cmd).stdout, "s3cret\n");
    let missing = asc(&sb, &root, &["--config-env=demo.tok=NO_SUCH_VAR", "config", "demo.tok"]);
    assert_eq!(missing.code, 128);
    assert_eq!(
        missing.stderr,
        "fatal: missing environment variable 'NO_SUCH_VAR' for configuration 'demo.tok'\n"
    );
}

#[test]
fn config_flags_outside_the_table_exit_129() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    let out = asc(&sb, &root, &["config", "--bogus"]);
    assert_eq!(out.code, 129);
    assert_eq!(out.stderr, "fatal: asc: option '--bogus' is not supported\n");
    assert_eq!(out.stdout, "");
    let out = asc(&sb, &root, &["config", "--list", "--bogus=1"]);
    assert_eq!(out.stderr, "fatal: asc: option '--bogus' is not supported\n");
}

// ---- the frame: globals, aliases, children ----------------------------------------------

#[test]
fn global_option_errors_use_git_wording() {
    let sb = Sandbox::new();
    let dir = sb.dir("d");
    let out = asc(&sb, &dir, &["--foo"]);
    assert_eq!(out.code, 129);
    assert!(out.stderr.starts_with("unknown option: --foo\nusage: asc [-v | --version]"), "{}", out.stderr);
    let out = asc(&sb, &dir, &["-C"]);
    assert_eq!(out.code, 129);
    assert!(out.stderr.starts_with("no directory given for '-C' option\n"), "{}", out.stderr);
    let out = asc(&sb, &dir, &["-c"]);
    assert!(out.stderr.starts_with("-c expects a configuration string\n"), "{}", out.stderr);
    let out = asc(&sb, &dir, &["--bare", "status"]);
    assert_eq!((out.code, out.stderr.as_str()), (129, "fatal: asc: option '--bare' is not supported\n"));
}

#[test]
fn version_lines() {
    let sb = Sandbox::new();
    let dir = sb.dir("d");
    let out = asc(&sb, &dir, &["--version"]);
    assert_eq!(out.code, 0);
    assert!(out.stdout.starts_with("asc 0.1.0 (git "), "{}", out.stdout);
    let out = git(&sb, &dir, &["--version"]);
    assert_eq!(out.code, 0);
    let rest = out.stdout.strip_prefix("git version ").expect("git version prefix");
    let mut digits = rest.split('.');
    for _ in 0..3 {
        assert!(digits.next().unwrap().chars().all(|c| c.is_ascii_digit()), "{}", out.stdout);
    }
    assert!(out.stdout.trim_end().ends_with(".asc.0.1.0"), "{}", out.stdout);
}

#[test]
fn alias_to_config_and_alias_chains() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    assert_eq!(asc(&sb, &root, &["config", "demo.key", "value"]).code, 0);
    append_global(
        &sb,
        "[alias]\n\tshowconfig = config\n\tshowconfigget = showconfig --get\n\tloopone = looptwo\n\tlooptwo = loopone\n",
    );
    assert_eq!(asc(&sb, &root, &["showconfig", "demo.key"]).stdout, "value\n");
    assert_eq!(asc(&sb, &root, &["showconfigget", "demo.key"]).stdout, "value\n");
    let out = asc(&sb, &root, &["-c", "alias.q=config --get", "q", "demo.key"]);
    assert_eq!(out.stdout, "value\n");
    let looped = asc(&sb, &root, &["loopone"]);
    assert_eq!(looped.code, 128);
    assert!(
        looped.stderr.starts_with("fatal: alias loop detected: expansion of 'loopone' does not terminate:"),
        "{}",
        looped.stderr
    );
    // ASC_NO_ALIASES=1 switches alias resolution off.
    let mut cmd = sb.asc(&root);
    cmd.env("ASC_NO_ALIASES", "1").args(["showconfig", "demo.key"]);
    let off = run(&mut cmd);
    assert_eq!(off.code, 1);
    assert!(
        off.stderr.starts_with("asc: 'showconfig' is not a git command. See 'asc --help'.\n"),
        "{}",
        off.stderr
    );
}

#[test]
fn dash_c_reaches_children_as_git_config_parameters() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    append_global(&sb, "[alias]\n\tshowcfg = !echo $GIT_CONFIG_PARAMETERS\n\tshowdepth = !echo $ASC_SHIM_DEPTH\n");
    let out = asc(&sb, &root, &["-c", "demo.one=1", "-c", "demo.two=a b", "showcfg"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, "'demo.one=1' 'demo.two=a b'\n");
    let out = asc(&sb, &root, &["showdepth"]);
    assert_eq!(out.stdout, "1\n", "children run one level deeper");
}

/// `/c/dir/git` or `C:\dir\git.exe` as a comparable lowercase `c:/dir/git`.
fn comparable(path: &str) -> String {
    let mut p = path.trim().replace('\\', "/");
    if let Some(rest) = p.strip_prefix('/')
        && rest.len() > 1
        && rest.as_bytes()[1] == b'/'
        && rest.as_bytes()[0].is_ascii_alphabetic()
    {
        p = format!("{}:{}", &rest[..1], &rest[1..]);
    }
    p.trim_end_matches(".exe").to_ascii_lowercase()
}

#[test]
fn shell_alias_finds_the_private_shim_dir_before_real_git() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    append_global(&sb, "[alias]\n\twhere = \"!sh -c \\\"command -v git\\\"\"\n");
    // The inherited PATH may hold real git first; the child PATH must still lead with the shim.
    let out = asc(&sb, &root, &["where"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let want = bin().parent().unwrap().join("shim").join("git");
    assert_eq!(comparable(&out.stdout), comparable(&want.display().to_string()));
}

// ---- dispatch (argv[0] = git) -----------------------------------------------------------

#[test]
fn git_prefers_aliases_then_git_externals_then_asc_verbs() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    append_global(&sb, "[alias]\n\tgraph = tag --list\n");
    // `git graph` is the alias (it expands to `tag`, which has not landed).
    let out = git(&sb, &root, &["graph"]);
    assert_eq!(out.code, 129);
    assert_eq!(out.stderr, "fatal: asc: 'tag' is not supported yet\n");
    // Under `asc`, the asc-only verb `graph` wins and is also not built yet.
    let out = asc(&sb, &root, &["graph"]);
    assert_eq!((out.code, out.stderr.as_str()), (129, "fatal: asc: 'graph' is not supported yet\n"));

    // A `git-affected` script on PATH wins over the asc-only `affected` when invoked as git.
    let bindir = sb.dir("bin");
    let script = if cfg!(windows) { "git-affected.cmd" } else { "git-affected" };
    let body = if cfg!(windows) { "@echo external-affected\r\n" } else { "#!/bin/sh\necho external-affected\n" };
    fs::write(bindir.join(script), body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bindir.join(script), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths(
        std::iter::once(bindir.clone()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let mut cmd = sb.command(git_binary(), &root);
    cmd.env("PATH", &path).arg("affected");
    let out = run(&mut cmd);
    assert_eq!(out.stdout.trim(), "external-affected", "{}", out.stderr);
    // Without the script, the asc verb answers.
    let out = git(&sb, &root, &["affected"]);
    assert_eq!((out.code, out.stderr.as_str()), (129, "fatal: asc: 'affected' is not supported yet\n"));
}

#[test]
fn git_unknown_and_unavailable_commands() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    let out = git(&sb, &root, &["daemon"]);
    assert_eq!(out.code, 129);
    assert_eq!(out.stderr, "fatal: asc: 'daemon' is not supported in an asc workspace\n");
    let out = git(&sb, &root, &["lfs", "ls-files"]);
    assert_eq!(out.stderr, "fatal: asc: 'lfs' is not supported in an asc workspace\n");
    let out = git(&sb, &root, &["nosuch"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "git: 'nosuch' is not a git command. See 'git --help'.\n");
    let out = asc(&sb, &root, &["nosuch"]);
    assert_eq!(out.stderr, "asc: 'nosuch' is not a git command. See 'asc --help'.\n");
    // A git builtin nobody serves.
    let out = git(&sb, &root, &["bugreport"]);
    assert_eq!(out.stderr, "fatal: asc: 'bugreport' is not supported in an asc workspace\n");
}

#[test]
fn autocorrect_never_picks_a_asc_only_verb() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    // Suggestions list git commands only.
    let out = git(&sb, &root, &["sttaus"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("The most similar command is\n\tstatus\n"), "{}", out.stderr);
    // `immediate` runs the single git match.
    let out = git(&sb, &root, &["-c", "help.autocorrect=immediate", "sttaus"]);
    assert!(out.stderr.contains("WARNING: You called a Git command named 'sttaus'"), "{}", out.stderr);
    assert!(out.stderr.contains("Continuing under the assumption that you meant 'status'."), "{}", out.stderr);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with("On branch master\n"), "the corrected command ran: {}", out.stdout);
    // A near miss of an asc-only verb is not corrected to it.
    let out = git(&sb, &root, &["-c", "help.autocorrect=immediate", "projectz"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.starts_with("git: 'projectz' is not a git command."), "{}", out.stderr);
    assert!(!out.stderr.contains("WARNING"), "{}", out.stderr);
    assert!(!out.stderr.contains("projects'"), "{}", out.stderr);
    // `never` prints no suggestions.
    let out = git(&sb, &root, &["-c", "help.autocorrect=never", "sttaus"]);
    assert_eq!(out.stderr, "git: 'sttaus' is not a git command. See 'git --help'.\n");
}

#[test]
fn commands_not_yet_landed_exit_129() {
    let sb = Sandbox::new();
    let root = init_repo(&sb, "repo");
    for cmd in ["branch", "tag", "merge", "stash", "rebase", "push"] {
        let out = git(&sb, &root, &[cmd, "--any-flag"]);
        assert_eq!(out.code, 129, "{cmd}");
        assert_eq!(out.stderr, format!("fatal: asc: '{cmd}' is not supported yet\n"), "{cmd}");
        assert_eq!(out.stdout, "");
    }
}

#[test]
fn the_frame_leaves_no_stray_output() {
    // The no-op sinks (ext.rs) must not change what a command prints.
    let sb = Sandbox::new();
    let root = sb.dir("quiet");
    let out = asc(&sb, &root, &["init", "-q"]);
    assert_eq!((out.code, out.stdout.as_str(), out.stderr.as_str()), (0, "", ""));
    let out = asc(&sb, &root, &["config", "--local", "--list", "--name-only"]);
    assert_eq!(out.stderr, "");
}
