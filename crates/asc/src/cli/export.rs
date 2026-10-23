//! `asc export --git <dir> [--refs <glob>]…` (SPEC §14.2): writes the repository's history into a
//! git repository as ordinary git objects, so existing git tooling can read it.
//!
//! `<dir>` is a bare repository, a work tree with a `.git` directory, or a directory that does
//! not exist or is empty (a bare repository is created there with `git init --bare`). A
//! directory that holds something else is refused. Objects the target already has are skipped;
//! new ones are written children first (loose objects, or one undeltified pack past 1,000
//! objects, indexed by `git index-pack`), and refs move in one `git update-ref --stdin`
//! transaction. Every git child comes from `RealGit`. Export is deterministic: a changeset
//! always renders to the same SHA.
//!
//! `--refs <glob>` selects local refs (repeatable; `*` matches any run of characters, `?` one).
//! The default is `refs/heads/*` and `refs/tags/*`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use asc_engine::Engine;
use asc_git::export::{ExportRoot, Exporter};
use asc_git::gitremote::GitRemoteRepo;
use asc_git::odb::Odb;
use asc_store::refs::Refs as _;

use super::common::{CliError, Ctx, Result};
use super::refs_plumbing::args::{Cursor, Word};
use crate::native::NativeSpec;
use crate::refs_guard::ReadOnlyRefs;

const USAGE: &str = "usage: asc export --git <dir> [--refs <glob>]...";

/// Most refs one export writes.
const MAX_REFS: usize = 100_000;

/// Longest `--refs` glob.
const MAX_GLOB: usize = 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "export", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "export",
    flags: &["--git", "--refs"],
    placeholders: &[],
}];

#[derive(Debug, PartialEq, Eq)]
struct Opts {
    dir: String,
    refs: Vec<String>,
}

fn parse(rest: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(rest, USAGE);
    let (mut dir, mut refs) = (None, Vec::new());
    while let Some(word) = cur.next() {
        match word {
            Word::Long(n, inline) => match n.as_str() {
                "git" => dir = Some(cur.value("git", inline)?),
                "refs" => {
                    let g = cur.value("refs", inline)?;
                    if g.is_empty() || g.len() > MAX_GLOB || g.contains('\0') {
                        return Err(CliError::Fatal(format!("asc: bad ref pattern {g:?}")));
                    }
                    refs.push(g);
                }
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
            Word::End | Word::Operand(_) | Word::Short(_) => return Err(cur.usage_only()),
        }
    }
    let Some(dir) = dir else {
        return Err(cur.usage_only());
    };
    if refs.is_empty() {
        refs = vec!["refs/heads/*".to_owned(), "refs/tags/*".to_owned()];
    }
    Ok(Opts { dir, refs })
}

/// Whether `name` matches `pattern`, where `*` is any run (`/` included) and `?` one character.
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) && p[pi] != b'*' {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// The git directory to export into, creating a bare repository for a missing or empty `dir`.
// lint:allow-worktree-fs: `dir` is an export target the user named, not a work tree.
fn target_git_dir(ctx: &Ctx, dir: &Path) -> Result<PathBuf> {
    let is_git = |d: &Path| d.join("HEAD").is_file() && d.join("objects").is_dir();
    if is_git(dir) {
        return Ok(dir.to_owned());
    }
    let dot_git = dir.join(".git");
    if dot_git.is_dir() && is_git(&dot_git) {
        return Ok(dot_git);
    }
    let listing = std::fs::read_dir(dir); // lint:allow-worktree-fs: an export target, not a work tree
    let empty = match listing {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            return Err(CliError::Fatal(format!(
                "asc: cannot read {}: {e}",
                dir.display()
            )));
        }
    };
    if !empty {
        return Err(CliError::Fatal(format!(
            "asc: {} is not a git repository and is not empty",
            dir.display()
        )));
    }
    let repo = GitRemoteRepo::open_or_init(ctx.real_git()?.clone(), dir.to_owned())
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    Ok(repo.dir().to_owned())
}

fn failure(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: export failed: {e}"))
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let o = parse(&args.rest)?;
    let ws = ctx.ws()?;
    let dir = if Path::new(&o.dir).is_absolute() {
        PathBuf::from(&o.dir)
    } else {
        ctx.cwd.join(&o.dir)
    };
    let git = ctx.real_git()?.clone();
    let git_dir = target_git_dir(ctx, &dir)?;

    let mut names: Vec<String> = Vec::new();
    let mut roots: Vec<ExportRoot> = Vec::new();
    for entry in ws.db.refs().list(ws.repo.key, "refs/").await? {
        let Ok(name) = std::str::from_utf8(entry.name.as_bytes()) else {
            continue;
        };
        if !o.refs.iter().any(|g| glob_match(g, name)) {
            continue;
        }
        if names.len() >= MAX_REFS {
            return Err(failure(format!("more than {MAX_REFS} refs selected")));
        }
        names.push(name.to_owned());
        roots.push(match entry.target {
            asc_core::id::RefTarget::Changeset(c) => ExportRoot::Changeset(c),
            asc_core::id::RefTarget::Tag(t) => ExportRoot::Tag(t),
        });
    }

    let engine = Arc::new(Engine::new(
        ws.objects.clone(),
        Arc::new(ReadOnlyRefs(ws.db.refs())),
        ws.repo.key,
    ));
    let objects = ws.objects.clone();
    let handle = tokio::runtime::Handle::current();
    let dir_for_task = git_dir.clone();
    let (written, skipped, tips) = tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            let odb = Odb::open(&dir_for_task).map_err(failure)?;
            let exporter = Exporter::new(&engine, &*objects, &git, &dir_for_task);
            let report = exporter.export(&roots, &odb).await.map_err(failure)?;
            let pairs: Vec<(&str, _)> = names
                .iter()
                .map(String::as_str)
                .zip(report.tips.iter().copied())
                .collect();
            exporter.set_refs(&pairs).map_err(failure)?;
            Ok::<_, CliError>((report.written, report.skipped, pairs.len()))
        })
    })
    .await
    .map_err(|e| failure(format!("export task: {e}")))??;
    println!(
        "exported {written} objects ({skipped} already there) and {tips} refs to {}",
        dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn globs() {
        assert!(glob_match("refs/heads/*", "refs/heads/master"));
        assert!(glob_match("refs/heads/*", "refs/heads/a/b"));
        assert!(!glob_match("refs/heads/*", "refs/tags/v1"));
        assert!(glob_match("refs/tags/v?", "refs/tags/v1"));
        assert!(!glob_match("refs/tags/v?", "refs/tags/v10"));
        assert!(glob_match("*", "refs/anything"));
        assert!(glob_match("refs/*/x", "refs/heads/x"));
        assert!(!glob_match("refs/*/x", "refs/heads/y"));
        assert!(glob_match("a*b*c", "aXbYbZc"));
        assert!(!glob_match("", "x"));
        assert!(glob_match("", ""));
    }

    #[test]
    fn arguments() {
        assert!(parse(&os(&[])).is_err());
        let o = parse(&os(&["--git", "out"])).unwrap();
        assert_eq!(o.refs, vec!["refs/heads/*", "refs/tags/*"]);
        let o = parse(&os(&[
            "--git=out",
            "--refs",
            "refs/heads/x",
            "--refs=refs/tags/*",
        ]))
        .unwrap();
        assert_eq!(o.refs, vec!["refs/heads/x", "refs/tags/*"]);
        assert!(parse(&os(&["--git", "out", "--refs="])).is_err());
        assert!(parse(&os(&["--git", "out", "extra"])).is_err());
    }
}
