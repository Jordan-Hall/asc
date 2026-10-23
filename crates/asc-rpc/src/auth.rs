//! Credential parsing and the authorisation seam (SPEC §7, §15.1, §15.6.2).
//!
//! [`parse_authorization`] is the one parser every surface shares: the gRPC [`AuthInterceptor`]
//! and every HTTP route of `asc serve`. It accepts `Basic` (git's form), `Bearer` and `token` and
//! produces a [`Credential`]. [`AuthInterceptor`] only parses and puts the [`Credential`] into the
//! request extensions; it never decides anything. The server supplies an [`Authorizer`], and
//! `RepoService` asks it about the named repository on every RPC.

use std::collections::HashMap;
use std::fmt;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::{Request, Status};

/// Longest accepted secret (token or password), in bytes.
pub const MAX_TOKEN_LEN: usize = 4096;
/// Longest accepted `Authorization` header value, in bytes: a [`MAX_TOKEN_LEN`] secret in Basic
/// form plus a user name.
pub const MAX_HEADER_LEN: usize = 8192;
/// The `WWW-Authenticate` challenges of the gRPC surface, Basic first (SPEC §15.6.2).
pub const GRPC_CHALLENGE: [&str; 2] = [
    "Basic realm=\"asc\", charset=\"UTF-8\"",
    "Bearer realm=\"asc\"",
];

/// Why a call was refused before it reached a handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No usable credential, or it is not valid.
    #[error("authentication required")]
    Unauthenticated,
    /// The `authorization` header is repeated, too long or has an unknown scheme (a 400, never
    /// logged). A header of a known scheme whose content is bad is `Unauthenticated`.
    #[error("malformed authorization header")]
    Malformed,
    /// The credential is valid but does not allow this access to this repository.
    #[error("not permitted")]
    Forbidden,
}

impl From<AuthError> for Status {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Unauthenticated => {
                let mut status = Status::unauthenticated(e.to_string());
                for challenge in GRPC_CHALLENGE {
                    status
                        .metadata_mut()
                        .append("www-authenticate", MetadataValue::from_static(challenge));
                }
                status
            }
            AuthError::Malformed => Status::invalid_argument(e.to_string()),
            AuthError::Forbidden => Status::permission_denied(e.to_string()),
        }
    }
}

/// A secret: a token, or a Basic password. `Debug` never prints it.
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

    /// A Basic password: any text without control characters, 1..=[`MAX_TOKEN_LEN`] bytes.
    fn password(text: &str) -> Result<Self, AuthError> {
        if text.len() > MAX_TOKEN_LEN {
            return Err(AuthError::Malformed);
        }
        if text.is_empty() || text.chars().any(char::is_control) {
            return Err(AuthError::Unauthenticated);
        }
        Ok(Self(text.to_owned()))
    }

    /// The secret itself; only for sending it or looking it up.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// How a [`Credential`] was presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// `Authorization: Basic base64(user:secret)`, git's form.
    Basic,
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// `Authorization: token <secret>`, the Forgejo and GitHub API form.
    Token,
}

impl Scheme {
    /// The audit label (SPEC §15.6.2): `basic`, `bearer` or `token`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Bearer => "bearer",
            Self::Token => "token",
        }
    }
}

/// A credential as presented: the scheme, the user name hint (Basic only) and the secret.
///
/// The user name is kept for the audit record and is not part of any decision: `x-access-token`,
/// `oauth2`, `gitlab-ci-token`, empty and real logins all mean the same. `Debug` never prints the
/// secret. This is also the type the client resolver hands to `RpcClient`.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    scheme: Scheme,
    user: Option<String>,
    secret: BearerToken,
}

impl Credential {
    /// A `Bearer` credential.
    pub fn bearer(secret: BearerToken) -> Self {
        Self {
            scheme: Scheme::Bearer,
            user: None,
            secret,
        }
    }

    /// A `token` credential.
    pub fn token(secret: BearerToken) -> Self {
        Self {
            scheme: Scheme::Token,
            user: None,
            secret,
        }
    }

    /// A Basic credential. An empty `user` is no user name; an empty `secret` is refused.
    pub fn basic(user: &str, secret: &str) -> Result<Self, AuthError> {
        if user.len() > MAX_TOKEN_LEN || user.contains(':') || user.chars().any(char::is_control) {
            return Err(AuthError::Unauthenticated);
        }
        Ok(Self {
            scheme: Scheme::Basic,
            user: (!user.is_empty()).then(|| user.to_owned()),
            secret: BearerToken::password(secret)?,
        })
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The Basic user name hint, if one was given.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// The secret every scheme resolves to; the single input of the `Authorizer`.
    pub fn secret(&self) -> &BearerToken {
        &self.secret
    }

    /// The `Authorization` header value to send. It holds the secret: mark the header sensitive.
    pub fn header_value(&self) -> String {
        match self.scheme {
            Scheme::Basic => {
                let raw = format!("{}:{}", self.user.as_deref().unwrap_or(""), self.secret.0);
                format!("Basic {}", STANDARD.encode(raw))
            }
            Scheme::Bearer => format!("Bearer {}", self.secret.0),
            Scheme::Token => format!("token {}", self.secret.0),
        }
    }
}

impl From<BearerToken> for Credential {
    fn from(secret: BearerToken) -> Self {
        Self::bearer(secret)
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Credential({}, <redacted>)", self.scheme.label())
    }
}

/// Parses the `Authorization` header values of one request, once, for every surface.
///
/// No value is anonymous (`Ok(None)`). Refused as [`AuthError::Malformed`] (a 400): two values, a
/// value over [`MAX_HEADER_LEN`], a secret over [`MAX_TOKEN_LEN`] and an unknown scheme. Refused
/// as [`AuthError::Unauthenticated`] (a 401 with the challenge): bad base64 or UTF-8, a Basic
/// value without `:`, and an empty or ill-formed secret. The scheme is case-insensitive. Basic
/// takes the secret from the password, or from the user name when the password is empty
/// (`https://TOKEN@host`).
pub fn parse_authorization<'a>(
    values: impl IntoIterator<Item = &'a [u8]>,
) -> Result<Option<Credential>, AuthError> {
    let mut values = values.into_iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() || first.len() > MAX_HEADER_LEN {
        return Err(AuthError::Malformed);
    }
    let text = std::str::from_utf8(first)
        .ok()
        .filter(|t| t.is_ascii())
        .ok_or(AuthError::Unauthenticated)?;
    let (scheme, rest) = text.split_once(' ').unwrap_or((text, ""));
    let rest = rest.trim_start_matches(' ');
    let scheme = if scheme.eq_ignore_ascii_case("basic") {
        Scheme::Basic
    } else if scheme.eq_ignore_ascii_case("bearer") {
        Scheme::Bearer
    } else if scheme.eq_ignore_ascii_case("token") {
        Scheme::Token
    } else {
        return Err(AuthError::Malformed);
    };
    if scheme != Scheme::Basic {
        if rest.len() > MAX_TOKEN_LEN {
            return Err(AuthError::Malformed);
        }
        let secret = BearerToken::new(rest).map_err(|_| AuthError::Unauthenticated)?;
        return Ok(Some(Credential {
            scheme,
            user: None,
            secret,
        }));
    }
    let raw = STANDARD
        .decode(rest)
        .or_else(|_| STANDARD_NO_PAD.decode(rest))
        .map_err(|_| AuthError::Unauthenticated)?;
    let raw = String::from_utf8(raw).map_err(|_| AuthError::Unauthenticated)?;
    let (user, password) = raw.split_once(':').ok_or(AuthError::Unauthenticated)?;
    if password.is_empty() {
        // `https://TOKEN@host`: the token is the user name.
        return Ok(Some(Credential {
            scheme,
            user: None,
            secret: BearerToken::password(user)?,
        }));
    }
    Credential::basic(user, password).map(Some)
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

    /// The same decision for a parsed [`Credential`] of any scheme. Every surface calls this, so
    /// the three schemes carrying one secret resolve to one principal and one cache entry. The
    /// default decides on the secret alone; an authorizer that can use the Basic user name
    /// overrides it. A wrapper must forward this method to its inner authorizer.
    async fn authorize_credential(
        &self,
        credential: Option<&Credential>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.authorize(credential.map(Credential::secret), repo, access)
            .await
    }
}

/// Extracts `authorization` (`Basic`, `Bearer` or `token`) into the request extensions as a
/// [`Credential`].
///
/// A missing header is anonymous. A repeated, oversized or unknown-scheme header is refused as
/// `INVALID_ARGUMENT`, a bad one of a known scheme as `UNAUTHENTICATED`. The header is removed
/// from the metadata so later layers cannot log or forward it by accident.
#[derive(Clone, Copy, Debug, Default)]
pub struct AuthInterceptor;

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let credential = parse_authorization(
            req.metadata()
                .get_all("authorization")
                .iter()
                .map(MetadataValue::as_bytes),
        )?;
        let Some(credential) = credential else {
            return Ok(req);
        };
        req.metadata_mut().remove("authorization");
        req.extensions_mut().insert(credential);
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

    fn basic(raw: &str) -> String {
        format!("Basic {}", STANDARD.encode(raw))
    }

    fn parse(value: &str) -> Result<Option<Credential>, AuthError> {
        parse_authorization([value.as_bytes()])
    }

    fn secret_of(value: &str) -> String {
        parse(value).unwrap().unwrap().secret().expose().to_owned()
    }

    #[test]
    fn extracts_and_strips_the_credential() {
        let r = AuthInterceptor
            .call(with_header(&["Bearer abc.def"]))
            .unwrap();
        let c = r.extensions().get::<Credential>().unwrap();
        assert_eq!(
            (c.scheme(), c.secret().expose()),
            (Scheme::Bearer, "abc.def")
        );
        assert!(r.metadata().get("authorization").is_none());
        let r = AuthInterceptor.call(with_header(&["bearer t"])).unwrap();
        assert!(r.extensions().get::<Credential>().is_some());
    }

    #[test]
    fn missing_header_is_anonymous() {
        let r = AuthInterceptor.call(with_header(&[])).unwrap();
        assert!(r.extensions().get::<Credential>().is_none());
    }

    #[test]
    fn basic_bearer_and_token_carry_one_secret() {
        for header in [
            "Bearer s3cret".to_owned(),
            "TOKEN s3cret".to_owned(),
            basic("x-access-token:s3cret"),
            basic("oauth2:s3cret"),
            basic("gitlab-ci-token:s3cret"),
            basic(":s3cret"),
            basic("alice:s3cret"),
            // `https://TOKEN@host`: the token is the user name, the password is empty.
            basic("s3cret:"),
            "bAsIc ".to_owned() + &STANDARD.encode("git:s3cret"),
        ] {
            assert_eq!(secret_of(&header), "s3cret", "{header}");
        }
        let c = parse(&basic("alice:pw")).unwrap().unwrap();
        assert_eq!((c.scheme(), c.user()), (Scheme::Basic, Some("alice")));
        // Account passwords may hold spaces and colons; only the first colon splits.
        assert_eq!(secret_of(&basic("alice:a b:c")), "a b:c");
        // Unpadded base64 is accepted.
        assert_eq!(secret_of("Basic OnMzY3JldA"), "s3cret");
    }

    #[test]
    fn header_value_round_trips_for_every_scheme() {
        let t = || BearerToken::new("tok").unwrap();
        for c in [
            Credential::bearer(t()),
            Credential::token(t()),
            Credential::basic("alice", "tok").unwrap(),
            Credential::basic("", "tok").unwrap(),
        ] {
            assert_eq!(parse(&c.header_value()).unwrap().unwrap(), c);
        }
    }

    #[test]
    fn a_bad_credential_of_a_known_scheme_is_unauthenticated() {
        for bad in [
            "Basic",
            "Basic ",
            "Basic !!!not-base64",
            "Basic bm9jb2xvbg==", // "nocolon"
            "Basic Og==",         // ":"
            "Basic /w==",         // not UTF-8 once decoded
            "Bearer",
            "Bearer ",
            "Bearer a b",
            "token",
        ] {
            assert_eq!(parse(bad), Err(AuthError::Unauthenticated), "{bad}");
        }
        assert_eq!(
            parse(&basic("a:\u{7}b")),
            Err(AuthError::Unauthenticated),
            "control character"
        );
        assert_eq!(
            parse_authorization([&b"Basic \xff\xfe"[..]]),
            Err(AuthError::Unauthenticated)
        );
        let e = AuthInterceptor
            .call(with_header(&["Basic !!!"]))
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
        let challenges: Vec<_> = e.metadata().get_all("www-authenticate").iter().collect();
        assert!(challenges[0].to_str().unwrap().starts_with("Basic realm="));
        assert!(challenges[1].to_str().unwrap().starts_with("Bearer realm="));
    }

    #[test]
    fn repeated_oversized_and_unknown_headers_are_malformed() {
        let long_token = format!("Bearer {}", "x".repeat(MAX_TOKEN_LEN + 1));
        let long_secret = basic(&format!("u:{}", "x".repeat(MAX_TOKEN_LEN + 1)));
        let long_header = format!("Bearer {}", "x".repeat(MAX_HEADER_LEN));
        for bad in [
            "Digest abc",
            "Negotiate abc",
            "abc",
            "",
            long_token.as_str(),
            long_secret.as_str(),
            long_header.as_str(),
        ] {
            assert_eq!(parse(bad), Err(AuthError::Malformed), "{}", bad.len());
            let e = AuthInterceptor.call(with_header(&[bad]));
            if let Err(e) = e {
                assert_eq!(e.code(), tonic::Code::InvalidArgument, "{}", bad.len());
            }
        }
        let e = AuthInterceptor
            .call(with_header(&["Bearer a", "Bearer b"]))
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        // Exactly at the limit is accepted.
        assert!(parse(&format!("Bearer {}", "x".repeat(MAX_TOKEN_LEN))).is_ok());
    }

    #[test]
    fn credential_debug_never_prints_the_secret() {
        let c = Credential::basic("alice", "s3cret").unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("s3cret") && !shown.contains("alice"));
    }

    #[tokio::test]
    async fn authorize_credential_defaults_to_the_secret() {
        let auth = StaticTokens::new().with_token("rw", "alice", Access::Write);
        for header in ["Bearer rw", "token rw", &basic("anyone:rw"), &basic("rw:")] {
            let c = parse(header).unwrap();
            let p = auth
                .authorize_credential(c.as_ref(), "o/r", Access::Read)
                .await
                .unwrap();
            assert_eq!(p.id, "alice", "{header}");
        }
        let wrong = parse(&basic("alice:nope")).unwrap();
        assert_eq!(
            auth.authorize_credential(wrong.as_ref(), "o/r", Access::Read)
                .await,
            Err(AuthError::Unauthenticated)
        );
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
