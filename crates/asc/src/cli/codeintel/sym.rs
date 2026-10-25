//! `asc sym def|refs|callers <name> [--kind k] [--at path:line]`, `asc sym outline <path>` and
//! `asc sym find <prefix>` (SPEC §23.4.2): symbol queries through `CodeIntel.Symbols`. Paths are
//! relative to the repository root.

use std::io::Write;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{
    ScopeOpts, clean, clean_str, emit_json, exit, header, lines, notice_truncated, plan,
    with_stdout,
};
use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc sym (def|refs|callers) <name> [--kind <kind>]... [--at <path>:<line>]
              [--rev <rev> | --change <change> | --stack [--diff-only]] [--lang <lang>] [--remote <name|url>] [--json]
   or: asc sym outline <path> [scope options]
   or: asc sym find <prefix> [--kind <kind>]... [scope options]";

#[derive(clap::Parser, Debug)]
#[command(name = "sym", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// `def`, `refs`, `callers`, `outline` or `find`.
    pub op: String,
    /// The symbol name, the path (for `outline`) or the name prefix (for `find`).
    pub subject: String,
    /// Restrict to a symbol kind (repeatable).
    #[arg(long = "kind")]
    pub kinds: Vec<String>,
    /// The definition the use is at, as `path:line` (for `refs` and `callers`).
    #[arg(long)]
    pub at: Option<String>,
    #[command(flatten)]
    pub scope: ScopeOpts,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "sym",
    flags: &[
        "--kind",
        "--at",
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

fn usage() -> CliError {
    CliError::Usage(USAGE.to_owned())
}

/// A repository path as typed: forward slashes, no leading `./`.
fn repo_path(word: &str) -> String {
    let w = word.replace('\\', "/");
    w.strip_prefix("./").unwrap_or(&w).to_owned()
}

/// The request for `a` against `scope`.
pub fn request(a: &Args, scope: wire::Scope) -> Result<wire::SymbolsRequest> {
    let op = match a.op.as_str() {
        "def" => wire::SymbolOp::Def,
        "refs" => wire::SymbolOp::Refs,
        "callers" => wire::SymbolOp::Callers,
        "outline" => wire::SymbolOp::Outline,
        "find" => wire::SymbolOp::Find,
        _ => return Err(usage()),
    };
    let by_name = matches!(
        op,
        wire::SymbolOp::Def | wire::SymbolOp::Refs | wire::SymbolOp::Callers
    );
    if a.at.is_some() && !by_name {
        return Err(usage());
    }
    let (def_path, def_line) = match &a.at {
        None => (Vec::new(), 0),
        Some(at) => {
            let (p, l) = at.rsplit_once(':').ok_or_else(usage)?;
            let line: u32 = l.parse().map_err(|_| usage())?;
            if p.is_empty() || line == 0 {
                return Err(usage());
            }
            (repo_path(p).into_bytes(), line)
        }
    };
    let mut req = wire::SymbolsRequest {
        scope: Some(scope),
        op: op as i32,
        kinds: a.kinds.clone(),
        def_path: def_path.into(),
        def_line,
        ..Default::default()
    };
    if op == wire::SymbolOp::Outline {
        req.path = repo_path(&a.subject).into_bytes().into();
    } else {
        req.name.clone_from(&a.subject);
    }
    Ok(req)
}

/// Prints the symbols as `path:line:text` with the kind and confidence after a tab.
pub fn render(out: &mut dyn Write, r: &wire::SymbolsResponse) -> std::io::Result<bool> {
    for s in &r.symbols {
        let shown = if s.signature.is_empty() {
            clean_str(&s.name)
        } else {
            clean(&s.signature)
        };
        let mut meta = clean_str(&s.kind);
        if !s.is_def {
            meta.push_str(" ref");
        }
        if !s.container.is_empty() {
            meta.push_str(&format!(" in {}", clean_str(&s.container)));
        }
        if !s.confidence.is_empty() {
            meta.push_str(&format!(" ({})", clean_str(&s.confidence)));
        }
        writeln!(out, "{}:{}:{shown}\t[{meta}]", clean(&s.path), s.line)?;
    }
    Ok(!r.symbols.is_empty())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let p = plan(ctx, &a.scope, Vec::new()).await?;
    let req = request(&a, p.scope.clone())?;
    let r = p.backend.symbols(req).await?;
    eprintln!("{}", header("sym", &p.scope, r.scope.as_ref()));
    notice_truncated(r.truncated);
    with_stdout(|out| {
        if a.scope.json {
            emit_json(&mut lines(out), "asc.v1.SymbolsResponse", &r)?;
            return Ok(exit(!r.symbols.is_empty()));
        }
        Ok(exit(render(out, &r)?))
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use clap::Parser as _;

    use super::*;

    fn args(v: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("sym").chain(v.iter().copied())).unwrap()
    }

    #[test]
    fn at_names_the_definition() {
        let r = request(
            &args(&["refs", "foo", "--at", "src\\a.rs:12", "--kind", "function"]),
            wire::Scope::default(),
        )
        .unwrap();
        assert_eq!(r.op, wire::SymbolOp::Refs as i32);
        assert_eq!(r.def_path.as_ref(), b"src/a.rs");
        assert_eq!(r.def_line, 12);
        assert_eq!(r.kinds, ["function"]);
        assert_eq!(r.name, "foo");
    }

    #[test]
    fn outline_takes_a_path_and_bad_input_is_usage() {
        let r = request(&args(&["outline", "./src/a.rs"]), wire::Scope::default()).unwrap();
        assert_eq!(r.path.as_ref(), b"src/a.rs");
        assert!(request(&args(&["nope", "x"]), wire::Scope::default()).is_err());
        assert!(request(&args(&["def", "x", "--at", "a.rs"]), wire::Scope::default()).is_err());
        assert!(
            request(
                &args(&["def", "x", "--at", "a.rs:0"]),
                wire::Scope::default()
            )
            .is_err()
        );
        assert!(
            request(
                &args(&["find", "x", "--at", "a.rs:3"]),
                wire::Scope::default()
            )
            .is_err()
        );
    }

    #[test]
    fn output_is_escaped() {
        let r = wire::SymbolsResponse {
            symbols: vec![wire::Symbol {
                name: "n\x1b[0m".into(),
                kind: "fn".into(),
                is_def: true,
                path: Bytes::from_static(b"a\x1b.rs"),
                line: 4,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut buf = Vec::new();
        assert!(render(&mut buf, &r).unwrap());
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains('\x1b'), "{s:?}");
        assert!(s.starts_with("a\\x1b.rs:4:n\\x1b[0m\t[fn"), "{s}");
    }
}
