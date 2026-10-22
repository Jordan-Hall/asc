//! Bearer-token extraction and the authorisation seam (SPEC §7, §15.1).
//!
//! [`AuthInterceptor`] runs on every call. It only parses the `authorization` header and puts a
//! [`BearerToken`] into the request extensions; it never decides anything. The server supplies an
//! [`Authorizer`], and `RepoService` asks it about the named repository on every RPC.

use std::collections::HashMap;
use std::fmt;

use async_trait::async_trait;
use tonic::service::Interceptor;
use tonic::{Request, Status};

/// Longest accepted token, in bytes.
pub const MAX_TOKEN_LEN: usize = 4096;

/// Why a call was refused before it reached a handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No usable credential, or it is not valid.
    #[error("authentication required")]
    Unauthenticated,
    /// The `authorization` header is not a single well-formed bearer credential.
    #[error("malformed authorization header")]
    Malformed,
    /// The credential is valid but does not allow this access to this repository.
    #[error("not permitted")]
    Forbidden,
}

impl From<AuthError> for Status {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Unauthenticated => Status::unauthenticated(e.to_string()),
            AuthError::Malformed => Status::invalid_argument(e.to_string()),
            AuthError::Forbidden => Status::permission_denied(e.to_string()),
        }
    }
}

/// A bearer token. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    /// Accepts 1..=[`MAX_TOKEN_LEN`] visible ASCII characters.
    pub fn new(token: impl Into<String>) -> Result<Self, AuthError> {
        let token = token.into();
        let ok = !token.is_empty()
            && token.len() <= MAX_TOKEN_LEN
            && token.bytes().all(|b| b.is_ascii_graphic());
        if ok {
            Ok(Self(token))
        } else {
            Err(AuthError::Malformed)
        }
    }

    /// The secret itself; only for sending it or looking it up.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

/// Who an authorised call acts as.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Principal {
    /// Stable identifier recorded in the ref log.
    pub id: String,
}

/// What a call needs on a repository, in increasing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    Read,
    Write,
    Admin,
}

/// Decides whether a token may do `access` on `repo` (`owner/name`).
#[async_trait]
pub trait Authorizer: Send + Sync {
    /// `token` is `None` for an anonymous call.
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError>;
}

/// Extracts `authorization: Bearer <token>` into the request extensions.
///
/// A missing header is anonymous. A repeated or malformed header is rejected. The header is
/// removed from the metadata so later layers cannot log or forward it by accident.
#[derive(Clone, Copy, Debug, Default)]
pub struct AuthInterceptor;

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let token = {
            let mut values = req.metadata().get_all("authorization").iter();
            let Some(first) = values.next() else {
                return Ok(req);
            };
            if values.next().is_some() {
                return Err(AuthError::Malformed.into());
            }
            let text = first.to_str().map_err(|_| AuthError::Malformed)?;
            let (scheme, rest) = text.split_once(' ').ok_or(AuthError::Malformed)?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return Err(AuthError::Malformed.into());
            }
            BearerToken::new(rest)?
        };
        req.metadata_mut().remove("authorization");
        req.extensions_mut().insert(token);
        Ok(req)
    }
}

/// What one static token may do.
#[derive(Clone, Debug)]
struct Grant {
    principal: Principal,
    max: Access,
}

/// A fixed token table for tests and single-user servers. Tokens are stored as BLAKE3 digests, so
/// a lookup compares hashes, never the secret.
#[derive(Default)]
pub struct StaticTokens {
    grants: HashMap<[u8; 32], Grant>,
}

impl StaticTokens {
    /// An empty table: every token is refused.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `token`, allowing everything up to `max` on every repository.
    #[must_use]
    pub fn with_token(mut self, token: &str, principal: &str, max: Access) -> Self {
        self.grants.insert(
            *blake3::hash(token.as_bytes()).as_bytes(),
            Grant {
                principal: Principal {
                    id: principal.to_owned(),
                },
                max,
            },
        );
        self
    }
}

#[async_trait]
impl Authorizer for StaticTokens {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        _repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        let token = token.ok_or(AuthError::Unauthenticated)?;
        let grant = self
            .grants
            .get(blake3::hash(token.expose().as_bytes()).as_bytes())
            .ok_or(AuthError::Unauthenticated)?;
        if access > grant.max {
            return Err(AuthError::Forbidden);
        }
        Ok(grant.principal.clone())
    }
}

#[cfg(test)]
mod tests {
    use tonic::metadata::MetadataValue;

    use super::*;

    fn with_header(values: &[&str]) -> Request<()> {
        let mut r = Request::new(());
        for v in values {
            r.metadata_mut()
                .append("authorization", MetadataValue::try_from(*v).unwrap());
        }
        r
    }

    #[test]
    fn extracts_and_strips_the_token() {
        let r = AuthInterceptor
            .call(with_header(&["Bearer abc.def"]))
            .unwrap();
        assert_eq!(
            r.extensions().get::<BearerToken>().unwrap().expose(),
            "abc.def"
        );
        assert!(r.metadata().get("authorization").is_none());
        let r = AuthInterceptor.call(with_header(&["bearer t"])).unwrap();
        assert!(r.extensions().get::<BearerToken>().is_some());
    }

    #[test]
    fn missing_header_is_anonymous() {
        let r = AuthInterceptor.call(with_header(&[])).unwrap();
        assert!(r.extensions().get::<BearerToken>().is_none());
    }

    #[test]
    fn malformed_headers_are_rejected() {
        for bad in [
            &["Basic abc"][..],
            &["Bearer"],
            &["Bearer "],
            &["Bearer a b"],
            &["Bearer a", "Bearer b"],
        ] {
            let e = AuthInterceptor.call(with_header(bad)).unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{bad:?}");
        }
        let long = format!("Bearer {}", "x".repeat(MAX_TOKEN_LEN + 1));
        assert!(AuthInterceptor.call(with_header(&[&long])).is_err());
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let t = BearerToken::new("s3cret").unwrap();
        assert!(!format!("{t:?}").contains("s3cret"));
    }

    #[tokio::test]
    async fn static_tokens_enforce_the_ceiling() {
        let auth = StaticTokens::new().with_token("rw", "alice", Access::Write);
        let t = BearerToken::new("rw").unwrap();
        let p = auth.authorize(Some(&t), "o/r", Access::Read).await.unwrap();
        assert_eq!(p.id, "alice");
        assert_eq!(
            auth.authorize(Some(&t), "o/r", Access::Admin).await,
            Err(AuthError::Forbidden)
        );
        let other = BearerToken::new("nope").unwrap();
        assert_eq!(
            auth.authorize(Some(&other), "o/r", Access::Read).await,
            Err(AuthError::Unauthenticated)
        );
        assert_eq!(
            auth.authorize(None, "o/r", Access::Read).await,
            Err(AuthError::Unauthenticated)
        );
    }
}
