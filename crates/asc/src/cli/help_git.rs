//! `help [git | <cmd>]` (SPEC §11.1, §11.3): `asc help git` prints the git to asc cheat sheet,
//! which is `docs/CHEATSHEET.md` embedded at build time. `asc help <cmd>` prints asc's notes for
//! the command, if any, and then git's manual through real git. An asc-only verb has no git manual,
//! so only its note is printed.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "help", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// `[git | <command>]` and git's own `help` options.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

/// The options real git's `help` takes; all are passed to it.
pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "help",
    flags: &[
        "-a",
        "--all",
        "-g",
        "--guides",
        "-c",
        "--config",
        "-i",
        "--info",
        "-m",
        "--man",
        "-w",
        "--web",
        "-v",
        "--verbose",
        "--no-verbose",
        "--no-external-commands",
        "--no-aliases",
    ],
    placeholders: &[],
}];

/// The cheat sheet; the file is the single source (a test pins the SPEC §11.3 content).
pub const CHEAT_SHEET: &str = include_str!("../../../../docs/CHEATSHEET.md");

/// asc-only verbs: they have no git manual page.
const ASC_ONLY: &[(&str, &str)] = &[
    ("affected", "list the projects and targets a change touches"),
    ("projects", "list the projects of the workspace"),
    ("graph", "show the project graph"),
    ("landed", "show whether a change has landed"),
    ("watch", "watch the workspace for changes"),
    ("hide", "hide a draft change"),
    (
        "doctor",
        "check the install; --fix repairs, --report-reclaimable measures",
    ),
    ("install-shim", "put git beside asc and print the PATH line"),
];

/// asc's own notes on git commands whose behaviour differs (SPEC §11.3).
const NOTES: &[(&str, &str)] = &[
    (
        "clone",
        "asc: clone is instant and virtual; --mode plain writes files; --reference borrows objects.",
    ),
    (
        "worktree",
        "asc: worktree add is instant and virtual; --ephemeral --ttl <dur> makes a throwaway worktree.",
    ),
    (
        "fetch",
        "asc: fetch downloads no files; missing objects arrive when a path is read.",
    ),
    (
        "pull",
        "asc: pull downloads no files; missing objects arrive when a path is read.",
    ),
    (
        "push",
        "asc: push is git's push; --land (or -o land) rebases on the server when paths do not conflict; \
         in a branchless repo pushing a branch also submits its stack as draft changes.",
    ),
    (
        "submodule",
        "asc: submodule update is not needed; submodules are composed read-only.",
    ),
    (
        "gc",
        "asc: there are no packfiles; gc, repack, prune and maintenance are no-ops.",
    ),
    (
        "lfs",
        "asc: LFS is not needed and not supported; big files are plain blobs.",
    ),
];

fn note_for(cmd: &str) -> Option<&'static str> {
    NOTES.iter().find(|(c, _)| *c == cmd).map(|(_, n)| *n)
}

fn asc_only(cmd: &str) -> Option<&'static str> {
    ASC_ONLY.iter().find(|(c, _)| *c == cmd).map(|(_, d)| *d)
}

/// The topic: the first argument that is not an option.
fn topic(rest: &[OsString]) -> Option<&str> {
    rest.iter()
        .filter_map(|a| a.to_str())
        .find(|a| !a.starts_with('-'))
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode> {
    let rest = args.rest;
    let topic = topic(&rest);
    let mut out = std::io::stdout().lock();
    match topic {
        Some("git") if rest.len() == 1 => {
            out.write_all(CHEAT_SHEET.as_bytes())?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(cmd) => {
            if let Some(about) = asc_only(cmd) {
                writeln!(
                    out,
                    "asc {cmd}: {about}. See docs/ONBOARDING.md and docs/CHEATSHEET.md."
                )?;
                return Ok(ExitCode::SUCCESS);
            }
            if let Some(note) = note_for(cmd) {
                writeln!(out, "{note}\n")?;
            }
        }
        None => {}
    }
    out.flush()?;
    drop(out);
    let real = ctx.real_git()?;
    let status = real
        .command()?
        .arg("help")
        .args(&rest)
        .status()
        .map_err(CliError::from)?;
    Ok(ExitCode::from(
        status.code().unwrap_or(1).clamp(0, 255) as u8
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_sheet_matches_the_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/CHEATSHEET.md");
        let on_disk = std::fs::read_to_string(path).expect("docs/CHEATSHEET.md");
        assert_eq!(CHEAT_SHEET, on_disk);
    }

    #[test]
    fn sheet_covers_spec_11_3() {
        for needle in [
            "Instant and virtual",
            "git push --land",
            "git submodule update",
            "git lfs",
            "ASC's VS Code and JetBrains plugins",
            "git push origin feat",
            "asc hide",
            "`master`, never `main`",
        ] {
            assert!(CHEAT_SHEET.contains(needle), "sheet lacks: {needle}");
        }
        assert!(CHEAT_SHEET.ends_with('\n'));
        assert!(CHEAT_SHEET.is_ascii());
    }

    #[test]
    fn topic_skips_options() {
        let v = |s: &[&str]| s.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(topic(&v(&["git"])), Some("git"));
        assert_eq!(topic(&v(&["-a"])), None);
        assert_eq!(topic(&v(&["-w", "log"])), Some("log"));
        assert!(asc_only("hide").is_some());
        assert!(note_for("push").is_some());
        assert!(note_for("log").is_none());
    }

    /// The `section 1.1 table of a markdown text: the numbers of its rows.
    fn rows(text: &str, from: &str) -> Vec<u32> {
        text.lines()
            .skip_while(|l| !l.starts_with(from))
            .skip(1)
            .take_while(|l| !l.starts_with("## ") && !l.starts_with("### "))
            .filter_map(|l| {
                l.strip_prefix("| ")?
                    .split(" |")
                    .next()?
                    .trim()
                    .parse()
                    .ok()
            })
            .collect()
    }

    #[test]
    fn traceability_rows() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/");
        let spec = std::fs::read_to_string(format!("{root}SPEC.md")).expect("SPEC.md");
        let want: Vec<u32> = (1..=32).collect();
        assert_eq!(rows(&spec, "### 1.1 "), want, "SPEC section 1.1 rows");
        let doc = std::fs::read_to_string(format!("{root}SCION-TRACEABILITY.md")).expect("doc");
        let table = |t: &str| {
            t.lines()
                .skip_while(|l| !l.starts_with("| 1 |"))
                .take_while(|l| l.starts_with("| "))
                .filter(|l| {
                    l.starts_with("| ")
                        && l.split(" |")
                            .next()
                            .is_some_and(|c| c[2..].trim().parse::<u32>().is_ok())
                })
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            table(&doc),
            table(&spec),
            "SCION-TRACEABILITY.md copies SPEC 1.1"
        );
    }
}
