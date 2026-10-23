//! The diff options shared by `diff`, `diff-tree`, `diff-index` and `diff-files` (SPEC §4.3,
//! §11.1): parsing, the `diff.*` configuration they default from and the output selection.
//! A flag a command owns alone (`--cached`, `-r`, `--no-index`) is handed back in `extra`.

use std::ffi::OsString;
use std::path::PathBuf;

use asc_engine::diff::Status;
use asc_engine::diff::{Algorithm, DiffOptions, RenameOptions};

use crate::cli::common::{CliError, Ctx, Result};
use crate::msg::Msg;

/// The raw command line: the command name and everything after it. Option parsing is manual
/// because git's diff flags (`-U3`, `-M50%`, `--stat=80,40`) are not clap shapes.
#[derive(Debug)]
pub struct RawArgs {
    pub cmd: String,
    pub rest: Vec<OsString>,
}

impl clap::CommandFactory for RawArgs {
    fn command() -> clap::Command {
        clap::Command::new("diff")
    }

    fn command_for_update() -> clap::Command {
        Self::command()
    }
}

impl clap::FromArgMatches for RawArgs {
    fn from_arg_matches(_: &clap::ArgMatches) -> Result<Self, clap::Error> {
        Ok(Self {
            cmd: String::new(),
            rest: Vec::new(),
        })
    }

    fn update_from_arg_matches(&mut self, _: &clap::ArgMatches) -> Result<(), clap::Error> {
        Ok(())
    }
}

impl clap::Parser for RawArgs {
    fn try_parse_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut all = itr.into_iter().map(Into::into);
        let cmd = all
            .next()
            .map(|c: OsString| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            cmd,
            rest: all.collect(),
        })
    }
}

/// Which output forms were asked for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Formats {
    pub raw: bool,
    pub patch: bool,
    pub stat: bool,
    pub numstat: bool,
    pub shortstat: bool,
    pub summary: bool,
    pub name_only: bool,
    pub name_status: bool,
    pub word_diff: bool,
    /// `-s`: no diff output at all.
    pub none: bool,
}

impl Formats {
    fn any(self) -> bool {
        self.raw
            || self.patch
            || self.stat
            || self.numstat
            || self.shortstat
            || self.summary
            || self.name_only
            || self.name_status
            || self.word_diff
    }

    /// Applies the command's default form when none was named, and `-s`.
    pub fn resolve(mut self, default_raw: bool) -> Self {
        if self.none {
            return Self {
                none: true,
                ..Self::default()
            };
        }
        if !self.any() {
            if default_raw {
                self.raw = true;
            } else {
                self.patch = true;
            }
        }
        self
    }
}

/// `--diff-filter=<letters>`: uppercase selects a status, lowercase excludes it.
#[derive(Clone, Debug, Default)]
pub struct StatusFilter {
    include: Vec<char>,
    exclude: Vec<char>,
}

impl StatusFilter {
    fn parse(text: &str) -> Self {
        let mut f = Self::default();
        for c in text.chars() {
            if c.is_ascii_uppercase() {
                f.include.push(c);
            } else if c.is_ascii_lowercase() {
                f.exclude.push(c.to_ascii_uppercase());
            }
        }
        f
    }

    /// Whether a change of this status is shown.
    pub fn keeps(&self, status: Status) -> bool {
        let letter = status.letter();
        (self.include.is_empty() || self.include.contains(&letter))
            && !self.exclude.contains(&letter)
    }
}

/// The options common to the four commands.
pub struct Common {
    pub opts: DiffOptions,
    pub fmt: Formats,
    /// `--abbrev` or `--full-index` was typed.
    pub abbrev_given: bool,
    pub binary: bool,
    pub text: bool,
    pub quiet: bool,
    pub exit_code: bool,
    pub json: bool,
    pub output: Option<PathBuf>,
    pub filter: Option<StatusFilter>,
    /// Rename detection was asked for or against on the command line.
    pub renames_flag: Option<bool>,
    /// Flags the command itself owns, as typed.
    pub extra: Vec<String>,
    /// Arguments before `--`: revisions and paths in git's guessing order.
    pub positional: Vec<OsString>,
    /// Arguments after `--`: paths.
    pub paths: Vec<OsString>,
    /// `--` was given.
    pub dashdash: bool,
}

fn unsupported(flag: &str) -> CliError {
    CliError::Msg(Msg::AscOptionUnsupported {
        flag: flag.to_owned(),
    })
}

fn usage(text: String) -> CliError {
    CliError::Usage(text)
}

fn number(flag: &str, value: &str) -> Result<usize> {
    value
        .trim()
        .parse()
        .map_err(|_| CliError::Fatal(format!("invalid argument to {flag}: '{value}'")))
}

/// git's rename score: `50%` is 50 percent, bare digits are a decimal fraction (`5` is 50%).
fn rename_threshold(text: &str) -> Option<u8> {
    if text.is_empty() {
        return Some(50);
    }
    let (digits, percent) = match text.strip_suffix('%') {
        Some(d) => (d, true),
        None => (text, false),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u64 = digits.parse().ok()?;
    let scaled = if percent {
        value
    } else {
        value * 100 / 10u64.checked_pow(u32::try_from(digits.len()).ok()?)?
    };
    u8::try_from(scaled.min(100)).ok()
}

/// `--stat=<width>[,<name-width>[,<count>]]`.
fn stat_value(opts: &mut DiffOptions, value: &str) -> Result<()> {
    let mut parts = value.split(',');
    if let Some(w) = parts.next().filter(|w| !w.is_empty()) {
        opts.stat_width = Some(number("--stat", w)?);
    }
    if let Some(n) = parts.next().filter(|n| !n.is_empty()) {
        opts.stat_name_width = Some(number("--stat", n)?);
    }
    if let Some(c) = parts.next().filter(|c| !c.is_empty()) {
        opts.stat_count = Some(number("--stat", c)?);
    }
    Ok(())
}

/// The `diff.*` and `core.*` keys diff reads. `renames` applies `diff.renames`, which only the
/// porcelain `diff` honours (git's plumbing ignores it).
pub fn apply_config(ctx: &Ctx, opts: &mut DiffOptions, porcelain: bool) -> Result<()> {
    if porcelain {
        // git's default: detect renames unless `diff.renames` says no.
        opts.renames = Some(RenameOptions::default());
    }
    let mut keys = vec![
        "diff.algorithm",
        "diff.indentHeuristic",
        "diff.context",
        "diff.interHunkContext",
        "diff.noprefix",
        "diff.mnemonicPrefix",
        "diff.relative",
        "core.bigFileThreshold",
        "core.abbrev",
    ];
    if porcelain {
        keys.extend(["diff.renameLimit", "diff.renames"]);
    }
    for key in keys {
        if let Some(value) = ctx.config.get(key) {
            opts.apply_config(key, &value).map_err(|e| {
                CliError::Fatal(format!(
                    "bad config variable '{}' value '{}'",
                    e.key, e.value
                ))
            })?;
        }
    }
    Ok(())
}

/// Parses `argv` (after the command name) against `opts`, which `apply_config` has seeded.
pub fn parse(mut opts: DiffOptions, argv: &[OsString]) -> Result<Common> {
    let mut c = Common {
        opts: DiffOptions::default(),
        fmt: Formats::default(),
        abbrev_given: false,
        binary: false,
        text: false,
        quiet: false,
        exit_code: false,
        json: false,
        output: None,
        filter: None,
        renames_flag: None,
        extra: Vec::new(),
        positional: Vec::new(),
        paths: Vec::new(),
        dashdash: false,
    };
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        if c.dashdash {
            c.paths.push(arg.clone());
            continue;
        }
        let Some(text) = arg.to_str() else {
            c.positional.push(arg.clone());
            continue;
        };
        if text == "--" {
            c.dashdash = true;
            continue;
        }
        if !text.starts_with('-') || text == "-" {
            c.positional.push(arg.clone());
            continue;
        }
        let (name, value) = match text.split_once('=') {
            Some((n, v)) if text.starts_with("--") => (n, Some(v)),
            _ => (text, None),
        };
        match name {
            "-p" | "-u" | "--patch" => c.fmt.patch = true,
            "-s" | "--no-patch" => c.fmt.none = true,
            "--raw" => c.fmt.raw = true,
            "--patch-with-raw" => (c.fmt.patch, c.fmt.raw) = (true, true),
            "--patch-with-stat" => (c.fmt.patch, c.fmt.stat) = (true, true),
            "--stat" => {
                c.fmt.stat = true;
                if let Some(v) = value {
                    stat_value(&mut opts, v)?;
                }
            }
            "--stat-width" | "--stat-name-width" | "--stat-graph-width" | "--stat-count" => {
                c.fmt.stat = true;
                let n = number(name, value.unwrap_or_default())?;
                match name {
                    "--stat-width" => opts.stat_width = Some(n),
                    "--stat-name-width" => opts.stat_name_width = Some(n),
                    "--stat-graph-width" => opts.stat_graph_width = Some(n),
                    _ => opts.stat_count = Some(n),
                }
            }
            "--numstat" => c.fmt.numstat = true,
            "--shortstat" => c.fmt.shortstat = true,
            "--summary" => c.fmt.summary = true,
            "--name-only" => c.fmt.name_only = true,
            "--name-status" => c.fmt.name_status = true,
            "-z" => opts.nul = true,
            "--unified" => opts.context = number(name, value.unwrap_or_default())?,
            "--inter-hunk-context" => opts.inter_hunk = number(name, value.unwrap_or_default())?,
            "--minimal" => opts.algorithm = Algorithm::Minimal,
            "--patience" => opts.algorithm = Algorithm::Patience,
            "--histogram" => opts.algorithm = Algorithm::Histogram,
            "--diff-algorithm" => {
                let v = value.unwrap_or_default();
                opts.algorithm = v
                    .parse()
                    .map_err(|_| CliError::Fatal(format!("option diff-algorithm accepts \"myers\", \"minimal\", \"patience\" and \"histogram\" (not \"{v}\")")))?;
            }
            "--indent-heuristic" => opts.indent_heuristic = true,
            "--no-indent-heuristic" => opts.indent_heuristic = false,
            "--no-renames" => {
                opts.renames = None;
                c.renames_flag = Some(false);
            }
            "--renames" => c.renames_flag = Some(true),
            "--find-renames" => {
                let t = rename_threshold(value.unwrap_or_default())
                    .ok_or_else(|| usage(format!("error: invalid argument to {name}")))?;
                opts.renames = Some(RenameOptions {
                    threshold: t,
                    ..opts.renames.unwrap_or_default()
                });
                c.renames_flag = Some(true);
            }
            "--src-prefix" => opts.src_prefix = Some(value.unwrap_or_default().to_owned()),
            "--dst-prefix" => opts.dst_prefix = Some(value.unwrap_or_default().to_owned()),
            "--no-prefix" => opts.no_prefix = true,
            "--relative" => {
                opts.relative_config = true;
                if let Some(v) = value {
                    opts.relative =
                        asc_core::path::RepoPath::new(v.trim_end_matches('/').as_bytes().to_vec())
                            .ok();
                }
            }
            "--no-relative" => {
                opts.relative_config = false;
                opts.relative = None;
            }
            "--abbrev" => {
                c.abbrev_given = true;
                opts.abbrev = match value {
                    Some(v) => number(name, v)?.clamp(4, 40),
                    None => 7,
                };
            }
            "--full-index" => (opts.abbrev, c.abbrev_given) = (40, true),
            "--binary" => {
                c.binary = true;
                c.fmt.patch = true;
            }
            "-a" | "--text" => c.text = true,
            "--no-color" | "--no-ext-diff" | "--no-textconv" | "--no-color-moved" => {}
            "--color" => match value {
                Some("never" | "auto" | "false" | "no" | "off") => {}
                _ => return Err(unsupported("--color")),
            },
            "--submodule" => match value {
                None | Some("short") => {}
                Some(_) => return Err(unsupported("--submodule")),
            },
            "--word-diff" => match value {
                None | Some("plain") => c.fmt.word_diff = true,
                Some(_) => return Err(unsupported("--word-diff")),
            },
            "--exit-code" => c.exit_code = true,
            "--quiet" => (c.quiet, c.exit_code) = (true, true),
            "--output" => c.output = Some(PathBuf::from(value.unwrap_or_default())),
            "--diff-filter" => c.filter = Some(StatusFilter::parse(value.unwrap_or_default())),
            "--json" => c.json = true,
            _ => {
                if let Some(n) = name
                    .strip_prefix("-U")
                    .filter(|n| n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty())
                {
                    opts.context = number("-U", n)?;
                } else if name == "-U" {
                    let v = args.next().and_then(|v| v.to_str()).unwrap_or_default();
                    opts.context = number("-U", v)?;
                } else if let Some(t) = name.strip_prefix("-M") {
                    let t = rename_threshold(t)
                        .ok_or_else(|| usage("error: invalid argument to -M".into()))?;
                    opts.renames = Some(RenameOptions {
                        threshold: t,
                        ..opts.renames.unwrap_or_default()
                    });
                    c.renames_flag = Some(true);
                } else if text.starts_with("--") || text.len() == 2 {
                    c.extra.push(text.to_owned());
                } else {
                    // A short cluster such as `-pz`: each letter alone.
                    for letter in text[1..].chars() {
                        match letter {
                            'p' | 'u' => c.fmt.patch = true,
                            's' => c.fmt.none = true,
                            'z' => opts.nul = true,
                            'a' => c.text = true,
                            other => c.extra.push(format!("-{other}")),
                        }
                    }
                }
            }
        }
    }
    c.opts = opts;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn rename_scores() {
        assert_eq!(rename_threshold(""), Some(50));
        assert_eq!(rename_threshold("5"), Some(50));
        assert_eq!(rename_threshold("75"), Some(75));
        assert_eq!(rename_threshold("05"), Some(5));
        assert_eq!(rename_threshold("90%"), Some(90));
        assert_eq!(rename_threshold("x"), None);
    }

    #[test]
    fn formats_default_and_override() {
        let c = parse(
            DiffOptions::default(),
            &args(&["--stat", "-p", "a", "--", "b"]),
        )
        .unwrap();
        let f = c.fmt.resolve(false);
        assert!(f.stat && f.patch && !f.raw);
        assert_eq!(c.positional, args(&["a"]));
        assert_eq!(c.paths, args(&["b"]));
        let none = parse(DiffOptions::default(), &args(&["-s"]))
            .unwrap()
            .fmt
            .resolve(false);
        assert!(none.none && !none.patch);
        assert!(
            parse(DiffOptions::default(), &[])
                .unwrap()
                .fmt
                .resolve(true)
                .raw
        );
    }

    #[test]
    fn value_flags() {
        let c = parse(
            DiffOptions::default(),
            &args(&[
                "-U7",
                "--stat=100,40,5",
                "-M60%",
                "--abbrev=10",
                "--src-prefix=x/",
                "--cached",
            ]),
        )
        .unwrap();
        assert_eq!(c.opts.context, 7);
        assert_eq!(
            (c.opts.stat_width, c.opts.stat_name_width, c.opts.stat_count),
            (Some(100), Some(40), Some(5))
        );
        assert_eq!(c.opts.renames.map(|r| r.threshold), Some(60));
        assert_eq!(c.opts.abbrev, 10);
        assert_eq!(c.opts.src_prefix.as_deref(), Some("x/"));
        assert_eq!(c.extra, vec!["--cached".to_owned()]);
    }

    #[test]
    fn binary_implies_patch() {
        let c = parse(DiffOptions::default(), &args(&["--binary"])).unwrap();
        assert!(c.fmt.patch && c.binary && c.opts.abbrev == 7);
    }

    #[test]
    fn status_filter() {
        let f = StatusFilter::parse("ACMR");
        assert!(
            f.keeps(Status::Added) && f.keeps(Status::Renamed(90)) && !f.keeps(Status::Deleted)
        );
        let f = StatusFilter::parse("d");
        assert!(!f.keeps(Status::Deleted) && f.keeps(Status::Modified));
    }
}
