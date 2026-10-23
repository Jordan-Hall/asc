//! `asc internal claude-hook worktree-create|worktree-remove` (SPEC §9.3, "Claude Code agents").
//!
//! Claude Code runs these as its `WorktreeCreate` and `WorktreeRemove` hooks with a JSON object
//! on stdin. `worktree-create` makes the worktree `<repo-root>/.claude/worktrees/<name>` on the
//! branch `agent/<name>` from the current HEAD, ephemeral for seven days, and prints its
//! absolute path as the only thing on stdout. `worktree-remove` removes the worktree at the
//! given path (`worktree remove --force`); the branch keeps any unpushed commits reachable.
//! Errors go to stderr with a non-zero exit.
//!
//! The JSON is untrusted: it is read with a size limit, the name must be one plain path
//! component, and only a registered worktree is ever removed.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
            ephemeral_secs: Some(wt::DEFAULT_TTL_SECS),
            virtual_mode: false,
        },
    )
    .await?;
    println!("{}", done.path.display());
    Ok(ExitCode::from(done.hook_exit))
}

async fn remove_one(ctx: &Ctx, input: &[u8]) -> Result<ExitCode> {
    let req = parse_remove(input).map_err(bad)?;
    // The repository comes from the JSON's cwd, else the process's, else the worktree itself.
    let opened = context(ctx, req.cwd.as_deref()).await?;
    let ctx = opened.as_ref().unwrap_or(ctx);
    let fallback;
    let ctx = if ctx.ws_opt().is_some() {
        ctx
    } else {
        fallback = Ctx::open(ctx.prog, ctx.env.clone(), req.path.clone()).await?;
        &fallback
    };
    remove(ctx, &req.path.clone().into_os_string(), 1).await?;
    Ok(ExitCode::SUCCESS)
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
}
