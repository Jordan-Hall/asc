//! Option parsing for `log` and `show`: git's flag grammar (`-n3`, `-3`, `--format=x`,
//! `--author x`, clustered short flags), by hand, because revisions and flags interleave and
//! `--not` changes the meaning of what follows.

use std::ffi::OsString;

use super::order::Sort;
use crate::cli::common::{CliError, Result};

/// One word of the revision part of the command line, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    Rev(String),
    Not,
    All,
    Branches,
    Tags,
    Remotes,
}

/// `--decorate[=short|full|no]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decorate {
    Short,
    Full,
    No,
}

/// `--color[=when]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorWhen {
    Always,
    Never,
    Auto,
}

/// The diff outputs asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffReq {
    /// `Some(true)` for `-p`, `Some(false)` for `-s`.
    pub patch: Option<bool>,
    pub stat: bool,
    pub stat_args: Option<String>,
    pub shortstat: bool,
    pub numstat: bool,
    pub name_only: bool,
    pub name_status: bool,
    pub raw: bool,
    pub summary: bool,
    pub unified: Option<usize>,
    pub no_renames: bool,
    pub algorithm: Option<String>,
}

impl DiffReq {
    /// Whether any non-patch diff format was requested.
    pub fn has_format(&self) -> bool {
        self.stat
            || self.shortstat
            || self.numstat
            || self.name_only
            || self.name_status
            || self.raw
            || self.summary
    }
}

/// Everything the command line says.
#[derive(Clone, Debug, Default)]
pub struct Opts {
    pub revs: Vec<Tok>,
    pub paths: Vec<OsString>,
    /// A `--` separated revisions from pathspecs.
    pub dashdash: bool,
    pub max_count: Option<usize>,
    pub skip: usize,
    pub since: Option<String>,
    pub until: Option<String>,
    pub authors: Vec<String>,
    pub committers: Vec<String>,
    pub greps: Vec<String>,
    pub icase: bool,
    pub fixed: bool,
    pub extended: bool,
    pub invert_grep: bool,
    pub first_parent: bool,
    pub follow: bool,
    pub min_parents: Option<usize>,
    pub max_parents: Option<usize>,
    pub reverse: bool,
    /// `--topo-order`, `--date-order`, `--author-date-order`.
    pub order: Option<Sort>,
    pub left_right: bool,
    pub walk_reflogs: bool,
    pub graph: bool,
    pub pretty: Option<String>,
    pub abbrev_commit: Option<bool>,
    pub abbrev: Option<usize>,
    pub date: Option<String>,
    pub relative_date: bool,
    pub decorate: Option<Decorate>,
    pub source: bool,
    pub color: Option<ColorWhen>,
    pub diff: DiffReq,
    pub nul: bool,
    pub json: bool,
}

fn bad_number(_flag: &str, value: &str) -> CliError {
    CliError::Raw(format!("fatal: '{value}': not an integer"), 128)
}

fn number(flag: &str, value: &str) -> Result<usize> {
    value.parse().map_err(|_| bad_number(flag, value))
}

fn color_when(value: Option<&str>) -> Result<ColorWhen> {
    match value {
        None | Some("always" | "true" | "yes" | "on" | "1") => Ok(ColorWhen::Always),
        Some("never" | "false" | "no" | "off" | "0") => Ok(ColorWhen::Never),
        Some("auto") => Ok(ColorWhen::Auto),
        Some(v) => Err(CliError::Raw(
            format!(
                "error: option `color' expects \"always\", \"auto\", or \"never\"\nfatal: bad color value '{v}'"
            ),
            129,
        )),
    }
}

impl Opts {
    /// Parses `args` (the words after the command name). `--` starts the pathspecs.
    pub fn parse(args: &[OsString]) -> Result<Self> {
        let mut o = Self::default();
        let mut it = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .peekable();
        let mut args_done = false;
        while let Some(arg) = it.next() {
            if args_done {
                o.paths.push(arg.into());
                continue;
            }
            if arg == "--" {
                args_done = true;
                o.dashdash = true;
                continue;
            }
            if arg == "--end-of-options" {
                if let Some(rev) = it.next() {
                    o.revs.push(Tok::Rev(rev));
                }
                continue;
            }
            let Some(flag) = arg.strip_prefix('-').filter(|f| !f.is_empty()) else {
                o.revs.push(Tok::Rev(arg));
                continue;
            };
            if let Some(long) = flag.strip_prefix('-') {
                o.long(long, &mut it)?;
            } else if flag.bytes().all(|b| b.is_ascii_digit()) {
                o.max_count = Some(number("-<n>", flag)?);
            } else {
                o.short(flag, &mut it)?;
            }
        }
        Ok(o)
    }

    fn short(&mut self, cluster: &str, it: &mut impl Iterator<Item = String>) -> Result<()> {
        let chars = cluster.char_indices();
        for (at, c) in chars {
            let rest = &cluster[at + c.len_utf8()..];
            match c {
                'n' | 'U' => {
                    let value = if rest.is_empty() {
                        it.next().unwrap_or_default()
                    } else {
                        rest.to_owned()
                    };
                    let n = number(&format!("-{c}"), value.trim_start_matches('='))?;
                    if c == 'n' {
                        self.max_count = Some(n);
                    } else {
                        self.diff.unified = Some(n);
                    }
                    return Ok(());
                }
                'M' => {
                    self.diff.no_renames = false;
                    return Ok(());
                }
                'p' | 'u' => self.diff.patch = Some(true),
                's' => self.diff.patch = Some(false),
                'i' => self.icase = true,
                'E' => self.extended = true,
                'F' => self.fixed = true,
                'z' => self.nul = true,
                'g' => self.walk_reflogs = true,
                other => {
                    return Err(CliError::Usage(format!("error: unknown switch `{other}'")));
                }
            }
        }
        Ok(())
    }

    fn long(&mut self, flag: &str, it: &mut impl Iterator<Item = String>) -> Result<()> {
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) => (n, Some(v.to_owned())),
            None => (flag, None),
        };
        // A required value comes from `=value` or the next word.
        let mut value = |inline: Option<String>| -> Result<String> {
            inline
                .or_else(|| it.next())
                .ok_or_else(|| CliError::Usage(format!("error: option `{name}' requires a value")))
        };
        match name {
            "max-count" => self.max_count = Some(number("--max-count", &value(inline)?)?),
            "skip" => self.skip = number("--skip", &value(inline)?)?,
            "since" | "after" => self.since = Some(value(inline)?),
            "until" | "before" => self.until = Some(value(inline)?),
            "author" => self.authors.push(value(inline)?),
            "committer" => self.committers.push(value(inline)?),
            "grep" => self.greps.push(value(inline)?),
            "regexp-ignore-case" => self.icase = true,
            "extended-regexp" => self.extended = true,
            "fixed-strings" => self.fixed = true,
            "invert-grep" => self.invert_grep = true,
            "first-parent" => self.first_parent = true,
            "follow" => self.follow = true,
            "merges" => self.min_parents = Some(2),
            "no-merges" => self.max_parents = Some(1),
            "min-parents" => self.min_parents = Some(number("--min-parents", &value(inline)?)?),
            "max-parents" => self.max_parents = Some(number("--max-parents", &value(inline)?)?),
            "reverse" => self.reverse = true,
            "left-right" => self.left_right = true,
            "walk-reflogs" => self.walk_reflogs = true,
            "graph" => self.graph = true,
            "topo-order" => self.order = Some(Sort::Topo),
            "date-order" => self.order = Some(Sort::TopoDate),
            "author-date-order" => self.order = Some(Sort::TopoAuthorDate),
            "all" => self.revs.push(Tok::All),
            "branches" => self.revs.push(Tok::Branches),
            "tags" => self.revs.push(Tok::Tags),
            "remotes" => self.revs.push(Tok::Remotes),
            "not" => self.revs.push(Tok::Not),
            "oneline" => {
                self.pretty = Some("oneline".into());
                self.abbrev_commit.get_or_insert(true);
            }
            "format" => self.pretty = Some(format!("tformat:{}", value(inline)?)),
            "pretty" => self.pretty = Some(inline.unwrap_or_else(|| "medium".into())),
            "abbrev-commit" => self.abbrev_commit = Some(true),
            "no-abbrev-commit" => self.abbrev_commit = Some(false),
            "abbrev" => {
                self.abbrev = Some(match inline {
                    Some(v) => number("--abbrev", &v)?.clamp(4, 40),
                    None => 7,
                });
            }
            "no-abbrev" => self.abbrev = Some(40),
            "date" => self.date = Some(value(inline)?),
            "relative-date" => self.relative_date = true,
            "decorate" => {
                self.decorate = Some(match inline.as_deref() {
                    None | Some("short") => Decorate::Short,
                    Some("full") => Decorate::Full,
                    Some("no") => Decorate::No,
                    Some(v) => {
                        return Err(CliError::Raw(
                            format!("fatal: invalid --decorate option: {v}"),
                            128,
                        ));
                    }
                });
            }
            "no-decorate" => self.decorate = Some(Decorate::No),
            "source" => self.source = true,
            "color" => self.color = Some(color_when(inline.as_deref())?),
            "no-color" => self.color = Some(ColorWhen::Never),
            "patch" => self.diff.patch = Some(true),
            "no-patch" => self.diff.patch = Some(false),
            "stat" => {
                self.diff.stat = true;
                self.diff.stat_args = inline;
            }
            "shortstat" => self.diff.shortstat = true,
            "numstat" => self.diff.numstat = true,
            "name-only" => self.diff.name_only = true,
            "name-status" => self.diff.name_status = true,
            "raw" => self.diff.raw = true,
            "summary" => self.diff.summary = true,
            "unified" => self.diff.unified = Some(number("--unified", &value(inline)?)?),
            "no-renames" => self.diff.no_renames = true,
            "patience" => self.diff.algorithm = Some("patience".into()),
            "histogram" => self.diff.algorithm = Some("histogram".into()),
            "minimal" => self.diff.algorithm = Some("minimal".into()),
            "diff-algorithm" => self.diff.algorithm = Some(value(inline)?),
            "json" => self.json = true,
            other => {
                return Err(CliError::Usage(format!("error: unknown option `{other}'")));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(items: &[&str]) -> Opts {
        let v: Vec<OsString> = items.iter().map(OsString::from).collect();
        Opts::parse(&v).unwrap()
    }

    #[test]
    fn counts_in_every_spelling() {
        assert_eq!(parse(&["-3"]).max_count, Some(3));
        assert_eq!(parse(&["-n", "4"]).max_count, Some(4));
        assert_eq!(parse(&["-n5"]).max_count, Some(5));
        assert_eq!(parse(&["--max-count=6"]).max_count, Some(6));
    }

    #[test]
    fn revisions_flags_and_paths_interleave() {
        let o = parse(&[
            "master",
            "--oneline",
            "^dev",
            "--not",
            "x",
            "--",
            "a.txt",
            "-weird",
        ]);
        assert_eq!(
            o.revs,
            vec![
                Tok::Rev("master".into()),
                Tok::Rev("^dev".into()),
                Tok::Not,
                Tok::Rev("x".into())
            ]
        );
        assert_eq!(o.pretty.as_deref(), Some("oneline"));
        assert_eq!(
            o.paths,
            vec![OsString::from("a.txt"), OsString::from("-weird")]
        );
    }

    #[test]
    fn values_come_inline_or_next() {
        assert_eq!(
            parse(&["--author", "bob", "--grep=fix"]).authors,
            vec!["bob"]
        );
        assert_eq!(
            parse(&["--format=%H"]).pretty.as_deref(),
            Some("tformat:%H")
        );
        assert_eq!(parse(&["--pretty"]).pretty.as_deref(), Some("medium"));
        assert!(parse(&["-ps"]).diff.patch == Some(false));
    }

    #[test]
    fn bad_number_is_fatal() {
        let v = [OsString::from("-n"), OsString::from("x")];
        assert!(matches!(Opts::parse(&v), Err(CliError::Raw(_, 128))));
    }
}
