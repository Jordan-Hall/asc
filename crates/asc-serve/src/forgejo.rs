//! The Forgejo client: authorisation and hooks (SPEC §8.3 step 7, §13.5, §15.1, §15.5; PLAN U44).
//!
//! * [`PublicApiAuthz`] (Stage 1) asks Forgejo's public API with the caller's token.
//! * [`InternalAuthz`] (Stage 2+) asks `POST /api/internal/asc/authz` (K07).
//! * [`HookClient`] calls Forgejo's `pre-receive`, `post-receive` and `proc-receive` endpoints.
//! * [`ForgejoHooks`] is pipeline step 7 and [`PostReceiveEffect`] is the outbox `post_receive`.
//!
//! Everything Forgejo returns is untrusted: bodies are size-capped, identities are validated, and
//! any failure to get a clear "yes" is a refusal.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use asc_core::id::RefTarget;
use asc_core::transport_policy::{Policy, classify_url};
use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Decision as SshDecision, Principal,
    Scheme, SshIdentity, SshKey,
};
use asc_rpc::changes::MergeOutcome;
use asc_rpc::server::{CallCtx, MAX_REF_UPDATES, RepoName};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{AnyPool, Row};
use url::Url;
use uuid::Uuid;

use crate::authz::{
    HOST_SERVICE_PRINCIPAL, NEGATIVE_TTL, POSITIVE_TTL, UserIdentity, is_host_service,
};
use crate::changes::request_merge::{self, MergeAsk, MergeQueue, QueueError};
use crate::config::{CompositionGrant, ForgejoConfig, ServeConfig};
use crate::mirror::Mirror;
use crate::outbox::{Effect, EffectError, Job, Outbox};
use crate::registry::{Extensions, Ns, Registry};
use crate::update::{HookRejected, HookRequest, PushHooks};

#[path = "forgejo_ssh.rs"]
mod forgejo_ssh;
#[path = "forgejo_webhook.rs"]
mod forgejo_webhook;
pub use forgejo_ssh::{MAX_SSH_LOOKUPS_IN_FLIGHT, SSH_LOOKUP_TIMEOUT, SshLookupLimits, SshSupport};
pub use forgejo_webhook::{WEBHOOK_PATH, verify_signature};

/// Positive and negative authorisation answers are cached this long (SPEC §15.1).
/// Most cached authorisation answers.
const CACHE_ENTRIES: usize = 16 * 1024;
/// Largest response body read from Forgejo.
const MAX_BODY: usize = 1024 * 1024;
/// Longest hook rejection message relayed to a user.
const MAX_MESSAGE: usize = 4096;
/// Largest accepted internal token file.
const MAX_TOKEN_FILE: u64 = 4096;
/// Whole-request timeout and connect timeout toward Forgejo.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The all-zero git id of a created or deleted ref.
const ZERO_SHA: &str = "0000000000000000000000000000000000000000";
/// `PushTrigger` of every hook call made by asc.
const PUSH_TRIGGER: &str = "asc";
/// Header carrying the operation id so Forgejo-side retries can be recognised.
const OP_ID_HEADER: &str = "x-asc-op-id";
/// `git_map.kind` codes (see `registry::kind_code`).
const KIND_COMMIT: i32 = 3;
const KIND_TAG: i32 = 4;

/// Why a Forgejo call failed.
#[derive(Debug, thiserror::Error)]
pub enum ForgejoError {
    #[error("invalid forgejo configuration: {0}")]
    Config(String),
    #[error("forgejo is unreachable: {0}")]
    Transport(String),
    #[error("forgejo answered {status}: {message}")]
    Status { status: u16, message: String },
    #[error("forgejo sent an invalid answer: {0}")]
    Decode(String),
    #[error("forgejo response is larger than {MAX_BODY} bytes")]
    TooLarge,
    #[error("the internal token is required for this call")]
    NoInternalToken,
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("{0}")]
    Invalid(String),
}

// ------------------------------------------------------------------------------ token

/// Forgejo's `INTERNAL_TOKEN`. `Debug` never prints it; comparison is constant time.
#[derive(Clone)]
pub struct InternalToken(String);

impl InternalToken {
    /// Accepts 1..=4096 visible ASCII characters.
    pub fn new(token: impl Into<String>) -> Result<Self, ForgejoError> {
        let token = token.into();
        let ok = !token.is_empty()
            && token.len() <= MAX_TOKEN_FILE as usize
            && token.bytes().all(|b| b.is_ascii_graphic());
        if ok {
            Ok(Self(token))
        } else {
            Err(ForgejoError::Config("invalid internal token".into()))
        }
    }

    /// Reads the token from `path` (at most 4 KiB, surrounding whitespace removed).
    pub fn from_file(path: &FsPath) -> Result<Self, ForgejoError> {
        let bad = |e: std::io::Error| ForgejoError::Config(format!("{}: {e}", path.display()));
        let file = std::fs::File::open(path).map_err(bad)?;
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::Read::take(file, MAX_TOKEN_FILE + 1), &mut buf)
            .map_err(bad)?;
        Self::new(buf.trim())
    }

    fn matches(&self, presented: &BearerToken) -> bool {
        blake3::hash(self.0.as_bytes()) == blake3::hash(presented.expose().as_bytes())
    }
}

impl fmt::Debug for InternalToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InternalToken(<redacted>)")
    }
}

// ------------------------------------------------------------------------------ http

/// The HTTP connection to one Forgejo instance. It never follows redirects, so a token cannot be
/// forwarded to another host.
pub struct ForgejoClient {
    http: reqwest::Client,
    base: Url,
    internal: Option<InternalToken>,
}

impl ForgejoClient {
    /// `base` is the Forgejo root URL (`http` or `https`, without credentials).
    pub fn new(base: &str, internal: Option<InternalToken>) -> Result<Self, ForgejoError> {
        Self::new_under(base, internal, Policy::current())
    }

    /// [`ForgejoClient::new`] under an explicit transport policy.
    pub fn new_under(
        base: &str,
        internal: Option<InternalToken>,
        policy: Policy,
    ) -> Result<Self, ForgejoError> {
        let base = Url::parse(base).map_err(|e| ForgejoError::Config(format!("url: {e}")))?;
        let ok = matches!(base.scheme(), "http" | "https")
            && base.host_str().is_some()
            && base.username().is_empty()
            && base.password().is_none()
            && !base.cannot_be_a_base();
        if !ok {
            return Err(ForgejoError::Config(
                "forgejo.url must be an https URL without credentials".into(),
            ));
        }
        // https, or plain http to the local machine under the opt-in (U89): the internal token
        // and every forwarded credential go to this host.
        classify_url(&base, policy)
            .map_err(|why| ForgejoError::Config(format!("forgejo.url: {why}")))?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| ForgejoError::Config(e.to_string()))?;
        Ok(Self {
            http,
            base,
            internal,
        })
    }

    /// `base` joined with `segments`, each percent-encoded as one path segment.
    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut s) = url.path_segments_mut() {
            s.pop_if_empty().extend(segments);
        }
        url
    }

    fn internal_request(&self, segments: &[&str]) -> Result<RequestBuilder, ForgejoError> {
        let token = self
            .internal
            .as_ref()
            .ok_or(ForgejoError::NoInternalToken)?;
        Ok(self
            .http
            .request(Method::POST, self.url(segments))
            .bearer_auth(&token.0))
    }

    /// Sends `req` and reads at most [`MAX_BODY`] bytes of the answer.
    async fn send(req: RequestBuilder) -> Result<(StatusCode, Vec<u8>), ForgejoError> {
        let mut resp = req
            .send()
            .await
            .map_err(|e| ForgejoError::Transport(e.without_url().to_string()))?;
        let status = resp.status();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ForgejoError::Transport(e.without_url().to_string()))?
        {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(ForgejoError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }
}

fn decode<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, ForgejoError> {
    serde_json::from_slice(body).map_err(|e| ForgejoError::Decode(e.to_string()))
}

/// A user-safe version of a Forgejo message: printable, bounded.
fn clean_message(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c == '\n' || !c.is_control() { c } else { ' ' })
        .take(MAX_MESSAGE)
        .collect();
    out.truncate(out.trim_end().len());
    out
}

/// `{"err": ..., "user_msg": ...}` as Forgejo's private API answers a failure.
#[derive(Deserialize, Default)]
struct FailBody {
    #[serde(default, alias = "Err")]
    err: String,
    #[serde(default, alias = "UserMsg")]
    user_msg: String,
}

fn failure_message(what: &str, status: StatusCode, body: &[u8]) -> String {
    let parsed: FailBody = serde_json::from_slice(body).unwrap_or_default();
    // Only `user_msg` is meant for the pushing client. `err` is Forgejo's internal error and may
    // hold paths, SQL or stack text, so it goes to the log and never to the client.
    let text = clean_message(&parsed.user_msg);
    if !text.is_empty() {
        return text;
    }
    if !parsed.err.is_empty() {
        tracing::warn!(
            what,
            status = status.as_u16(),
            err = %clean_message(&parsed.err),
            "forgejo failure carried no user message"
        );
    }
    format!("{what} failed with status {}", status.as_u16())
}

// ------------------------------------------------------------------------------ identity

/// What the caller may do on one repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Perm {
    None,
    Read,
    Write,
    Admin,
}

impl Perm {
    fn parse(s: &str) -> Self {
        match s {
            "admin" | "owner" => Self::Admin,
            "write" => Self::Write,
            "read" => Self::Read,
            _ => Self::None,
        }
    }

    fn allows(self, access: Access) -> bool {
        self >= match access {
            Access::Read => Self::Read,
            Access::Write => Self::Write,
            Access::Admin => Self::Admin,
        }
    }
}

/// The Actions task behind a task token.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct TaskInfo {
    #[serde(default)]
    pub run_id: i64,
    #[serde(default)]
    pub event: String,
    #[serde(default, rename = "ref")]
    pub git_ref: String,
    #[serde(default)]
    pub pr_index: i64,
    #[serde(default)]
    pub is_fork_pr: bool,
    /// The repository (`owner/name`) the task token is bound to.
    #[serde(default)]
    pub repo: Option<String>,
}

/// One authorisation answer, with what later pipeline stages need from it.
#[derive(Clone, Debug)]
pub struct Decision {
    pub principal: Principal,
    pub identity: Option<UserIdentity>,
    pub perm: Perm,
    pub is_actions_task: bool,
    pub task: Option<TaskInfo>,
    /// `branch_rules.require_signed`: a land must refuse a signed stack it would rewrite.
    pub require_signed: bool,
}

impl Decision {
    fn internal() -> Self {
        Self {
            principal: Principal {
                id: HOST_SERVICE_PRINCIPAL.into(),
            },
            identity: None,
            perm: Perm::Admin,
            is_actions_task: false,
            task: None,
            require_signed: false,
        }
    }

    fn for_identity(identity: UserIdentity, perm: Perm) -> Self {
        Self {
            principal: Principal {
                id: identity.principal_id(),
            },
            identity: Some(identity),
            perm,
            is_actions_task: false,
            task: None,
            require_signed: false,
        }
    }
}

// ------------------------------------------------------------------------------ cache

/// What the caller presented, as Forgejo must see it. A Basic credential with a user name goes to
/// Forgejo as HTTP Basic `user:secret`, so Forgejo decides as it does for a git client: a personal
/// access token or an account password, and no password when two-factor is on. Every other
/// credential (`Bearer`, `token`, and Basic without a user name) goes as a Bearer token. There is
/// deliberately no `Debug`: the secret must not reach a log.
#[derive(Clone, Copy)]
struct Login<'a> {
    user: Option<&'a str>,
    secret: &'a BearerToken,
}

impl<'a> Login<'a> {
    fn bearer(secret: &'a BearerToken) -> Self {
        Self { user: None, secret }
    }

    fn of(credential: &'a Credential) -> Self {
        let user = match credential.scheme() {
            Scheme::Basic => credential.user().filter(|u| !u.is_empty()),
            Scheme::Bearer | Scheme::Token => None,
        };
        Self {
            user,
            secret: credential.secret(),
        }
    }

    /// Adds the `Authorization` header (marked sensitive by reqwest).
    fn apply(self, req: RequestBuilder) -> RequestBuilder {
        match self.user {
            Some(user) => req.basic_auth(user, Some(self.secret.expose())),
            None => req.bearer_auth(self.secret.expose()),
        }
    }
}

/// Cache key: `(sha256(user NUL secret), repo, mode)` (SPEC §15.1). The user is empty for every
/// non-Basic credential, so only a Basic user name separates two entries: the same password under
/// another user name never shares an answer. Only the digest is kept, never the secret.
type CacheKey = ([u8; 32], String, Access);

fn cache_key(login: Option<Login<'_>>, repo: &str, access: Access) -> CacheKey {
    let mut h = Sha256::new();
    if let Some(l) = login {
        h.update(l.user.unwrap_or("").as_bytes());
        h.update([0u8]);
        h.update(l.secret.expose().as_bytes());
    }
    (h.finalize().into(), repo.to_owned(), access)
}

/// Why a lookup produced no decision.
enum Fetch {
    /// A definite refusal (cached briefly).
    Denied(AuthError),
    /// Forgejo could not answer; the caller is refused and nothing is cached.
    Unavailable,
}

#[derive(Clone)]
struct Cached {
    at: Instant,
    answer: Result<Arc<Decision>, AuthError>,
}

/// Cache sizes and lifetimes.
#[derive(Clone, Copy, Debug)]
pub struct AuthzCacheConfig {
    pub positive: Duration,
    pub negative: Duration,
    pub entries: usize,
}

impl Default for AuthzCacheConfig {
    fn default() -> Self {
        Self {
            positive: POSITIVE_TTL,
            negative: NEGATIVE_TTL,
            entries: CACHE_ENTRIES,
        }
    }
}

/// The cache and internal-token shortcut shared by both authorizers.
struct AuthzCore {
    cache: quick_cache::sync::Cache<CacheKey, Cached>,
    cfg: AuthzCacheConfig,
    internal: Option<InternalToken>,
}

impl AuthzCore {
    fn new(cfg: AuthzCacheConfig, internal: Option<InternalToken>) -> Self {
        Self {
            cache: quick_cache::sync::Cache::new(cfg.entries.max(1)),
            cfg,
            internal,
        }
    }

    async fn resolve<Fut>(
        &self,
        login: Option<Login<'_>>,
        repo: &str,
        access: Access,
        fetch: impl FnOnce() -> Fut,
    ) -> Result<Arc<Decision>, AuthError>
    where
        Fut: Future<Output = Result<Decision, Fetch>>,
    {
        if let (Some(l), Some(i)) = (login, &self.internal)
            && i.matches(l.secret)
        {
            return Ok(Arc::new(Decision::internal()));
        }
        let key = cache_key(login, repo, access);
        if let Some(hit) = self.cache.get(&key) {
            let ttl = if hit.answer.is_ok() {
                self.cfg.positive
            } else {
                self.cfg.negative
            };
            if hit.at.elapsed() < ttl {
                return hit.answer;
            }
        }
        let answer = match fetch().await {
            Ok(d) => Ok(Arc::new(d)),
            Err(Fetch::Denied(e)) => Err(e),
            Err(Fetch::Unavailable) => return Err(AuthError::Forbidden),
        };
        self.cache.insert(
            key,
            Cached {
                at: Instant::now(),
                answer: answer.clone(),
            },
        );
        answer
    }
}

fn parse_repo(repo: &str) -> Result<RepoName, AuthError> {
    RepoName::parse(repo).map_err(|_| AuthError::Forbidden)
}

fn denied_for(login: Option<Login<'_>>) -> AuthError {
    if login.is_some() {
        AuthError::Forbidden
    } else {
        AuthError::Unauthenticated
    }
}

fn unavailable(e: &ForgejoError) -> Fetch {
    tracing::warn!(error = %e, "forgejo authorisation lookup failed");
    Fetch::Unavailable
}

// ------------------------------------------------------------------------------ stage 1

#[derive(Deserialize)]
struct ApiUser {
    id: i64,
    login: String,
}

#[derive(Deserialize, Default)]
struct ApiPerms {
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    push: bool,
    #[serde(default)]
    pull: bool,
}

#[derive(Deserialize, Default)]
struct ApiRepo {
    #[serde(default)]
    permissions: ApiPerms,
}

/// Stage 1: `GET /api/v1/repos/{o}/{r}` with the caller's token, reading `permissions`.
pub struct PublicApiAuthz {
    client: Arc<ForgejoClient>,
    core: AuthzCore,
}

impl PublicApiAuthz {
    pub fn new(client: Arc<ForgejoClient>, cache: AuthzCacheConfig) -> Self {
        let internal = client.internal.clone();
        Self {
            client,
            core: AuthzCore::new(cache, internal),
        }
    }

    /// The full decision, including the identity and branch rules.
    pub async fn decide(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Arc<Decision>, AuthError> {
        self.decide_login(token.map(Login::bearer), repo, access)
            .await
    }

    /// [`decide`](Self::decide) for a parsed credential: Basic `user:password` is forwarded to
    /// Forgejo as Basic.
    pub async fn decide_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Arc<Decision>, AuthError> {
        self.decide_login(credential.map(Login::of), repo, access)
            .await
    }

    async fn decide_login(
        &self,
        login: Option<Login<'_>>,
        repo: &str,
        access: Access,
    ) -> Result<Arc<Decision>, AuthError> {
        let name = parse_repo(repo)?;
        self.core
            .resolve(login, repo, access, || self.fetch(login, &name, access))
            .await
    }

    async fn get(
        &self,
        segments: &[&str],
        login: Option<Login<'_>>,
    ) -> Result<(StatusCode, Vec<u8>), Fetch> {
        let mut req = self.client.http.get(self.client.url(segments));
        if let Some(l) = login {
            req = l.apply(req);
        }
        ForgejoClient::send(req).await.map_err(|e| unavailable(&e))
    }

    async fn fetch(
        &self,
        login: Option<Login<'_>>,
        repo: &RepoName,
        access: Access,
    ) -> Result<Decision, Fetch> {
        let identity = match login {
            None => UserIdentity::anonymous(),
            Some(_) => {
                let (status, body) = self.get(&["api", "v1", "user"], login).await?;
                match status {
                    StatusCode::OK => {
                        let user: ApiUser = decode(&body).map_err(|e| unavailable(&e))?;
                        UserIdentity::new(user.id, &user.login, 0)
                            .ok_or(Fetch::Denied(AuthError::Forbidden))?
                    }
                    StatusCode::UNAUTHORIZED => {
                        return Err(Fetch::Denied(AuthError::Unauthenticated));
                    }
                    StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                        return Err(Fetch::Denied(AuthError::Forbidden));
                    }
                    _ => return Err(Fetch::Unavailable),
                }
            }
        };
        let (owner, name) = repo.as_str().split_once('/').unwrap_or_default();
        let (status, body) = self
            .get(&["api", "v1", "repos", owner, name], login)
            .await?;
        match status {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED => return Err(Fetch::Denied(AuthError::Unauthenticated)),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                return Err(Fetch::Denied(denied_for(login)));
            }
            _ => return Err(Fetch::Unavailable),
        }
        let repo: ApiRepo = decode(&body).map_err(|e| unavailable(&e))?;
        let p = repo.permissions;
        let mut perm = if p.admin {
            Perm::Admin
        } else if p.push {
            Perm::Write
        } else if p.pull || login.is_none() {
            // An anonymous 200 means a public repository.
            Perm::Read
        } else {
            Perm::None
        };
        // `permissions` describe the user, not the token. A `read:repository` token of a user with
        // push rights must not write, so Write and Admin stand only when the token's scope is
        // proven to allow a write; any other answer caps the token at Read.
        if perm > Perm::Read {
            let proven = match login {
                Some(l) if access != Access::Read => self.token_may_write(l, owner, name).await?,
                _ => false,
            };
            if !proven {
                perm = Perm::Read;
            }
        }
        if perm.allows(access) {
            Ok(Decision::for_identity(identity, perm))
        } else {
            Err(Fetch::Denied(denied_for(login)))
        }
    }

    /// Whether the token passes Forgejo's own `write:repository` scope check. The probe is
    /// `POST /api/v1/repos/{o}/{r}/markdown`: it only renders the posted text and changes nothing.
    /// Forgejo checks the token scope for the `/repos` group before any other middleware or handler
    /// (`routers/api/v1/api.go`, `tokenRequiresScopes(repository)` at the end of the group, run
    /// first by `modules/web/route.go` `wrapMiddlewareAndHandler`), and a POST needs the write level.
    /// A token without the scope gets 403 there. Tokens without scopes (Actions task tokens) skip the
    /// check; their write power is already in `permissions` (`permissions/repo_access.go`). Only
    /// a 200 counts as proof, so a missing route, a 5xx or any other answer fails closed. A Basic
    /// login is sent as Basic like every other call, so a password login (full user scope, no
    /// token scope to check) passes exactly when Forgejo says so.
    async fn token_may_write(
        &self,
        login: Login<'_>,
        owner: &str,
        name: &str,
    ) -> Result<bool, Fetch> {
        let req = login
            .apply(
                self.client.http.post(
                    self.client
                        .url(&["api", "v1", "repos", owner, name, "markdown"]),
                ),
            )
            .header("content-type", "application/json")
            .body(r#"{"Text":"x","Mode":"markdown"}"#);
        let (status, _) = ForgejoClient::send(req)
            .await
            .map_err(|e| unavailable(&e))?;
        if status != StatusCode::OK {
            tracing::debug!(
                status = status.as_u16(),
                "token lacks write:repository; capped at read"
            );
        }
        Ok(status == StatusCode::OK)
    }
}

#[async_trait]
impl Authorizer for PublicApiAuthz {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        Ok(self.decide(token, repo, access).await?.principal.clone())
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        Ok(self
            .decide_credential(credential, repo, access)
            .await?
            .principal
            .clone())
    }

    /// Stage 1 has only the public API, which cannot map a key to a user: SSH keys are
    /// [`SshSupport::Unsupported`], so every key is unknown (SPEC §25.11.3 item 5).
    async fn identify_ssh_key(&self, _key: &SshKey) -> Result<Option<SshIdentity>, AuthError> {
        Ok(None)
    }

    async fn authorize_ssh_key(
        &self,
        _key: &SshKey,
        _repo: &str,
        _access: Access,
    ) -> Result<SshDecision, AuthError> {
        Ok(SshDecision::Denied)
    }
}

impl PublicApiAuthz {
    /// Stage 1 cannot resolve SSH keys.
    pub const SSH_KEYS: SshSupport = SshSupport::Unsupported;
}

// ------------------------------------------------------------------------------ stage 2+

/// `modules/private/asc.go` `AscAuthzRequest`, token variant (K07, K07b). The handler decodes it
/// strictly: an unknown field is a 400, so the field names here are exactly the Go JSON tags.
/// `mode` is `read` or `write` and nothing else; `ref` is optional and asc sends none.
#[derive(Serialize)]
struct AuthzRequest<'a> {
    /// The secret: a token, or the password of a Basic login. Absent for an anonymous caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<&'a str>,
    /// The Basic user name, present only for a Basic login with one. Forgejo then checks
    /// `user:token` as it checks a git client's HTTP Basic credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
    owner: &'a str,
    repo: &'a str,
    mode: &'static str,
}

#[derive(Deserialize, Default)]
struct BranchRules {
    #[serde(default)]
    require_signed: bool,
}

/// The answer of `POST /api/internal/asc/authz` (K07).
#[derive(Deserialize, Default)]
struct AuthzAnswer {
    #[serde(default)]
    allowed: bool,
    #[serde(default)]
    user_id: i64,
    #[serde(default)]
    username: String,
    #[serde(default)]
    perm: String,
    #[serde(default)]
    deploy_key_id: i64,
    #[serde(default)]
    is_actions_task: bool,
    #[serde(default)]
    task: Option<TaskInfo>,
    #[serde(default)]
    branch_rules: BranchRules,
}

/// Stage 2+: `POST /api/internal/asc/authz` (K07), with composition grants (SPEC §15.5).
pub struct InternalAuthz {
    client: Arc<ForgejoClient>,
    core: AuthzCore,
    grants: Vec<CompositionGrant>,
    ssh: forgejo_ssh::SshLookup,
}

impl InternalAuthz {
    pub fn new(
        client: Arc<ForgejoClient>,
        grants: Vec<CompositionGrant>,
        cache: AuthzCacheConfig,
    ) -> Self {
        let internal = client.internal.clone();
        Self {
            client,
            core: AuthzCore::new(cache, internal),
            grants,
            ssh: forgejo_ssh::SshLookup::new(cache),
        }
    }

    /// Sets the timeout and the concurrency cap of the SSH key lookups (SPEC §25.11.3).
    #[must_use]
    pub fn with_ssh_limits(mut self, limits: SshLookupLimits) -> Self {
        self.ssh = self.ssh.with_limits(limits);
        self
    }

    /// The full decision, including the identity, task and branch rules.
    pub async fn decide(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Arc<Decision>, AuthError> {
        self.decide_login(token.map(Login::bearer), repo, access)
            .await
    }

    /// [`decide`](Self::decide) for a parsed credential: a Basic login is forwarded with its user
    /// name, which Forgejo checks as HTTP Basic `user:secret`.
    pub async fn decide_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Arc<Decision>, AuthError> {
        self.decide_login(credential.map(Login::of), repo, access)
            .await
    }

    async fn decide_login(
        &self,
        login: Option<Login<'_>>,
        repo: &str,
        access: Access,
    ) -> Result<Arc<Decision>, AuthError> {
        parse_repo(repo)?;
        self.core
            .resolve(login, repo, access, || self.fetch(login, repo, access))
            .await
    }

    /// Repository names compare exactly. The registry resolves names case-sensitively, so a
    /// case-folded match would let a grant for `asc/asc` reach a distinct `asc/ASC`.
    fn granted(&self, parent: &str, child: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g.parent == parent && g.child == child)
    }

    async fn fetch(
        &self,
        login: Option<Login<'_>>,
        repo: &str,
        access: Access,
    ) -> Result<Decision, Fetch> {
        // The forge takes `read` and `write` only. `Admin` asks for `write` and `judge` then
        // requires the answered permission to reach admin.
        let mode = match access {
            Access::Read => "read",
            Access::Write | Access::Admin => "write",
        };
        let (owner, name) = repo
            .split_once('/')
            .ok_or(Fetch::Denied(AuthError::Forbidden))?;
        let body = AuthzRequest {
            token: login.map(|l| l.secret.expose()),
            user: login.and_then(|l| l.user),
            owner,
            repo: name,
            mode,
        };
        let req = self
            .client
            .internal_request(&["api", "internal", "asc", "authz"])
            .map_err(|e| unavailable(&e))?
            .json_body(&body)
            .map_err(|e| unavailable(&e))?;
        let (status, bytes) = ForgejoClient::send(req)
            .await
            .map_err(|e| unavailable(&e))?;
        match status {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED => return Err(Fetch::Denied(AuthError::Unauthenticated)),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                return Err(Fetch::Denied(denied_for(login)));
            }
            _ => return Err(Fetch::Unavailable),
        }
        let a: AuthzAnswer = decode(&bytes).map_err(|e| unavailable(&e))?;
        self.judge(a, login, repo, access)
    }

    fn judge(
        &self,
        a: AuthzAnswer,
        login: Option<Login<'_>>,
        repo: &str,
        access: Access,
    ) -> Result<Decision, Fetch> {
        let deny = || Fetch::Denied(denied_for(login));
        if !a.allowed {
            // The fork answers a credential that did not authenticate with every field zero
            // and a refusal after authentication with the user's own id. Unauthenticated
            // becomes a 401 challenge, so git erases the stored credential and asks again.
            return Err(Fetch::Denied(if a.user_id == 0 {
                AuthError::Unauthenticated
            } else {
                AuthError::Forbidden
            }));
        }
        let mut perm = Perm::parse(&a.perm);
        let task = a.task.filter(|_| a.is_actions_task);
        if a.is_actions_task {
            // A task token is bound to its own repository. Any other repository needs a
            // composition grant, which gives read only (SPEC §15.5).
            let bound = task
                .as_ref()
                .and_then(|t| t.repo.as_deref())
                .ok_or_else(deny)?;
            // The fork reports the bound repo in Forgejo's canonical `owner/name`. Compare the
            // bytes: a different case is a different registry entry, so it is another repo.
            if bound != repo {
                perm = if self.granted(bound, repo) {
                    Perm::Read
                } else {
                    Perm::None
                };
            }
        }
        if !perm.allows(access) {
            return Err(deny());
        }
        let identity = if login.is_none() && a.user_id == 0 && a.username.is_empty() {
            UserIdentity::anonymous()
        } else {
            UserIdentity::new(a.user_id, &a.username, a.deploy_key_id).ok_or_else(deny)?
        };
        let mut d = Decision::for_identity(identity, perm);
        d.is_actions_task = a.is_actions_task;
        d.task = task;
        d.require_signed = a.branch_rules.require_signed;
        Ok(d)
    }
}

#[async_trait]
impl Authorizer for InternalAuthz {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        Ok(self.decide(token, repo, access).await?.principal.clone())
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        Ok(self
            .decide_credential(credential, repo, access)
            .await?
            .principal
            .clone())
    }

    async fn identify_ssh_key(&self, key: &SshKey) -> Result<Option<SshIdentity>, AuthError> {
        self.ssh.identify(&self.client, key).await
    }

    async fn authorize_ssh_key(
        &self,
        key: &SshKey,
        repo: &str,
        access: Access,
    ) -> Result<SshDecision, AuthError> {
        self.ssh.authorize(&self.client, key, repo, access).await
    }
}

impl InternalAuthz {
    /// Stage 2+ resolves keys through K19.
    pub const SSH_KEYS: SshSupport = SshSupport::Supported;
}

/// Small helper so the request builder reads as one chain.
trait JsonBody: Sized {
    fn json_body<T: Serialize>(self, body: &T) -> Result<Self, ForgejoError>;
}

impl JsonBody for RequestBuilder {
    fn json_body<T: Serialize>(self, body: &T) -> Result<Self, ForgejoError> {
        let bytes = serde_json::to_vec(body).map_err(|e| ForgejoError::Invalid(e.to_string()))?;
        Ok(self
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes))
    }
}

// ------------------------------------------------------------------------------ hooks

/// `modules/private/hook.go` `HookOptions`. Go has no JSON tags there, so the wire names are the
/// Go field names exactly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookOptions {
    #[serde(rename = "OldCommitIDs")]
    pub old_commit_ids: Vec<String>,
    #[serde(rename = "NewCommitIDs")]
    pub new_commit_ids: Vec<String>,
    #[serde(rename = "RefFullNames")]
    pub ref_full_names: Vec<String>,
    #[serde(rename = "UserID")]
    pub user_id: i64,
    #[serde(rename = "UserName")]
    pub user_name: String,
    #[serde(rename = "GitObjectDirectory")]
    pub git_object_directory: String,
    #[serde(rename = "GitAlternativeObjectDirectories")]
    pub git_alternative_object_directories: Vec<String>,
    #[serde(rename = "GitQuarantinePath")]
    pub git_quarantine_path: String,
    #[serde(rename = "GitPushOptions")]
    pub git_push_options: HashMap<String, String>,
    #[serde(rename = "PullRequestID")]
    pub pull_request_id: i64,
    #[serde(rename = "PushTrigger")]
    pub push_trigger: String,
    #[serde(rename = "DeployKeyID")]
    pub deploy_key_id: i64,
    #[serde(rename = "IsWiki")]
    pub is_wiki: bool,
    #[serde(rename = "ActionPerm")]
    pub action_perm: i32,
}

impl HookOptions {
    /// Options for an asc push: `PushTrigger = "asc"` and the pusher from `identity`.
    pub fn for_push(identity: Option<&UserIdentity>, push_options: &[String]) -> Self {
        let mut o = Self {
            push_trigger: PUSH_TRIGGER.into(),
            ..Self::default()
        };
        if let Some(i) = identity {
            o.user_id = i.user_id;
            o.user_name = i.handle.clone();
            o.deploy_key_id = i.deploy_key_id;
        }
        for opt in push_options {
            let (k, v) = opt.split_once('=').unwrap_or((opt, "true"));
            o.git_push_options.insert(k.to_owned(), v.to_owned());
        }
        o
    }

    /// Adds one ref change; `None` is the all-zero id.
    pub fn push_ref(&mut self, name: &str, old: Option<&str>, new: Option<&str>) {
        self.ref_full_names.push(name.to_owned());
        self.old_commit_ids.push(old.unwrap_or(ZERO_SHA).to_owned());
        self.new_commit_ids.push(new.unwrap_or(ZERO_SHA).to_owned());
    }
}

/// One entry of Forgejo's proc-receive answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ProcRefResult {
    #[serde(default, rename = "OldOID")]
    pub old_oid: String,
    #[serde(default, rename = "NewOID")]
    pub new_oid: String,
    #[serde(default, rename = "Ref")]
    pub ref_name: String,
    #[serde(default, rename = "OriginalRef")]
    pub original_ref: String,
    #[serde(default, rename = "IsForcePush")]
    pub is_force_push: bool,
    #[serde(default, rename = "IsCreatePR")]
    pub is_create_pr: bool,
    #[serde(default, rename = "Err")]
    pub err: String,
}

#[derive(Deserialize, Default)]
struct ProcAnswer {
    #[serde(default, rename = "Results")]
    results: Vec<ProcRefResult>,
    #[serde(default, rename = "Err")]
    err: String,
}

/// Calls Forgejo's internal hook endpoints with `INTERNAL_TOKEN`.
#[derive(Clone)]
pub struct HookClient {
    client: Arc<ForgejoClient>,
}

impl HookClient {
    pub fn new(client: Arc<ForgejoClient>) -> Self {
        Self { client }
    }

    fn request(&self, hook: &str, repo: &RepoName) -> Result<RequestBuilder, ForgejoError> {
        let (owner, name) = repo.as_str().split_once('/').unwrap_or_default();
        self.client
            .internal_request(&["api", "internal", "hook", hook, owner, name])
    }

    /// Pre-receive: `Err` carries Forgejo's rejection message (or why it could not be asked).
    pub async fn pre_receive(
        &self,
        repo: &RepoName,
        opts: &HookOptions,
    ) -> Result<(), HookRejected> {
        let sent = async {
            let req = self.request("pre-receive", repo)?.json_body(opts)?;
            ForgejoClient::send(req).await
        }
        .await;
        match sent {
            Ok((StatusCode::OK, _)) => Ok(()),
            Ok((status, body)) => Err(HookRejected(failure_message("pre-receive", status, &body))),
            Err(e) => {
                tracing::warn!(error = %e, "pre-receive hook call failed");
                Err(HookRejected(
                    "the forge could not be reached for pre-receive".into(),
                ))
            }
        }
    }

    /// Post-receive. `op_id` lets the receiver recognise a redelivery; any failure is retried.
    pub async fn post_receive(
        &self,
        repo: &RepoName,
        opts: &HookOptions,
        op_id: Uuid,
    ) -> Result<(), ForgejoError> {
        let req = self
            .request("post-receive", repo)?
            .header(OP_ID_HEADER, op_id.to_string())
            .json_body(opts)?;
        let (status, body) = ForgejoClient::send(req).await?;
        if status == StatusCode::OK {
            Ok(())
        } else {
            Err(ForgejoError::Status {
                status: status.as_u16(),
                message: failure_message("post-receive", status, &body),
            })
        }
    }

    /// Proc-receive (AGit `refs/for/*`): the PR refs Forgejo created or updated.
    pub async fn proc_receive(
        &self,
        repo: &RepoName,
        opts: &HookOptions,
    ) -> Result<Vec<ProcRefResult>, HookRejected> {
        let sent = async {
            let req = self.request("proc-receive", repo)?.json_body(opts)?;
            ForgejoClient::send(req).await
        }
        .await;
        let (status, body) = match sent {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "proc-receive hook call failed");
                return Err(HookRejected(
                    "the forge could not be reached for proc-receive".into(),
                ));
            }
        };
        if status != StatusCode::OK {
            return Err(HookRejected(failure_message("proc-receive", status, &body)));
        }
        let answer: ProcAnswer =
            decode(&body).map_err(|e| HookRejected(format!("proc-receive: {e}")))?;
        if !answer.err.is_empty() {
            return Err(HookRejected(clean_message(&answer.err)));
        }
        if answer.results.len() > MAX_REF_UPDATES {
            return Err(HookRejected("proc-receive: too many results".into()));
        }
        Ok(answer.results)
    }
}

/// `RequestMerge` (PLAN C50): the forge's queue is asked through the same internal channel, in
/// the other direction. The body and the answer are documented in `changes::request_merge`.
#[async_trait]
impl MergeQueue for HookClient {
    async fn request_merge(
        &self,
        repo: &RepoName,
        ask: &MergeAsk,
        op_id: Option<Uuid>,
    ) -> Result<MergeOutcome, QueueError> {
        let (owner, name) = repo.as_str().split_once('/').unwrap_or_default();
        let sent = async {
            let mut req = self.client.internal_request(&[
                "api",
                "internal",
                "asc",
                "request-merge",
                owner,
                name,
            ])?;
            if let Some(op) = op_id {
                req = req.header(OP_ID_HEADER, op.to_string());
            }
            ForgejoClient::send(req.json_body(&request_merge::forge_body(ask))?).await
        }
        .await;
        let (status, body) = match sent {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "request-merge call failed");
                return Err(QueueError::Unavailable);
            }
        };
        match status {
            StatusCode::OK => request_merge::parse_forge_answer(&body),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(QueueError::Denied(
                failure_message("request-merge", status, &body),
            )),
            _ => {
                tracing::warn!(
                    status = status.as_u16(),
                    "request-merge answered with a failure status"
                );
                Err(QueueError::Unavailable)
            }
        }
    }
}

// ------------------------------------------------------------------------------ git ids

/// Maps asc ref targets to the git ids Forgejo knows (`git_map`, SPEC §8.2).
#[derive(Clone)]
struct GitIds {
    pool: AnyPool,
}

impl GitIds {
    async fn sha(&self, ns: &Ns, target: &RefTarget) -> Result<String, ForgejoError> {
        let bytes = target.to_bytes();
        let kind = match target {
            RefTarget::Changeset(_) => KIND_COMMIT,
            RefTarget::Tag(_) => KIND_TAG,
        };
        let row = sqlx::query("SELECT git FROM git_map WHERE ns = $1 AND asc = $2 AND kind = $3")
            .bind(ns.as_bytes().to_vec())
            .bind(bytes[1..].to_vec())
            .bind(kind)
            .fetch_optional(&self.pool)
            .await?;
        let git: Vec<u8> = row
            .ok_or_else(|| ForgejoError::Invalid("a pushed object has no git id".into()))?
            .try_get("git")?;
        if git.len() != 20 {
            return Err(ForgejoError::Invalid(
                "a git id has the wrong length".into(),
            ));
        }
        Ok(hex(&git))
    }

    async fn opt_sha(
        &self,
        ns: &Ns,
        t: Option<&RefTarget>,
    ) -> Result<Option<String>, ForgejoError> {
        match t {
            Some(t) => self.sha(ns, t).await.map(Some),
            None => Ok(None),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Decodes the outbox form of a target (hex of the 33-byte wire form).
fn parse_target(s: &str) -> Option<RefTarget> {
    RefTarget::from_bytes(&unhex(s)?).ok()
}

// ------------------------------------------------------------------------------ pipeline

/// Pipeline step 7 (pre-receive) and the internal-token rule for `SKIP_PRE`.
pub struct ForgejoHooks {
    client: HookClient,
    registry: Arc<Registry>,
    ids: GitIds,
}

impl ForgejoHooks {
    pub fn new(client: HookClient, registry: Arc<Registry>) -> Self {
        let ids = GitIds {
            pool: registry.pool().clone(),
        };
        Self {
            client,
            registry,
            ids,
        }
    }

    async fn options(&self, req: &HookRequest<'_>) -> Result<HookOptions, HookRejected> {
        let internal = |e: ForgejoError| {
            tracing::warn!(error = %e, "cannot build pre-receive options");
            HookRejected("the push cannot be checked by the forge".into())
        };
        let identity = UserIdentity::parse(&req.principal.id);
        let entry = self
            .registry
            .open(req.repo)
            .await
            .map_err(|e| internal(ForgejoError::Invalid(e.to_string())))?;
        let mut opts = HookOptions::for_push(identity.as_ref(), req.push_options);
        for u in req.updates {
            let old = self
                .ids
                .opt_sha(&entry.ns, u.old.as_ref())
                .await
                .map_err(internal)?;
            let new = self
                .ids
                .opt_sha(&entry.ns, u.new.as_ref())
                .await
                .map_err(internal)?;
            opts.push_ref(&u.name, old.as_deref(), new.as_deref());
        }
        Ok(opts)
    }
}

#[async_trait]
impl PushHooks for ForgejoHooks {
    fn may_skip_pre(&self, ctx: &CallCtx) -> bool {
        is_host_service(&ctx.principal.id)
    }

    async fn pre_receive(&self, req: &HookRequest<'_>) -> Result<(), HookRejected> {
        let opts = self.options(req).await?;
        self.client.pre_receive(req.repo, &opts).await
    }
}

// ------------------------------------------------------------------------------ outbox

/// The `post_receive` outbox effect: tells Forgejo about a committed push (SPEC §8.3). Forgejo's
/// `PushUpdates` is idempotent per `(ref, new)`, and the operation id is sent as well, so the
/// at-least-once delivery of the outbox is harmless.
pub struct PostReceiveEffect {
    client: HookClient,
    ids: GitIds,
}

impl PostReceiveEffect {
    pub fn new(client: HookClient, pool: AnyPool) -> Self {
        Self {
            client,
            ids: GitIds { pool },
        }
    }

    async fn deliver(&self, job: &Job) -> Result<(), ForgejoError> {
        let bad = |m: &str| ForgejoError::Invalid(m.to_owned());
        let refs = job
            .doc
            .get("refs")
            .and_then(|r| r.as_array())
            .ok_or_else(|| bad("outbox row has no refs"))?;
        if refs.len() > MAX_REF_UPDATES {
            return Err(bad("outbox row lists too many refs"));
        }
        let op_id = Uuid::from_slice(&job.op_id).map_err(|_| bad("outbox op id"))?;
        let row = sqlx::query("SELECT name, ns FROM repos WHERE id = $1")
            .bind(job.repo_id)
            .fetch_optional(&self.ids.pool)
            .await?
            .ok_or_else(|| bad("outbox row names an unknown repository"))?;
        let name: String = row.try_get("name")?;
        let ns: Vec<u8> = row.try_get("ns")?;
        let ns = Ns::from_bytes(&ns).ok_or_else(|| bad("repository namespace"))?;
        let repo = RepoName::parse(&name).map_err(|_| bad("repository name"))?;
        let pusher = job.doc.get("pusher").and_then(|p| p.as_str()).unwrap_or("");
        let mut opts = HookOptions::for_push(UserIdentity::parse(pusher).as_ref(), &[]);
        for r in refs {
            let ref_name = r.get("name").and_then(|n| n.as_str()).unwrap_or("");
            // User refs are private to their owner and never reach the forge (SPEC §21.5.1).
            if ref_name.starts_with("refs/users/") {
                continue;
            }
            let target = |k: &str| -> Result<Option<RefTarget>, ForgejoError> {
                match r.get(k).and_then(|v| v.as_str()) {
                    None => Ok(None),
                    Some(s) => parse_target(s).map(Some).ok_or_else(|| bad("ref target")),
                }
            };
            let old = self.ids.opt_sha(&ns, target("old")?.as_ref()).await?;
            let new = self.ids.opt_sha(&ns, target("new")?.as_ref()).await?;
            opts.push_ref(ref_name, old.as_deref(), new.as_deref());
        }
        if opts.ref_full_names.is_empty() {
            return Ok(());
        }
        self.client.post_receive(&repo, &opts, op_id).await
    }
}

#[async_trait]
impl Effect for PostReceiveEffect {
    fn kind(&self) -> &'static str {
        "post_receive"
    }

    async fn run(&self, job: &Job) -> Result<(), EffectError> {
        self.deliver(job)
            .await
            .map_err(|e| EffectError(e.to_string()))
    }
}

// ------------------------------------------------------------------------------ startup

fn client_of(cfg: &ForgejoConfig) -> Result<(Arc<ForgejoClient>, bool), ForgejoError> {
    let token = cfg
        .internal_token_file
        .as_deref()
        .map(InternalToken::from_file)
        .transpose()?;
    let has_token = token.is_some();
    Ok((Arc::new(ForgejoClient::new(&cfg.url, token)?), has_token))
}

/// The authorizer for a server configured with Forgejo: [`InternalAuthz`] when the internal token
/// is configured (Stage 2+), otherwise [`PublicApiAuthz`] (Stage 1). `None` without `[forgejo]`.
pub fn authorizer(cfg: &ServeConfig) -> Result<Option<Arc<dyn Authorizer>>, ForgejoError> {
    let Some(f) = &cfg.forgejo else {
        return Ok(None);
    };
    let (client, internal) = client_of(f)?;
    let cache = AuthzCacheConfig::default();
    Ok(Some(if internal {
        Arc::new(InternalAuthz::new(
            client,
            cfg.composition_grants.clone(),
            cache,
        ))
    } else {
        Arc::new(PublicApiAuthz::new(client, cache))
    }))
}

/// Whether [`authorizer`] can resolve SSH keys for this configuration: only with the internal
/// token (Stage 2+, K19). The SSH listener start check refuses `provider = "forge"` otherwise.
pub fn ssh_support(cfg: &ServeConfig) -> SshSupport {
    match &cfg.forgejo {
        Some(f) if f.internal_token_file.is_some() => InternalAuthz::SSH_KEYS,
        _ => PublicApiAuthz::SSH_KEYS,
    }
}

/// Wires pipeline step 7 and the `post_receive` effect. Does nothing without `[forgejo]`;
/// pushes then skip pre-receive (SPEC §8.3 step 7). Hooks need the internal token.
pub fn register(
    ext: &Extensions,
    registry: &Arc<Registry>,
    outbox: &Outbox,
    cfg: Option<&ForgejoConfig>,
) -> Result<(), ForgejoError> {
    let Some(cfg) = cfg else {
        return Ok(());
    };
    let (client, internal) = client_of(cfg)?;
    if !internal {
        return Err(ForgejoError::Config(
            "forgejo.internal_token_file is required for hooks".into(),
        ));
    }
    let hooks = HookClient::new(client);
    // With a forge attached, `RequestMerge` is the forge's queue and never lands by itself.
    request_merge::attach_queue(ext, Arc::new(hooks.clone()))
        .map_err(|e| ForgejoError::Config(e.into()))?;
    ext.pipeline
        .set_push_hooks(Arc::new(ForgejoHooks::new(hooks.clone(), registry.clone())))
        .map_err(|_| ForgejoError::Config("push hooks are already set".into()))?;
    outbox.effects().register(Arc::new(PostReceiveEffect::new(
        hooks,
        registry.pool().clone(),
    )));
    Ok(())
}

/// Serves Forgejo's push webhook ([`WEBHOOK_PATH`]) when `[forgejo] webhook_secret_file` is set
/// and mirror mode is on; every signed push triggers [`Mirror::trigger`] for the repository it
/// names. Without a secret only the mirror's poll runs.
pub fn register_webhook(
    ext: &Extensions,
    mirror: Option<&Arc<Mirror>>,
    cfg: Option<&ForgejoConfig>,
) -> Result<(), ForgejoError> {
    let (Some(mirror), Some(path)) = (mirror, cfg.and_then(|c| c.webhook_secret_file.as_deref()))
    else {
        return Ok(());
    };
    let secret = forgejo_webhook::read_secret(path)?;
    ext.http
        .register(forgejo_webhook::router(mirror.clone(), secret));
    Ok(())
}

#[cfg(test)]
mod tests {
    use asc_core::id::{ChangesetId, TagId};

    use super::*;

    #[test]
    fn target_hex_round_trips() {
        let t = RefTarget::Changeset(ChangesetId::from_bytes([5; 32]));
        assert_eq!(parse_target(&hex(&t.to_bytes())), Some(t));
        let g = RefTarget::Tag(TagId::from_bytes([6; 32]));
        assert_eq!(parse_target(&hex(&g.to_bytes())), Some(g));
        assert_eq!(parse_target("zz"), None);
        assert_eq!(parse_target("0102"), None);
    }

    #[test]
    fn messages_are_bounded_and_printable() {
        assert_eq!(clean_message("a\u{1b}[31mb\n"), "a [31mb");
        assert_eq!(
            clean_message(&"x".repeat(MAX_MESSAGE * 2)).len(),
            MAX_MESSAGE
        );
    }

    #[test]
    fn failure_messages_never_show_forgejo_internal_errors() {
        let st = StatusCode::INTERNAL_SERVER_ERROR;
        let body = br#"{"err":"pq: /var/lib/forgejo/data: SELECT secret","user_msg":""}"#;
        assert_eq!(
            failure_message("pre-receive", st, body),
            "pre-receive failed with status 500"
        );
        // `err` alone, and a body that is not JSON, give the same generic text.
        assert_eq!(
            failure_message("pre-receive", st, br#"{"err":"stack at /srv/x.go:1"}"#),
            "pre-receive failed with status 500"
        );
        assert_eq!(
            failure_message("pre-receive", st, b"<html>"),
            "pre-receive failed with status 500"
        );
        // `user_msg` is shown, cleaned, even when `err` is present.
        let ok = br#"{"err":"internal","user_msg":"branch is protected\u001b[0m"}"#;
        assert_eq!(
            failure_message("pre-receive", StatusCode::FORBIDDEN, ok),
            "branch is protected [0m"
        );
    }

    #[test]
    fn client_refuses_urls_with_credentials_or_other_schemes() {
        assert!(ForgejoClient::new("http://u:p@h/", None).is_err());
        assert!(ForgejoClient::new("ftp://h/", None).is_err());
        assert!(ForgejoClient::new("https://forge.example/", None).is_ok());
        // Plain http is refused: the internal token goes to this host (U89).
        for url in [
            "http://forgejo:3000/",
            "http://192.0.2.7/",
            "http://127.0.0.1:3000/",
        ] {
            assert!(
                ForgejoClient::new_under(url, None, Policy::STRICT).is_err(),
                "{url}"
            );
        }
        assert!(
            ForgejoClient::new_under("http://127.0.0.1:3000/", None, Policy::LOOPBACK_HTTP).is_ok()
        );
        assert!(
            ForgejoClient::new_under("http://forgejo:3000/", None, Policy::LOOPBACK_HTTP).is_err()
        );
        let why = ForgejoClient::new_under(
            "http://forgejo.example:3000/o?t=hunter2",
            None,
            Policy::STRICT,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            why.contains("http://forgejo.example:3000") && !why.contains("hunter2"),
            "{why}"
        );
    }
}
