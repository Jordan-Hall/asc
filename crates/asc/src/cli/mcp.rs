//! `asc mcp`: the MCP server over stdio (SPEC 22, unit M09).
//!
//! ```text
//! asc mcp [--read-only] [--write[=local|remote]] [--toolsets <list>] [--tools <list>] [--max-tokens <n>]
//! ```
//!
//! Standard input and output carry newline-delimited JSON-RPC only; everything else goes to
//! standard error (see `crate::mcp`). Every flag can only narrow what `asc.mcp.*` configuration
//! grants (SPEC 22.1, M-D4). `asc mcp install` is M09b and `asc mcp --remote` is M31.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::process::ExitCode;

use asc_mcp::catalogue::WriteLevel;
use asc_mcp::config::{Flags, parse_toolsets};

use super::common::{CliError, Ctx, Result};
use crate::msg::Msg;
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc mcp [--read-only] [--write[=local|remote]] [--toolsets <list>] \
[--tools <list>] [--max-tokens <n>]";

#[derive(clap::Parser, Debug)]
#[command(name = "mcp", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "mcp",
    flags: &[
        "--read-only",
        "--write",
        "--toolsets",
        "--tools",
        "--max-tokens",
        "--remote",
        "--scope",
    ],
    placeholders: &[],
}];

fn usage() -> CliError {
    CliError::Usage(USAGE.to_owned())
}

/// Parses the arguments into narrowing flags.
fn parse(argv: &[OsString]) -> Result<Flags> {
    let mut flags = Flags::default();
    let mut it = argv.iter().map(|a| a.to_string_lossy().into_owned());
    let value = |it: &mut dyn Iterator<Item = String>, inline: Option<String>| {
        inline.or_else(|| it.next()).ok_or_else(usage)
    };
    while let Some(a) = it.next() {
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (a.clone(), None),
        };
        match name.as_str() {
            "--read-only" if inline.is_none() => flags.read_only = true,
            "--write" => {
                flags.write = Some(match inline.as_deref() {
                    None | Some("local") => WriteLevel::Local,
                    Some("remote") => WriteLevel::Remote,
                    Some(_) => return Err(usage()),
                });
            }
            "--toolsets" => {
                let v = value(&mut it, inline)?;
                flags.toolsets = Some(parse_toolsets(&v));
            }
            "--tools" => {
                let v = value(&mut it, inline)?;
                let set: BTreeSet<String> = v
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect();
                flags.tools = Some(set);
            }
            "--max-tokens" => {
                let v = value(&mut it, inline)?;
                flags.max_tokens = Some(v.parse().map_err(|_| usage())?);
            }
            "--remote" => {
                return Err(CliError::Msg(Msg::AscNotYet {
                    cmd: "mcp --remote".to_owned(),
                }));
            }
            "install" => {
                return Err(CliError::Msg(Msg::AscNotYet {
                    cmd: "mcp install".to_owned(),
                }));
            }
            _ => return Err(usage()),
        }
    }
    Ok(flags)
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    if args.rest.first().is_some_and(|a| a == "install") {
        return crate::mcp::install::run(ctx, &args.rest[1..]);
    }
    let flags = parse(&args.rest)?;
    crate::mcp::serve(ctx, flags).await
}

#[cfg(test)]
mod tests {
    use asc_mcp::catalogue::Toolset;

    use super::*;

    fn p(items: &[&str]) -> Result<Flags> {
        parse(&items.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn flags_parse_and_narrow() {
        let f = p(&[
            "--read-only",
            "--toolsets=core,history",
            "--tools",
            "status,log",
            "--max-tokens",
            "500",
        ])
        .unwrap();
        assert!(f.read_only);
        assert_eq!(
            f.toolsets.unwrap(),
            BTreeSet::from([Toolset::Core, Toolset::History])
        );
        assert_eq!(f.tools.unwrap().len(), 2);
        assert_eq!(f.max_tokens, Some(500));
        assert_eq!(p(&["--write"]).unwrap().write, Some(WriteLevel::Local));
        assert_eq!(
            p(&["--write=remote"]).unwrap().write,
            Some(WriteLevel::Remote)
        );
    }

    #[test]
    fn bad_flags_are_usage_errors() {
        for bad in [
            &["--write=root"][..],
            &["--max-tokens", "x"],
            &["--toolsets"],
            &["--nope"],
            &["--read-only=1"],
        ] {
            assert!(matches!(p(bad), Err(CliError::Usage(_))), "{bad:?}");
        }
        assert!(matches!(
            p(&["--remote", "https://x"]),
            Err(CliError::Msg(_))
        ));
    }
}
