//! The client credential resolver (SPEC §15.6.1, §15.6.4): one function for every asc
//! transport, in git's order, with git's helpers.
//!
//! Order, the first source that yields a credential winning:
//! 1. `http.extraHeader` and `http.<url>.extraHeader`;
//! 2. URL userinfo;
//! 3. `ASC_TOKEN`, for the remote's own origin and the `objects` hosts it names, no other;
//! 4. `~/.netrc` (`_netrc` on Windows);
//! 5. git credential helpers, by `git credential fill` through real git;
//! 6. the prompt, which `git credential fill` itself runs under [`Prompt::Allowed`].
//!
//! `credential.*` and `core.askPass` come from the config snapshot's trusted layers only
//! ([`asc_git::credential::HelperConfig`]); the resolver never reads a repository config by
//! itself, so the reduced-trust filter of `cli::config` decides what a foreign repository may
//! set. A credential is bound to one origin: [`Resolved::headers_for`] gives nothing for any
//! other, and [`redirect_action`] never lets a redirect carry one across. Nothing here logs,
//! formats or `Debug`-prints a secret.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use asc_git::credential::netrc::Netrc;
use asc_git::credential::urlmatch::Cascade;
use asc_git::credential::{
    self as helper, Credential, CredentialError, CredentialRequest, HelperConfig, section_entries,
};
use asc_git::realgit::RealGit;
use gix_config::File as Config;
use url::Url;

pub use asc_git::credential::Prompt;

/// Redirects followed before giving up (SPEC §15.6.1, host binding).
pub const MAX_REDIRECTS: usize = 5;

/// A secret value. `Debug` shows nothing; there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the one place that puts it on the wire.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Why a credential could not be produced or may not be sent.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The URL has no host.
    #[error("fatal: no host in URL '{0}'")]
    NoHost(String),
    /// A credential was found for a URL that is neither https nor loopback.
    #[error("fatal: refusing to send credentials over plain http to {0}")]
    Insecure(String),
    /// A header, token or helper answer holds NUL, CR or LF.
    #[error("fatal: {0} contains a control character")]
    Control(&'static str),
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

/// Scheme, host and port: the unit a credential is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl Origin {
    /// The origin of `url`; `None` without a host or a known port.
    pub fn of(url: &Url) -> Option<Self> {
        Some(Self {
            scheme: url.scheme().to_owned(),
            host: url.host_str()?.to_ascii_lowercase(),
            port: url.port_or_known_default()?,
        })
    }

    /// https, or any scheme to a loopback host.
    pub fn is_secure(&self) -> bool {
        self.scheme == "https" || self.is_loopback()
    }

    pub fn is_loopback(&self) -> bool {
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        host == "localhost"
            || host.ends_with(".localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}:{}", self.scheme, self.host, self.port)
    }
}

/// Whether two URLs share scheme, host and port.
pub fn same_origin(a: &Url, b: &Url) -> bool {
    matches!((Origin::of(a), Origin::of(b)), (Some(x), Some(y)) if x == y)
}

/// `url` with any user name and password replaced, for messages, traces and the op log.
pub fn mask_url(url: &Url) -> String {
    if url.username().is_empty() && url.password().is_none() {
        return url.to_string();
    }
    let mut masked = url.clone();
    let _ = masked.set_username("***");
    let _ = masked.set_password(None);
    masked.to_string()
}

/// What to do with a redirect from `from` to `to` after `hops` earlier ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedirectAction {
    /// Same origin: follow it, keeping the credential.
    Follow,
    /// Another origin: send nothing from this credential; ask the resolver afresh for `to`.
    ReAsk,
    /// Not followed at all.
    Refuse(&'static str),
}

/// SPEC §15.6.1, host binding: https to http is refused, at most [`MAX_REDIRECTS`] hops, and
/// a different scheme, host or port drops the credential.
pub fn redirect_action(from: &Url, to: &Url, hops: usize) -> RedirectAction {
    if hops >= MAX_REDIRECTS {
        return RedirectAction::Refuse("too many redirects");
    }
    if from.scheme() == "https" && to.scheme() != "https" {
        return RedirectAction::Refuse("redirect from https to http refused");
    }
    if same_origin(from, to) {
        RedirectAction::Follow
    } else {
        RedirectAction::ReAsk
    }
}

/// A reqwest policy applying [`redirect_action`]: it follows a same-origin redirect itself
/// and hands a cross-origin one back as the 3xx response, so the caller decides, with the
/// resolver, what the new origin gets. reqwest alone would carry every non-sensitive extra
/// header to the new host.
pub fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let hops = attempt.previous().len().saturating_sub(1);
        let Some(from) = attempt.previous().last() else {
            return attempt.stop();
        };
        match redirect_action(from, attempt.url(), hops) {
            RedirectAction::Follow => attempt.follow(),
            RedirectAction::ReAsk => attempt.stop(),
            RedirectAction::Refuse(why) => attempt.error(why),
        }
    })
}

/// RPC calls follow no redirect.
pub fn rpc_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::none()
}

/// Which source supplied a credential, in resolver order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    ExtraHeader,
    UrlUserinfo,
    AscToken,
    Netrc,
    Helper,
}

impl Source {
    /// The name `asc auth status` prints; never a secret.
    pub fn name(self) -> &'static str {
        match self {
            Self::ExtraHeader => "http.extraHeader",
            Self::UrlUserinfo => "url",
            Self::AscToken => "ASC_TOKEN",
            Self::Netrc => "netrc",
            Self::Helper => "credential helper",
        }
    }
}

/// How the secret is presented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// `Authorization: Basic base64(user:secret)`.
    Basic,
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// Any other scheme word, sent as given (`token`, or a helper's `authtype`).
    Other(String),
}

/// The credential a [`Resolved`] holds.
#[derive(Clone, PartialEq, Eq)]
pub struct Presented {
    pub scheme: Scheme,
    /// The user name; empty for Bearer and for a token given as the user name.
    pub user: String,
    pub secret: Secret,
}

impl fmt::Debug for Presented {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Presented")
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

impl Presented {
    /// The `Authorization` header value.
    pub fn authorization(&self) -> Secret {
        let value = match &self.scheme {
            Scheme::Basic => format!(
                "Basic {}",
                base64_encode(format!("{}:{}", self.user, self.secret.expose()).as_bytes())
            ),
            Scheme::Bearer => format!("Bearer {}", self.secret.expose()),
            Scheme::Other(word) => format!("{word} {}", self.secret.expose()),
        };
        Secret::new(value)
    }

    /// Reads an `Authorization` header value back into scheme, user and secret.
    fn from_authorization(value: &str) -> Self {
        let (word, rest) = value.split_once(' ').unwrap_or((value, ""));
        let rest = rest.trim();
        if word.eq_ignore_ascii_case("basic") {
            let decoded = base64_decode(rest).and_then(|b| String::from_utf8(b).ok());
            if let Some((user, pass)) = decoded.as_deref().and_then(|d| d.split_once(':')) {
                return Self {
                    scheme: Scheme::Basic,
                    user: user.to_owned(),
                    secret: Secret::new(pass),
                };
            }
        } else if word.eq_ignore_ascii_case("bearer") {
            return Self {
                scheme: Scheme::Bearer,
                user: String::new(),
                secret: Secret::new(rest),
            };
        }
        Self {
            scheme: Scheme::Other(word.to_owned()),
            user: String::new(),
            secret: Secret::new(rest),
        }
    }
}

/// What `git credential fill` produced, kept to answer `approve` or `reject`.
#[derive(Clone)]
struct FillRecord {
    real: RealGit,
    request: CredentialRequest,
    credential: Credential,
    session: helper::Session,
}

/// The outcome of a resolution: a credential bound to one origin, or nothing.
pub struct Resolved {
    origin: Origin,
    source: Option<Source>,
    presented: Option<Presented>,
    extra_headers: Vec<(String, Secret)>,
    fill: Option<FillRecord>,
    reported: AtomicBool,
    attempt: u32,
}

impl fmt::Debug for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolved")
            .field("origin", &self.origin)
            .field("source", &self.source)
            .field("scheme", &self.presented.as_ref().map(|p| &p.scheme))
            .finish_non_exhaustive()
    }
}

impl Resolved {
    fn new(origin: Origin, extra_headers: Vec<(String, Secret)>) -> Self {
        Self {
            origin,
            source: None,
            presented: None,
            extra_headers,
            fill: None,
            reported: AtomicBool::new(false),
            attempt: 0,
        }
    }

    /// The origin the credential is bound to.
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// No source produced a credential.
    pub fn is_anonymous(&self) -> bool {
        self.presented.is_none()
    }

    /// The source, or `None` when anonymous.
    pub fn source_kind(&self) -> Option<Source> {
        self.source
    }

    /// The source's name, for `asc auth status` and messages: `"none"` when anonymous.
    pub fn source(&self) -> &'static str {
        self.source.map_or("none", Source::name)
    }

    /// The credential, when there is one.
    pub fn presented(&self) -> Option<&Presented> {
        self.presented.as_ref()
    }

    /// The scheme (Basic or Bearer for asc's own server), when there is a credential.
    pub fn scheme(&self) -> Option<&Scheme> {
        self.presented.as_ref().map(|p| &p.scheme)
    }

    /// The user name, when there is a credential.
    pub fn user(&self) -> Option<&str> {
        self.presented.as_ref().map(|p| p.user.as_str())
    }

    /// The secret, when there is a credential.
    pub fn secret(&self) -> Option<&Secret> {
        self.presented.as_ref().map(|p| &p.secret)
    }

    /// Every header to send to `url`: the `http.extraHeader` values verbatim and the
    /// `Authorization` of the credential. Empty for any other origin, and for plain http
    /// except to loopback.
    pub fn headers_for(&self, url: &Url) -> Vec<(String, Secret)> {
        if Origin::of(url).as_ref() != Some(&self.origin) || !self.origin.is_secure() {
            return Vec::new();
        }
        let mut headers = self.extra_headers.clone();
        // A credential from `http.extraHeader` is already among the extra headers, verbatim.
        if let Some(p) = &self.presented
            && self.source != Some(Source::ExtraHeader)
        {
            headers.push(("Authorization".to_owned(), p.authorization()));
        }
        headers
    }

    /// `request` with [`Resolved::headers_for`] applied, marked sensitive.
    pub fn apply(&self, request: reqwest::RequestBuilder, url: &Url) -> reqwest::RequestBuilder {
        use reqwest::header::{HeaderName, HeaderValue};
        let mut request = request;
        for (name, value) in self.headers_for(url) {
            let (Ok(name), Ok(mut value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value.expose()),
            ) else {
                continue;
            };
            value.set_sensitive(true);
            request = request.header(name, value);
        }
        request
    }
}

/// Tells the helpers how the server answered (SPEC §15.6.1): `approve` once after an accepted
/// request, `reject` after a 401, and only for a credential `git credential fill` produced.
/// Credentials from the first four sources are never reported. Best effort: a helper that
/// fails to store or erase does not fail the transfer.
pub fn report_result(resolved: &Resolved, accepted: bool) {
    let Some(fill) = &resolved.fill else {
        return;
    };
    if resolved.reported.swap(true, Ordering::SeqCst) {
        return;
    }
    let result = if accepted {
        helper::approve(&fill.real, &fill.request, &fill.credential, &fill.session)
    } else {
        helper::reject(&fill.real, &fill.request, &fill.credential, &fill.session)
    };
    drop(result);
}

/// git's wording when no credential is available or the one found was refused; exit 128.
pub fn failure_message(url: &Url, refused: bool, prompts_disabled: bool) -> String {
    let url = mask_url(url);
    match (refused, prompts_disabled) {
        (false, true) => {
            format!("fatal: could not read Username for '{url}': terminal prompts disabled")
        }
        _ => format!("fatal: Authentication failed for '{url}'"),
    }
}

/// `Prompt::Allowed` when standard input is a terminal, else `Prompt::Never`. Only a CLI
/// command calls this; libraries pass `Prompt::Never`.
pub fn terminal_prompt() -> Prompt {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        Prompt::Allowed
    } else {
        Prompt::Never
    }
}

type EnvFn<'a> = Box<dyn Fn(&str) -> Option<String> + 'a>;

/// The resolver over one config snapshot. [`resolve`] is the common entry point.
pub struct Resolver<'a> {
    config: &'a Config,
    helpers: HelperConfig,
    env: EnvFn<'a>,
    home: Option<PathBuf>,
    real: Option<RealGit>,
    token_origins: Vec<Origin>,
}

impl<'a> Resolver<'a> {
    /// A resolver over `config`, which must be a `ConfigSnapshot::file()` (or any config
    /// whose reduced-trust sections the snapshot already filtered). It reads the process
    /// environment and the user's home. No origin gets `ASC_TOKEN` until
    /// [`Resolver::allow_asc_token`] names it.
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            helpers: HelperConfig::from_file(config),
            env: Box::new(|k| std::env::var(k).ok()),
            home: None,
            real: None,
            token_origins: Vec::new(),
        }
    }

    /// Replaces the environment lookup (tests).
    pub fn with_env(mut self, env: impl Fn(&str) -> Option<String> + 'a) -> Self {
        self.env = Box::new(env);
        self
    }

    /// The home directory holding `.netrc`; by default `$HOME`, else the OS home.
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// The real git to run helpers through; by default resolved on first use.
    pub fn with_real_git(mut self, real: RealGit) -> Self {
        self.real = Some(real);
        self
    }

    /// Lets `ASC_TOKEN` go to the origin of `url`: the remote itself and the `objects` hosts
    /// its discovery document names. No other origin ever gets it.
    pub fn allow_asc_token(mut self, url: &Url) -> Self {
        self.token_origins.extend(Origin::of(url));
        self
    }

    fn var(&self, name: &str) -> Option<String> {
        (self.env)(name).filter(|v| !v.is_empty())
    }

    fn home(&self) -> Option<PathBuf> {
        self.home
            .clone()
            .or_else(|| self.var("HOME").map(PathBuf::from))
            .or_else(std::env::home_dir)
    }

    fn real_git(&self) -> Option<RealGit> {
        if let Some(real) = &self.real {
            return Some(real.clone());
        }
        let configured = section_entries(self.config, "asc")
            .into_iter()
            .filter(|e| e.trusted && e.sub.is_none() && e.name == "realgit")
            .filter_map(|e| e.value)
            .next_back()
            .map(PathBuf::from);
        let asc_home = self.var("ASC_HOME").map(PathBuf::from);
        RealGit::resolve(configured, asc_home.as_deref()).ok()
    }

    /// The headers `http.extraHeader` and `http.<url>.extraHeader` give `url`, with git's
    /// semantics: plain entries always apply, URL entries by the most specific match, an
    /// empty value resets the list.
    fn extra_headers(&self, url: &Url) -> Result<Vec<(String, Secret)>, ResolveError> {
        let mut cascade = Cascade::new(url.as_str());
        let mut headers: Vec<(String, Secret)> = Vec::new();
        for entry in section_entries(self.config, "http") {
            // A reduced-trust section (a repository config another user owns) supplies none.
            if !entry.trusted || entry.name != "extraheader" || !cascade.offer(entry.sub.as_deref())
            {
                continue;
            }
            match entry.value.as_deref() {
                None => {}
                Some("") => headers.clear(),
                Some(line) => {
                    if line.contains(['\0', '\r', '\n']) {
                        return Err(ResolveError::Control("http.extraHeader"));
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        let name = name.trim();
                        if is_header_name(name) {
                            headers.push((name.to_owned(), Secret::new(value.trim_start())));
                        }
                    }
                }
            }
        }
        Ok(headers)
    }

    /// Resolves a credential for a request to `url`.
    pub fn resolve(&self, url: &Url, prompt: Prompt) -> Result<Resolved, ResolveError> {
        self.run(url, prompt, None, &[])
    }

    /// Resolves for a request the server answered 401 although none of ours was refused
    /// yet (an anonymous first try): `wwwauth` is the challenge, which helpers such as Git
    /// Credential Manager use to pick Bearer or OAuth.
    pub fn resolve_with_challenge(
        &self,
        url: &Url,
        prompt: Prompt,
        wwwauth: &[String],
    ) -> Result<Resolved, ResolveError> {
        self.run(url, prompt, None, wwwauth)
    }

    /// Resolves again after the server refused `rejected` (401 or `UNAUTHENTICATED`):
    /// the helpers are told to `reject` it, then the next source is tried. A helper's
    /// credential gets one re-ask when `prompt` is [`Prompt::Never`] and as many as the
    /// person allows under [`Prompt::Allowed`]. `wwwauth` is the 401's challenge lines.
    pub fn resolve_after(
        &self,
        url: &Url,
        prompt: Prompt,
        rejected: &Resolved,
        wwwauth: &[String],
    ) -> Result<Resolved, ResolveError> {
        report_result(rejected, false);
        self.run(url, prompt, Some(rejected), wwwauth)
    }

    fn run(
        &self,
        url: &Url,
        prompt: Prompt,
        rejected: Option<&Resolved>,
        wwwauth: &[String],
    ) -> Result<Resolved, ResolveError> {
        let origin = Origin::of(url).ok_or_else(|| ResolveError::NoHost(mask_url(url)))?;
        let secure = origin.is_secure();
        // Sources before and including the refused one are spent; a refused helper
        // credential is asked for again, once without a prompt.
        let skip_through = rejected.and_then(|r| r.source);
        let past = |source: Source| skip_through.is_some_and(|s| s >= source);
        let attempt = rejected.map_or(0, |r| {
            r.attempt + u32::from(r.source == Some(Source::Helper))
        });
        let found = |source: Source, presented: Presented, extras: Vec<(String, Secret)>| {
            if !secure {
                return Err(ResolveError::Insecure(origin.to_string()));
            }
            let mut resolved = Resolved::new(origin.clone(), extras);
            resolved.source = Some(source);
            resolved.presented = Some(presented);
            resolved.attempt = attempt;
            Ok(resolved)
        };

        // 1. http.extraHeader
        let mut extras = self.extra_headers(url)?;
        let is_authorization = |h: &(String, Secret)| h.0.eq_ignore_ascii_case("authorization");
        if !past(Source::ExtraHeader) {
            if let Some(h) = extras.iter().rev().find(|h| is_authorization(h)) {
                // A matching `Authorization` header is the credential and goes out verbatim.
                let presented = Presented::from_authorization(h.1.expose());
                return found(Source::ExtraHeader, presented, extras);
            }
        } else {
            extras.retain(|h| !is_authorization(h));
        }

        // 2. URL userinfo
        if !past(Source::UrlUserinfo) && (!url.username().is_empty() || url.password().is_some()) {
            let user = percent_decode(url.username());
            let pass = percent_decode(url.password().unwrap_or(""));
            if user.contains(['\0', '\r', '\n']) || pass.contains(['\0', '\r', '\n']) {
                return Err(ResolveError::Control("URL user information"));
            }
            let presented = Presented {
                scheme: Scheme::Basic,
                user,
                secret: Secret::new(pass),
            };
            return found(Source::UrlUserinfo, presented, extras);
        }

        // 3. ASC_TOKEN, for the remote's own origin and its objects hosts only.
        if !past(Source::AscToken)
            && self.token_origins.contains(&origin)
            && let Some(token) = self.var("ASC_TOKEN")
        {
            if token.contains(['\0', '\r', '\n']) {
                return Err(ResolveError::Control("ASC_TOKEN"));
            }
            let presented = Presented {
                scheme: Scheme::Bearer,
                user: String::new(),
                secret: Secret::new(token),
            };
            return found(Source::AscToken, presented, extras);
        }

        // 4. ~/.netrc
        if !past(Source::Netrc)
            && let Some(home) = self.home()
            && let Some(netrc) = Netrc::read_home(&home)
            && let Some(host) = url.host_str()
            && let Some(login) = netrc.lookup(host, userinfo_name(url).as_deref())
            && !login.login.contains(['\0', '\r', '\n'])
            && !login.password.contains(['\0', '\r', '\n'])
        {
            let presented = Presented {
                scheme: Scheme::Basic,
                user: login.login,
                secret: Secret::new(login.password),
            };
            return found(Source::Netrc, presented, extras);
        }

        // 5 and 6. Credential helpers, then the prompt, both inside `git credential fill`.
        let one_reask_used = prompt == Prompt::Never && attempt >= 2;
        if secure
            && !one_reask_used
            && let Some(real) = self.real_git()
        {
            let request = self.helper_request(url, wwwauth);
            let session = helper::Session {
                prompt,
                config: Some(self.helpers.clone()),
            };
            if let Some(credential) = helper::fill(&real, &request, &session)? {
                let presented = presented_from_helper(&credential);
                let mut resolved = found(Source::Helper, presented, extras)?;
                resolved.fill = Some(FillRecord {
                    real,
                    request,
                    credential,
                    session,
                });
                return Ok(resolved);
            }
        }
        Ok(Resolved::new(origin, extras))
    }

    /// The `git credential` request for `url`: protocol, host, port when not the default,
    /// the path only under `credential.useHttpPath`, the user from the URL or
    /// `credential.username`, and the challenge.
    fn helper_request(&self, url: &Url, wwwauth: &[String]) -> CredentialRequest {
        let settings = self.helpers.settings_for(url.as_str());
        let mut request = CredentialRequest::new(url.scheme(), url.host_str().unwrap_or_default());
        request.port = url.port();
        if settings.use_http_path {
            let path = url.path().trim_start_matches('/');
            if !path.is_empty() {
                request.path = Some(percent_decode(path));
            }
        }
        request.username = userinfo_name(url).or(settings.username);
        request.wwwauth = wwwauth.to_vec();
        request
    }
}

/// Resolves `url` with the process environment: `ASC_TOKEN` counts for `url`'s origin only.
pub fn resolve(config: &Config, url: &Url, prompt: Prompt) -> Result<Resolved, ResolveError> {
    Resolver::new(config)
        .allow_asc_token(url)
        .resolve(url, prompt)
}

fn userinfo_name(url: &Url) -> Option<String> {
    Some(percent_decode(url.username())).filter(|u| !u.is_empty())
}

fn presented_from_helper(credential: &Credential) -> Presented {
    if let (Some(authtype), Some(value)) = (&credential.authtype, &credential.credential) {
        let scheme = if authtype.eq_ignore_ascii_case("bearer") {
            Scheme::Bearer
        } else {
            Scheme::Other(authtype.clone())
        };
        return Presented {
            scheme,
            user: credential.username.clone(),
            secret: Secret::new(value.clone()),
        };
    }
    Presented {
        scheme: Scheme::Basic,
        user: credential.username.clone(),
        secret: Secret::new(credential.password.clone()),
    }
}

fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%')
            .then(|| {
                text.get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            })
            .flatten();
        match hex {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.trim_end_matches('=').bytes() {
        let v = B64.iter().position(|b| *b == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}
