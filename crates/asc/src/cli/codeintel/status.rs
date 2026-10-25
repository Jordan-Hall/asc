//! `asc codeintel status [--rev R] [--json] [--remote R]` (SPEC §23.4.2): how much of a scope each
//! index covers, through `CodeIntel.IndexStatus`. The other `codeintel` subcommands live in
//! `codeintel_local`.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use asc_rpc::server::wire;
use clap::Parser as _;

use super::{ScopeOpts, clean_str, emit_json, header, lines, plan, with_stdout};
use crate::cli::common::{CliError, Ctx, Result};

const USAGE: &str = "usage: asc codeintel status [--rev <rev>] [--remote <name|url>] [--json]";

#[derive(clap::Parser, Debug)]
#[command(name = "status", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(long)]
    pub rev: Option<String>,
    #[arg(long)]
    pub remote: Option<String>,
    #[arg(long)]
    pub json: bool,
}

/// Prints one row per index.
pub fn render(out: &mut dyn Write, r: &wire::IndexStatusResponse) -> std::io::Result<()> {
    for i in &r.indexes {
        writeln!(
            out,
            "{}\tcoverage {:.0}%\tlag {}s\tgeneration {}\tversion {}",
            clean_str(&i.index),
            i.coverage * 100.0,
            i.lag_seconds.max(0),
            i.generation,
            clean_str(&i.version)
        )?;
    }
    Ok(())
}

/// `words` are the arguments after `status`.
pub async fn run(ctx: &Ctx, words: &[OsString]) -> Result<ExitCode> {
    let a = Args::try_parse_from(
        std::iter::once(OsString::from("status")).chain(words.iter().cloned()),
    )
    .map_err(|_| CliError::Usage(USAGE.to_owned()))?;
    let opts = ScopeOpts {
        rev: a.rev,
        remote: a.remote,
        json: a.json,
        ..ScopeOpts::default()
    };
    let p = plan(ctx, &opts, Vec::new()).await?;
    let r = p
        .backend
        .index_status(wire::IndexStatusRequest {
            scope: Some(p.scope.clone()),
        })
        .await?;
    eprintln!("{}", header("codeintel status", &p.scope, r.scope.as_ref()));
    with_stdout(|out| {
        if opts.json {
            emit_json(&mut lines(out), "asc.v1.IndexStatusResponse", &r)?;
        } else {
            render(out, &r)?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_name_coverage_and_generation() {
        let r = wire::IndexStatusResponse {
            indexes: vec![wire::IndexStatus {
                index: "trigram\x1b".into(),
                coverage: 0.5,
                lag_seconds: 3,
                generation: 7,
                version: "v1".into(),
            }],
            ..Default::default()
        };
        let mut buf = Vec::new();
        render(&mut buf, &r).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(
            s,
            "trigram\\x1b\tcoverage 50%\tlag 3s\tgeneration 7\tversion v1\n"
        );
    }
}
