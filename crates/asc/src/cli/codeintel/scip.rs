//! `asc codeintel upload-scip <index.scip> [--rev <revspec>] [--root <dir>] [--remote <name|url>]`
//! (SPEC §23.6 CI-D3, PLAN I31): reads a SCIP index built by a language indexer and sends it to
//! the server (`CodeIntel.UploadScip`, a client stream: one header, then data parts), which keys
//! the documents by `FileId` and answers `Symbols` with `confidence = precise` where it has
//! them. The server needs write permission on the repository and re-validates every path, size
//! and document; this side only refuses what can never be accepted, before any bytes leave the
//! machine.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bytes::Bytes;
use asc_rpc::server::wire;

use super::{call_server, clean_str, client, with_stdout};
use crate::cli::common::{CliError, Ctx, Result};

/// Same bound as the server (`asc_serve::codeintel::scip::MAX_INDEX_BYTES`).
pub const MAX_INDEX_BYTES: u64 = 64 << 20;

/// Bytes per data message (the server accepts up to 4 MiB).
const PART: usize = 1 << 20;

pub const USAGE: &str = "usage: asc codeintel upload-scip <index.scip> [--rev <revspec>] [--root <dir>] [--remote <name|url>]

    --rev     the revision the index was built at (default: HEAD in a workspace, else the server default)
    --root    repository directory the indexer ran in (default: the repository root)
    --remote  the server to upload to (a remote name or URL); default `origin`
";

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub file: PathBuf,
    pub rev: Option<String>,
    pub root: Option<String>,
    pub remote: Option<String>,
}

pub fn parse_args<'a>(words: impl Iterator<Item = &'a str>) -> Result<Options> {
    let usage = || CliError::Usage(USAGE.trim_end().to_owned());
    let mut words = words;
    let (mut file, mut rev, mut root, mut remote) = (None, None, None, None);
    while let Some(w) = words.next() {
        match w {
            "--rev" if rev.is_none() => rev = Some(words.next().ok_or_else(usage)?.to_owned()),
            "--root" if root.is_none() => root = Some(words.next().ok_or_else(usage)?.to_owned()),
            "--remote" if remote.is_none() => {
                remote = Some(words.next().ok_or_else(usage)?.to_owned());
            }
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
        remote,
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

/// The request stream: the header, then the index in [`PART`]-sized data messages.
pub fn messages(
    repo: &str,
    opts: &Options,
    rev: &str,
    index: &Bytes,
) -> Vec<wire::UploadScipRequest> {
    use wire::upload_scip_request::Part;
    let mut out = vec![wire::UploadScipRequest {
        part: Some(Part::Header(wire::UploadScipHeader {
            repo: Some(wire::RepoRef {
                name: repo.to_owned(),
                id_mode: wire::IdMode::Asc as i32,
            }),
            rev: rev.to_owned(),
            root: opts.root.clone().unwrap_or_default(),
            size: index.len() as u64,
        })),
    }];
    let mut at = 0;
    while at < index.len() {
        let end = (at + PART).min(index.len());
        out.push(wire::UploadScipRequest {
            part: Some(Part::Data(index.slice(at..end))),
        });
        at = end;
    }
    out
}

/// The revision to match the index against: `--rev`, else `HEAD` of the workspace, else the
/// server's default (an empty revision).
async fn revision(ctx: &Ctx, opts: &Options) -> Result<String> {
    if let Some(r) = &opts.rev {
        return Ok(r.clone());
    }
    if ctx.ws_opt().is_none() {
        return Ok(String::new());
    }
    let cs = crate::cli::restore::resolve_commit(ctx, "HEAD")
        .await?
        .ok_or_else(|| {
            CliError::Fatal("asc: codeintel: HEAD does not name a revision".to_owned())
        })?;
    Ok(cs.to_string())
}

/// Sends the index to the server.
async fn send(ctx: &Ctx, opts: &Options, index: Vec<u8>) -> Result<wire::UploadScipResponse> {
    let rev = revision(ctx, opts).await?;
    let server = crate::remote::server::open_for(ctx, opts.remote.as_deref()).await?;
    let repo = server.target().repo.clone();
    let index = Bytes::from(index);
    let op = uuid::Uuid::now_v7();
    call_server(&server, |s| {
        let msgs = messages(&repo, opts, &rev, &index);
        async move {
            Ok(client(&s)
                .upload_scip(s.wire.request(futures::stream::iter(msgs), Some(op)))
                .await?
                .into_inner())
        }
    })
    .await
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The one-line outcome, with what the server dropped.
pub fn summary(r: &wire::UploadScipResponse) -> String {
    let mut s = format!(
        "uploaded {} documents ({} occurrences) at {}",
        r.documents,
        r.occurrences,
        clean_str(&hex(&r.changeset))
    );
    let dropped = [
        (r.unknown_path, "documents not in the revision"),
        (r.stale, "documents whose text differs from the revision"),
        (r.skipped_occurrences, "occurrences skipped"),
    ];
    let parts: Vec<String> = dropped
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    if !parts.is_empty() {
        s.push_str("; dropped ");
        s.push_str(&parts.join(", "));
    }
    s
}

pub async fn run<'a>(ctx: &Ctx, args: impl Iterator<Item = &'a str>) -> Result<ExitCode> {
    let opts = parse_args(args)?;
    let index = read_index(&opts.file)?;
    let r = send(ctx, &opts, index).await?;
    with_stdout(|out| {
        writeln!(out, "{}", summary(&r))?;
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_and_reject() {
        let o = parse_args(
            [
                "x.scip", "--rev", "main", "--root", "svc/a", "--remote", "up",
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(o.file, PathBuf::from("x.scip"));
        assert_eq!(o.rev.as_deref(), Some("main"));
        assert_eq!(o.root.as_deref(), Some("svc/a"));
        assert_eq!(o.remote.as_deref(), Some("up"));
        assert!(parse_args([].into_iter()).is_err());
        assert!(parse_args(["a", "b"].into_iter()).is_err());
        assert!(parse_args(["a", "--rev"].into_iter()).is_err());
        for bad in ["../x", "/abs", "a//b", "a/./b", "", "a\\b"] {
            assert!(
                parse_args(["a", "--root", bad].into_iter()).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_stream_is_a_header_then_bounded_parts_that_add_up() {
        let opts = parse_args(["x.scip", "--root", "svc"].into_iter()).unwrap();
        let index = Bytes::from(vec![7u8; PART * 2 + 5]);
        let m = messages("o/r", &opts, "main", &index);
        assert_eq!(m.len(), 4);
        let Some(wire::upload_scip_request::Part::Header(h)) = &m[0].part else {
            panic!("first message is not the header");
        };
        assert_eq!(h.repo.as_ref().map(|r| r.name.as_str()), Some("o/r"));
        assert_eq!((h.rev.as_str(), h.root.as_str()), ("main", "svc"));
        assert_eq!(h.size as usize, index.len());
        let data: usize = m[1..]
            .iter()
            .map(|p| match &p.part {
                Some(wire::upload_scip_request::Part::Data(d)) => {
                    assert!(d.len() <= PART);
                    d.len()
                }
                _ => panic!("not a data part"),
            })
            .sum();
        assert_eq!(data, index.len());
    }

    #[test]
    fn the_summary_names_what_the_server_dropped() {
        let r = wire::UploadScipResponse {
            changeset: Bytes::from_static(&[0xab, 0x01]),
            documents: 3,
            occurrences: 40,
            unknown_path: 2,
            stale: 0,
            skipped_occurrences: 1,
        };
        assert_eq!(
            summary(&r),
            "uploaded 3 documents (40 occurrences) at ab01; dropped 2 documents not in the revision, 1 occurrences skipped"
        );
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
