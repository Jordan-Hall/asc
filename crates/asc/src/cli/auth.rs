//! `asc auth status [<url>]` (SPEC §15.6.1): names the source that would supply the credential
//! for a URL, and never the secret. It is read-only: it resolves under `Prompt::Never` and
//! tells no helper `approve` or `reject`. There is no `login` or `logout`: a user signs in with
//! `git credential approve` or the helper's own UI, as with git.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_state::{AscHome, StateDb};
use url::Url;

use super::common::{CliError, Ctx, Result};
use crate::credential::{self, Prompt, Resolver};
use crate::native::NativeSpec;
use crate::remote::server::{self, Resolution, cfg_get};
use crate::remote::url::{self as remote_url, RefsMode, RemoteSpec};

const USAGE: &str = "usage: asc auth status [--json] [<url>]";

#[derive(clap::Parser, Debug)]
#[command(name = "auth", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "auth",
    flags: &["--json"],
    placeholders: &[],
}];

fn usage() -> CliError {
    CliError::Usage(USAGE.to_owned())
}

/// How a URL authenticates, for printing.
struct Status {
    /// The URL with any userinfo masked.
    url: String,
    /// `asc server`, `git remote` or `other`.
    mode: &'static str,
    /// The origin the credential is for (asc servers and http(s) URLs).
    origin: Option<String>,
    /// The source's name, `none` when nothing would supply a credential, or `real git`.
    source: String,
    scheme: Option<String>,
    user: Option<String>,
}

fn scheme_name(s: &credential::Scheme) -> String {
    match s {
        credential::Scheme::Basic => "basic".to_owned(),
        credential::Scheme::Bearer => "bearer".to_owned(),
        credential::Scheme::Other(w) => w
            .chars()
            .filter(|c| c.is_ascii_graphic())
            .take(32)
            .collect::<String>()
            .to_ascii_lowercase(),
    }
}

fn resolve_for(ctx: &Ctx, url: &Url) -> Result<Status> {
    let real = ctx.real_git().ok().cloned();
    let mut resolver = Resolver::new(ctx.config.file()).allow_asc_token(url);
    if let Some(real) = real {
        resolver = resolver.with_real_git(real);
    }
    let resolved = resolver
        .resolve(url, Prompt::Never)
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
    Ok(Status {
        url: credential::mask_url(url),
        mode: "asc server",
        origin: Some(format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or(""),
            url.port().map(|p| format!(":{p}")).unwrap_or_default()
        )),
        source: resolved.source().to_owned(),
        scheme: resolved.scheme().map(scheme_name),
        user: resolved.user().filter(|u| !u.is_empty()).map(str::to_owned),
    })
}

async fn db_for(ctx: &Ctx) -> Result<StateDb> {
    Ok(match ctx.ws_opt() {
        Some(ws) => ws.db.clone(),
        None => StateDb::open(&AscHome::locate()?).await?,
    })
}

async fn status_of(ctx: &Ctx, word: &str) -> Result<Status> {
    let (name, stored) = match cfg_get(ctx.config.file(), &format!("remote.{word}.url")) {
        Some(url) => (Some(word.to_owned()), url),
        None => (None, word.to_owned()),
    };
    let spec = remote_url::parse(&stored, RefsMode::Local)
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
    let masked = crate::cli::url_display::anonymize(&stored);
    let git_mode = |mode: &'static str| Status {
        url: masked.clone(),
        mode,
        origin: None,
        source: "real git".to_owned(),
        scheme: None,
        user: None,
    };
    match &spec {
        RemoteSpec::Server { .. } | RemoteSpec::Discover { .. } => {
            let db = db_for(ctx).await?;
            match server::resolve(ctx.config.file(), &db, name.as_deref(), &stored).await? {
                Resolution::Server(target) => {
                    let rpc = Url::parse(&target.rpc_url)
                        .map_err(|_| CliError::Fatal("asc: the server URL is not valid".into()))?;
                    let mut status = resolve_for(ctx, &rpc)?;
                    status.url = masked;
                    Ok(status)
                }
                Resolution::Git => Ok(git_mode("git remote")),
            }
        }
        RemoteSpec::Git { .. } => Ok(git_mode("git remote")),
        RemoteSpec::Bucket { .. } => Ok(git_mode("bucket")),
    }
}

fn render(status: &Status) -> String {
    let mut out = format!("url: {}\nmode: {}\n", status.url, status.mode);
    if let Some(origin) = &status.origin {
        out.push_str(&format!("origin: {origin}\n"));
    }
    out.push_str(&format!("credential source: {}\n", status.source));
    if let Some(s) = &status.scheme {
        out.push_str(&format!("scheme: {s}\n"));
    }
    if let Some(u) = &status.user {
        out.push_str(&format!("user: {u}\n"));
    }
    if status.mode != "asc server" {
        out.push_str(match status.mode {
            "bucket" => "note: S3 credentials come from AWS_* (or asc.s3.credentialHelper)\n",
            _ => "note: real git authenticates this remote; asc never sees the credential\n",
        });
    }
    out
}

fn json(status: &Status) -> serde_json::Value {
    serde_json::json!({
        "url": status.url,
        "mode": status.mode,
        "origin": status.origin,
        "source": status.source,
        "scheme": status.scheme,
        "user": status.user,
        "anonymous": status.source == "none",
    })
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let words: Vec<String> = a
        .rest
        .iter()
        .map(|w| w.to_string_lossy().into_owned())
        .collect();
    let mut it = words.iter();
    match it.next().map(String::as_str) {
        Some("status") => {}
        Some("login" | "logout") => {
            return Err(CliError::Fatal(
                "asc: there is no 'auth login' or 'auth logout'; sign in with 'git credential approve' or your credential helper"
                    .into(),
            ));
        }
        _ => return Err(usage()),
    }
    let mut as_json = false;
    let mut target: Option<String> = None;
    for w in it {
        match w.as_str() {
            "--json" => as_json = true,
            w if w.starts_with('-') => return Err(usage()),
            w => {
                if target.replace(w.to_owned()).is_some() {
                    return Err(usage());
                }
            }
        }
    }
    let word = match target {
        Some(t) => t,
        None => match ctx.ws_opt() {
            Some(_) if cfg_get(ctx.config.file(), "remote.origin.url").is_some() => {
                "origin".to_owned()
            }
            _ => return Err(usage()),
        },
    };
    let status = status_of(ctx, &word).await?;
    if as_json {
        println!("{}", json(&status));
    } else {
        print!("{}", render(&status));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_never_holds_a_secret() {
        let s = Status {
            url: "https://h/o/r".into(),
            mode: "asc server",
            origin: Some("https://h".into()),
            source: "netrc".into(),
            scheme: Some("basic".into()),
            user: Some("alice".into()),
        };
        let text = render(&s);
        assert!(text.contains("credential source: netrc"));
        assert!(text.contains("user: alice"));
        assert_eq!(json(&s)["anonymous"], false);
    }

    #[test]
    fn anonymous_is_reported() {
        let s = Status {
            url: "https://h/o/r".into(),
            mode: "asc server",
            origin: None,
            source: "none".into(),
            scheme: None,
            user: None,
        };
        assert_eq!(json(&s)["anonymous"], true);
        assert!(render(&s).contains("credential source: none"));
    }
}
