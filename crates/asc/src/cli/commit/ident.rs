//! Author and committer identities of a commit: `--author`, `--date`, `GIT_AUTHOR_*` /
//! `GIT_COMMITTER_*`, `author.*`, `committer.*`, `user.*` and `EMAIL`, in git's order.

use asc_core::sig::Signature;
use gix_date::Time;

use crate::cli::common::{CliError, Ctx, Result};
use crate::env::IdentEnv;

/// Git's text when no identity is configured.
const UNKNOWN: &str = "Author identity unknown\n\n*** Please tell me who you are.\n\nRun\n\n  git config --global user.email \"you@example.com\"\n  git config --global user.name \"Your Name\"\n\nto set your account's default identity.\nOmit --global to set the identity only in this repository.\n\nfatal: unable to auto-detect email address";

/// Who wrote and who committed.
#[derive(Clone, Debug)]
pub struct Idents {
    pub author: Signature,
    pub committer: Signature,
}

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

/// Parses a git date (`@<secs> <tz>`, ISO 8601, RFC 2822, and the other formats git reads).
pub fn parse_date(text: &str) -> Result<Time> {
    gix_date::parse(text, None).map_err(|_| fatal(format!("invalid date format: {text}")))
}

fn signature(name: &str, email: &str, time: Time) -> Result<Signature> {
    let tz = i16::try_from(time.offset / 60).unwrap_or(0);
    Signature::new(
        name.as_bytes().to_vec(),
        email.as_bytes().to_vec(),
        time.seconds,
        tz,
    )
    .map_err(|e| fatal(e.to_string()))
}

/// `Name <email>` split into its parts.
fn split_ident(text: &str) -> Option<(&str, &str)> {
    let (name, rest) = text.split_once('<')?;
    let email = rest.strip_suffix('>')?;
    Some((name.trim(), email.trim()))
}

fn pick(ctx: &Ctx, role: &str, env: &IdentEnv) -> (Option<String>, Option<String>) {
    let name = env
        .name
        .clone()
        .or_else(|| ctx.config.get(&format!("{role}.name")))
        .or_else(|| ctx.config.get("user.name"));
    let email = env
        .email
        .clone()
        .or_else(|| ctx.config.get(&format!("{role}.email")))
        .or_else(|| ctx.config.get("user.email"))
        .or_else(|| std::env::var("EMAIL").ok().filter(|e| !e.is_empty()));
    (name, email)
}

fn ident(ctx: &Ctx, role: &str, env: &IdentEnv, date: Option<&str>) -> Result<Signature> {
    let (name, email) = pick(ctx, role, env);
    let Some(email) = email.filter(|e| !e.is_empty()) else {
        return Err(CliError::Raw(UNKNOWN.to_owned(), 128));
    };
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(fatal(format!(
            "empty ident name (for <{email}>) not allowed"
        )));
    };
    let time = match date.or(env.date.as_deref()) {
        Some(d) => parse_date(d)?,
        None => Time::now_local_or_utc(),
    };
    signature(&name, &email, time)
}

/// The identities for a new commit. `amend` is the author of the commit being amended, kept
/// unless `--author` or `--date` replaces it.
pub fn resolve(
    ctx: &Ctx,
    author: Option<&str>,
    date: Option<&str>,
    amend: Option<&Signature>,
) -> Result<Idents> {
    let committer = ident(ctx, "committer", &ctx.env.committer, None)?;
    let author = match (author, amend) {
        (Some(text), _) => {
            let Some((name, email)) = split_ident(text) else {
                return Err(fatal(format!(
                    "--author '{text}' is not 'Name <email>' and matches no existing author"
                )));
            };
            let time = match date.or(ctx.env.author.date.as_deref()) {
                Some(d) => parse_date(d)?,
                None => Time::now_local_or_utc(),
            };
            signature(name, email, time)?
        }
        (None, Some(old)) => match date {
            Some(d) => Signature::new(
                old.name().to_vec(),
                old.email().to_vec(),
                parse_date(d)?.seconds,
                i16::try_from(parse_date(d)?.offset / 60).unwrap_or(0),
            )
            .map_err(|e| fatal(e.to_string()))?,
            None => old.clone(),
        },
        (None, None) => ident(ctx, "author", &ctx.env.author, date)?,
    };
    Ok(Idents { author, committer })
}

/// `Name <email>` as git prints it.
pub fn display(sig: &Signature) -> String {
    format!(
        "{} <{}>",
        String::from_utf8_lossy(sig.name()),
        String::from_utf8_lossy(sig.email())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_name_and_email() {
        assert_eq!(split_ident("A B <a@b.c>"), Some(("A B", "a@b.c")));
        assert_eq!(split_ident("nobody"), None);
    }

    #[test]
    fn parses_raw_and_iso_dates() {
        let raw = parse_date("1700000000 +0100").unwrap();
        assert_eq!((raw.seconds, raw.offset), (1_700_000_000, 3600));
        let iso = parse_date("2023-11-14T22:13:20Z").unwrap();
        assert_eq!(iso.seconds, 1_700_000_000);
        assert!(parse_date("not a date").is_err());
    }
}
