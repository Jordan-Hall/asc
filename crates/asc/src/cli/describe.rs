//! `describe` (SPEC §11.2): the nearest tag of a commit as `tag-N-gHASH`, using the same tag
//! search as `log`'s `%(describe)` ([`super::log::describe::Describer`]).

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use super::common::{CliError, Ctx, Result};
use super::log::describe::Describer;
use super::log::refs;
use super::log::walk::Hist;
use super::output::Output;
use super::pager::PagerWant;
use super::revparse::commit_of;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(
    name = "describe",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// Flags and commit-ishes.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

const FLAGS: &[&str] = &[
    "--tags",
    "--long",
    "--always",
    "--abbrev",
    "--match",
    "--exclude",
];

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "describe",
    flags: FLAGS,
    placeholders: &[],
}];

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

#[derive(Default)]
struct Opts {
    tags: bool,
    long: bool,
    always: bool,
    abbrev: Option<usize>,
    matches: Vec<String>,
    excludes: Vec<String>,
    commits: Vec<String>,
}

impl Opts {
    fn parse(args: &[OsString]) -> Result<Self> {
        let mut o = Self::default();
        let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
        while let Some(arg) = it.next() {
            let (name, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(n, v)| (n, Some(v.to_owned())));
            let mut value = |inline: Option<String>| {
                inline.or_else(|| it.next()).ok_or_else(|| {
                    CliError::Usage(format!(
                        "error: option `{}' requires a value",
                        name.trim_start_matches('-')
                    ))
                })
            };
            match name {
                "--tags" => o.tags = true,
                "--long" => o.long = true,
                "--always" => o.always = true,
                "--abbrev" => {
                    if let Some(v) = inline {
                        o.abbrev = Some(v.parse::<usize>().map_err(|_| {
                            CliError::Raw(
                                format!("fatal: '{v}': not an integer (for --abbrev)"),
                                128,
                            )
                        })?);
                    }
                }
                "--match" => o.matches.push(value(inline)?),
                "--exclude" => o.excludes.push(value(inline)?),
                _ => o.commits.push(arg),
            }
        }
        Ok(o)
    }

    /// The option string `Describer` reads.
    fn describer_options(&self, abbrev: usize) -> String {
        let mut parts = vec![format!("abbrev={abbrev}")];
        if self.tags {
            parts.push("tags".into());
        }
        parts.extend(self.matches.iter().map(|m| format!("match={m}")));
        parts.extend(self.excludes.iter().map(|m| format!("exclude={m}")));
        parts.join(",")
    }
}

/// Whether `text` already ends in `-<n>-g<hex>`.
fn has_distance(text: &str) -> bool {
    let Some((head, hash)) = text.rsplit_once("-g") else {
        return false;
    };
    let Some((_, n)) = head.rsplit_once('-') else {
        return false;
    };
    !hash.is_empty()
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && n.bytes().all(|b| b.is_ascii_digit())
        && !n.is_empty()
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = Opts::parse(&a.rest)?;
    if o.long && o.abbrev == Some(0) {
        return Err(fatal(
            "options '--long' and '--abbrev=0' cannot be used together",
        ));
    }
    let ws = ctx.ws()?;
    let default = ctx
        .config
        .get("core.abbrev")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(7, |n| n.clamp(4, 40));
    let abbrev = o
        .abbrev
        .map_or(default, |n| if n == 0 { 0 } else { n.clamp(4, 40) });
    let describer = Describer::load(ws, abbrev).await?;
    let mut out = Output::open(ctx, "describe", PagerWant::Never)?;
    let mut result = Ok(());
    let words: Vec<String> = if o.commits.is_empty() {
        vec!["HEAD".into()]
    } else {
        o.commits.clone()
    };
    for word in &words {
        result = describe_one(ctx, &o, &describer, abbrev, word, &mut out).await;
        if result.is_err() {
            break;
        }
    }
    let flushed = out.finish();
    result?;
    flushed?;
    Ok(ExitCode::SUCCESS)
}

async fn describe_one(
    ctx: &Ctx,
    o: &Opts,
    describer: &Describer,
    abbrev: usize,
    word: &str,
    out: &mut Output,
) -> Result<()> {
    let ws = ctx.ws()?;
    let id = commit_of(ctx, word)
        .await?
        .ok_or_else(|| fatal(format!("Not a valid object name {word}")))?;
    let hex = ws
        .engine
        .git_commit_of(id)
        .await
        .map_err(fatal)?
        .to_string();
    let hist = Hist::select(ws);
    let found = describer
        .describe(ws, hist.api(ws), id, &hex, &o.describer_options(abbrev))
        .await;
    let line = if found.is_empty() {
        if o.always {
            hex[..abbrev.clamp(4, 40)].to_owned()
        } else {
            return Err(none_found(ws, o, &hex).await?);
        }
    } else if o.long && !has_distance(&found) {
        format!("{found}-0-g{}", &hex[..abbrev.clamp(4, 40)])
    } else {
        found
    };
    writeln!(out, "{line}")?;
    Ok(())
}

/// A shell glob (`*`, `?`, `[set]`) matched against the whole of `text`.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let mut re = String::from("^");
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                re.push('[');
                for b in chars.by_ref() {
                    re.push(b);
                    if b == ']' {
                        break;
                    }
                }
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re).is_ok_and(|r| r.is_match(text))
}

impl Opts {
    /// Whether `--match`/`--exclude` leave this tag name in play.
    fn names(&self, tag: &str) -> bool {
        (self.matches.is_empty() || self.matches.iter().any(|m| glob_matches(m, tag)))
            && !self.excludes.iter().any(|m| glob_matches(m, tag))
    }
}

/// git's error for a commit no tag describes.
async fn none_found(ws: &super::common::WsCtx, o: &Opts, hex: &str) -> Result<CliError> {
    let tips = refs::tips(ws, "refs/tags/").await?;
    let names: Vec<_> = tips
        .iter()
        .filter(|t| o.names(t.name.strip_prefix("refs/tags/").unwrap_or(&t.name)))
        .collect();
    Ok(if names.is_empty() {
        CliError::Fatal("No names found, cannot describe anything.".into())
    } else if !o.tags && names.iter().all(|t| t.tag.is_none()) {
        CliError::Raw(
            format!(
                "fatal: No annotated tags can describe '{hex}'.\nHowever, there were unannotated tags: try --tags."
            ),
            128,
        )
    } else {
        CliError::Raw(
            format!("fatal: No tags can describe '{hex}'.\nTry --always, or create some tags."),
            128,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_suffix_is_recognised() {
        assert!(has_distance("v1-3-gabc1234"));
        assert!(!has_distance("v1"));
        assert!(!has_distance("v1-final"));
        assert!(!has_distance("rel-x-gabc"));
    }

    #[test]
    fn filters_apply_to_tag_names() {
        let o = Opts {
            matches: vec!["v*".into()],
            excludes: vec!["v2".into()],
            ..Opts::default()
        };
        assert!(o.names("v1") && !o.names("v2") && !o.names("x1"));
        assert!(glob_matches("v[0-9]", "v7") && !glob_matches("v1.0", "v1x0"));
    }

    #[test]
    fn options_become_describer_text() {
        let args: Vec<OsString> = ["--tags", "--match", "v*", "--exclude=x*", "HEAD~1"]
            .iter()
            .map(OsString::from)
            .collect();
        let o = Opts::parse(&args).unwrap();
        assert_eq!(o.describer_options(9), "abbrev=9,tags,match=v*,exclude=x*");
        assert_eq!(o.commits, ["HEAD~1"]);
    }
}
