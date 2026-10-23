//! `diff --no-index <a> <b>`: two files or two directory trees on disk, outside any repository
//! state. Names print as the arguments were typed, with git's `a/` and `b/` prefixes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use asc_core::git::blob_oid;
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_engine::diff::{Status, TreeChange};
use asc_wc::status::Eol;

use super::work::LocalFiles;
use crate::cli::add::convert::{blob_oid as file_blob_oid, exec_bit, link_target};
use crate::cli::common::{CliError, Result};

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// A name as git prints it: `/` separated, without a leading `/` or `./`.
fn shown(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.trim_start_matches("./").trim_start_matches('/');
    text.trim_end_matches('/').to_owned()
}

/// Every file below `dir`, by path relative to it.
fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, PathBuf>) -> io::Result<()> {
    // `--no-index` walks paths given on the command line, outside the work tree.
    let entries = fs::read_dir(dir)?; // lint:allow-worktree-fs: --no-index walk, not the work tree
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let at = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &at, out)?;
        } else {
            out.insert(at, entry.path());
        }
    }
    Ok(())
}

/// One side: the files by key, each with the name it prints under.
type Files = BTreeMap<String, (String, PathBuf)>;

fn side(cwd: &Path, arg: &Path) -> Result<(Files, bool)> {
    let abs = cwd.join(arg);
    let meta = fs::symlink_metadata(&abs)
        .map_err(|_| fatal(format!("Could not access '{}'", arg.display())))?;
    let mut found = BTreeMap::new();
    let is_dir = meta.is_dir();
    if is_dir {
        walk(&abs, "", &mut found).map_err(CliError::Io)?;
    } else {
        found.insert(String::new(), abs);
    }
    let base = shown(arg);
    let files = found
        .into_iter()
        .map(|(rel, abs)| {
            let name = match (base.is_empty(), rel.is_empty()) {
                (_, true) => base.clone(),
                (true, false) => rel.clone(),
                (false, false) => format!("{base}/{rel}"),
            };
            (rel, (name, abs))
        })
        .collect();
    Ok((files, is_dir))
}

fn entry(
    local: &mut LocalFiles<'_>,
    name: &RepoPath,
    abs: &Path,
) -> Result<asc_core::object::Entry> {
    let meta = fs::symlink_metadata(abs).map_err(CliError::Io)?;
    let (kind, git) = if meta.file_type().is_symlink() {
        (
            Kind::Symlink,
            blob_oid(&link_target(abs).map_err(CliError::Io)?),
        )
    } else {
        let kind = if exec_bit(&meta).unwrap_or(false) {
            Kind::Exec
        } else {
            Kind::File
        };
        (kind, file_blob_oid(abs, Eol::Keep).map_err(CliError::Io)?)
    };
    Ok(local.add(name, abs.to_owned(), kind, meta.len(), git))
}

fn repo_path(name: &str) -> Result<RepoPath> {
    RepoPath::new(name.as_bytes().to_vec()).map_err(|e| fatal(format!("{name}: {e}")))
}

/// The changes between the files under `a` and under `b`. A file and a directory compare the
/// file with the same name inside the directory.
pub fn changes(
    local: &mut LocalFiles<'_>,
    cwd: &Path,
    a: &Path,
    b: &Path,
) -> Result<Vec<TreeChange>> {
    let ((mut left, a_dir), (mut right, b_dir)) = (side(cwd, a)?, side(cwd, b)?);
    if a_dir != b_dir {
        let (file, files) = if a_dir {
            (b, &mut right)
        } else {
            (a, &mut left)
        };
        let key = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(only) = files.remove("") {
            files.insert(key, only);
        }
    }
    let keys: BTreeSet<String> = left.keys().chain(right.keys()).cloned().collect();
    // With a file on one side, only the file's name is compared in the directory.
    let keep = |key: &String| a_dir == b_dir || left.contains_key(key) && right.contains_key(key);
    let mut out = Vec::new();
    for key in keys.iter().filter(|k| keep(k)) {
        let (old_side, new_side) = (left.get(key), right.get(key));
        let old_name = old_side.map(|(n, _)| repo_path(n)).transpose()?;
        let new_name = new_side.map(|(n, _)| repo_path(n)).transpose()?;
        let old = match (&old_name, old_side) {
            (Some(n), Some((_, abs))) => Some(entry(local, n, abs)?),
            _ => None,
        };
        let new = match (&new_name, new_side) {
            (Some(n), Some((_, abs))) => Some(entry(local, n, abs)?),
            _ => None,
        };
        let status = match (&old, &new) {
            (None, Some(_)) => Status::Added,
            (Some(_), None) => Status::Deleted,
            (Some(o), Some(n)) if o.git == n.git && o.kind == n.kind => continue,
            (Some(o), Some(n)) if (o.kind == Kind::Symlink) != (n.kind == Kind::Symlink) => {
                Status::TypeChanged
            }
            (Some(_), Some(_)) => Status::Modified,
            (None, None) => continue,
        };
        let Some(path) = new_name.clone().or_else(|| old_name.clone()) else {
            continue;
        };
        let old_path = old_name.or(new_name).unwrap_or_else(|| path.clone());
        out.push(TreeChange {
            path,
            old_path,
            old,
            new,
            status,
        });
    }
    Ok(out)
}
