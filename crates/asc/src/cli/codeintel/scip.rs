//! `asc codeintel upload-scip <index.scip> [--rev <revspec>] [--root <dir>]` (SPEC §23.6 CI-D3,
//! PLAN I31): reads a SCIP index built by a language indexer and sends it to the server, which
//! keys the documents by `FileId` and answers `Symbols` with `confidence = precise` where it has
//! them. The server re-validates every path, size and document; this side only refuses what can
//! never be accepted, before any bytes leave the machine.
//!
//! TODO(I17): this file is wired into the dispatcher by `codeintel_local::cli` until I17 lands
//! the `cli/codeintel` module tree.
//! TODO(I16): the `UploadScip` RPC does not exist yet; [`send`] is the adapter point.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cli::common::{CliError, Ctx, Result};

/// Same bound as the server (`asc_serve::codeintel::scip::MAX_INDEX_BYTES`).
pub const MAX_INDEX_BYTES: u64 = 64 << 20;

pub const USAGE: &str = "usage: asc codeintel upload-scip <index.scip> [--rev <revspec>] [--root <dir>]

    --rev     the revision the index was built at (default: the current revision)
    --root    repository directory the indexer ran in (default: the repository root)
";

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub file: PathBuf,
    pub rev: Option<String>,
    pub root: Option<String>,
}

pub fn parse_args<'a>(words: impl Iterator<Item = &'a str>) -> Result<Options> {
    let usage = || CliError::Usage(USAGE.trim_end().to_owned());
    let mut words = words;
    let (mut file, mut rev, mut root) = (None, None, None);
    while let Some(w) = words.next() {
        match w {
            "--rev" if rev.is_none() => rev = Some(words.next().ok_or_else(usage)?.to_owned()),
            "--root" if root.is_none() => root = Some(words.next().ok_or_else(usage)?.to_owned()),
            w if !w.starts_with('-') && file.is_none() => file = Some(PathBuf::from(w)),
            _ => return Err(usage()),
        }
    }
    if let Some(r) = &root {
        // A root outside the repository can never name a document of it.
        let bad = r.is_empty()
            || r.starts_with('/')
            || r.contains(char::from(92))
            || r.split('/').any(|c| c.is_empty() || c == "." || c == "..");
        if bad {
            return Err(CliError::Fatal(format!(
                "asc: codeintel: --root must be a relative path inside the repository: {r:?}"
            )));
        }
    }
    Ok(Options {
        file: file.ok_or_else(usage)?,
        rev,
        root,
    })
}

/// Reads the index, refusing one over the size bound without reading past it.
pub fn read_index(path: &Path) -> Result<Vec<u8>> {
    let fatal = |m: String| CliError::Fatal(format!("asc: codeintel: {m}"));
    let f = std::fs::File::open(path).map_err(|e| fatal(format!("{}: {e}", path.display())))?;
    let mut buf = Vec::new();
    f.take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| fatal(format!("{}: {e}", path.display())))?;
    if buf.len() as u64 > MAX_INDEX_BYTES {
        return Err(fatal(format!(
            "{}: over the {MAX_INDEX_BYTES} byte limit",
            path.display()
        )));
    }
    if buf.is_empty() {
        return Err(fatal(format!("{}: empty index", path.display())));
    }
    Ok(buf)
}

/// Sends the index to the server.
async fn send(_ctx: &Ctx, _opts: &Options, _index: Vec<u8>) -> Result<()> {
    // TODO(I16): call `CodeIntel.UploadScip` once the RPC exists.
    Err(CliError::Fatal(
        "asc: codeintel: this server does not offer SCIP upload yet".to_owned(),
    ))
}

pub async fn run<'a>(ctx: &Ctx, args: impl Iterator<Item = &'a str>) -> Result<ExitCode> {
    let opts = parse_args(args)?;
    let index = read_index(&opts.file)?;
    send(ctx, &opts, index).await?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_and_reject() {
        let o = parse_args(["x.scip", "--rev", "main", "--root", "svc/a"].into_iter()).unwrap();
        assert_eq!(o.file, PathBuf::from("x.scip"));
        assert_eq!(o.rev.as_deref(), Some("main"));
        assert_eq!(o.root.as_deref(), Some("svc/a"));
        assert!(parse_args([].into_iter()).is_err());
        assert!(parse_args(["a", "b"].into_iter()).is_err());
        assert!(parse_args(["a", "--rev"].into_iter()).is_err());
        for bad in ["../x", "/abs", "a//b", "a/./b", "", "a\\b"] {
            assert!(parse_args(["a", "--root", bad].into_iter()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn empty_and_missing_files_are_refused() {
        let d = std::env::temp_dir().join(format!("asc-scip-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let e = d.join("empty.scip");
        std::fs::write(&e, b"").unwrap();
        assert!(read_index(&e).is_err());
        assert!(read_index(&d.join("none.scip")).is_err());
        let ok = d.join("ok.scip");
        std::fs::write(&ok, b"\x12\x00").unwrap();
        assert_eq!(read_index(&ok).unwrap(), b"\x12\x00");
        let _ = std::fs::remove_dir_all(&d);
    }
}
