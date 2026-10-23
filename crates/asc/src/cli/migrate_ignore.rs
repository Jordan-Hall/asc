//! `asc migrate ignore [--move]` (SPEC §2.4.2): gives every directory that holds a `.gitignore`
//! a `.ascignore` with the same content. The default copies, so tools that read `.gitignore`
//! directly (ripgrep, prettier, eslint, biome) keep working; `--move` renames instead.
//!
//! The walk lists directories under the work tree root without following links and skips the
//! metadata directories. A `.gitignore` that is a link, is larger than [`MAX_IGNORE`], or whose
//! directory already holds a different `.ascignore` is left alone with a warning; a `.ascignore`
//! is never overwritten. `asc migrate branchless` belongs to the branchless units.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use super::doctor::walk_dirs;
use crate::msg::Msg;
use crate::native::NativeSpec;

/// Largest ignore file migrated (git itself reads at most 100 MiB; this is a sane bound).
const MAX_IGNORE: u64 = 1024 * 1024;

/// Most directory entries the walk visits.
const WALK_LIMIT: usize = 20_000_000;

const USAGE: &str = "usage: asc migrate ignore [--move]";

#[derive(clap::Parser, Debug)]
#[command(
    name = "migrate",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "migrate",
    flags: &["--move"],
    placeholders: &[],
}];

/// What happened to one `.gitignore`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Copied,
    Moved,
    /// The `.ascignore` already has the same content (and `.gitignore` was removed by `--move`).
    Same,
    /// A different `.ascignore` is there, or the `.gitignore` is not a plain file in bounds.
    Skipped(&'static str),
}

fn usage(msg: &str) -> CliError {
    CliError::Usage(format!("error: {msg}\n{USAGE}"))
}

/// Migrates one directory's `.gitignore`.
// lint:allow-worktree-fs: `dir` comes from `walk_dirs`, which never enters a link; both names are
// fixed, and the destination is created exclusively so nothing is overwritten or followed.
fn migrate_dir(dir: &Path, relocate: bool) -> std::io::Result<Option<Outcome>> {
    let from = dir.join(".gitignore");
    let Ok(meta) = fs::symlink_metadata(&from) else {
        return Ok(None);
    };
    if !meta.is_file() {
        return Ok(Some(Outcome::Skipped("not a regular file")));
    }
    if meta.len() > MAX_IGNORE {
        return Ok(Some(Outcome::Skipped("too large")));
    }
    let to = dir.join(".ascignore");
    let content = fs::read(&from)?;
    match fs::symlink_metadata(&to) {
        Ok(existing) => {
            if existing.is_file() && existing.len() <= MAX_IGNORE && fs::read(&to)? == content {
                if relocate {
                    fs::remove_file(&from)?;
                }
                return Ok(Some(Outcome::Same));
            }
            return Ok(Some(Outcome::Skipped(
                "a different .ascignore is already there",
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if relocate {
        // A rename never replaces on Windows and `to` was just seen absent.
        fs::rename(&from, &to)?;
        return Ok(Some(Outcome::Moved));
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&to)?;
    out.write_all(&content)?;
    out.flush()?;
    Ok(Some(Outcome::Copied))
}

/// `dir` below `root` as a `/`-separated path (`""` for the root).
fn relative(root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

fn skip_dir(name: &OsStr) -> bool {
    name == ".git" || name == ".asc"
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let mut words = args.rest.iter().map(|a| a.to_string_lossy().into_owned());
    match words.next().as_deref() {
        Some("ignore") => {}
        Some("branchless") => {
            return Err(CliError::Msg(Msg::AscNotYet {
                cmd: "migrate branchless".to_owned(),
            }));
        }
        Some(other) => return Err(usage(&format!("unknown migrate subcommand '{other}'"))),
        None => return Err(CliError::Usage(USAGE.to_owned())),
    }
    let mut relocate = false;
    for w in words {
        match w.as_str() {
            "--move" => relocate = true,
            other => return Err(usage(&format!("unknown argument '{other}'"))),
        }
    }
    let ws = ctx.ws()?;
    let root = ws.projection.toplevel().to_owned();
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let truncated = walk_dirs(&root, &skip_dir, WALK_LIMIT, &mut |dir, entries| {
        if entries
            .iter()
            .any(|(e, _)| e.file_name() == OsStr::new(".gitignore"))
        {
            dirs.push(dir.to_owned());
        }
    });
    dirs.sort();
    let mut skipped = false;
    for dir in &dirs {
        let rel = relative(&root, dir);
        let shown = |name: &str| {
            if rel.is_empty() {
                name.to_owned()
            } else {
                format!("{rel}/{name}")
            }
        };
        match migrate_dir(dir, relocate) {
            Ok(Some(Outcome::Copied)) => {
                println!("copied {} to {}", shown(".gitignore"), shown(".ascignore"));
            }
            Ok(Some(Outcome::Moved)) => {
                println!("moved {} to {}", shown(".gitignore"), shown(".ascignore"));
            }
            Ok(Some(Outcome::Same)) => {
                println!("{} already has the same content", shown(".ascignore"));
            }
            Ok(Some(Outcome::Skipped(why))) => {
                eprintln!("warning: asc: left {} alone: {why}", shown(".gitignore"));
                skipped = true;
            }
            Ok(None) => {}
            Err(e) => {
                return Err(CliError::Fatal(format!(
                    "asc: cannot migrate {}: {e}",
                    shown(".gitignore")
                )));
            }
        }
    }
    if truncated {
        eprintln!("warning: asc: the work tree is too large to scan completely");
        skipped = true;
    }
    Ok(if skipped {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        fs::write(dir.join(name), text).unwrap();
    }

    #[test]
    fn copy_keeps_the_original() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), ".gitignore", "target/\n");
        assert_eq!(migrate_dir(t.path(), false).unwrap(), Some(Outcome::Copied));
        assert_eq!(fs::read(t.path().join(".ascignore")).unwrap(), b"target/\n");
        assert!(t.path().join(".gitignore").exists());
    }

    #[test]
    fn move_renames() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), ".gitignore", "a\n");
        assert_eq!(migrate_dir(t.path(), true).unwrap(), Some(Outcome::Moved));
        assert!(!t.path().join(".gitignore").exists());
        assert_eq!(fs::read(t.path().join(".ascignore")).unwrap(), b"a\n");
    }

    #[test]
    fn a_different_ascignore_is_never_overwritten() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), ".gitignore", "a\n");
        write(t.path(), ".ascignore", "b\n");
        assert!(matches!(
            migrate_dir(t.path(), true).unwrap(),
            Some(Outcome::Skipped(_))
        ));
        assert_eq!(fs::read(t.path().join(".ascignore")).unwrap(), b"b\n");
        assert!(t.path().join(".gitignore").exists());
    }

    #[test]
    fn identical_content_is_reported_and_move_drops_the_original() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), ".gitignore", "a\n");
        write(t.path(), ".ascignore", "a\n");
        assert_eq!(migrate_dir(t.path(), true).unwrap(), Some(Outcome::Same));
        assert!(!t.path().join(".gitignore").exists());
    }

    #[test]
    fn nothing_to_do_without_a_gitignore() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(migrate_dir(t.path(), false).unwrap(), None);
    }
}
