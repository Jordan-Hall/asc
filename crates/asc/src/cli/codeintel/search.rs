//! `asc search <pattern> [-F|-E] [-i] [-w] [-C n] [--rev R|--change C|--stack [--diff-only]]
//! [--lang L] [--json] [-- pathspec]` (SPEC §23.4.2): exact search through `CodeIntel.Search`.
//! The pattern is a literal unless `-E` makes it a regular expression; `-w` matches whole words.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitCode;

use asc_rpc::server::wire;

use super::{
    ScopeOpts, clean, emit_json, exit, header, lines, notice_truncated, plan, with_stdout,
};
use crate::cli::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

const USAGE: &str = "usage: asc search <pattern> [-F | -E | -w] [-i] [-C <n>] [--rev <rev> | --change <change> | --stack [--diff-only]]
                 [--lang <lang>] [--remote <name|url>] [--json] [-- <pathspec>...]";

#[derive(clap::Parser, Debug)]
#[command(name = "search", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// The text or expression to find.
    pub pattern: String,
    /// The pattern is a literal string (the default).
    #[arg(short = 'F', conflicts_with_all = ["extended", "word"])]
    pub fixed: bool,
    /// The pattern is a regular expression.
    #[arg(short = 'E', conflicts_with = "word")]
    pub extended: bool,
    /// Ignore case.
    #[arg(short = 'i')]
    pub ignore_case: bool,
    /// Match whole words only.
    #[arg(short = 'w')]
    pub word: bool,
    /// Lines of context around a match.
    #[arg(short = 'C', default_value_t = 0)]
    pub context: u32,
    #[command(flatten)]
    pub scope: ScopeOpts,
    /// Pathspecs after `--`.
    #[arg(last = true)]
    pub paths: Vec<String>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "search",
    flags: &[
        "-F",
        "-E",
        "-i",
        "-w",
        "-C",
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
pub fn request(a: &Args, scope: wire::Scope) -> wire::SearchRequest {
    let mode = if a.extended {
        wire::SearchMode::Regex
    } else if a.word {
        wire::SearchMode::Word
    } else {
        wire::SearchMode::Literal
    };
    wire::SearchRequest {
        scope: Some(scope),
        pattern: a.pattern.clone(),
        mode: mode as i32,
        case_sensitive: !a.ignore_case,
        context_lines: a.context,
        budget: None,
        page_token: String::new(),
    }
}

/// Prints the hits in `git grep -n` layout; true when there was a match.
pub fn render(out: &mut dyn Write, responses: &[wire::SearchResponse]) -> std::io::Result<bool> {
    let mut found = false;
    let mut files = 0usize;
    let mut context = false;
    for f in responses.iter().flat_map(|r| &r.files) {
        let path = clean(&f.path);
        if f.redacted {
            writeln!(out, "{path}: [content withheld]")?;
            found = true;
            continue;
        }
        let mut rows: BTreeMap<u32, (bool, String)> = BTreeMap::new();
        for m in &f.matches {
            let n_before = u32::try_from(m.before.len()).unwrap_or(u32::MAX);
            let first = m.line.saturating_sub(n_before);
            for (i, b) in (0u32..).zip(&m.before) {
                rows.entry(first + i).or_insert_with(|| (false, clean(b)));
            }
            for (i, a) in (1u32..).zip(&m.after) {
                rows.entry(m.line.saturating_add(i))
                    .or_insert_with(|| (false, clean(a)));
            }
            rows.insert(m.line, (true, clean(&m.text)));
            context |= !m.before.is_empty() || !m.after.is_empty();
        }
        if rows.is_empty() {
            continue;
        }
        found = true;
        if context && files > 0 {
            writeln!(out, "--")?;
        }
        files += 1;
        let mut prev: Option<u32> = None;
        for (line, (is_match, text)) in &rows {
            if context && prev.is_some_and(|p| *line > p.saturating_add(1)) {
                writeln!(out, "--")?;
            }
            let sep = if *is_match { ':' } else { '-' };
            writeln!(out, "{path}{sep}{line}{sep}{text}")?;
            prev = Some(*line);
        }
    }
    Ok(found)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if a.pattern.is_empty() {
        return Err(CliError::Usage(USAGE.to_owned()));
    }
    let p = plan(ctx, &a.scope, a.paths.clone()).await?;
    let responses = p.backend.search(request(&a, p.scope.clone())).await?;
    eprintln!(
        "{}",
        header(
            "search",
            &p.scope,
            responses.first().and_then(|r| r.scope.as_ref())
        )
    );
    notice_truncated(responses.iter().any(|r| r.truncated));
    with_stdout(|out| {
        if a.scope.json {
            let mut w = lines(out);
            for r in &responses {
                emit_json(&mut w, "asc.v1.SearchResponse", r)?;
            }
            return Ok(exit(responses.iter().any(|r| !r.files.is_empty())));
        }
        Ok(exit(render(out, &responses)?))
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn hit(
        path: &[u8],
        line: u32,
        text: &[u8],
        before: &[&[u8]],
        after: &[&[u8]],
    ) -> wire::FileHit {
        wire::FileHit {
            path: Bytes::copy_from_slice(path),
            matches: vec![wire::LineMatch {
                line,
                cols: vec![],
                text: Bytes::copy_from_slice(text),
                before: before.iter().map(|b| Bytes::copy_from_slice(b)).collect(),
                after: after.iter().map(|b| Bytes::copy_from_slice(b)).collect(),
            }],
            ..Default::default()
        }
    }

    fn show(files: Vec<wire::FileHit>) -> String {
        let mut buf = Vec::new();
        let r = wire::SearchResponse {
            files,
            ..Default::default()
        };
        render(&mut buf, &[r]).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn layout_is_path_line_text() {
        let s = show(vec![hit(b"src/a.rs", 3, b"let x = 1;", &[], &[])]);
        assert_eq!(s, "src/a.rs:3:let x = 1;\n");
    }

    #[test]
    fn context_lines_use_dashes_and_groups_are_separated() {
        let s = show(vec![
            hit(b"a", 5, b"hit", &[b"up"], &[b"down"]),
            hit(b"b", 1, b"hit", &[], &[b"next"]),
        ]);
        assert_eq!(s, "a-4-up\na:5:hit\na-6-down\n--\nb:1:hit\nb-2-next\n");
    }

    #[test]
    fn terminal_control_bytes_in_text_and_paths_never_reach_the_output() {
        let s = show(vec![hit(
            b"dir/\x1b]0;pwn\x07name",
            1,
            b"\x1b[2J\x1b[31mboom\r",
            &[],
            &[],
        )]);
        assert!(
            !s.contains('\x1b') && !s.contains('\x07') && !s.contains('\r'),
            "{s:?}"
        );
        assert!(s.contains("\\x1b[2J"), "{s}");
    }

    #[test]
    fn options_map_to_the_request() {
        use clap::Parser as _;
        let a = Args::try_parse_from([
            "search", "-E", "-i", "-C", "2", "--lang", "rust", "foo", "--", "src",
        ])
        .unwrap();
        let r = request(&a, wire::Scope::default());
        assert_eq!(r.mode, wire::SearchMode::Regex as i32);
        assert!(!r.case_sensitive);
        assert_eq!(r.context_lines, 2);
        assert_eq!(a.paths, ["src"]);
        assert_eq!(a.scope.langs, ["rust"]);
        assert!(Args::try_parse_from(["search", "-E", "-w", "x"]).is_err());
        assert!(Args::try_parse_from(["search", "--diff-only", "x"]).is_err());
        assert!(Args::try_parse_from(["search", "--rev", "a", "--stack", "x"]).is_err());
    }
}
