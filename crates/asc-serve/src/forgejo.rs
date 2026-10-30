//! The Forgejo client: authorisation and hooks (SPEC §8.3 step 7, §13.5, §15.1, §15.5; PLAN U44).
//!
//! * [`PublicApiAuthz`] (Stage 1) asks Forgejo's public API with the caller's token.
//! * [`InternalAuthz`] (Stage 2+) asks `POST /api/internal/asc/authz` (K07).
//! * [`HookClient`] calls Forgejo's `pre-receive`, `post-receive` and `proc-receive` endpoints.
//! * [`ForgejoHooks`] is pipeline step 7 and [`PostReceiveEffect`] is the outbox `post_receive`.
//!
//! Everything Forgejo returns is untrusted: bodies are size-capped, identities are validated, and
//! any failure to get a clear "yes" is a refusal.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asc_core::id::{ChangesetId, RefTarget};
use asc_core::transport_policy::{Policy, classify_url};
use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Decision as SshDecision, Principal,
    Scheme, SshIdentity, SshKey, Standing,
};
use asc_rpc::changes::MergeOutcome;
use asc_rpc::server::{CallCtx, MAX_REF_UPDATES, RepoName};
use async_trait::async_trait;
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

    /// `POST /api/internal/asc/event`: tells the hub that something happened which people on `repo`
    /// have to know about. Returns how many people the hub told. The hub decides who may be told
    /// (a person who cannot read the repository is skipped) and how (inbox, email).
    pub(crate) async fn asc_event(
        &self,
        id: &str,
        body: &serde_json::Value,
    ) -> Result<u64, ForgejoError> {
        let req = self
            .internal_request(&["api", "internal", "asc", "event"])?
            .header("x-asc-event-id", id)
            .json_body(body)?;
        let (status, answer) = Self::send(req).await?;
        if status != StatusCode::OK {
            return Err(ForgejoError::Status {
                status: status.as_u16(),
                message: failure_message("event", status, &answer),
            });
        }
        #[derive(Deserialize)]
        struct Told {
            #[serde(default)]
            notified: u64,
        }
        Ok(decode::<Told>(&answer)?.notified)
    }

    /// `POST /api/internal/asc/agent-profile`: the profile of `login` that the person lets agents read.
    /// `None` when the hub says no, whatever the reason; any failure is an error for the caller to
    /// treat as no.
    pub(crate) async fn agent_profile(
        &self,
        login: &str,
    ) -> Result<Option<crate::profile::Profile>, ForgejoError> {
        let req = self
            .internal_request(&["api", "internal", "asc", "agent-profile"])?
            .json_body(&serde_json::json!({ "login": login }))?;
        let (status, answer) = Self::send(req).await?;
        if status != StatusCode::OK {
            return Err(ForgejoError::Status {
                status: status.as_u16(),
                message: failure_message("agent-profile", status, &answer),
            });
        }
        let got: crate::profile::HubAnswer = decode(&answer)?;
        Ok(got.allowed.then(|| got.into_profile()))
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
    /// The hub's fingerprint of every permission fact behind this answer (K07 `perm_version`);
    /// 0 from a forge that sends none. It is not a counter: only equality means anything.
    pub perm_version: u64,
}

impl Decision {
    /// The part of an answer that may be kept for the next caller: what is about the repository
    /// and the asker. The branch rules and the task belong to the request (to its ref), so they
    /// are cleared; an Actions task is never kept at all.
    fn reusable(&self) -> Option<Self> {
        (!self.is_actions_task).then(|| Self {
            task: None,
            require_signed: false,
            ..self.clone()
        })
    }

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
            perm_version: 0,
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
            perm_version: 0,
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

/// The longest an answer of the forge is kept, whatever is configured: a revocation cannot be
/// seen without asking, so the window stays short.
pub const MAX_AUTHZ_TTL: Duration = Duration::from_secs(5);

/// Cache sizes and lifetimes. Lifetimes above [`MAX_AUTHZ_TTL`] are cut to it.
#[derive(Clone, Copy, Debug)]
pub struct AuthzCacheConfig {
    pub positive: Duration,
    pub negative: Duration,
    pub entries: usize,
}

impl Default for AuthzCacheConfig {
    fn default() -> Self {
        Self {
            positive: POSITIVE_TTL.min(MAX_AUTHZ_TTL),
            negative: NEGATIVE_TTL.min(MAX_AUTHZ_TTL),
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
    fn new(mut cfg: AuthzCacheConfig, internal: Option<InternalToken>) -> Self {
        cfg.positive = cfg.positive.min(MAX_AUTHZ_TTL);
        cfg.negative = cfg.negative.min(MAX_AUTHZ_TTL);
        Self {
            cache: quick_cache::sync::Cache::new(cfg.entries.max(1)),
            cfg,
            internal,
        }
    }

    /// `request_level` is true for a caller that reads the branch rules or the task of the answer:
    /// it always asks the forge. So does every call that changes something (`write` and `admin`
    /// access), and every Actions task. Only a plain read decision is reused, for at most
    /// [`MAX_AUTHZ_TTL`], and what is reused has no branch rules and no task.
    async fn resolve<Fut>(
        &self,
        login: Option<Login<'_>>,
        repo: &str,
        access: Access,
        request_level: bool,
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
        let fresh = request_level || access != Access::Read;
        let mut previous = None;
        if let Some(hit) = self.cache.get(&key) {
            let ttl = if hit.answer.is_ok() {
                self.cfg.positive
            } else {
                self.cfg.negative
            };
            if !fresh && hit.at.elapsed() < ttl {
                return hit.answer;
            }
            previous = hit.answer.ok().map(|d| d.perm_version);
        }
        // A forge that cannot answer refuses the caller, and nothing it said before is reused.
        let answer = match fetch().await {
            Ok(d) => Ok(Arc::new(d)),
            Err(Fetch::Denied(e)) => Err(e),
            Err(Fetch::Unavailable) => {
                self.cache.remove(&key);
                return Err(AuthError::Forbidden);
            }
        };
        if let (Ok(d), Some(before)) = (&answer, previous)
            && before != d.perm_version
        {
            tracing::info!(
                event = "authz.perm_version_changed",
                repo,
                "the forge's permission facts changed since the last answer"
            );
        }
        let kept = match &answer {
            Ok(d) => d.reusable().map(|r| Ok(Arc::new(r))),
            Err(e) => Some(Err(*e)),
        };
        match kept {
            Some(answer) => self.cache.insert(
                key,
                Cached {
                    at: Instant::now(),
                    answer,
                },
            ),
            None => {
                self.cache.remove(&key);
            }
        }
        answer
    }
}

impl AuthzCore {
    /// Who `login` is, with no repository: `GET /api/v1/user` with the caller's own credential.
    /// The internal token is the host service. Cached under the empty repository name, which no
    /// real repository has, so it never shares an entry with a repository decision. Anonymous, or
    /// any answer but a clear user, is refused.
    async fn authenticate(
        &self,
        client: &ForgejoClient,
        login: Option<Login<'_>>,
    ) -> Result<Principal, AuthError> {
        let Some(l) = login else {
            return Err(AuthError::Unauthenticated);
        };
        let decision = self
            .resolve(login, "", Access::Read, false, || async {
                let req = l.apply(client.http.get(client.url(&["api", "v1", "user"])));
                let (status, body) = ForgejoClient::send(req)
                    .await
                    .map_err(|e| unavailable(&e))?;
                match status {
                    StatusCode::OK => {
                        let user: ApiUser = decode(&body).map_err(|e| unavailable(&e))?;
                        let identity = UserIdentity::new(user.id, &user.login, 0)
                            .ok_or(Fetch::Denied(AuthError::Unauthenticated))?;
                        Ok(Decision::for_identity(identity, Perm::None))
                    }
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                        Err(Fetch::Denied(AuthError::Unauthenticated))
                    }
                    _ => Err(Fetch::Unavailable),
                }
            })
            .await?;
        Ok(decision.principal.clone())
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
        self.decide_scoped(login, repo, access, true).await
    }

    /// [`decide_login`](Self::decide_login), with `request_level` false for a caller that needs
    /// only who the asker is and what they may do.
    async fn decide_scoped(
        &self,
        login: Option<Login<'_>>,
        repo: &str,
        access: Access,
        request_level: bool,
    ) -> Result<Arc<Decision>, AuthError> {
        let name = parse_repo(repo)?;
        self.core
            .resolve(login, repo, access, request_level, || {
                self.fetch(login, &name, access)
            })
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
        Ok(self
            .decide_scoped(token.map(Login::bearer), repo, access, false)
            .await?
            .principal
            .clone())
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        Ok(self
            .decide_scoped(credential.map(Login::of), repo, access, false)
            .await?
            .principal
            .clone())
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        self.core
            .authenticate(&self.client, credential.map(Login::of))
            .await
    }

    /// The public API cannot say what another person may do: a token tied to a person is refused.
    async fn user_standing(&self, _user_id: i64, _repo: &str, _access: Access) -> Standing {
        Standing::Person(false)
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
    /// Answer as if this forge user asked, with no credential of theirs (the internal token
    /// only): the standing of a person whose agent token is in use.
    #[serde(skip_serializing_if = "Option::is_none")]
    as_user_id: Option<i64>,
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
    /// The forge's stable id of the repository; 0 from a forge that does not send one.
    #[serde(default)]
    repo_id: u64,
    /// The hub's fingerprint of the permission facts behind this answer; 0 when absent.
    #[serde(default)]
    perm_version: u64,
}

/// Binds a repository name to the forge's id of it, in the registry's `repos.forge_id`.
#[derive(Clone)]
pub struct RepoIdBinding {
    pool: sqlx::AnyPool,
}

impl RepoIdBinding {
    pub fn new(pool: sqlx::AnyPool) -> Self {
        Self { pool }
    }

    /// `Ok(true)`: `id` is the id bound to `repo` (it is bound now if none was), or the registry
    /// does not know the name yet. `Ok(false)`: the name is bound to another id.
    pub async fn check(&self, repo: &str, id: u64) -> Result<bool, sqlx::Error> {
        let Ok(id) = i64::try_from(id) else {
            return Ok(false);
        };
        sqlx::query("UPDATE repos SET forge_id = $2 WHERE name = $1 AND forge_id = 0")
            .bind(repo)
            .bind(id)
            .execute(&self.pool)
            .await?;
        let bound: Option<i64> = sqlx::query_scalar("SELECT forge_id FROM repos WHERE name = $1")
            .bind(repo)
            .fetch_optional(&self.pool)
            .await?;
        Ok(bound.is_none_or(|b| b == id))
    }
}

/// Stage 2+: `POST /api/internal/asc/authz` (K07), with composition grants (SPEC §15.5).
pub struct InternalAuthz {
    client: Arc<ForgejoClient>,
    core: AuthzCore,
    grants: Vec<CompositionGrant>,
    ssh: forgejo_ssh::SshLookup,
    binding: Option<RepoIdBinding>,
    /// What the forge last said a person may do: `(user id, repo, access)` to when and whether.
    standing: quick_cache::sync::Cache<(i64, String, Access), (Instant, bool)>,
}

impl InternalAuthz {
    /// Refuses an answer whose `repo_id` is not the one bound to the repository's registry entry.
    #[must_use]
    pub fn with_binding(mut self, binding: RepoIdBinding) -> Self {
        self.binding = Some(binding);
        self
    }

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
            binding: None,
            standing: quick_cache::sync::Cache::new(CACHE_ENTRIES),
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
        self.decide_scoped(login, repo, access, true).await
    }

    /// [`decide_login`](Self::decide_login), with `request_level` false for a caller that needs
    /// only who the asker is and what they may do.
    async fn decide_scoped(
        &self,
        login: Option<Login<'_>>,
        repo: &str,
        access: Access,
        request_level: bool,
    ) -> Result<Arc<Decision>, AuthError> {
        parse_repo(repo)?;
        self.core
            .resolve(login, repo, access, request_level, || {
                self.fetch(login, repo, access)
            })
            .await
    }

    /// Repository names compare exactly. The registry resolves names case-sensitively, so a
    /// case-folded match would let a grant for `asc/asc` reach a distinct `asc/ASC`.
    fn granted(&self, parent: &str, child: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g.parent == parent && g.child == child)
    }

    /// The identity of a credential: `{token, user, owner: "", repo: "", mode: "read"}`. The
    /// hub answers `allowed` with the user's id and login when the credential authenticates.
    /// `{as_user_id, owner, repo, mode}`: what the forge says the person may do. Anything but a
    /// clear yes is a no; a read is kept for [`MAX_AUTHZ_TTL`] at most, a change is always asked.
    async fn person_may(&self, user_id: i64, repo: &str, access: Access) -> bool {
        let key = (user_id, repo.to_owned(), access);
        if access == Access::Read
            && let Some((at, yes)) = self.standing.get(&key)
            && at.elapsed() < self.core.cfg.positive.min(self.core.cfg.negative)
        {
            return yes;
        }
        let Some((owner, name)) = repo.split_once('/') else {
            return false;
        };
        let mode = match access {
            Access::Read => "read",
            Access::Write | Access::Admin => "write",
        };
        let body = AuthzRequest {
            token: None,
            user: None,
            as_user_id: Some(user_id),
            owner,
            repo: name,
            mode,
        };
        let sent = async {
            let req = self
                .client
                .internal_request(&["api", "internal", "asc", "authz"])
                .map_err(|e| unavailable(&e))?
                .json_body(&body)
                .map_err(|e| unavailable(&e))?;
            let (status, bytes) = ForgejoClient::send(req)
                .await
                .map_err(|e| unavailable(&e))?;
            if status != StatusCode::OK {
                return Err(Fetch::Unavailable);
            }
            decode::<AuthzAnswer>(&bytes).map_err(|e| unavailable(&e))
        };
        let yes = match sent.await {
            Ok(a) => {
                let perm = Perm::parse(&a.perm);
                a.allowed
                    && a.user_id == user_id
                    && !a.is_actions_task
                    && a.task.is_none()
                    && match access {
                        Access::Admin => perm.allows(Access::Admin),
                        other => perm.allows(other),
                    }
            }
            // The forge cannot be reached: the person's standing is not known, so it is no.
            Err(_) => false,
        };
        self.standing.insert(key, (Instant::now(), yes));
        yes
    }

    async fn fetch_identity(&self, login: Option<Login<'_>>) -> Result<Decision, Fetch> {
        let body = AuthzRequest {
            token: login.map(|l| l.secret.expose()),
            user: login.and_then(|l| l.user),
            as_user_id: None,
            owner: "",
            repo: "",
            mode: "read",
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
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                return Err(Fetch::Denied(AuthError::Unauthenticated));
            }
            _ => return Err(Fetch::Unavailable),
        }
        let a: AuthzAnswer = decode(&bytes).map_err(|e| unavailable(&e))?;
        // The same refusals as the SSH lookup: an Actions task or a deploy key is not a person,
        // and nobody who is not a real account (a negative or zero id) is one either. A task is
        // modelled only on a repository decision, where it is bound to its repository and never
        // kept; here it is never a user.
        if !a.allowed
            || a.user_id <= 0
            || a.is_actions_task
            || a.task.is_some()
            || a.deploy_key_id != 0
        {
            return Err(Fetch::Denied(AuthError::Unauthenticated));
        }
        let identity = UserIdentity::new(a.user_id, &a.username, 0)
            .ok_or(Fetch::Denied(AuthError::Unauthenticated))?;
        let mut d = Decision::for_identity(identity, Perm::None);
        d.perm_version = a.perm_version;
        Ok(d)
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
            as_user_id: None,
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
        if a.allowed
            && a.repo_id != 0
            && let Some(binding) = &self.binding
        {
            match binding.check(repo, a.repo_id).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        event = "authz.repo_id_mismatch",
                        repo,
                        "the forge's id of this repository is not the one the registry entry is bound to"
                    );
                    return Err(Fetch::Denied(denied_for(login)));
                }
                Err(e) => {
                    tracing::error!(error = %e, "repository id binding");
                    return Err(Fetch::Unavailable);
                }
            }
        }
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
        d.perm_version = a.perm_version;
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
        Ok(self
            .decide_scoped(token.map(Login::bearer), repo, access, false)
            .await?
            .principal
            .clone())
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        Ok(self
            .decide_scoped(credential.map(Login::of), repo, access, false)
            .await?
            .principal
            .clone())
    }

    async fn user_standing(&self, user_id: i64, repo: &str, access: Access) -> Standing {
        Standing::Person(self.person_may(user_id, repo, access).await)
    }

    /// Who the credential is, from the same internal authz endpoint every other decision uses:
    /// an empty owner and repository ask for the identity alone. The caller's own token is never
    /// sent to the public API, so it needs no `read:user` scope.
    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        let login = credential.map(Login::of);
        if login.is_none() {
            return Err(AuthError::Unauthenticated);
        }
        let decision = self
            .core
            .resolve(login, "", Access::Read, false, || {
                self.fetch_identity(login)
            })
            .await?;
        Ok(decision.principal.clone())
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

/// Makes the forge's git directory hold the commits of some changesets, under refs of their own,
/// so the forge can read them (AGit `refs/for/*` is never stored as a ref, SPEC section 13.5).
#[async_trait]
pub trait BridgeHold: Send + Sync {
    async fn hold(
        &self,
        repo: &Arc<crate::registry::RepoEntry>,
        items: &[(String, ChangesetId)],
    ) -> Result<(), String>;

    /// Removes the named refs a [`hold`](Self::hold) made; a ref that is already gone is no error.
    async fn release(
        &self,
        repo: &Arc<crate::registry::RepoEntry>,
        names: &[String],
    ) -> Result<(), String>;

    /// The names of the refs under [`HOLD_PREFIX`] now in the repository's git directory.
    async fn held(&self, repo: &Arc<crate::registry::RepoEntry>) -> Result<Vec<String>, String>;
}

/// The ref namespace of the holds of a submit in the forge's git directory.
pub const HOLD_PREFIX: &str = "refs/asc/for/";

/// How long a hold ref may stay in the forge's git directory, unclaimed by any submit this process
/// is running, before [`ForgejoHooks::sweep_orphans`] removes it.
pub const HOLD_TTL: Duration = Duration::from_secs(15 * 60);

/// How often the background sweep runs.
const HOLD_SWEEP_EVERY: Duration = Duration::from_secs(5 * 60);

/// The holds the submits running in this process own: which refs each operation made, and how many
/// operations name each ref (the same changeset can be submitted twice at once, and the ref is
/// named by the changeset), plus when a sweep first saw each unowned ref.
#[derive(Default)]
struct HoldBook {
    by_op: HashMap<Uuid, (RepoName, Vec<String>)>,
    live: HashMap<(String, String), usize>,
    seen: HashMap<(String, String), Instant>,
}

impl HoldBook {
    fn claim(&mut self, op: Uuid, repo: &RepoName, names: &[String]) {
        for n in names {
            *self
                .live
                .entry((repo.as_str().to_owned(), n.clone()))
                .or_default() += 1;
        }
        let e = self
            .by_op
            .entry(op)
            .or_insert_with(|| (repo.clone(), Vec::new()));
        e.1.extend(names.iter().cloned());
    }

    /// Gives up what `op` claimed; returns the repository and the refs no other operation names.
    fn unclaim(&mut self, op: Uuid) -> Option<(RepoName, Vec<String>)> {
        let (repo, names) = self.by_op.remove(&op)?;
        let mut free = Vec::new();
        for n in names {
            let key = (repo.as_str().to_owned(), n.clone());
            match self.live.get_mut(&key) {
                Some(c) if *c > 1 => *c -= 1,
                _ => {
                    self.live.remove(&key);
                    if !free.contains(&n) {
                        free.push(n);
                    }
                }
            }
        }
        Some((repo, free))
    }
}

/// Where the git layer, which starts after the forge hooks, hands them its [`BridgeHold`].
#[derive(Clone, Default)]
pub struct BridgeSlot(Arc<std::sync::OnceLock<Arc<dyn BridgeHold>>>);

impl BridgeSlot {
    /// Installs the hold; the first call wins.
    pub fn set(&self, hold: Arc<dyn BridgeHold>) {
        let _ = self.0.set(hold);
    }
}

/// Pipeline step 7 (pre-receive) and the internal-token rule for `SKIP_PRE`.
pub struct ForgejoHooks {
    client: HookClient,
    registry: Arc<Registry>,
    ids: GitIds,
    bridge_promote: bool,
    bridge: BridgeSlot,
    holds: Mutex<HoldBook>,
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
            bridge_promote: false,
            bridge: BridgeSlot::default(),
            holds: Mutex::default(),
        }
    }

    fn book(&self) -> std::sync::MutexGuard<'_, HoldBook> {
        self.holds.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Removes the hold refs the submit `op` made and no other running submit needs. Idempotent:
    /// proc-receive calls it when the forge has answered, and `abort` calls it again.
    async fn release_holds(&self, op: Uuid) {
        let Some(hold) = self.bridge.0.get() else {
            return;
        };
        let Some((repo, names)) = self.book().unclaim(op) else {
            return;
        };
        if names.is_empty() {
            return;
        }
        let released = async {
            let entry = self.registry.open(&repo).await.map_err(|e| e.to_string())?;
            hold.release(&entry, &names).await
        }
        .await;
        if let Err(e) = released {
            // The sweep removes what is left.
            tracing::warn!(error = %e, repo = %repo, "the holds of a submit could not be removed");
        }
    }

    /// Removes the hold refs of every repository that no submit of this process owns and that
    /// sweeps have seen for `ttl` (the sweep of [`HOLD_TTL`], run in the background by
    /// [`spawn_hold_sweep`]; `now` is the sweep's clock). Returns how many it removed.
    pub async fn sweep_orphans(&self, now: Instant, ttl: Duration) -> usize {
        let Some(hold) = self.bridge.0.get() else {
            return 0;
        };
        let names: Vec<String> = match sqlx::query("SELECT name FROM repos")
            .fetch_all(self.registry.pool())
            .await
        {
            Ok(rows) => rows
                .iter()
                .filter_map(|r| r.try_get::<String, _>("name").ok())
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "hold sweep: the repositories cannot be listed");
                return 0;
            }
        };
        let mut removed = 0;
        let mut listed: HashSet<(String, String)> = HashSet::new();
        for name in names {
            let Ok(repo) = RepoName::parse(&name) else {
                continue;
            };
            let Ok(entry) = self.registry.open(&repo).await else {
                continue;
            };
            // A repository without a git directory in the forge has nothing to sweep.
            let Ok(held) = hold.held(&entry).await else {
                continue;
            };
            let mut stale = Vec::new();
            {
                let mut book = self.book();
                for h in held {
                    let key = (name.clone(), h.clone());
                    listed.insert(key.clone());
                    if book.live.contains_key(&key) {
                        book.seen.remove(&key);
                        continue;
                    }
                    let first = *book.seen.entry(key).or_insert(now);
                    if now.saturating_duration_since(first) >= ttl {
                        stale.push(h);
                    }
                }
            }
            if stale.is_empty() {
                continue;
            }
            match hold.release(&entry, &stale).await {
                Ok(()) => {
                    removed += stale.len();
                    let mut book = self.book();
                    for h in &stale {
                        book.seen.remove(&(name.clone(), h.clone()));
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, repo = %name, "hold sweep: removal failed");
                }
            }
        }
        // Forget what is no longer there.
        self.book().seen.retain(|k, _| listed.contains(k));
        removed
    }

    /// The slot the git layer fills with the means to put commits in the forge's git directory.
    pub fn with_bridge(mut self, slot: BridgeSlot) -> Self {
        self.bridge = slot;
        self
    }

    /// Queues `bridge_promote` before `post_receive` on every push, so the forge's git directory
    /// is current when it hears about the push (`[forgejo] bridge_promote`).
    pub fn with_bridge_promote(mut self, on: bool) -> Self {
        self.bridge_promote = on;
        self
    }

    async fn options(&self, req: &HookRequest<'_>) -> Result<HookOptions, HookRejected> {
        let internal = |e: ForgejoError| {
            tracing::warn!(error = %e, "cannot build pre-receive options");
            HookRejected("the push cannot be checked by the forge".into())
        };
        let identity = pusher_identity(&req.principal.id);
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

/// The forge identity a push is checked as: the user the principal records, or, for an agent, the
/// user it acts for. An agent is no forge user, so without this the forge's `pre-receive` would see
/// no pusher and refuse every write of a run. The forge then applies its own checks (write access to
/// the code, protected refs, quota) for the owner: no more than the owner may do, and asc's own agent
/// gate has already narrowed it to the agent's drafts. An owner the forge does not know (a synthetic
/// id) is refused by the forge, so the push fails closed.
fn pusher_identity(principal_id: &str) -> Option<UserIdentity> {
    if let Some(i) = UserIdentity::parse(principal_id) {
        return Some(i);
    }
    let a = crate::agents::tokens::parse_principal(principal_id)?;
    UserIdentity::new(a.owner_id, &format!("agent-{}", a.name), 0)
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

    async fn proc_receive(
        &self,
        req: &HookRequest<'_>,
    ) -> Result<Vec<ProcRefResult>, HookRejected> {
        // The forge reads the commits of a `refs/for` update from its own git directory, and no
        // ref of asc names them yet: put them there under a ref of their own first.
        if let Some(hold) = self.bridge.0.get() {
            let items: Vec<(String, ChangesetId)> = req
                .updates
                .iter()
                .filter(|u| u.name.starts_with("refs/for/"))
                .filter_map(|u| match &u.new {
                    Some(RefTarget::Changeset(cs)) => {
                        Some((format!("refs/asc/for/{}", hex(cs.as_bytes())), *cs))
                    }
                    _ => None,
                })
                .collect();
            if !items.is_empty() {
                let entry = self.registry.open(req.repo).await.map_err(|e| {
                    tracing::warn!(error = %e, "proc-receive: the repository cannot be opened");
                    HookRejected("the push cannot be checked by the forge".into())
                })?;
                // Claimed before they exist, so a sweep never takes them from under the call.
                let names: Vec<String> = items.iter().map(|(n, _)| n.clone()).collect();
                self.book().claim(req.op_id, req.repo, &names);
                if let Err(e) = hold.hold(&entry, &items).await {
                    tracing::warn!(error = %e, "proc-receive: the forge's git directory is not current");
                    self.release_holds(req.op_id).await;
                    return Err(HookRejected(
                        "the forge's git directory cannot be made current".into(),
                    ));
                }
            }
        }
        // The forge has taken what it needs from the holds (the pull request's own ref) when it
        // answers, and a refusal has no use for them: they go either way.
        let answer = async {
            let opts = self.options(req).await?;
            self.client.proc_receive(req.repo, &opts).await
        }
        .await;
        self.release_holds(req.op_id).await;
        answer
    }

    async fn abort(&self, req: &HookRequest<'_>) {
        self.release_holds(req.op_id).await;
    }

    fn outbox_effects(&self) -> &'static [&'static str] {
        if self.bridge_promote {
            &[
                "bridge_promote",
                "post_receive",
                crate::codeintel::queue::EFFECT,
                crate::notify::policy::EFFECT,
            ]
        } else {
            &[
                "post_receive",
                crate::codeintel::queue::EFFECT,
                crate::notify::policy::EFFECT,
            ]
        }
    }
}

/// Runs [`ForgejoHooks::sweep_orphans`] every few minutes for as long as `hooks` lives: a crash
/// between a hold and its release leaves a ref that nothing else removes.
pub fn spawn_hold_sweep(hooks: &Arc<ForgejoHooks>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let weak = Arc::downgrade(hooks);
    rt.spawn(async move {
        loop {
            tokio::time::sleep(HOLD_SWEEP_EVERY).await;
            let Some(hooks) = weak.upgrade() else {
                return;
            };
            let n = hooks.sweep_orphans(Instant::now(), HOLD_TTL).await;
            if n > 0 {
                tracing::info!(removed = n, "hold sweep removed orphaned submit holds");
            }
        }
    });
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

/// The client that reports events to the hub, or `None` without `[forgejo]` and its internal token.
pub fn event_client(
    cfg: Option<&ForgejoConfig>,
) -> Result<Option<Arc<ForgejoClient>>, ForgejoError> {
    let Some(cfg) = cfg else {
        return Ok(None);
    };
    let (client, internal) = client_of(cfg)?;
    Ok(internal.then_some(client))
}

/// The authorizer for a server configured with Forgejo: [`InternalAuthz`] when the internal token
/// is configured (Stage 2+), otherwise [`PublicApiAuthz`] (Stage 1). `None` without `[forgejo]`.
///
/// With a `binding`, the forge's `repo_id` of a repository is bound to its registry entry.
pub fn authorizer(
    cfg: &ServeConfig,
    binding: Option<RepoIdBinding>,
) -> Result<Option<Arc<dyn Authorizer>>, ForgejoError> {
    let Some(f) = &cfg.forgejo else {
        return Ok(None);
    };
    let (client, internal) = client_of(f)?;
    let cache = AuthzCacheConfig::default();
    Ok(Some(if internal {
        let authz = InternalAuthz::new(client, cfg.composition_grants.clone(), cache);
        Arc::new(match binding {
            Some(b) => authz.with_binding(b),
            None => authz,
        })
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
) -> Result<BridgeSlot, ForgejoError> {
    let slot = BridgeSlot::default();
    let Some(cfg) = cfg else {
        return Ok(slot);
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
    let forge_hooks = Arc::new(
        ForgejoHooks::new(hooks.clone(), registry.clone())
            .with_bridge_promote(cfg.bridge_promote)
            .with_bridge(slot.clone()),
    );
    ext.pipeline
        .set_push_hooks(forge_hooks.clone())
        .map_err(|_| ForgejoError::Config("push hooks are already set".into()))?;
    spawn_hold_sweep(&forge_hooks);
    outbox.effects().register(Arc::new(PostReceiveEffect::new(
        hooks,
        registry.pool().clone(),
    )));
    Ok(slot)
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
    fn an_agent_push_is_checked_as_the_user_it_acts_for() {
        let u = pusher_identity("7:alice").expect("a user");
        assert_eq!((u.user_id, u.handle.as_str()), (7, "alice"));
        let a = pusher_identity("agent:coder-1:42:at_abc").expect("an agent");
        assert_eq!((a.user_id, a.handle.as_str(), a.deploy_key_id), (42, "agent-coder-1", 0));
        assert!(pusher_identity("agent:coder-1:0:at_abc").is_none());
        assert!(pusher_identity("agent:coder-1:42:tok").is_none());
        assert!(pusher_identity("service:host").is_none());
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

#[cfg(test)]
mod binding_tests {
    use super::*;

    async fn registry_with(names: &[&str]) -> sqlx::AnyPool {
        let pool = crate::migrate::connect("sqlite::memory:", 1).await.unwrap();
        crate::migrate::migrate(&pool).await.unwrap();
        for n in names {
            sqlx::query("INSERT INTO repos (name, ns, created) VALUES ($1, $2, 1)")
                .bind(*n)
                .bind(n.as_bytes().to_vec())
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    #[test]
    fn the_answer_carries_the_repository_id_and_an_old_forge_has_none() {
        let a: AuthzAnswer = decode(br#"{"allowed": true, "repo_id": 42}"#).unwrap();
        assert_eq!(a.repo_id, 42);
        let old: AuthzAnswer = decode(br#"{"allowed": true}"#).unwrap();
        assert_eq!(old.repo_id, 0);
    }

    #[tokio::test]
    async fn the_first_id_binds_and_another_id_for_the_same_name_is_refused() {
        let pool = registry_with(&["o/r", "o/other"]).await;
        let b = RepoIdBinding::new(pool.clone());
        assert!(b.check("o/r", 7).await.unwrap(), "binds");
        assert!(b.check("o/r", 7).await.unwrap(), "the same id again");
        assert!(!b.check("o/r", 8).await.unwrap(), "a recreated repository");
        assert!(
            b.check("o/other", 8).await.unwrap(),
            "ids are per repository"
        );
        // A name the registry does not know yet has nothing to bind to.
        assert!(b.check("o/new", 9).await.unwrap());
        let bound: i64 = sqlx::query_scalar("SELECT forge_id FROM repos WHERE name = 'o/r'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(bound, 7);
    }
}
