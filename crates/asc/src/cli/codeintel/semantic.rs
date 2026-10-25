//! `asc semantic "<query>" [--vector] [-k n] [--rerank]` (SPEC §23.4.2): hybrid or vector search
//! through `CodeIntel.Semantic`. Each chunk prints as `path:line:text` lines, chunks separated
//! by `--`; the score and sources are in `--json`.

use std::io::Write;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{
    ScopeOpts, clean, emit_json, exit, header, lines, notice_truncated, plan, with_stdout,
};
use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc semantic <query> [--vector] [-k <n>] [--rerank] [--rev <rev> | --change <change> | --stack [--diff-only]]
                   [--lang <lang>] [--remote <name|url>] [--json] [-- <pathspec>...]";

#[derive(clap::Parser, Debug)]
#[command(
    name = "semantic",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// What to look for, in words.
    pub query: String,
    /// Vector similarity only (no keyword half).
    #[arg(long)]
    pub vector: bool,
    /// How many chunks to return.
    #[arg(short = 'k')]
    pub k: Option<u32>,
    /// Re-rank the candidates with the reranker model.
    #[arg(long)]
    pub rerank: bool,
    #[command(flatten)]
    pub scope: ScopeOpts,
    /// Pathspecs after `--`.
    #[arg(last = true)]
    pub paths: Vec<String>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "semantic",
    flags: &[
        "--vector",
        "-k",
        "--rerank",
        "--rev",
        "--change",
        "--stack",
        "--diff-only",
        "--lang",
        "--remote",
        "--json",
    ],
    placeholders: &[],
}];

/// The request for `a` against `scope`.
pub fn request(a: &Args, scope: wire::Scope) -> wire::SemanticRequest {
    wire::SemanticRequest {
        scope: Some(scope),
        query: a.query.clone(),
        mode: if a.vector {
            wire::SemanticMode::Vector
        } else {
            wire::SemanticMode::Hybrid
        } as i32,
        k: a.k.unwrap_or(0),
        budget: None,
        rerank: a.rerank,
    }
}

/// Prints the chunks; true when there was one.
pub fn render(out: &mut dyn Write, r: &wire::SemanticResponse) -> std::io::Result<bool> {
    for (i, c) in r.chunks.iter().enumerate() {
        if i > 0 {
            writeln!(out, "--")?;
        }
        let path = clean(&c.path);
        if c.redacted {
            writeln!(out, "{path}:{}: [content withheld]", c.start_line)?;
            continue;
        }
        // `clean` escapes line breaks, so split the raw bytes first and clean each line.
        for (n, line) in (u64::from(c.start_line)..).zip(c.text.split(|&b| b == b'\n')) {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            writeln!(out, "{path}:{n}:{}", clean(line))?;
        }
    }
    Ok(!r.chunks.is_empty())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if a.query.trim().is_empty() || a.k == Some(0) {
        return Err(CliError::Usage(USAGE.to_owned()));
    }
    let p = plan(ctx, &a.scope, a.paths.clone()).await?;
    let r = p.backend.semantic(request(&a, p.scope.clone())).await?;
    eprintln!("{}", header("semantic", &p.scope, r.scope.as_ref()));
    if r.ann_exhausted {
        eprintln!(
            "asc: semantic: the vector search ran out of candidates; results may be fewer than asked"
        );
    }
    notice_truncated(false);
    with_stdout(|out| {
        if a.scope.json {
            emit_json(&mut lines(out), "asc.v1.SemanticResponse", &r)?;
            return Ok(exit(!r.chunks.is_empty()));
        }
        Ok(exit(render(out, &r)?))
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use clap::Parser as _;

    use super::*;

    #[test]
    fn options_map_to_the_request() {
        let a = Args::try_parse_from([
            "semantic",
            "where is auth",
            "--vector",
            "-k",
            "5",
            "--rerank",
        ])
        .unwrap();
        let r = request(&a, wire::Scope::default());
        assert_eq!(r.mode, wire::SemanticMode::Vector as i32);
        assert_eq!((r.k, r.rerank), (5, true));
    }

    #[test]
    fn chunk_lines_are_numbered_and_escaped() {
        let r = wire::SemanticResponse {
            chunks: vec![wire::Chunk {
                path: Bytes::from_static(b"a.rs"),
                start_line: 10,
                text: Bytes::from_static(b"one\x1b[31m\r\ntwo"),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut buf = Vec::new();
        assert!(render(&mut buf, &r).unwrap());
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "a.rs:10:one\\x1b[31m\na.rs:11:two\n");
    }
}
