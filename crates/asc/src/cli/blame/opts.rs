//! `blame` arguments in git's grammar: options, an optional revision and the file.

use std::ffi::OsString;
use std::ops::Range;

use crate::cli::common::{CliError, Result};

pub const USAGE: &str = "usage: git blame [<options>] [<rev-opts>] [<rev>] [--] <file>";

/// Most `-L` ranges accepted; each one is bounded work, but the count is input.
const MAX_RANGES: usize = 1024;

/// How the result is printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Default,
    Porcelain,
    LinePorcelain,
    Incremental,
    Json,
}

/// One end of a `-L` range as typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// Absent: the first line (start) or the last line (end).
    Open,
    At(u32),
    /// `+N`: N lines from the start.
    Plus(u32),
    /// `-N`: N lines ending at the start.
    Minus(u32),
}

/// A `-L start,end` range as typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeSpec {
    start: End,
    end: End,
}

#[derive(Debug)]
pub struct Opts {
    pub format: Format,
    pub ranges: Vec<RangeSpec>,
    pub ignore_ws: bool,
    pub reverse: bool,
    pub root: bool,
    /// The revision word before the file, if any.
    pub rev: Option<String>,
    pub file: OsString,
}

fn usage() -> CliError {
    CliError::Usage(USAGE.to_owned())
}

fn bad_line(text: &str) -> CliError {
    CliError::Fatal(format!("-L invalid line number: {text}"))
}

/// A line number as git reads it: digits only, nonzero. Anything else is a usage error.
fn number(text: &str) -> Result<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(usage());
    }
    match text.parse::<u32>() {
        Ok(0) => Err(bad_line(text)),
        Ok(n) => Ok(n),
        Err(_) => Ok(u32::MAX),
    }
}

fn empty_range() -> CliError {
    CliError::Fatal("-L invalid empty range".into())
}

fn end_of(text: &str, is_end: bool) -> Result<End> {
    if text.is_empty() {
        return Ok(End::Open);
    }
    if text.starts_with(['/', ':']) {
        return Err(CliError::Fatal(
            "asc: blame -L with a regular expression or a function name is not supported".into(),
        ));
    }
    let count = |n: &str| match n {
        "0" => Err(empty_range()),
        n => number(n),
    };
    if is_end && let Some(n) = text.strip_prefix('+') {
        return Ok(End::Plus(count(n)?));
    }
    if is_end && let Some(n) = text.strip_prefix('-') {
        return Ok(End::Minus(count(n)?));
    }
    Ok(End::At(number(
        text.strip_prefix('+').filter(|_| !is_end).unwrap_or(text),
    )?))
}

impl RangeSpec {
    pub fn parse(text: &str) -> Result<Self> {
        let (a, b) = text.split_once(',').unwrap_or((text, ""));
        Ok(Self {
            start: end_of(a, false)?,
            end: end_of(b, true)?,
        })
    }

    /// The zero-based half-open range in a file of `lines` lines, as git clamps it.
    fn resolve(self, lines: u32, file: &str) -> Result<Range<u32>> {
        let start = match self.start {
            End::At(n) => n,
            _ => 1,
        };
        let (mut a, mut b) = match self.end {
            End::Open => (start, lines),
            End::At(n) => (start, n),
            End::Plus(n) => (start, start.saturating_add(n - 1)),
            End::Minus(n) => ((start + 1).saturating_sub(n).max(1), start),
        };
        if a > b && matches!(self.end, End::At(_)) {
            std::mem::swap(&mut a, &mut b);
        }
        if a > lines {
            let s = if lines == 1 { "" } else { "s" };
            return Err(CliError::Fatal(format!(
                "file {file} has only {lines} line{s}"
            )));
        }
        Ok(a - 1..b.min(lines))
    }
}

/// The zero-based ranges to show, sorted and merged; the whole file when none was typed.
pub fn resolve_ranges(specs: &[RangeSpec], lines: u32, file: &str) -> Result<Vec<Range<u32>>> {
    if specs.is_empty() {
        #[allow(clippy::single_range_in_vec_init)] // one line range, not a list of lines
        return Ok(vec![0..lines]);
    }
    let mut all = specs
        .iter()
        .map(|s| s.resolve(lines, file))
        .collect::<Result<Vec<_>>>()?;
    all.sort_by_key(|r| (r.start, r.end));
    let mut out: Vec<Range<u32>> = Vec::new();
    for r in all {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    Ok(out)
}

impl Opts {
    pub fn parse(rest: &[OsString]) -> Result<Self> {
        let mut o = Opts {
            format: Format::Default,
            ranges: Vec::new(),
            ignore_ws: false,
            reverse: false,
            root: false,
            rev: None,
            file: OsString::new(),
        };
        let mut before: Vec<String> = Vec::new();
        let mut after: Vec<OsString> = Vec::new();
        let mut dashes = false;
        let mut options_ended = false;
        let mut i = 0;
        while i < rest.len() {
            let arg = &rest[i];
            i += 1;
            let text = arg.to_string_lossy().into_owned();
            if dashes {
                after.push(arg.clone());
                continue;
            }
            if options_ended || !text.starts_with('-') || text == "-" {
                before.push(text);
                continue;
            }
            if text == "--" {
                dashes = true;
                continue;
            }
            if text == "--end-of-options" {
                options_ended = true;
                continue;
            }
            if let Some(long) = text.strip_prefix("--") {
                match long {
                    "porcelain" => o.format = Format::Porcelain,
                    "line-porcelain" => o.format = Format::LinePorcelain,
                    "incremental" => o.format = Format::Incremental,
                    "json" => o.format = Format::Json,
                    "reverse" => o.reverse = true,
                    "root" => o.root = true,
                    other => {
                        return Err(CliError::Usage(format!("error: unknown option `{other}'")));
                    }
                }
                continue;
            }
            for (at, c) in text[1..].char_indices() {
                match c {
                    'w' => o.ignore_ws = true,
                    'p' => o.format = Format::Porcelain,
                    'L' => {
                        let attached = &text[1 + at + 1..];
                        let value = if attached.is_empty() {
                            let v = rest.get(i).ok_or_else(|| {
                                CliError::Usage("error: switch `L' requires a value".into())
                            })?;
                            i += 1;
                            v.to_string_lossy().into_owned()
                        } else {
                            attached.to_owned()
                        };
                        if o.ranges.len() >= MAX_RANGES {
                            return Err(CliError::Fatal("too many -L ranges".into()));
                        }
                        o.ranges.push(RangeSpec::parse(&value)?);
                        break;
                    }
                    other => {
                        return Err(CliError::Usage(format!("error: unknown switch `{other}'")));
                    }
                }
            }
        }
        if dashes {
            match after.as_slice() {
                [file] if before.len() <= 1 => {
                    o.file = file.clone();
                    o.rev = before.pop();
                }
                _ => return Err(usage()),
            }
        } else {
            match before.len() {
                1 => o.file = OsString::from(before.remove(0)),
                2 => {
                    o.file = OsString::from(before.remove(1));
                    o.rev = Some(before.remove(0));
                }
                _ => return Err(usage()),
            }
        }
        Ok(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn range(s: &str, lines: u32) -> Result<Range<u32>> {
        RangeSpec::parse(s)?.resolve(lines, "f")
    }

    #[test]
    fn files_and_revisions() {
        let o = Opts::parse(&args(&["f"])).unwrap();
        assert_eq!((o.rev, o.file), (None, "f".into()));
        let o = Opts::parse(&args(&["HEAD~1", "f"])).unwrap();
        assert_eq!((o.rev.as_deref(), o.file), (Some("HEAD~1"), "f".into()));
        let o = Opts::parse(&args(&["-w", "--porcelain", "--", "f"])).unwrap();
        assert!(o.ignore_ws && o.format == Format::Porcelain && o.rev.is_none());
        let o = Opts::parse(&args(&["v1", "--", "f"])).unwrap();
        assert_eq!(o.rev.as_deref(), Some("v1"));
        assert!(matches!(Opts::parse(&args(&[])), Err(CliError::Usage(_))));
        assert!(matches!(
            Opts::parse(&args(&["a", "b", "c"])),
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            Opts::parse(&args(&["--", "a", "b"])),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn option_values_attach_or_follow() {
        let o = Opts::parse(&args(&["-L1,2", "-L", "4,+2", "-wL5", "f"])).unwrap();
        assert_eq!(o.ranges.len(), 3);
        assert!(o.ignore_ws);
        assert!(Opts::parse(&args(&["f", "-L"])).is_err());
    }

    #[test]
    fn ranges_follow_git() {
        assert_eq!(range("2,3", 6).unwrap(), 1..3);
        assert_eq!(range("3,+2", 6).unwrap(), 2..4);
        assert_eq!(range("5,-2", 6).unwrap(), 3..5);
        assert_eq!(range("5,3", 6).unwrap(), 2..5);
        assert_eq!(range(",2", 6).unwrap(), 0..2);
        assert_eq!(range("5", 6).unwrap(), 4..6);
        assert_eq!(range("5,50", 6).unwrap(), 4..6);
        assert!(range("9", 6).is_err());
        assert!(matches!(RangeSpec::parse("0,2"), Err(CliError::Fatal(_))));
        assert!(matches!(RangeSpec::parse("/x/"), Err(CliError::Fatal(_))));
        assert!(matches!(RangeSpec::parse("a,2"), Err(CliError::Usage(_))));
        assert!(matches!(RangeSpec::parse("1,+0"), Err(CliError::Fatal(_))));
        assert_eq!(range("+3", 6).unwrap(), 2..6);
    }

    #[test]
    fn ranges_merge() {
        let specs: Vec<_> = ["1,3", "2,5", "8,9"]
            .iter()
            .map(|s| RangeSpec::parse(s).unwrap())
            .collect();
        assert_eq!(resolve_ranges(&specs, 10, "f").unwrap(), vec![0..5, 7..9]);
        let whole = resolve_ranges(&[], 4, "f").unwrap();
        assert_eq!((whole.len(), whole[0].clone()), (1, 0..4));
    }
}
