//! `Bridge.Token` and `Bridge.StoreToken` (SPEC §24.6), the editor's only credential calls.
//!
//! `Token` answers `ASC_TOKEN` when the bridge was started with one (a host-supplied value is
//! the process environment, so it wins over anything inherited), else `git credential fill`
//! for the remote host under `Prompt::Never`: `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`,
//! `GIT_ASKPASS` neutralised, so no Git Credential Manager dialog ever opens. No credential is
//! `NOT_FOUND`. `StoreToken` first checks that a credential helper other than `store` is set;
//! with none it fails `NO_CREDENTIAL_HELPER` and writes nothing. The token is never logged,
//! never in argv, and the reply goes to the host only (never relayed to a webview).

use asc_core::transport_policy::Policy;
use asc_git::credential::{
    self as helper, Credential, CredentialRequest, HelperConfig, Prompt, RawEntry, Session,
};
use asc_git::realgit::RealGit;
use tonic::Status;
use url::Url;

use crate::remote::server::transport_refusal;

/// The user name a token is stored under, as `asc auth login` does.
const TOKEN_USER: &str = "asc";
/// Longest accepted token.
const MAX_TOKEN: usize = 16 * 1024;
/// Longest accepted host.
const MAX_HOST: usize = 512;

/// The remote origin for `host`: a bare `host[:port]` or an origin URL, https unless the
/// loopback opt-in allows http. No user information, path, query or fragment.
fn origin_of(host: &str) -> Result<Url, Status> {
    let bad = |why: &str| Status::invalid_argument(format!("host {why}"));
    let host = host.trim();
    if host.is_empty() || host.len() > MAX_HOST || host.contains(|c: char| c.is_control()) {
        return Err(bad("is empty or malformed"));
    }
    let text = if host.contains("://") {
        host.to_owned()
    } else {
        format!("https://{host}")
    };
    let url = Url::parse(&text).map_err(|_| bad("is not a valid host"))?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(bad("must be a bare remote host or origin"));
    }
    if !matches!(url.scheme(), "https" | "http") {
        return Err(bad("must be https"));
    }
    if let Some(why) = transport_refusal(&url, Policy::current()) {
        return Err(Status::failed_precondition(why));
    }
    Ok(url)
}

fn request_of(url: &Url, user: Option<&str>) -> CredentialRequest {
    let mut request = CredentialRequest::new(url.scheme(), url.host_str().unwrap_or_default());
    request.port = url.port();
    request.username = user.map(str::to_owned);
    request
}

/// A token from the environment, when the host supplied one.
fn env_token(value: Option<std::ffi::OsString>) -> Result<Option<String>, Status> {
    let Some(raw) = value else { return Ok(None) };
    let text = raw.to_string_lossy();
    let token = text.trim();
    if token.is_empty() {
        return Ok(None);
    }
    if token.len() > MAX_TOKEN || token.contains(['\0', '\r', '\n']) {
        return Err(Status::failed_precondition("ASC_TOKEN is not a valid token"));
    }
    Ok(Some(token.to_owned()))
}

/// The token for `host`, or `NOT_FOUND`.
pub async fn token(
    host: &str,
    real: Option<RealGit>,
    helpers: HelperConfig,
) -> Result<String, Status> {
    let url = origin_of(host)?;
    if let Some(t) = env_token(std::env::var_os("ASC_TOKEN"))? {
        return Ok(t);
    }
    let not_found = || Status::not_found("NOT_FOUND: no credential is stored for the remote host");
    let Some(real) = real else {
        return Err(not_found());
    };
    let request = request_of(&url, None);
    let session = Session {
        prompt: Prompt::Never,
        config: Some(helpers),
    };
    let found = tokio::task::spawn_blocking(move || helper::fill(&real, &request, &session))
        .await
        .map_err(|_| Status::internal("the credential lookup stopped"))?
        .map_err(|e| Status::unavailable(format!("the credential helper failed: {e}")))?;
    let token = found.and_then(|c| {
        Some(c.password)
            .filter(|p| !p.is_empty())
            .or(c.credential.filter(|p| !p.is_empty()))
    });
    match token {
        Some(t) if t.len() <= MAX_TOKEN && !t.contains(['\0', '\r', '\n']) => Ok(t),
        _ => Err(not_found()),
    }
}

/// Whether the trusted config names a credential helper other than `store`. `credential.helper`
/// entries that apply to every host or to this one count; an empty value resets the list.
fn has_helper(helpers_cfg: &[RawEntry], url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    let mut list: Vec<String> = Vec::new();
    for e in helpers_cfg {
        if !e.trusted || e.name != "helper" {
            continue;
        }
        let applies = match &e.sub {
            None => true,
            Some(sub) => Url::parse(sub).is_ok_and(|u| u.host_str() == Some(host)),
        };
        if !applies {
            continue;
        }
        match e.value.as_deref().map(str::trim) {
            None | Some("") => list.clear(),
            Some(v) => list.push(v.to_owned()),
        }
    }
    list.iter().any(|h| !is_store(h))
}

/// `store`, `git-credential-store` or `store --file ...`: the plaintext helper.
fn is_store(helper: &str) -> bool {
    let first = helper.split_whitespace().next().unwrap_or("");
    let name = first.rsplit(['/', '\\']).next().unwrap_or(first);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    matches!(name, "store" | "git-credential-store")
}

/// Hands `token` to the credential helpers; writes nothing when none is set.
pub async fn store_token(
    host: &str,
    token: &str,
    real: Option<RealGit>,
    helpers: HelperConfig,
    entries: &[RawEntry],
) -> Result<(), Status> {
    let url = origin_of(host)?;
    if token.is_empty() || token.len() > MAX_TOKEN || token.contains(['\0', '\r', '\n']) {
        return Err(Status::invalid_argument("token is empty or malformed"));
    }
    if !has_helper(entries, &url) {
        return Err(Status::failed_precondition(
            "NO_CREDENTIAL_HELPER: no credential helper is configured (set credential.helper)",
        ));
    }
    let Some(real) = real else {
        return Err(Status::failed_precondition("git was not found"));
    };
    let request = request_of(&url, Some(TOKEN_USER));
    let credential = Credential::new(TOKEN_USER, token);
    let session = Session {
        prompt: Prompt::Never,
        config: Some(helpers),
    };
    tokio::task::spawn_blocking(move || helper::approve(&real, &request, &credential, &session))
        .await
        .map_err(|_| Status::internal("the credential store stopped"))?
        .map_err(|e| Status::unavailable(format!("the credential helper failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_forge_origins_only() {
        assert!(origin_of("forge.example.test").is_ok());
        assert!(origin_of("https://forge.example.test:3000").is_ok());
        for bad in [
            "",
            "ftp://x.test",
            "ssh://x.test",
            "https://u:p@x.test",
            "https://x.test/a/b",
            "https://x.test?q=1",
            "x.test\nhost=evil",
        ] {
            assert!(origin_of(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_store_helper_is_recognised() {
        for s in [
            "store",
            "store --file x",
            "git-credential-store",
            "/usr/bin/git-credential-store",
        ] {
            assert!(is_store(s), "{s}");
        }
        for s in ["manager", "osxkeychain", "!sh x", "cache", "libsecret"] {
            assert!(!is_store(s), "{s}");
        }
    }

    #[test]
    fn an_empty_or_malformed_env_token() {
        assert_eq!(env_token(None).unwrap(), None);
        assert_eq!(env_token(Some("  ".into())).unwrap(), None);
        assert_eq!(env_token(Some(" t ".into())).unwrap().as_deref(), Some("t"));
        assert!(env_token(Some("a\nb".into())).is_err());
    }
}
