//! Agent tokens as credentials: the [`Authorizer`] that recognises `ascat_` secrets.
//!
//! [`AgentAuthorizer`] wraps the server's real authorizer. A secret that starts with `ascat_` is
//! decided here and never forwarded (so a forge never sees it); every other credential goes to the
//! inner authorizer untouched. A token authorises:
//!
//! * `Read` and `Write` on its own repository, nothing on any other;
//! * never `Admin`;
//! * `Write` only while it is neither revoked, expired nor exhausted. Expiry and revocation are
//!   read from the database on every call, so a revocation takes effect on the next call.
//!
//! * **Where it may call.** Only the surface in [`agent_may_reach`]: repository reads, search and
//!   writes (which the ref gate confines to the token's own branches), the read side of agent runs
//!   and the git transport. Every other path answers `PERMISSION_DENIED` to an agent credential
//!   before any service sees it ([`confine`]); a new service is closed to agents until it is
//!   named here.
//!
//! The principal is `agent:<name>:<owner id>:<token id>` (see [`super::tokens::principal_id`]); the
//! policy, the ref log and the audit trail all see the agent's own name, never the person's.

use std::sync::Arc;

use asc_rpc::auth::{
    Access, AuthError, Authorizer, BearerToken, Credential, Decision, Principal, SshIdentity,
    SshKey,
};
use async_trait::async_trait;

use super::tokens::{PREFIX, TokenRow, TokenStore, looks_like_token};
use crate::ai::clock;

/// Wraps `inner`.
pub struct AgentAuthorizer {
    inner: Arc<dyn Authorizer>,
    tokens: TokenStore,
    enabled: bool,
}

impl AgentAuthorizer {
    pub fn new(inner: Arc<dyn Authorizer>, tokens: TokenStore, enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            inner,
            tokens,
            enabled,
        })
    }

    /// The live token for `secret`, or the reason it does not authenticate.
    async fn live(&self, secret: &str) -> Result<TokenRow, AuthError> {
        if !self.enabled || !looks_like_token(secret) {
            return Err(AuthError::Unauthenticated);
        }
        let row = match self.tokens.by_secret(secret).await {
            Ok(Some(r)) => r,
            Ok(None) => return Err(AuthError::Unauthenticated),
            Err(e) => {
                // A database failure is not "unknown token", but it must not let anyone in.
                tracing::error!(class = %crate::ai::store::class(&e), "agent token lookup failed");
                return Err(AuthError::Unauthenticated);
            }
        };
        let now = clock::now();
        if row.revoked_at > 0 || row.expires_at <= now {
            return Err(AuthError::Unauthenticated);
        }
        self.tokens.touch(&row.id, now).await;
        Ok(row)
    }

    fn decide(row: &TokenRow, repo: &str, access: Access) -> Result<Principal, AuthError> {
        if !row.repo.eq_ignore_ascii_case(repo) {
            return Err(AuthError::Forbidden);
        }
        match access {
            Access::Read => {}
            Access::Write if !row.exhausted => {}
            // An exhausted token authenticates for nothing that changes state.
            Access::Write => return Err(AuthError::Unauthenticated),
            Access::Admin => return Err(AuthError::Forbidden),
        }
        Ok(Principal {
            id: row.principal(),
        })
    }
}

fn is_agent_secret(s: &str) -> bool {
    s.starts_with(PREFIX)
}

/// The services an agent credential may call whole.
const AGENT_SERVICES: &[&str] = &["asc.v1.Repo", "asc.v1.CodeIntel", "asc.v1.CrossSearch"];
/// The single methods of other services an agent credential may call: the read side of its runs.
const AGENT_METHODS: &[&str] = &[
    "asc.v1.Agents/GetRun",
    "asc.v1.Agents/ListRuns",
    "asc.v1.Agents/WatchRun",
];

/// Whether a request for `path` may carry an agent credential. gRPC paths are
/// `/<package>.<Service>/<Method>`; the git transport is `/<owner>/<repo>/info/refs`,
/// `/git-upload-pack` and `/git-receive-pack`. Anything else is closed.
pub fn agent_may_reach(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    if rest.starts_with("asc.v1.") {
        let Some((service, method)) = rest.split_once('/') else {
            return false;
        };
        return AGENT_SERVICES.contains(&service)
            || AGENT_METHODS.contains(&format!("{service}/{method}").as_str());
    }
    !rest.contains("..")
        && ["/info/refs", "/git-upload-pack", "/git-receive-pack"]
            .iter()
            .any(|tail| rest.ends_with(tail))
}

/// Middleware for the public routes: a request that carries an agent credential and names a path
/// outside [`agent_may_reach`] is refused. A gRPC client sees `PERMISSION_DENIED` (trailers-only
/// answer); anything else gets a 403. Other credentials pass untouched.
pub async fn confine(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use asc_rpc::auth::parse_authorization;
    let carries_agent = parse_authorization(
        req.headers()
            .get_all(axum::http::header::AUTHORIZATION)
            .iter()
            .map(axum::http::HeaderValue::as_bytes),
    )
    .ok()
    .flatten()
    .is_some_and(|c| is_agent_secret(c.secret().expose()));
    if carries_agent && !agent_may_reach(req.uri().path()) {
        let grpc = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("application/grpc"));
        return denied_to_agents(grpc);
    }
    next.run(req).await
}

fn denied_to_agents(grpc: bool) -> axum::response::Response {
    use axum::response::IntoResponse;
    let message = "an agent credential cannot call this";
    if grpc {
        // Code 7 is PERMISSION_DENIED; a trailers-only gRPC answer carries it in the headers.
        return (
            [
                ("content-type", "application/grpc"),
                ("grpc-status", "7"),
                ("grpc-message", message),
            ],
            axum::body::Body::empty(),
        )
            .into_response();
    }
    (axum::http::StatusCode::FORBIDDEN, message).into_response()
}

#[async_trait]
impl Authorizer for AgentAuthorizer {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        match token {
            Some(t) if is_agent_secret(t.expose()) => {
                let row = self.live(t.expose()).await?;
                Self::decide(&row, repo, access)
            }
            _ => self.inner.authorize(token, repo, access).await,
        }
    }

    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        match credential {
            Some(c) if is_agent_secret(c.secret().expose()) => {
                let row = self.live(c.secret().expose()).await?;
                Self::decide(&row, repo, access)
            }
            _ => {
                self.inner
                    .authorize_credential(credential, repo, access)
                    .await
            }
        }
    }

    async fn authenticate(&self, credential: Option<&Credential>) -> Result<Principal, AuthError> {
        match credential {
            Some(c) if is_agent_secret(c.secret().expose()) => {
                let row = self.live(c.secret().expose()).await?;
                Ok(Principal {
                    id: row.principal(),
                })
            }
            _ => self.inner.authenticate(credential).await,
        }
    }

    async fn identify_ssh_key(&self, key: &SshKey) -> Result<Option<SshIdentity>, AuthError> {
        self.inner.identify_ssh_key(key).await
    }

    async fn authorize_ssh_key(
        &self,
        key: &SshKey,
        repo: &str,
        access: Access,
    ) -> Result<Decision, AuthError> {
        self.inner.authorize_ssh_key(key, repo, access).await
    }
}

#[cfg(test)]
mod confine_tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use prost_reflect::DescriptorPool;
    use tower::ServiceExt;

    use super::*;

    const AGENT: &str = "Bearer ascat_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn app() -> Router {
        Router::new()
            .fallback(|| async { StatusCode::OK })
            .layer(axum::middleware::from_fn(confine))
    }

    async fn call(path: &str, authorization: Option<&str>, grpc: bool) -> (StatusCode, String) {
        let mut req = Request::builder().method("POST").uri(path);
        if let Some(a) = authorization {
            req = req.header("authorization", a);
        }
        if grpc {
            req = req.header("content-type", "application/grpc");
        }
        let res = app()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res
            .headers()
            .get("grpc-status")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        (res.status(), status)
    }

    /// Every method of every service in the descriptor set (so a new service is covered the day
    /// it is added): an agent credential is refused on all of them but the named agent surface,
    /// and any other credential passes the layer.
    #[tokio::test]
    async fn an_agent_credential_is_refused_on_every_service_but_the_agent_surface() {
        let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
        let mut seen = 0;
        let mut open = Vec::new();
        for service in pool.services() {
            for method in service.methods() {
                seen += 1;
                let path = format!("/{}/{}", service.full_name(), method.name());
                let allowed = agent_may_reach(&path);
                let (status, grpc) = call(&path, Some(AGENT), true).await;
                if allowed {
                    open.push(path.clone());
                    assert_eq!(status, StatusCode::OK, "{path}");
                } else {
                    // A trailers-only gRPC answer: PERMISSION_DENIED.
                    assert_eq!(grpc, "7", "{path} must be closed to agents");
                }
                let (status, _) = call(&path, Some("Bearer someone-elses-token"), true).await;
                assert_eq!(status, StatusCode::OK, "{path} for another credential");
                let (status, _) = call(&path, None, true).await;
                assert_eq!(status, StatusCode::OK, "{path} for no credential");
            }
        }
        assert!(seen > 100, "the descriptor set lists the services: {seen}");
        // What is open to agents is a short, named list: no vault, storage, keys, audit, tokens.
        for path in &open {
            for closed in [
                "Vault",
                "Storage",
                "AiProviders",
                "Audit",
                "AgentTokens",
                "Workflows",
                "Policy",
                "Signing",
                "SecretScan",
                "DepsService",
                "Projects",
                "Cloud",
                "Changes",
                "Review",
            ] {
                assert!(!path.contains(&format!(".{closed}/")), "{path}");
            }
        }
        assert!(open.iter().any(|p| p.starts_with("/asc.v1.Repo/")));
        assert!(open.contains(&"/asc.v1.Agents/WatchRun".to_owned()));
        assert!(!open.contains(&"/asc.v1.Agents/CreateRun".to_owned()));
    }

    #[tokio::test]
    async fn the_git_transport_and_plain_paths_follow_the_same_rule() {
        for ok in [
            "/o/r/info/refs",
            "/o/r.git/git-upload-pack",
            "/o/r/git-receive-pack",
        ] {
            assert_eq!(call(ok, Some(AGENT), false).await.0, StatusCode::OK, "{ok}");
        }
        for closed in [
            "/mcp",
            "/o/r/../../admin/info/refs",
            "/metrics",
            "/",
            "/v1/tokens",
        ] {
            assert_eq!(
                call(closed, Some(AGENT), false).await.0,
                StatusCode::FORBIDDEN,
                "{closed}"
            );
        }
        // Basic with the token as the password is an agent credential too.
        let basic = format!(
            "Basic {}",
            base64_basic("x", "ascat_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        );
        assert_eq!(
            call("/mcp", Some(&basic), false).await.0,
            StatusCode::FORBIDDEN
        );
    }

    fn base64_basic(user: &str, password: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    }
}
