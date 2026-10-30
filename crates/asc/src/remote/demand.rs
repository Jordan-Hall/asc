//! Sign-in on demand: authentication belongs to a repository and is asked for when the server
//! says it is needed, as git does. A command starts anonymously; when the server answers 401 the
//! stored credentials (the credential helpers) are tried, and only when none works and a person
//! is at a terminal does this module ask: the device sign-in against the hub (a code and a link),
//! or, with `ASC_NO_BROWSER` set or a hub without the device endpoints, a pasted token. The caller
//! then retries the operation and tells the helpers to keep the token (`approve`) once the server
//! accepted it.
//!
//! Nothing here prompts without a terminal (or [`credential::AUTH_INTERACTIVE`]), and nothing
//! prints a token. Everything the hub answers is untrusted: `auth_device` bounds and cleans it.

use std::io::{BufRead, IsTerminal, Read};
use std::time::Duration;

use asc_core::transport_policy::Policy;
use asc_git::credential::{self as helper, CredentialRequest, HelperConfig, Prompt};
use asc_git::realgit::RealGit;
use url::Url;

use super::server::transport_refusal;
use crate::cli::auth_device::{self, DeviceError, Grant, clean};
use crate::credential::{TOKEN_USER, mask_url};

/// Set (and not `0`): never open the device sign-in, ask for a pasted token.
pub const NO_BROWSER: &str = "ASC_NO_BROWSER";
/// Longest token accepted.
const MAX_TOKEN: usize = 4096;

/// Whether the environment asks for the token prompt instead of the device sign-in.
pub fn no_browser(env: impl Fn(&str) -> Option<String>) -> bool {
    env(NO_BROWSER).is_some_and(|v| {
        let v = v.trim();
        !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
    })
}

/// A token as the hub or a person gave it: one visible, whitespace-free word.
fn usable(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN
        && !token.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// `scheme://host[:port]` of `url`, for messages (no user information, no path).
pub fn origin_text(url: &Url) -> String {
    let mut o = url.clone();
    let _ = o.set_username("");
    let _ = o.set_password(None);
    o.set_path("");
    o.set_query(None);
    o.set_fragment(None);
    o.as_str().trim_end_matches('/').to_owned()
}

/// What the person sees while the device sign-in waits.
fn show(g: &Grant) {
    let link = Url::parse(&g.verification_uri_complete).map_or_else(
        |_| clean(&g.verification_uri_complete),
        |u| clean(u.as_str()),
    );
    eprintln!(
        "To sign in, open:\n  {link}\nand check that the code is {}",
        g.user_code
    );
    eprintln!(
        "Waiting for you to approve it (the code lasts {} minutes)...",
        g.expires_in / 60
    );
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("cannot start the sign-in: {}", clean(&e.to_string())))
}

/// Gets a token for `server` from a person: the device sign-in against `hub`, else a pasted one.
/// `repo` names what needs it. The caller has checked that a person is there.
pub async fn acquire(
    hub: &Url,
    server: &Url,
    repo: &str,
    real: Option<&RealGit>,
    helpers: &HelperConfig,
) -> Result<String, String> {
    if let Some(why) = transport_refusal(hub, Policy::current()) {
        return Err(why);
    }
    eprintln!(
        "asc: sign-in required for '{}' on {}",
        clean(repo),
        origin_text(server)
    );
    if !no_browser(|k| std::env::var(k).ok()) {
        let client = http()?;
        match auth_device::start(&client, hub, "asc cli").await {
            Ok(grant) => {
                show(&grant);
                let token = auth_device::wait(&client, hub, &grant, |d| async move {
                    tokio::time::sleep(d).await;
                })
                .await
                .map_err(|e| e.to_string())?;
                return usable(&token)
                    .then_some(token)
                    .ok_or_else(|| "the hub's answer holds no usable token".to_owned());
            }
            // A hub that cannot do the device sign-in: fall back to a pasted token.
            Err(DeviceError::Hub(m)) => {
                eprintln!("asc: the device sign-in is not available ({m}); paste a token instead");
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    paste_token(hub, server, real, helpers)
}

/// A token typed or pasted by the person: through `git credential fill` on a terminal (git hides
/// the input), else one line of standard input when the session was forced interactive.
fn paste_token(
    hub: &Url,
    server: &Url,
    real: Option<&RealGit>,
    helpers: &HelperConfig,
) -> Result<String, String> {
    eprintln!(
        "Create a personal access token at {} and paste it here.",
        origin_text(hub)
    );
    let token = if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        let real = real.ok_or("git is needed to ask for a token and was not found")?;
        let mut request =
            CredentialRequest::new(server.scheme(), server.host_str().unwrap_or_default());
        request.port = server.port();
        request.username = Some(TOKEN_USER.to_owned());
        let session = helper::Session {
            prompt: Prompt::Allowed,
            config: Some(helpers.clone()),
        };
        let c = helper::fill(real, &request, &session)
            .map_err(|e| clean(&e.to_string()))?
            .ok_or("no token was entered")?;
        c.credential.filter(|t| !t.is_empty()).unwrap_or(c.password)
    } else {
        eprint!("Token for {}: ", origin_text(server));
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .take(MAX_TOKEN as u64 + 2)
            .read_line(&mut line)
            .map_err(|e| format!("cannot read the token: {}", clean(&e.to_string())))?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    };
    if usable(&token) {
        Ok(token)
    } else {
        Err("no usable token was entered".to_owned())
    }
}

/// The message of a command that needs a credential and may not ask for one: git's wording
/// (`could not read Username for '<url>': terminal prompts disabled`) and what to do about it.
pub fn cannot_ask(url: &Url) -> String {
    format!(
        "could not read Username for '{}': terminal prompts disabled. Authentication is required: set ASC_TOKEN to a personal access token, or store one with a git credential helper (`asc auth login --host {}`, or `git credential approve`), or run the command again in a terminal",
        mask_url(url),
        origin_text(url)
    )
}

/// The message when the server refused the credential `source` supplied: git's wording and
/// how to sign in again.
pub fn refused(url: &Url, source: &str, hub: &Url) -> String {
    format!(
        "Authentication failed for '{}': the credential from {source} was refused; sign in again with `asc auth login --host {}`",
        mask_url(url),
        origin_text(hub)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_browser_reads_the_environment() {
        let env = |v: &'static str| move |k: &str| (k == NO_BROWSER).then(|| v.to_owned());
        assert!(no_browser(env("1")));
        assert!(no_browser(env("yes")));
        assert!(!no_browser(env("0")));
        assert!(!no_browser(env("")));
        assert!(!no_browser(|_| None));
    }

    #[test]
    fn only_a_terminal_or_the_explicit_switch_may_be_asked() {
        use crate::credential::{AUTH_INTERACTIVE, interactive_with};
        let none = |_: &str| None;
        assert!(interactive_with(none, true, true));
        assert!(!interactive_with(none, true, false), "stderr piped");
        assert!(!interactive_with(none, false, true), "stdin piped");
        let forced = |k: &str| (k == AUTH_INTERACTIVE).then(|| "1".to_owned());
        assert!(interactive_with(forced, false, false));
        let off = |k: &str| match k {
            "GIT_TERMINAL_PROMPT" => Some("0".to_owned()),
            AUTH_INTERACTIVE => Some("1".to_owned()),
            _ => None,
        };
        assert!(
            !interactive_with(off, true, true),
            "GIT_TERMINAL_PROMPT=0 wins"
        );
    }

    #[test]
    fn tokens_are_one_visible_word() {
        assert!(usable("tok_abc"));
        assert!(!usable(""));
        assert!(!usable("a b"));
        assert!(!usable("a\nb"));
        assert!(!usable(&"x".repeat(MAX_TOKEN + 1)));
    }

    #[test]
    fn origins_carry_no_user_or_path() {
        let u = Url::parse("https://u:p@hub.example:8443/o/r?x=1").unwrap();
        assert_eq!(origin_text(&u), "https://hub.example:8443");
        let m = cannot_ask(&u);
        assert!(!m.contains('p') || !m.contains(":p@"));
        assert!(m.contains("ASC_TOKEN"));
        assert!(m.contains("credential helper"));
    }
}
