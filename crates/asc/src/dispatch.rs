//! Program detection, global options, alias expansion, external commands and verb precedence
//! (SPEC §11.1, §11.4). `resolve` is pure so the precedence is tested without a workspace.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cli::common::{CliError, Ctx, Result};
use crate::cli;
use crate::env::{EnvError, GitEnv};
use crate::msg::Msg;

/// How the binary was invoked (argv[0], SPEC §11.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Program {
    Asc,
    Git,
}

impl Program {
    /// `git` when argv[0]'s file stem is `git`, ignoring case and `.exe`; `asc` otherwise.
    pub fn from_argv0(argv0: &OsStr) -> Self {
        let stem = Path::new(argv0)
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase());
        if stem.as_deref() == Some("git") {
            Self::Git
        } else {
            Self::Asc
        }
    }

    /// The name the program prints in hints.
    pub fn name(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Git => "git",
        }
    }
}

/// The builtins of git 2.55 (`git --list-cmds=builtins`).
pub const GIT_BUILTINS: &[&str] = &[
    "add", "am", "annotate", "apply", "archive", "backfill", "bisect", "blame", "branch",
    "bugreport", "bundle", "cat-file", "check-attr", "check-ignore", "check-mailmap",
    "check-ref-format", "checkout", "checkout--worker", "checkout-index", "cherry", "cherry-pick",
    "clean", "clone", "column", "commit", "commit-graph", "commit-tree", "config", "count-objects",
    "credential", "credential-cache", "credential-cache--daemon", "credential-store", "describe",
    "diagnose", "diff", "diff-files", "diff-index", "diff-pairs", "diff-tree", "difftool",
    "fast-export", "fast-import", "fetch", "fetch-pack", "fmt-merge-msg", "for-each-ref",
    "for-each-repo", "format-patch", "format-rev", "fsck", "fsck-objects", "fsmonitor--daemon",
    "gc", "get-tar-commit-id", "grep", "hash-object", "help", "history", "hook", "index-pack",
    "init", "init-db", "interpret-trailers", "last-modified", "log", "ls-files", "ls-remote",
    "ls-tree", "mailinfo", "mailsplit", "maintenance", "merge", "merge-base", "merge-file",
    "merge-index", "merge-ours", "merge-recursive", "merge-recursive-ours",
    "merge-recursive-theirs", "merge-subtree", "merge-tree", "mktag", "mktree",
    "multi-pack-index", "mv", "name-rev", "notes", "pack-objects", "pack-redundant", "pack-refs",
    "patch-id", "pickaxe", "prune", "prune-packed", "pull", "push", "range-diff", "read-tree",
    "rebase", "receive-pack", "reflog", "refs", "remote", "remote-ext", "remote-fd", "repack",
    "replace", "replay", "repo", "rerere", "reset", "restore", "rev-list", "rev-parse", "revert",
    "rm", "send-pack", "shortlog", "show", "show-branch", "show-index", "show-ref",
    "sparse-checkout", "stage", "stash", "status", "stripspace", "submodule--helper", "survey",
    "switch", "symbolic-ref", "tag", "unpack-file", "unpack-objects", "update-index", "update-ref",
    "update-server-info", "upload-archive", "upload-archive--writer", "upload-pack", "url-parse",
    "var", "verify-commit", "verify-pack", "verify-tag", "version", "whatchanged", "worktree",
    "write-tree",
];

/// Git commands that are not available in an asc workspace (SPEC §11.2), whatever the program.
pub const UNAVAILABLE: &[&str] = &[
    "filter-branch",
    "filter-repo",
    "replace",
    "svn",
    "p4",
    "send-email",
    "instaweb",
    "lfs",
];

/// The asc-only verbs (SPEC §11.2, §12A.9, §20.7, §21, §22, §23). They never shadow a git
/// command and are never autocorrect candidates.
pub const ASC_ONLY_VERBS: &[&str] = &[
    "daemon", "serve", "import", "export", "auth", "doctor", "prefetch", "sparse", "debug", "ci",
    "affected", "projects", "graph", "landed", "api", "watch", "search", "sym", "semantic",
    "codeintel", "mcp", "install-shim", "migrate", "hide", "unhide", "cloud", "upgrade", "internal",
    "create", "modify", "absorb", "restack", "continue", "abort", "sync", "submit", "sl",
    "smartlog", "up", "down", "top", "bottom", "next", "prev", "reword", "uncommit", "unamend",
    "abandon", "change-id", "split", "squash", "fold", "move", "reorder", "stack", "undo", "redo",
    "op", "evolog", "interdiff", "share",
];

/// Whether `cmd` is unavailable in every asc workspace.
pub fn is_unavailable(cmd: &str) -> bool {
    UNAVAILABLE.contains(&cmd)
}

/// Whether `name` is a git command for `prog`: a 2.55 builtin or an unavailable one. `daemon` is
/// git's command under `git` (so `git daemon` is unsupported) and asc's own under `asc`.
fn is_git_command(prog: Program, name: &str) -> bool {
    GIT_BUILTINS.contains(&name)
        || is_unavailable(name)
        || (prog == Program::Git && name == "daemon")
}

/// How a command word resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// A builtin or asc-only verb; the native table decides the rest.
    Command(String),
    /// A git command no asc workspace offers.
    Unavailable(String),
    /// `alias.<name>`: the raw value (`!cmd` is a shell alias).
    Alias(String),
    /// `git-<name>` / `asc-<name>` found on `PATH` or in git's exec-path.
    External(PathBuf),
    /// Nothing matched.
    NotACommand,
}

/// What `resolve` asks of the environment.
pub trait Lookup {
    /// The value of `alias.<name>`; `None` when unset or `ASC_NO_ALIASES=1`.
    fn alias(&self, name: &str) -> Option<String>;
    /// An executable named `<prefix><name>`.
    fn external(&self, prefix: &str, name: &str) -> Option<PathBuf>;
}

/// Resolves `name` in git's order (SPEC §11.4). Under `git`: builtin, alias, `git-<name>`,
/// asc-only verb. Under `asc`: builtin, asc-only verb, alias, `asc-<name>`, `git-<name>`.
pub fn resolve(prog: Program, name: &str, lookup: &dyn Lookup) -> Resolved {
    let command = || {
        if is_unavailable(name) || (prog == Program::Git && name == "daemon") {
            Resolved::Unavailable(name.to_owned())
        } else {
            Resolved::Command(name.to_owned())
        }
    };
    let asc_only = ASC_ONLY_VERBS.contains(&name);
    match prog {
        Program::Git => {
            if is_git_command(prog, name) {
                return command();
            }
            if let Some(a) = lookup.alias(name) {
                return Resolved::Alias(a);
            }
            if let Some(p) = lookup.external("git-", name) {
                return Resolved::External(p);
            }
            if asc_only {
                return Resolved::Command(name.to_owned());
            }
        }
        Program::Asc => {
            if is_git_command(prog, name) || asc_only {
                return command();
            }
            if let Some(a) = lookup.alias(name) {
                return Resolved::Alias(a);
            }
            if let Some(p) = lookup
                .external("asc-", name)
                .or_else(|| lookup.external("git-", name))
            {
                return Resolved::External(p);
            }
        }
    }
    Resolved::NotACommand
}

/// Splits an alias value like git's `split_cmdline`: words separated by white space, with
/// single and double quotes and backslash escapes.
pub fn split_cmdline(text: &str) -> std::result::Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(q) if c == '\\' && q != '\'' => {
                cur.push(chars.next().ok_or("cmdline ends with \\")?);
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                have = true;
            }
            None if c.is_whitespace() => {
                if have || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            None if c == '\\' => {
                cur.push(chars.next().ok_or("cmdline ends with \\")?);
                have = true;
            }
            None => {
                cur.push(c);
                have = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unclosed quote");
    }
    if have || !cur.is_empty() {
        words.push(cur);
    }
    Ok(words)
}

/// git's `levenshtein` with its autocorrect costs: swap 0, substitution 2, insertion 1,
/// deletion 3.
pub fn levenshtein(a: &str, b: &str) -> u32 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (w, s, ins, del) = (0u32, 2u32, 1u32, 3u32);
    let mut row0 = vec![0u32; b.len() + 1];
    let mut row1: Vec<u32> = (0..=b.len() as u32).map(|j| j * ins).collect();
    let mut row2 = vec![0u32; b.len() + 1];
    for i in 0..a.len() {
        row2[0] = (i as u32 + 1) * del;
        for j in 0..b.len() {
            let mut v = row1[j] + if a[i] == b[j] { 0 } else { s };
            if i > 0 && j > 0 && a[i - 1] == b[j] && a[i] == b[j - 1] && v > row0[j - 1] + w {
                v = row0[j - 1] + w;
            }
            v = v.min(row1[j + 1] + del);
            v = v.min(row2[j] + ins);
            row2[j + 1] = v;
        }
        std::mem::swap(&mut row0, &mut row1);
        std::mem::swap(&mut row1, &mut row2);
    }
    row1[b.len()]
}

/// Distances under this count as similar (`SIMILARITY_FLOOR` in git's `help.c`).
const SIMILARITY_FLOOR: u32 = 7;

/// The candidates at the smallest distance, if that distance is under the floor. Asc-only verbs
/// are never candidates: a typo of a git command is corrected as in git (SPEC §11.4).
pub fn similar<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut scored: Vec<(u32, &str)> = candidates
        .filter(|c| !ASC_ONLY_VERBS.contains(c) || GIT_BUILTINS.contains(c))
        .map(|c| (levenshtein(name, c), c))
        .collect();
    scored.sort();
    let Some(&(best, _)) = scored.first() else {
        return Vec::new();
    };
    if best >= SIMILARITY_FLOOR {
        return Vec::new();
    }
    let mut out: Vec<String> = scored
        .into_iter()
        .take_while(|(d, _)| *d == best)
        .map(|(_, c)| c.to_owned())
        .collect();
    out.dedup();
    out
}

/// `help.autocorrect`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autocorrect {
    /// `never`: no suggestions at all.
    Never,
    /// Unset, `0`, `show`: list the similar commands.
    Show,
    Immediate,
    Prompt,
    /// Run the single match after this many deciseconds.
    Delay(u32),
}

impl Autocorrect {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::to_ascii_lowercase).as_deref() {
            None | Some("" | "0" | "false" | "no" | "off" | "show") => Self::Show,
            Some("never") => Self::Never,
            Some("immediate") => Self::Immediate,
            Some("prompt") => Self::Prompt,
            Some(n) => match n.parse::<i64>() {
                Ok(n) if n < 0 => Self::Immediate,
                Ok(n) => Self::Delay(n as u32),
                Err(_) => Self::Show,
            },
        }
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase());
        match ext.as_deref() {
            Some("exe" | "cmd" | "bat" | "com") => true,
            Some(_) => false,
            None => std::fs::read(path).is_ok_and(|b| b.starts_with(b"#!")),
        }
    }
}

/// The first executable `file` in `dirs`; on Windows also `file.exe`, `.cmd`, `.bat`, `.com`.
pub fn find_in(dirs: impl Iterator<Item = PathBuf>, file: &str) -> Option<PathBuf> {
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ".com", ""]
    } else {
        &[""]
    };
    for dir in dirs {
        for suffix in suffixes {
            let candidate = dir.join(format!("{file}{suffix}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Real git's exec-path, guessed from its location without spawning it.
fn exec_path_dirs(ctx: &Ctx) -> Vec<PathBuf> {
    let Ok(real) = ctx.real_git() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(root) = real.git_for_windows_root() {
        // The Git for Windows install, not a work tree.
        out.push(root.join("mingw64").join("libexec").join("git-core")); // lint:allow-worktree-fs
        out.push(root.join("libexec").join("git-core")); // lint:allow-worktree-fs
    }
    if let Some(prefix) = real.path().parent().and_then(Path::parent) {
        out.push(prefix.join("libexec").join("git-core"));
    }
    out
}

struct CtxLookup<'a>(&'a Ctx);

impl Lookup for CtxLookup<'_> {
    fn alias(&self, name: &str) -> Option<String> {
        if self.0.env.no_aliases {
            return None;
        }
        self.0.config.get(&format!("alias.{name}"))
    }

    fn external(&self, prefix: &str, name: &str) -> Option<PathBuf> {
        if self.0.env.no_aliases {
            return None;
        }
        let file = format!("{prefix}{name}");
        let path = std::env::var_os("PATH").unwrap_or_default();
        find_in(std::env::split_paths(&path), &file).or_else(|| {
            (prefix == "git-")
                .then(|| find_in(exec_path_dirs(self.0).into_iter(), &file))
                .flatten()
        })
    }
}

fn exit_code(code: Option<i32>) -> ExitCode {
    ExitCode::from(code.map_or(128, |c| c.clamp(0, 255) as u8))
}

fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_owned(),
        std::io::ErrorKind::NotADirectory => "Not a directory".to_owned(),
        _ => {
            let text = e.to_string();
            text.split(" (os error").next().unwrap_or(&text).to_owned()
        }
    }
}

fn print_usage(prog: Program, to_stderr: bool) {
    let text = format!("{}\n", Msg::Usage.text(prog));
    if to_stderr {
        eprint!("{text}");
    } else {
        print!("{text}");
    }
}

/// Reports a failed global option the way git does and returns the exit code.
fn report_env_error(prog: Program, err: EnvError) -> ExitCode {
    let (msg, usage) = match err {
        EnvError::MissingDir => (Msg::MissingDir { opt: "-C".into() }, true),
        EnvError::MissingValue(opt) => (Msg::MissingDir { opt }, true),
        EnvError::MissingConfig => (Msg::MissingConfigString, true),
        EnvError::Unknown(opt) => (Msg::UnknownOption { opt }, true),
        EnvError::Unsupported(flag) => (Msg::AscOptionUnsupported { flag }, false),
        EnvError::ConfigEnv(m) => (m, false),
    };
    eprint!("{}", msg.line(prog));
    if usage {
        print_usage(prog, true);
    }
    ExitCode::from(if usage { 129 } else { msg.exit_code() })
}

/// The version line (SPEC §11.4): `git version <real>.asc.<semver>` or `asc <semver> (git <real>)`.
fn version_line(prog: Program) -> Result<String> {
    let semver = env!("CARGO_PKG_VERSION");
    let real = asc_git::realgit::RealGit::resolve(None, None).ok().and_then(|g| {
        let out = g.command().ok()?.arg("--version").output().ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        text.trim().strip_prefix("git version ").map(str::to_owned)
    });
    match (prog, real) {
        (Program::Git, Some(real)) => Ok(format!("git version {real}.asc.{semver}")),
        (Program::Git, None) => Err(CliError::Raw(
            "fatal: asc: real git not found; set asc.realGit".into(),
            128,
        )),
        (Program::Asc, Some(real)) => Ok(format!("asc {semver} (git {real})")),
        (Program::Asc, None) => Ok(format!("asc {semver}")),
    }
}

/// Runs the invocation `args` (argv, including argv[0]) as `prog`.
pub async fn run(prog: Program, args: Vec<OsString>) -> ExitCode {
    match run_inner(prog, args).await {
        Ok(code) => code,
        Err(e) => e.report(prog),
    }
}

async fn run_inner(prog: Program, args: Vec<OsString>) -> Result<ExitCode> {
    let mut env = GitEnv::from_process();
    let get = |k: &str| std::env::var_os(k);
    let tail = args.get(1..).unwrap_or_default();
    let (opts, at) = match env.parse_globals(tail, &get) {
        Ok(parsed) => parsed,
        Err(e) => return Ok(report_env_error(prog, e)),
    };
    for dir in &opts.chdir {
        std::env::set_current_dir(dir).map_err(|e| {
            CliError::Msg(Msg::CannotChange {
                dir: dir.display().to_string().replace('\\', "/"),
                reason: io_reason(&e),
            })
        })?;
    }
    if opts.help {
        print_usage(prog, false);
        return Ok(ExitCode::SUCCESS);
    }
    let rest: Vec<OsString> = tail[at..].to_vec();
    let word = rest.first().and_then(|a| a.to_str());
    if opts.version || (word == Some("version") && rest.len() == 1) {
        println!("{}", version_line(prog)?);
        return Ok(ExitCode::SUCCESS);
    }
    if opts.print_exec_path {
        let real = asc_git::realgit::RealGit::resolve(None, None)?;
        let out = real.command()?.arg("--exec-path").output()?;
        print!("{}", String::from_utf8_lossy(&out.stdout));
        return Ok(exit_code(out.status.code()));
    }
    if rest.is_empty() {
        print_usage(prog, false);
        return Ok(ExitCode::from(1));
    }
    let cwd = std::env::current_dir()?;
    let ctx = Ctx::open(prog, env, cwd).await?;
    dispatch(&ctx, rest).await
}

/// Resolves the command word, expands aliases and runs the command.
pub async fn dispatch(ctx: &Ctx, mut argv: Vec<OsString>) -> Result<ExitCode> {
    let lookup = CtxLookup(ctx);
    let mut seen: Vec<String> = Vec::new();
    let mut corrected = false;
    loop {
        let Some(name) = argv[0].to_str().map(str::to_owned) else {
            return Err(CliError::Msg(Msg::NotACommand {
                name: argv[0].to_string_lossy().into_owned(),
            }));
        };
        match resolve(ctx.prog, &name, &lookup) {
            Resolved::Command(_) => return cli::run_command(ctx, argv).await,
            Resolved::Unavailable(n) => {
                return Err(CliError::Msg(Msg::AscNotInWorkspace { cmd: n }));
            }
            Resolved::Alias(value) => {
                if let Some(script) = value.strip_prefix('!') {
                    return run_shell_alias(ctx, script, &argv[1..]);
                }
                if seen.contains(&name) {
                    let chain: String = seen
                        .iter()
                        .map(|s| format!("\n  {s}"))
                        .collect::<Vec<_>>()
                        .concat();
                    return Err(CliError::Msg(Msg::AliasLoop { alias: name, chain }));
                }
                seen.push(name.clone());
                let words = split_cmdline(&value).map_err(|why| {
                    CliError::Fatal(format!("bad alias.{name} string: {why}"))
                })?;
                if words.is_empty() {
                    return Err(CliError::Fatal(format!("empty alias for {name}")));
                }
                // git: an expansion that starts with its own name is `recursive alias`.
                if words[0] == name {
                    return Err(CliError::Fatal(format!("recursive alias: {name}")));
                }
                let mut next: Vec<OsString> = words.into_iter().map(OsString::from).collect();
                next.extend(argv.drain(1..));
                argv = next;
            }
            Resolved::External(path) => return run_external(ctx, &path, &argv[1..]),
            Resolved::NotACommand => {
                let candidates = candidates(ctx);
                let found = similar(&name, candidates.iter().map(String::as_str));
                let mode = Autocorrect::parse(ctx.config.get("help.autocorrect").as_deref());
                let acts = matches!(
                    mode,
                    Autocorrect::Immediate | Autocorrect::Prompt | Autocorrect::Delay(_)
                );
                if !corrected && found.len() == 1 && acts && autocorrect_confirmed(ctx, mode, &name, &found[0]) {
                    argv[0] = OsString::from(&found[0]);
                    corrected = true;
                    continue;
                }
                let mut text = Msg::NotACommand { name }.line(ctx.prog);
                if mode != Autocorrect::Never && !found.is_empty() {
                    let head = if found.len() == 1 { Msg::SimilarOne } else { Msg::SimilarMany };
                    text.push_str(&format!("\n{}\n", head.text(ctx.prog)));
                    for c in &found {
                        text.push_str(&format!("\t{c}\n"));
                    }
                }
                eprint!("{text}");
                return Ok(ExitCode::from(1));
            }
        }
    }
}

/// Builtins and aliases: what a typo may be corrected to.
fn candidates(ctx: &Ctx) -> Vec<String> {
    let mut out: Vec<String> = GIT_BUILTINS.iter().map(|s| (*s).to_owned()).collect();
    if !ctx.env.no_aliases {
        for section in ctx.config.file().sections_by_name("alias").into_iter().flatten() {
            out.extend(section.body().value_names());
        }
    }
    let unique: HashSet<String> = out.drain(..).collect();
    let mut out: Vec<String> = unique.into_iter().collect();
    out.sort();
    out
}

/// Prints git's autocorrect warning and decides whether to run the suggestion.
fn autocorrect_confirmed(ctx: &Ctx, mode: Autocorrect, name: &str, to: &str) -> bool {
    let prog = ctx.prog;
    eprintln!("{}", Msg::AutocorrectWarning { name: name.to_owned() }.text(prog));
    match mode {
        Autocorrect::Immediate => {
            eprintln!("{}", Msg::AutocorrectContinue { cmd: to.to_owned() }.text(prog));
            true
        }
        Autocorrect::Delay(n) => {
            eprintln!(
                "Continuing in {:.1} seconds, assuming that you meant '{to}'.",
                f64::from(n) / 10.0
            );
            std::thread::sleep(std::time::Duration::from_millis(u64::from(n) * 100));
            true
        }
        Autocorrect::Prompt => {
            use std::io::{BufRead, IsTerminal, Write};
            eprint!("Run '{to}' instead [y/N]? ");
            let _ = std::io::stderr().flush();
            if !std::io::stdin().is_terminal() {
                return false;
            }
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).is_ok()
                && matches!(line.trim().chars().next(), Some('y' | 'Y'))
        }
        Autocorrect::Never | Autocorrect::Show => false,
    }
}

/// Runs `!script` like git: `sh -c 'script "$@"' script args…` at the workspace root with
/// `GIT_PREFIX` set, through the child `PATH`.
fn run_shell_alias(ctx: &Ctx, script: &str, rest: &[OsString]) -> Result<ExitCode> {
    let mut cmd = ctx.child(ctx.shell())?;
    cmd.arg("-c").arg(format!("{script} \"$@\"")).arg(script).args(rest);
    if let Some(ws) = ctx.ws_opt() {
        let top = ws.projection.toplevel();
        let prefix = ctx
            .cwd
            .strip_prefix(top)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| format!("{}/", p.display().to_string().replace('\\', "/")));
        cmd.current_dir(top);
        if let Some(p) = prefix {
            cmd.env("GIT_PREFIX", p);
        }
    }
    let status = cmd.status().map_err(|e| {
        CliError::Fatal(format!("while expanding alias: '{script}': {}", io_reason(&e)))
    })?;
    Ok(exit_code(status.code()))
}

/// Runs `git-<name>` / `asc-<name>`; an extension-less script on Windows goes through `sh`.
fn run_external(ctx: &Ctx, path: &Path, rest: &[OsString]) -> Result<ExitCode> {
    let script = cfg!(windows) && path.extension().is_none();
    let mut cmd = if script {
        let mut c = ctx.child(ctx.shell())?;
        c.arg(path);
        c
    } else {
        ctx.child(path)?
    };
    cmd.args(rest);
    let status = cmd.status().map_err(|e| {
        CliError::Fatal(format!("cannot run {}: {}", path.display(), io_reason(&e)))
    })?;
    Ok(exit_code(status.code()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Fake {
        aliases: HashMap<String, String>,
        externals: Vec<String>,
    }

    impl Lookup for Fake {
        fn alias(&self, name: &str) -> Option<String> {
            self.aliases.get(name).cloned()
        }
        fn external(&self, prefix: &str, name: &str) -> Option<PathBuf> {
            let file = format!("{prefix}{name}");
            self.externals.contains(&file).then(|| PathBuf::from(file))
        }
    }

    fn fake(aliases: &[(&str, &str)], externals: &[&str]) -> Fake {
        Fake {
            aliases: aliases.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect(),
            externals: externals.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn program_from_argv0() {
        for (arg, want) in [
            ("git", Program::Git),
            ("/usr/bin/git", Program::Git),
            ("C:\\x\\GIT.EXE", Program::Git),
            ("Git.exe", Program::Git),
            ("asc", Program::Asc),
            ("asc.exe", Program::Asc),
            ("git-foo", Program::Asc),
            ("", Program::Asc),
        ] {
            assert_eq!(Program::from_argv0(OsStr::new(arg)), want, "{arg}");
        }
    }

    #[test]
    fn git_prefers_builtin_then_alias_then_external_then_asc_verb() {
        let lk = fake(
            &[("graph", "log --oneline --graph"), ("status", "log"), ("foo", "log")],
            &["git-affected", "git-foo"],
        );
        let git = |n| resolve(Program::Git, n, &lk);
        assert_eq!(git("status"), Resolved::Command("status".into()), "builtins first");
        assert_eq!(git("graph"), Resolved::Alias("log --oneline --graph".into()));
        assert_eq!(git("affected"), Resolved::External(PathBuf::from("git-affected")));
        assert_eq!(git("foo"), Resolved::Alias("log".into()), "alias before git-foo");
        assert_eq!(git("projects"), Resolved::Command("projects".into()));
        assert_eq!(git("nosuch"), Resolved::NotACommand);
        assert_eq!(git("daemon"), Resolved::Unavailable("daemon".into()));
        assert_eq!(git("lfs"), Resolved::Unavailable("lfs".into()));
    }

    #[test]
    fn asc_prefers_builtin_and_verb_then_alias_then_asc_then_git_external() {
        let lk = fake(&[("graph", "log --graph"), ("x", "log")], &["asc-x", "git-y", "asc-y"]);
        let asc = |n| resolve(Program::Asc, n, &lk);
        assert_eq!(asc("graph"), Resolved::Command("graph".into()), "asc verbs beat aliases");
        assert_eq!(asc("daemon"), Resolved::Command("daemon".into()));
        assert_eq!(asc("x"), Resolved::Alias("log".into()));
        assert_eq!(asc("y"), Resolved::External(PathBuf::from("asc-y")), "asc-<name> first");
        assert_eq!(asc("nosuch"), Resolved::NotACommand);
    }

    #[test]
    fn builtins_list_matches_git_2_55_count() {
        assert_eq!(GIT_BUILTINS.len(), 150);
        let mut sorted = GIT_BUILTINS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, GIT_BUILTINS, "kept sorted");
    }

    #[test]
    fn splits_alias_values_like_git() {
        assert_eq!(
            split_cmdline("log --format='%h %s' \"a b\" c\\ d").unwrap(),
            ["log", "--format=%h %s", "a b", "c d"]
        );
        assert_eq!(split_cmdline("").unwrap(), Vec::<String>::new());
        assert_eq!(split_cmdline("a '' b").unwrap(), ["a", "", "b"]);
        assert!(split_cmdline("a 'b").is_err());
        assert!(split_cmdline("a\\").is_err());
    }

    #[test]
    fn typo_of_a_git_command_never_becomes_a_asc_verb() {
        // `graph` is an asc verb one edit from `grep`-like typos; `sttaus` is a git typo.
        let all = GIT_BUILTINS.iter().copied().chain(ASC_ONLY_VERBS.iter().copied());
        assert_eq!(similar("sttaus", all.clone()), ["status"]);
        // A name closest to an asc-only verb is not corrected to it.
        let found = similar("projectz", all);
        assert!(found.is_empty() || found.iter().all(|c| GIT_BUILTINS.contains(&c.as_str())));
        assert!(!found.contains(&"projects".to_owned()));
    }

    #[test]
    fn levenshtein_costs() {
        assert_eq!(levenshtein("status", "status"), 0);
        assert_eq!(levenshtein("sttaus", "status"), 0, "a swap is free");
        assert_eq!(levenshtein("stat", "status"), 2, "two insertions");
        assert_eq!(levenshtein("statuss", "status"), 3, "one deletion");
        assert_eq!(levenshtein("stxtus", "status"), 2, "one substitution");
    }

    #[test]
    fn autocorrect_values() {
        assert_eq!(Autocorrect::parse(None), Autocorrect::Show);
        assert_eq!(Autocorrect::parse(Some("immediate")), Autocorrect::Immediate);
        assert_eq!(Autocorrect::parse(Some("never")), Autocorrect::Never);
        assert_eq!(Autocorrect::parse(Some("prompt")), Autocorrect::Prompt);
        assert_eq!(Autocorrect::parse(Some("20")), Autocorrect::Delay(20));
        assert_eq!(Autocorrect::parse(Some("-1")), Autocorrect::Immediate);
        assert_eq!(Autocorrect::parse(Some("0")), Autocorrect::Show);
    }

    #[test]
    fn find_in_prefers_the_first_directory() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let name = if cfg!(windows) { "git-x.exe" } else { "git-x" };
        for d in [&a, &b] {
            let p = d.join(name);
            std::fs::write(&p, b"x").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let found = find_in([a.clone(), b].into_iter(), "git-x").unwrap();
        assert_eq!(found, a.join(name));
        assert!(find_in([a].into_iter(), "git-nope").is_none());
    }
}
