//! `asc auth status [<url>]` (SPEC §15.6.1): names the source that would supply the credential
//! for a URL, and never the secret. It is read-only: it resolves under `Prompt::Never` and
//! tells no helper `approve` or `reject`.
//!
//! `asc auth login --host <url> [--device | --token-stdin | --prompt]` stores a token with the git
//! credential helpers (`git credential approve`), and `asc auth logout --host <url>` forgets it
//! (`git credential reject`), as with git. The token is read from stdin, never argv. `--prompt`
//! lets `git credential fill` ask on the terminal (hidden by git), only when stdin is a TTY.
//! `--device` (the default when neither of the others is given) signs in through the browser: the hub
//! gives a code and a link, the person approves it there, and the CLI stores the personal access token
//! the hub hands back (see `auth_device`). Only this CLI layer ever prompts; the libraries resolve under
//! `Prompt::Never`.

use std::ffi::OsString;
use std::process::ExitCode;

use asc_core::transport_policy::Policy;
use asc_git::credential::{
    self as helper, Credential, CredentialRequest, HelperConfig, Prompt as HelperPrompt,
};
use asc_state::{AscHome, StateDb};
use url::Url;

use super::common::{CliError, Ctx, Result};
use crate::credential::{self, Prompt, Resolver};
use crate::native::NativeSpec;
use crate::remote::server::{self, Resolution, cfg_get};
use crate::remote::url::{self as remote_url, RefsMode, RemoteSpec};

const USAGE: &str = "usage: asc auth status [--json] [<url>]
       asc auth login --host <url> [--device | --token-stdin | --prompt]
       asc auth logout --host <url>";

/// The user name the token is stored under; the server reads the password as the token.
const TOKEN_USER: &str = "asc";
/// Longest token read from stdin.
const MAX_TOKEN: u64 = 16 * 1024;

#[derive(clap::Parser, Debug)]
#[command(name = "auth", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "auth",
    flags: &["--json", "--host", "--token-stdin", "--prompt", "--device"],
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

/// The credential request for `host`: an https (or opted-in loopback http) origin, no userinfo.
fn token_request(host: &str) -> Result<CredentialRequest> {
    let url =
        Url::parse(host).map_err(|_| CliError::Fatal("asc: --host is not a valid URL".into()))?;
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(CliError::Fatal(
            "asc: --host must be an origin URL without user information".into(),
        ));
    }
    if let Some(why) = server::transport_refusal(&url, Policy::current()) {
        return Err(CliError::Fatal(why));
    }
    if !matches!(url.scheme(), "https" | "http") {
        return Err(CliError::Fatal("asc: --host must be an https URL".into()));
    }
    let mut request = CredentialRequest::new(url.scheme(), url.host_str().unwrap_or_default());
    request.port = url.port();
    request.username = Some(TOKEN_USER.to_owned());
    Ok(request)
}

/// Signs in through the hub's device flow and returns the token it hands back.
async fn device_token(host: &str) -> Result<String> {
    let hub =
        Url::parse(host).map_err(|_| CliError::Fatal("asc: --host is not a valid URL".into()))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
    super::auth_device::login(
        &client,
        &hub,
        "asc cli",
        |g| {
            eprintln!(
                "To sign in, open:
  {}
and check that the code is {}",
                g.verification_uri_complete, g.user_code
            );
            eprintln!(
                "Waiting for you to approve it (the code lasts {} minutes)...",
                g.expires_in / 60
            );
        },
        |d| async move { tokio::time::sleep(d).await },
    )
    .await
    .map_err(|e| CliError::Fatal(format!("asc: {e}")))
}

fn read_token() -> Result<String> {
    use std::io::Read;
    let mut raw = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_TOKEN + 1)
        .read_to_end(&mut raw)
        .map_err(|e| CliError::Fatal(format!("asc: cannot read the token from stdin: {e}")))?;
    if raw.len() as u64 > MAX_TOKEN {
        return Err(CliError::Fatal("asc: the token is too long".into()));
    }
    let text = String::from_utf8(raw)
        .map_err(|_| CliError::Fatal("asc: the token is not valid UTF-8".into()))?;
    let token = text.trim_end_matches(['\r', '\n']);
    if token.is_empty() || token.contains(['\0', '\r', '\n']) || token != token.trim() {
        return Err(CliError::Fatal(
            "asc: the token on stdin must be one non-empty line".into(),
        ));
    }
    Ok(token.to_owned())
}

/// `login` and `logout`: `args` are the words after the verb.
async fn sign(ctx: &Ctx, login: bool, args: &[String]) -> Result<ExitCode> {
    use std::io::IsTerminal;
    let (mut host, mut stdin_token, mut prompt, mut device) = (None::<String>, false, false, false);
    let mut it = args.iter();
    while let Some(w) = it.next() {
        match w.as_str() {
            "--host" => {
                if host.replace(it.next().ok_or_else(usage)?.clone()).is_some() {
                    return Err(usage());
                }
            }
            "--token-stdin" if login => stdin_token = true,
            "--prompt" if login => prompt = true,
            "--device" if login => device = true,
            w if w.starts_with("--host=") => {
                if host.replace(w["--host=".len()..].to_owned()).is_some() {
                    return Err(usage());
                }
            }
            _ => return Err(usage()),
        }
    }
    let host = host.ok_or_else(usage)?;
    if login && usize::from(stdin_token) + usize::from(prompt) + usize::from(device) > 1 {
        return Err(usage());
    }
    // No method named: sign in through the browser.
    let device = login && !stdin_token && !prompt;
    let request = token_request(&host)?;
    let real = ctx
        .real_git()
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?
        .clone();
    let helpers = HelperConfig::from_file(ctx.config.file());
    let session = |prompt| helper::Session {
        prompt,
        config: Some(helpers.clone()),
    };
    let origin = format!("{}://{}", request.protocol, request.host_attribute());
    if !login {
        let cred = Credential::new(TOKEN_USER, "");
        helper::reject(&real, &request, &cred, &session(HelperPrompt::Never))
            .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
        println!("signed out of {origin}");
        return Ok(ExitCode::SUCCESS);
    }
    let cred = if device {
        Credential::new(TOKEN_USER, device_token(&host).await?)
    } else if stdin_token {
        Credential::new(TOKEN_USER, read_token()?)
    } else {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::Fatal(
                "asc: --prompt needs a terminal; use --token-stdin".into(),
            ));
        }
        // git itself asks, hidden, through its terminal prompt or askpass.
        helper::fill(&real, &request, &session(HelperPrompt::Allowed))
            .map_err(|e| CliError::Fatal(format!("asc: {e}")))?
            .filter(|c| !c.password.is_empty() || c.credential.is_some())
            .ok_or_else(|| CliError::Fatal("asc: no token was entered".into()))?
    };
    helper::approve(&real, &request, &cred, &session(HelperPrompt::Never))
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
    println!("signed in to {origin}");
    Ok(ExitCode::SUCCESS)
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
        Some(verb @ ("login" | "logout")) => {
            let rest: Vec<String> = it.cloned().collect();
            return sign(ctx, verb == "login", &rest).await;
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
