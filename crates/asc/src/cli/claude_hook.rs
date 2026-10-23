//! `asc internal claude-hook worktree-create|worktree-remove` (SPEC §9.3, "Claude Code agents").
//!
//! Claude Code runs these as its `WorktreeCreate` and `WorktreeRemove` hooks with a JSON object
//! on stdin. `worktree-create` makes the worktree `<repo-root>/.claude/worktrees/<name>` on the
//! branch `agent/<name>` from the current HEAD, ephemeral for seven days, and prints its
//! absolute path as the only thing on stdout. `worktree-remove` removes the worktree at the
//! given path (`worktree remove --force`) when the hook made it; the branch keeps any unpushed
//! commits reachable.
//! Errors go to stderr with a non-zero exit.
//!
//! The JSON is untrusted: it is read with a size limit, the name must be one plain path
//! component, and only a registered worktree is ever removed. The path is resolved through the
//! trusted-workspace gate and looked up as a worktree row; it is force-removed only when it lies
//! directly under `<main>/.claude/worktrees/<name>` and the row is the hook's (ephemeral, or on
//! `agent/<name>`). A hand-made worktree there gets a plain `worktree remove`, which keeps a
//! dirty one; anything else is refused. The JSON's `cwd` counts only as an ancestor of the path.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use asc_state::state::workspaces;
use asc_wc::worktree as wt;

use super::common::{CliError, Ctx, Result};
use super::worktree::{AddOpts, add, remove};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "internal",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

/// `internal` serves no flags.
pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "internal",
    flags: &[],
    placeholders: &[],
}];

const USAGE: &str =
    "usage: asc internal claude-hook (worktree-create | worktree-remove)  < hook.json";

/// Largest hook JSON read.
const MAX_JSON: u64 = 1 << 20;

/// What a `WorktreeCreate` hook asks for.
#[derive(Debug, PartialEq, Eq)]
pub struct Create {
    pub name: String,
    pub cwd: Option<PathBuf>,
}

/// What a `WorktreeRemove` hook asks for.
#[derive(Debug, PartialEq, Eq)]
pub struct Remove {
    pub path: PathBuf,
    pub cwd: Option<PathBuf>,
}

fn field<'a>(v: &'a serde_json::Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|n| v.get(*n).and_then(|f| f.as_str()))
        .filter(|s| !s.is_empty())
}

fn object(text: &[u8]) -> std::result::Result<serde_json::Value, String> {
    if text.len() as u64 > MAX_JSON {
        return Err(format!("the hook JSON is larger than {MAX_JSON} bytes"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(text).map_err(|e| format!("the hook JSON does not parse: {e}"))?;
    if !value.is_object() {
        return Err("the hook JSON is not an object".into());
    }
    Ok(value)
}

fn cwd_of(v: &serde_json::Value) -> std::result::Result<Option<PathBuf>, String> {
    match field(v, &["cwd"]) {
        None => Ok(None),
        Some(c) if Path::new(c).is_absolute() => Ok(Some(PathBuf::from(c))),
        Some(c) => Err(format!("the hook's cwd '{c}' is not an absolute path")),
    }
}

/// Reads a `WorktreeCreate` input: the `name` field (`worktree_name` is accepted too) and `cwd`.
pub fn parse_create(text: &[u8]) -> std::result::Result<Create, String> {
    let v = object(text)?;
    let name = field(&v, &["name", "worktree_name"]).ok_or("the hook JSON has no worktree name")?;
    if !wt::valid_name(name) {
        return Err(format!(
            "'{name}' is not a usable worktree name (letters, digits, '.', '_' and '-' only)"
        ));
    }
    Ok(Create {
        name: name.to_owned(),
        cwd: cwd_of(&v)?,
    })
}

/// Reads a `WorktreeRemove` input: the `worktree_path` field (`path` is accepted too) and `cwd`.
pub fn parse_remove(text: &[u8]) -> std::result::Result<Remove, String> {
    let v = object(text)?;
    let path = field(&v, &["worktree_path", "path"]).ok_or("the hook JSON has no worktree path")?;
    if !Path::new(path).is_absolute() {
        return Err(format!("the worktree path '{path}' is not absolute"));
    }
    Ok(Remove {
        path: PathBuf::from(path),
        cwd: cwd_of(&v)?,
    })
}

fn read_stdin() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_JSON + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn bad(text: String) -> CliError {
    CliError::Fatal(format!("asc: claude-hook: {text}"))
}

/// The context of the repository the hook is for: the process's own, or the one at the JSON's
/// `cwd`.
async fn context(ctx: &Ctx, cwd: Option<&Path>) -> Result<Option<Ctx>> {
    match cwd {
        Some(dir) if wt::path_key(dir) != wt::path_key(&ctx.cwd) || ctx.ws_opt().is_none() => {
            let mut env = ctx.env.clone();
            env.git_dir = None;
            env.work_tree = None;
            env.common_dir = None;
            env.index_file = None;
            Ok(Some(Ctx::open(ctx.prog, env, dir.to_owned()).await?))
        }
        _ => Ok(None),
    }
}

async fn create(ctx: &Ctx, input: &[u8]) -> Result<ExitCode> {
    let req = parse_create(input).map_err(bad)?;
    let opened = context(ctx, req.cwd.as_deref()).await?;
    let ctx = opened.as_ref().unwrap_or(ctx);
    let root = ctx.ws()?.projection.toplevel().to_owned();
    let path = wt::agent_path(&root, &req.name);
    let typed = path.display().to_string();
    let done = add(
        ctx,
        AddOpts {
            typed,
            path: path.clone(),
            branch: Some((format!("agent/{}", req.name), false)),
            detach: false,
            force: false,
            lock: None,
            quiet: true,
            start: Some("HEAD".to_owned()),
            track: super::switch::TrackOpt::Auto,
            ephemeral_secs: Some(wt::DEFAULT_TTL_SECS),
            virtual_mode: false,
        },
    )
    .await?;
    println!("{}", done.path.display());
    Ok(ExitCode::from(done.hook_exit))
}

/// How the hook may remove a registered worktree.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The hook made it: `worktree remove --force`.
    Force,
    /// Hand-made, but in the agent directory: `worktree remove`, which keeps a dirty one.
    Gentle,
    Refuse(String),
}

/// What the hook may do to the worktree registered at `path`, given the main worktree's root
/// and the row's head and ephemeral marker. The hook JSON is untrusted, so only a worktree that
/// `worktree-create` made (directly below `<main>/.claude/worktrees/`, ephemeral or on
/// `agent/<name>`) is ever force-removed.
fn judge(main: &Path, path: &Path, head: &str, ephemeral: bool) -> Verdict {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| wt::valid_name(n));
    let Some(name) = name else {
        return Verdict::Refuse(format!("'{}' has no usable worktree name", path.display()));
    };
    if wt::path_key(path) != wt::path_key(&wt::agent_path(main, name)) {
        return Verdict::Refuse(format!(
            "'{}' is not directly under '{}'; remove it with `asc worktree remove`",
            path.display(),
            wt::agent_path(main, "").display()
        ));
    }
    if ephemeral || head == format!("refs/heads/agent/{name}") {
        Verdict::Force
    } else {
        Verdict::Gentle
    }
}

/// Whether two paths name one directory: the same text, or the same canonical path (a nested
/// worktree is reached through a link to its mount).
fn same_dir(a: &Path, b: &Path) -> bool {
    let plain = |p: &Path| wt::path_key(&wt::absolutize(Path::new("/"), p));
    if plain(a) == plain(b) {
        return true;
    }
    matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)),
        (Ok(x), Ok(y)) if wt::path_key(&x) == wt::path_key(&y))
}

/// A context for the repository around `dir`, through the trusted-workspace gate, never from
/// the environment's `GIT_DIR` and kin.
async fn probe(ctx: &Ctx, dir: &Path) -> Option<Ctx> {
    let mut env = ctx.env.clone();
    env.git_dir = None;
    env.work_tree = None;
    env.common_dir = None;
    env.index_file = None;
    let opened = Ctx::open(ctx.prog, env, dir.to_owned()).await.ok()?;
    opened.ws_opt().is_some().then_some(opened)
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Checks that the worktree at `mount` is still the one registered: a real directory whose `.git`
/// is a regular file (not a link) naming `gitdir`, the gitdir registered for its row. An agent
/// can rewrite that file, and a subprocess started there would follow it to another repository.
fn check_gitfile(mount: &Path, gitdir: &Path) -> Result<()> {
    let refuse = |why: &str| bad(format!("'{}' {why}; leaving it in place", mount.display()));
    if !std::fs::symlink_metadata(mount).is_ok_and(|m| m.is_dir()) {
        return Err(refuse("is not a plain directory"));
    }
    let file = mount.join(".git");
    if !std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
        return Err(refuse("has no regular .git file"));
    }
    let text = std::fs::read_to_string(&file).map_err(|_| refuse("has an unreadable .git file"))?;
    let named = text
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .ok_or_else(|| refuse("has a malformed .git file"))?;
    let named = mount.join(named);
    match (std::fs::canonicalize(&named), std::fs::canonicalize(gitdir)) {
        (Ok(x), Ok(y)) if wt::path_key(&x) == wt::path_key(&y) => Ok(()),
        _ => Err(refuse("has a .git file that names another gitdir")),
    }
}

/// An empty directory to use as `core.hooksPath`, removed on drop.
struct NoHooks(PathBuf);

impl NoHooks {
    fn make() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("asc-no-hooks-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for NoHooks {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The trusted places a snapshot step works in.
struct Site<'a> {
    mount: &'a Path,
    gitdir: &'a Path,
    no_hooks: &'a NoHooks,
}

/// Runs `asc <args>` in the worktree after checking its gitfile again, with the repository named
/// by `GIT_DIR` and `GIT_WORK_TREE` (never found again from the directory), every other `GIT_*`
/// variable dropped, no hooks and a fixed identity; returns its trimmed stdout.
fn run_asc(site: &Site<'_>, args: &[&str]) -> Result<String> {
    // Before every step, so a swap of the gitfile between steps is caught too.
    check_gitfile(site.mount, site.gitdir)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().to_ascii_uppercase().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    let out = cmd
        .arg("-c")
        .arg(format!("core.hooksPath={}", site.no_hooks.0.display()))
        .args(args)
        .current_dir(site.mount)
        .env("GIT_DIR", site.gitdir)
        .env("GIT_WORK_TREE", site.mount)
        // No system config: nothing outside the user's own files and the registered repository.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "asc claude-hook")
        .env("GIT_AUTHOR_EMAIL", "claude-hook@asc.invalid")
        .env("GIT_COMMITTER_NAME", "asc claude-hook")
        .env("GIT_COMMITTER_EMAIL", "claude-hook@asc.invalid")
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(bad(format!(
            "`asc {}` failed in '{}': {}",
            args.join(" "),
            site.mount.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Before a worktree is force-removed, commits any modified or untracked files to
/// `refs/asc/agent-trash/<name>/<unix time>` (on top of the worktree's HEAD) and says where on
/// stderr. The worktree's `.git` must still name its registered `gitdir`, and no hook runs. A
/// failure aborts the removal: uncommitted work is never dropped silently.
fn save_dirty(mount: &Path, gitdir: &Path, name: &str) -> Result<()> {
    check_gitfile(mount, gitdir)?;
    let no_hooks = NoHooks::make()?;
    let site = Site {
        mount,
        gitdir,
        no_hooks: &no_hooks,
    };
    if run_asc(&site, &["status", "--porcelain"])?.is_empty() {
        return Ok(());
    }
    run_asc(&site, &["add", "-A"])?;
    // Detached, so the agent branch itself does not move.
    run_asc(&site, &["switch", "--detach"])?;
    let message = format!("agent worktree '{name}' removed with uncommitted work");
    run_asc(
        &site,
        &["-c", "commit.gpgsign=false", "commit", "--no-verify", "-m", &message],
    )?;
    let commit = run_asc(&site, &["rev-parse", "HEAD"])?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name_ref = format!("refs/asc/agent-trash/{name}/{secs}");
    run_asc(&site, &["update-ref", &name_ref, &commit, ""])?;
    eprintln!("asc: claude-hook: saved the uncommitted work of '{name}' as {name_ref} ({commit})");
    Ok(())
}

async fn remove_one(ctx: &Ctx, input: &[u8]) -> Result<ExitCode> {
    let req = parse_remove(input).map_err(bad)?;
    let path = wt::absolutize(Path::new("/"), &req.path);
    // Where to look the worktree up, best first: the repository at the JSON's cwd when that is
    // an ancestor of the path, the process's own, then the repository around the path itself.
    // Every one goes through the trusted-workspace gate, and a repository that does not list the
    // path as a worktree is skipped, so a foreign cwd changes nothing.
    let mut from_cwd = Vec::new();
    if let Some(cwd) = req.cwd.as_deref().filter(|c| wt::is_inside(&path, c)) {
        from_cwd.extend(probe(ctx, cwd).await);
    }
    let mut from_path = Vec::new();
    if let Some(dir) = path.ancestors().find(|d| d.is_dir()) {
        from_path.extend(probe(ctx, dir).await);
    }
    for c in from_cwd.iter().chain(std::iter::once(ctx)).chain(&from_path) {
        let Some(ws) = c.ws_opt() else { continue };
        let all = workspaces::list(&ws.db, Some(ws.repo.key)).await?;
        let Some(row) = all
            .iter()
            .find(|r| same_dir(Path::new(&r.ws.path), &path))
        else {
            continue;
        };
        let Some(main) = all.first().filter(|m| m.ws.id != row.ws.id) else {
            return Err(bad(format!("'{}' is the main working tree", path.display())));
        };
        let registered = Path::new(&row.ws.path);
        let verdict = judge(
            Path::new(&main.ws.path),
            registered,
            &row.ws.head,
            row.ws.ephemeral_until.is_some(),
        );
        let force = match verdict {
            Verdict::Force => 1,
            Verdict::Gentle => 0,
            Verdict::Refuse(why) => return Err(bad(why)),
        };
        if force > 0 {
            save_dirty(
                Path::new(&row.ws.mount),
                &c.ws()?.home.gitdir(row.ws.id),
                &name_of(registered),
            )?;
        }
        remove(c, &registered.as_os_str().to_owned(), force).await?;
        return Ok(ExitCode::SUCCESS);
    }
    Err(bad(format!(
        "'{}' is not a registered worktree",
        path.display()
    )))
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let words: Vec<&str> = a.rest.iter().filter_map(|w| w.to_str()).collect();
    match words.as_slice() {
        ["claude-hook", "worktree-create"] => create(ctx, &read_stdin()?).await,
        ["claude-hook", "worktree-remove"] => remove_one(ctx, &read_stdin()?).await,
        _ => Err(CliError::Usage(USAGE.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABS: &str = if cfg!(windows) { "C:/repo" } else { "/repo" };

    #[test]
    fn create_reads_name_and_cwd() {
        let json = format!(
            r#"{{"session_id":"s","hook_event_name":"WorktreeCreate","cwd":"{ABS}","name":"bold-fox"}}"#
        );
        let got = parse_create(json.as_bytes()).unwrap();
        assert_eq!(
            got,
            Create {
                name: "bold-fox".into(),
                cwd: Some(PathBuf::from(ABS))
            }
        );
        let bare = parse_create(br#"{"name":"x1"}"#).unwrap();
        assert_eq!(bare.cwd, None);
        assert_eq!(
            parse_create(br#"{"worktree_name":"alt"}"#).unwrap().name,
            "alt"
        );
    }

    #[test]
    fn create_refuses_names_that_escape_or_are_missing() {
        for name in [
            "../x", "a/b", "a\\b", "..", ".", ".hidden", "", "x y", "x\u{0}y",
        ] {
            let json = serde_json::json!({ "name": name }).to_string();
            assert!(parse_create(json.as_bytes()).is_err(), "{name:?}");
        }
        assert!(parse_create(b"{}").is_err());
        assert!(parse_create(br#"{"name":7}"#).is_err());
    }

    #[test]
    fn input_must_be_a_small_json_object_with_an_absolute_cwd() {
        for bad in [&b""[..], b"[]", b"\"x\"", b"{", b"null"] {
            assert!(parse_create(bad).is_err(), "{bad:?}");
            assert!(parse_remove(bad).is_err(), "{bad:?}");
        }
        assert!(parse_create(br#"{"name":"x","cwd":"relative/dir"}"#).is_err());
        let big = format!(
            r#"{{"name":"x","pad":"{}"}}"#,
            "a".repeat(MAX_JSON as usize)
        );
        assert!(parse_create(big.as_bytes()).unwrap_err().contains("larger"));
    }

    #[test]
    fn remove_reads_an_absolute_worktree_path() {
        let json = format!(
            r#"{{"hook_event_name":"WorktreeRemove","worktree_path":"{ABS}/.claude/worktrees/x"}}"#
        );
        let got = parse_remove(json.as_bytes()).unwrap();
        assert_eq!(
            got.path,
            PathBuf::from(format!("{ABS}/.claude/worktrees/x"))
        );
        assert!(parse_remove(br#"{"worktree_path":"rel/x"}"#).is_err());
        assert!(parse_remove(br#"{"cwd":"/x"}"#).is_err());
    }

    #[test]
    fn the_worktree_lives_under_claude_worktrees() {
        let p = wt::agent_path(Path::new(ABS), "bold-fox");
        assert!(
            wt::path_key(&p).ends_with("/repo/.claude/worktrees/bold-fox"),
            "{p:?}"
        );
    }

    #[test]
    fn only_hook_made_worktrees_directly_in_the_agent_directory_are_forced() {
        let main = Path::new(ABS);
        let at = |tail: &str| PathBuf::from(format!("{ABS}/{tail}"));
        let forced = |p: &Path, head: &str, eph: bool| judge(main, p, head, eph);
        let ok = at(".claude/worktrees/bold-fox");
        assert_eq!(forced(&ok, "refs/heads/agent/bold-fox", false), Verdict::Force);
        assert_eq!(forced(&ok, "", true), Verdict::Force);
        // Hand-made inside the directory: never forced.
        assert_eq!(forced(&ok, "refs/heads/mine", false), Verdict::Gentle);
        assert_eq!(forced(&ok, "refs/heads/agent/other", false), Verdict::Gentle);
        // Elsewhere, marked or not: refused.
        for tail in [
            "elsewhere",
            ".claude/worktrees",
            ".claude/worktrees/a/b",
            ".claude/other/bold-fox",
            ".claude/worktrees/.hidden",
        ] {
            let got = forced(&at(tail), "refs/heads/agent/bold-fox", true);
            assert!(matches!(got, Verdict::Refuse(_)), "{tail}: {got:?}");
        }
        assert!(matches!(
            forced(Path::new("/other/.claude/worktrees/bold-fox"), "", true),
            Verdict::Refuse(_)
        ));
    }
}
