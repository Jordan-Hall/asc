//! `asc files [<query>] [--rev <rev>] [-n <count>] [--json] [-z]` (SPEC §5, `pathsearch.v1`):
//! "go to file". Lists the paths of a revision's tree that match `query` as a fuzzy subsequence,
//! best first; with no query, the first paths in order.
//!
//! Reads the local store only: the path list comes from the `pathsearch` memo, which a commit on
//! top of a listed one derives from its parent's list and the changed subtrees, so a query does
//! not walk the tree after the first one.

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use asc_core::object::Kind;

use super::common::{CliError, Ctx, Result};
use super::revparse::commit_of;
use crate::native::NativeSpec;

/// Paths listed when `-n` is not given.
const DEFAULT_COUNT: usize = 50;

#[derive(clap::Parser, Debug)]
#[command(name = "files", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The revision whose tree is searched; `HEAD` when absent.
    #[arg(long)]
    pub rev: Option<String>,
    /// Most paths printed.
    #[arg(short = 'n', long = "max-count")]
    pub count: Option<usize>,
    /// One JSON object per path: `{"path", "mode", "score"}`.
    #[arg(long)]
    pub json: bool,
    /// End each path with NUL instead of a newline.
    #[arg(short = 'z')]
    pub nul: bool,
    /// The query, then anything else (a usage error).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "files",
    flags: &["--rev", "-n", "--max-count", "--json", "-z"],
    placeholders: &[],
}];

fn usage(text: &str) -> CliError {
    CliError::Usage(format!(
        "usage: asc files [<query>] [--rev <rev>] [-n <count>] [--json] [-z]\n{text}"
    ))
}

/// Git's octal mode of a leaf kind.
fn mode(k: Kind) -> &'static str {
    match k {
        Kind::File => "100644",
        Kind::Exec => "100755",
        Kind::Symlink => "120000",
        Kind::Gitlink => "160000",
        Kind::Dir => "040000",
    }
}

/// The query word: the first argument, which must be UTF-8; a second one is refused.
fn query_of(rest: &[OsString]) -> Result<String> {
    let mut words = rest.iter();
    let q = match words.next() {
        Some(w) => w
            .to_str()
            .ok_or_else(|| usage("error: the query is not UTF-8"))?
            .to_owned(),
        None => String::new(),
    };
    if let Some(extra) = words.next() {
        return Err(usage(&format!(
            "error: unexpected argument '{}'",
            extra.to_string_lossy()
        )));
    }
    Ok(q)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let query = query_of(&a.rest)?;
    let ws = ctx.ws()?;
    let word = a.rev.as_deref().unwrap_or("HEAD");
    let cs = commit_of(ctx, word)
        .await?
        .ok_or_else(|| CliError::Fatal(format!("bad revision '{word}'")))?;
    let hits = ws
        .engine
        .find_paths(cs, &query, a.count.unwrap_or(DEFAULT_COUNT))
        .await
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    let mut out = std::io::stdout().lock();
    let io = |e: std::io::Error| CliError::Fatal(e.to_string());
    for h in &hits {
        if a.json {
            let line = serde_json::json!({
                "path": String::from_utf8_lossy(h.path.as_bytes()),
                "mode": mode(h.kind),
                "score": h.score,
            });
            writeln!(out, "{line}").map_err(io)?;
        } else {
            out.write_all(h.path.as_bytes()).map_err(io)?;
            out.write_all(if a.nul { b"\0" } else { b"\n" })
                .map_err(io)?;
        }
    }
    out.flush().map_err(io)?;
    // Like grep: 1 when nothing matched a query.
    Ok(if hits.is_empty() && !query.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn arguments_parse_and_a_second_word_is_refused() {
        let a =
            Args::try_parse_from(["files", "--rev", "main", "-n", "5", "--json", "lib"]).unwrap();
        assert_eq!(a.rev.as_deref(), Some("main"));
        assert_eq!(a.count, Some(5));
        assert!(a.json);
        assert_eq!(query_of(&a.rest).unwrap(), "lib");
        let none = Args::try_parse_from(["files"]).unwrap();
        assert_eq!(query_of(&none.rest).unwrap(), "");
        let two = Args::try_parse_from(["files", "a", "b"]).unwrap();
        assert!(query_of(&two.rest).is_err());
    }

    #[test]
    fn modes_are_gits() {
        assert_eq!(mode(Kind::File), "100644");
        assert_eq!(mode(Kind::Exec), "100755");
        assert_eq!(mode(Kind::Symlink), "120000");
        assert_eq!(mode(Kind::Gitlink), "160000");
    }
}
