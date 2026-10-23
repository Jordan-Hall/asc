//! `var` (SPEC §11.2): git's logical variables (`GIT_AUTHOR_IDENT`, `GIT_EDITOR`, …), and with
//! `-l` the configuration followed by them.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use crate::cli::common::{CliError, Ctx, Result};
use crate::env::IdentEnv;

const USAGE: &str = super::usage::VAR;

/// The variables `-l` lists after the configuration, in git's order.
const LISTED: [&str; 7] = [
    "GIT_COMMITTER_IDENT",
    "GIT_AUTHOR_IDENT",
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_PAGER",
    "GIT_DEFAULT_BRANCH",
    "GIT_SHELL_PATH",
];

fn text(v: &std::ffi::OsStr) -> String {
    v.to_string_lossy().into_owned()
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

/// The zone of the process in minutes east of UTC, from a `TZ` of the form `UTC±h[:m]`; UTC else.
fn local_offset_min() -> i32 {
    let Ok(tz) = std::env::var("TZ") else {
        return 0;
    };
    let rest = tz.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let (sign, digits) = match rest.as_bytes().first() {
        Some(b'-') => (1, &rest[1..]),
        Some(b'+') => (-1, &rest[1..]),
        _ => (-1, rest),
    };
    let mut parts = digits.split(':');
    let h: i32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let m: i32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    sign * (h * 60 + m)
}

/// `Name <email> seconds +zone` for the author or committer.
fn ident(ctx: &Ctx, env: &IdentEnv, section: &str) -> Result<String> {
    let name = non_empty(env.name.clone())
        .or_else(|| non_empty(ctx.config.get(&format!("{section}.name"))))
        .or_else(|| non_empty(ctx.config.get("user.name")))
        .unwrap_or_default();
    let email = non_empty(env.email.clone())
        .or_else(|| non_empty(ctx.config.get(&format!("{section}.email"))))
        .or_else(|| non_empty(ctx.config.get("user.email")))
        .or_else(|| non_empty(std::env::var("EMAIL").ok()))
        .ok_or_else(|| CliError::Fatal("unable to auto-detect email address".into()))?;
    if name.is_empty() {
        return Err(CliError::Fatal(format!(
            "empty ident name (for <{email}>) not allowed"
        )));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (seconds, zone) = env
        .date
        .as_deref()
        .and_then(|d| {
            let d = d.strip_prefix('@').unwrap_or(d);
            let (s, z) = d.split_once(' ').unwrap_or((d, ""));
            let secs = s.parse::<i64>().ok()?;
            let zone = asc_core::git::parse_tz(z.trim().as_bytes()).unwrap_or(0);
            Some((secs, i32::from(zone)))
        })
        .unwrap_or((now, local_offset_min()));
    let sign = if zone < 0 { '-' } else { '+' };
    Ok(format!(
        "{name} <{email}> {seconds} {sign}{:02}{:02}",
        zone.abs() / 60,
        zone.abs() % 60
    ))
}

/// The editor: `$GIT_EDITOR`, `core.editor`, `$VISUAL`, `$EDITOR`, then `vi` unless the terminal
/// is dumb.
fn editor(ctx: &Ctx) -> Option<String> {
    let found = ctx
        .env
        .editor
        .as_deref()
        .map(text)
        .or_else(|| non_empty(ctx.config.get("core.editor")))
        .or_else(|| non_empty(std::env::var("VISUAL").ok()))
        .or_else(|| non_empty(std::env::var("EDITOR").ok()));
    if found.is_some() {
        return found;
    }
    let dumb = std::env::var("TERM").map_or(true, |t| t == "dumb");
    (!dumb).then(|| "vi".to_owned())
}

fn value(ctx: &Ctx, name: &str) -> Result<Option<String>> {
    Ok(match name {
        "GIT_AUTHOR_IDENT" => Some(ident(ctx, &ctx.env.author, "author")?),
        "GIT_COMMITTER_IDENT" => Some(ident(ctx, &ctx.env.committer, "committer")?),
        "GIT_EDITOR" => editor(ctx),
        "GIT_SEQUENCE_EDITOR" => ctx
            .env
            .sequence_editor
            .as_deref()
            .map(text)
            .or_else(|| non_empty(ctx.config.get("sequence.editor")))
            .or_else(|| editor(ctx)),
        "GIT_PAGER" => ctx
            .env
            .pager
            .as_deref()
            .map(text)
            .or_else(|| non_empty(ctx.config.get("core.pager")))
            .or_else(|| {
                ctx.env
                    .pager_fallback
                    .as_deref()
                    .map(text)
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| Some("less".to_owned())),
        "GIT_DEFAULT_BRANCH" => Some(
            non_empty(ctx.config.get("init.defaultBranch")).unwrap_or_else(|| "master".to_owned()),
        ),
        "GIT_SHELL_PATH" => Some(ctx.shell().to_string_lossy().replace('\\', "/")),
        _ => return Err(CliError::Usage(USAGE.to_owned())),
    })
}

/// `section[.sub].name=value` of every config entry, in file order.
fn config_lines(ctx: &Ctx) -> Vec<String> {
    let mut out = Vec::new();
    for section in ctx.config.file().sections() {
        let header = section.header();
        let name = header.name().to_string().to_ascii_lowercase();
        let prefix = match header.subsection_name() {
            Some(sub) => format!("{name}.{sub}"),
            None => name,
        };
        let body = section.body();
        let mut seen: std::collections::HashMap<String, usize> = Default::default();
        for key in body.value_names() {
            let lower = key.to_ascii_lowercase();
            let index = seen.entry(lower.clone()).or_insert(0);
            let value = body.values(&key).get(*index).map(|v| v.to_string());
            *index += 1;
            out.push(match value {
                Some(v) => format!("{prefix}.{lower}={v}"),
                None => format!("{prefix}.{lower}"),
            });
        }
    }
    out
}

pub fn run(ctx: &Ctx, args: &[OsString]) -> Result<ExitCode> {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut out = std::io::stdout().lock();
    match args.as_slice() {
        [flag] if flag == "-l" => {
            for line in config_lines(ctx) {
                writeln!(out, "{line}")?;
            }
            for name in LISTED {
                if let Some(v) = value(ctx, name)? {
                    writeln!(out, "{name}={v}")?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        [name] if !name.starts_with('-') => match value(ctx, name)? {
            Some(v) => {
                writeln!(out, "{v}")?;
                Ok(ExitCode::SUCCESS)
            }
            None => Ok(ExitCode::from(1)),
        },
        _ => Err(CliError::Usage(USAGE.to_owned())),
    }
}
