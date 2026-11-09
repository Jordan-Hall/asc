//! `asc doctor --report [<hub>]`: sends the outcome of the checks to the hub, which shows it in the
//! health check of its Get started page.
//!
//! The report holds the name and level of each check, the version of asc and of git, and the
//! operating system and architecture. It holds no path, no detail text and no secret: names are
//! cut to the characters the hub takes, and the git version is the text before ` at <path>`.
//! The hub is reached over https only (loopback http only by the transport opt-in), with the
//! credential `asc auth login` stored for it, no redirect and a 15 s limit. Errors never print
//! the credential.

use std::time::Duration;

use asc_core::transport_policy::Policy;
use asc_state::AscHome;
use asc_state::state::repos::RemoteKind;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use url::Url;

use super::{Check, Level};
use crate::cli::common::Ctx;
use crate::credential::{self, Prompt, Resolved, Resolver, Source};
use crate::remote::{demand, server};

/// Where the hub takes the report, under its address.
pub(super) const PATH: &str = "api/v1/user/asc/doctor";
/// The most checks the hub takes.
const MAX_CHECKS: usize = 64;
const MAX_NAME: usize = 40;
const MAX_TEXT: usize = 80;
const TIMEOUT: Duration = Duration::from_secs(15);

/// `name` as the hub takes it: lower case `a-z 0-9 . _ -`, starting with a letter or digit, at
/// most 40 characters; any other character becomes `-`.
fn report_name(name: &str) -> Option<String> {
    let mapped: String = name
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let out: String = mapped
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take(MAX_NAME)
        .collect();
    (!out.is_empty()).then_some(out)
}

/// `text` with only the characters the hub takes in a version (`A-Z a-z 0-9 space . _ + ( ) -`),
/// at most 80.
fn report_text(text: &str) -> String {
    text.chars()
        .filter(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '+' | '(' | ')' | '-')
        })
        .take(MAX_TEXT)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The version of git from the detail of the `real-git` check, without the path after ` at `.
fn git_version(detail: &str) -> Option<String> {
    let head = detail.split(" at ").next()?.trim();
    head.starts_with("git version ")
        .then(|| report_text(head))
        .filter(|v| !v.is_empty())
}

/// The JSON body of the report: each name once, at its worst level, in the order of the checks.
pub(super) fn body(checks: &[Check]) -> serde_json::Value {
    let mut seen: Vec<(String, Level)> = Vec::new();
    for c in checks {
        let Some(name) = report_name(&c.name) else {
            continue;
        };
        if let Some(slot) = seen.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = slot.1.max(c.level);
        } else if seen.len() < MAX_CHECKS {
            seen.push((name, c.level));
        }
    }
    let items: Vec<_> = seen
        .iter()
        .map(|(n, l)| serde_json::json!({ "name": n, "level": l.label() }))
        .collect();
    let mut v = serde_json::json!({
        "asc": report_text(env!("CARGO_PKG_VERSION")),
        "os": report_text(std::env::consts::OS),
        "arch": report_text(std::env::consts::ARCH),
        "checks": items,
    });
    if let Some(g) = checks
        .iter()
        .find(|c| c.name == "real-git")
        .and_then(|c| git_version(&c.detail))
    {
        v["git"] = g.into();
    }
    v
}

/// The address of the report under the hub `hub`: https (or the loopback http opt-in), a host,
/// no user information.
pub(super) fn endpoint(hub: &str) -> Result<Url, String> {
    let mut url = Url::parse(hub.strip_prefix("asc+").unwrap_or(hub))
        .map_err(|_| "the hub address is not a valid URL".to_owned())?;
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err("the hub address must name a host and no user information".into());
    }
    if let Some(why) = server::transport_refusal(&url, Policy::current()) {
        return Err(why.trim_start_matches("asc: ").to_owned());
    }
    if !matches!(url.scheme(), "https" | "http") {
        return Err("the hub address must be an https URL".into());
    }
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    url.join(PATH)
        .map_err(|_| "the hub address is not a valid URL".to_owned())
}

/// The hub of the workspace remote when `--report` names none: the origin of its https URL.
fn default_hub(ctx: &Ctx) -> Result<String, String> {
    const GIVE: &str = "name the hub: asc doctor --report https://<hub>";
    let ws = ctx.ws_opt().ok_or(GIVE)?;
    if ws.repo.repo.remote_kind == RemoteKind::Bucket {
        return Err(GIVE.into());
    }
    let stored = ws.repo.repo.remote_url.as_deref().ok_or(GIVE)?;
    let url = Url::parse(stored.strip_prefix("asc+").unwrap_or(stored)).map_err(|_| GIVE)?;
    if url.scheme() != "https" {
        return Err(GIVE.into());
    }
    Ok(demand::origin_text(&url))
}

/// The credential for `url`: what git and the environment give, else the token asc keeps.
fn credential_for(ctx: &Ctx, home: &AscHome, url: &Url) -> Result<Resolved, String> {
    let mut r = Resolver::new(ctx.config.file());
    if let Ok(real) = ctx.real_git() {
        r = r.with_real_git(real.clone());
    }
    let r = r.allow_asc_token(url);
    let resolved = r.resolve(url, Prompt::Never).map_err(|e| e.to_string())?;
    if !resolved.is_anonymous() {
        return Ok(resolved);
    }
    let kept = (!crate::keystore::helper_configured(ctx.config.file()))
        .then(|| {
            let store = crate::keystore::KeyStore::from_env(home.root());
            let account = crate::keystore::account_of(url)?;
            store.enabled().then(|| store.get(&account)).flatten()
        })
        .flatten();
    match kept {
        Some((token, _)) => r
            .with_token(url, token, Source::Store, false)
            .map_err(|e| e.to_string()),
        None => Err(format!(
            "no credential for {}: sign in with asc auth login --host {}",
            demand::origin_text(url),
            demand::origin_text(url)
        )),
    }
}

/// Posts `body` to `url` with `resolved`, and names what the hub answered.
pub(super) async fn send(
    client: &reqwest::Client,
    url: &Url,
    resolved: &Resolved,
    body: &serde_json::Value,
) -> Result<(), String> {
    let req = client
        .post(url.clone())
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .body(body.to_string());
    let resp = resolved
        .apply(req, url)
        .send()
        .await
        .map_err(|e| format!("the hub could not be reached: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    match status {
        200 | 204 => {
            credential::report_result(resolved, true);
            Ok(())
        }
        401 => {
            credential::report_result(resolved, false);
            Err(format!(
                "the hub refused the credential: sign in again with asc auth login --host {}",
                demand::origin_text(url)
            ))
        }
        403 => Err("the credential may not send a report: it needs the repository scope".into()),
        404 => Err("the hub takes no doctor report: asc is off there, or the hub is older".into()),
        422 => Err("the hub refused the report as not valid".into()),
        s => Err(format!("the hub answered {s}")),
    }
}

/// `--report`: sends the checks to `hub` (or the hub of the workspace remote when empty) and
/// returns the hub's origin.
pub(super) async fn run(
    ctx: &Ctx,
    home: &AscHome,
    hub: &str,
    checks: &[Check],
) -> Result<String, String> {
    let hub = if hub.is_empty() {
        default_hub(ctx)?
    } else {
        hub.to_owned()
    };
    let url = endpoint(&hub)?;
    let resolved = credential_for(ctx, home, &url)?;
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    send(&client, &url, &resolved, &body(checks)).await?;
    Ok(demand::origin_text(&url))
}

#[cfg(test)]
mod tests;
