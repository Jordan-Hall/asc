//! `CrossSearch` (`crosssearch.proto`, feature `cross-search.v1`): search and symbol lookup over
//! many repositories at once.
//!
//! The service holds no index of its own. For each repository it may search, it asks the
//! [`CodeIntelRpc`] the question `CodeIntel` answers for one, with the caller's own credential.
//! Which repositories those are is decided here, one by one, on every call: a repository the
//! caller cannot read (or that does not exist) contributes nothing, not even to a count or an
//! error, so a search cannot tell a private repository from a missing one.
//!
//! Paging: the candidates (the named repositories, or those of a scope) are sorted by name and
//! visited in that order. A page token holds the position to go on from (the last repository the
//! walk looked at, or a repository whose own search has more to give, with that search's token).
//! The token is encrypted and authenticated (XChaCha20-Poly1305 under a key derived from the
//! server's page key), bound to the caller and the request by its associated data, and it expires.
//! The client cannot read a position out of it, change it, hand it to another caller or use it for
//! another question: all of those are the same "invalid page token". A repository created or made
//! readable between two pages joins the walk where its name falls; nothing already returned is
//! repeated. The position is never a name the caller was shown, so nothing in a token, an error or
//! a count tells a caller about a repository it cannot read.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use asc_rpc::IdMode;
use asc_rpc::auth::{Access, AuthError, AuthInterceptor, Authorizer, Credential, Principal};
use asc_rpc::errors::{ErrorInfo, RpcError};
use asc_rpc::server::{ClientVersion, MAX_MESSAGE, RepoName, ServerInfo, rpc_status, wire};
use asc_rpc::services::cross_search_server::{CrossSearch, CrossSearchServer};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use tonic::metadata::MetadataMap;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

use super::rpc::{Caller, CodeIntelRpc};
use crate::registry::Registry;

/// Repositories one request may name (`CrossSearchTarget.repos`).
pub const MAX_NAMED_REPOS: usize = 64;
/// Candidate repositories read from the registry for one scope; more than this and the walk is
/// cut short and says so.
pub const MAX_CANDIDATES: usize = 5_000;
/// The shortest pattern; a shorter one matches too much to be a search.
pub const MIN_PATTERN: usize = 3;
/// The longest pattern.
pub const MAX_PATTERN: usize = 512;
/// The longest symbol name of `LookupSymbol`.
pub const MAX_SYMBOL_NAME: usize = 256;
/// Symbols `LookupSymbol` returns when the request names no limit, and the most it will.
pub const DEFAULT_SYMBOLS: u32 = 50;
pub const MAX_SYMBOLS: u32 = 200;
/// Other repositories a symbol panel looks in for references (`cross_repo`).
const PANEL_REPOS: usize = 20;
/// The most authorisation questions one call asks, however many repositories there are: eight per
/// repository it may visit, and never more than this. Each question may be a call to the forge, so
/// an instance of thousands of repositories the caller cannot read must not become thousands of
/// them. A call that runs out stops where it is and gives a page token to go on from.
pub const MAX_ASKS: usize = 1_000;

/// The deployment's bounds (`[codeintel] cross_search_max_repos`, `cross_search_timeout_secs`).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Readable repositories one scope search visits.
    pub max_repos: usize,
    /// How long one call may run before it returns what it has, marked truncated.
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_repos: 200,
            timeout: Duration::from_secs(20),
        }
    }
}

/// What one call may still ask of the authorizer, and until when.
struct Asks {
    left: usize,
    deadline: tokio::time::Instant,
}

impl Asks {
    fn new(limits: &Limits, repos: usize) -> Self {
        Self {
            left: repos.saturating_mul(8).min(MAX_ASKS),
            deadline: tokio::time::Instant::now() + limits.timeout,
        }
    }

    /// Whether one more question may be asked; spends it.
    fn take(&mut self) -> bool {
        if self.left == 0 || tokio::time::Instant::now() >= self.deadline {
            return false;
        }
        self.left -= 1;
        true
    }
}

/// How long a page token is good for.
const PAGE_TTL_SECS: u64 = 900;

/// Where a walk over the candidates goes on: after the repository `at` (or at it, with `inner`,
/// the token of its own search, when `inclusive`). Empty `at` is the start.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Cursor {
    at: String,
    inclusive: bool,
    inner: String,
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    cursor: Cursor,
    /// Unix seconds.
    expires: u64,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn invalid_token() -> Status {
    Status::invalid_argument("invalid page token")
}

/// The handler of `asc.v1.CrossSearch`.
pub struct CrossSearchService {
    rpc: Arc<CodeIntelRpc>,
    registry: Arc<Registry>,
    authorizer: Arc<dyn Authorizer>,
    info: ServerInfo,
    limits: Limits,
}

impl CrossSearchService {
    /// The service over `rpc`; the registry and the limits are the handler's own.
    pub fn new(rpc: Arc<CodeIntelRpc>, authorizer: Arc<dyn Authorizer>, info: ServerInfo) -> Self {
        Self {
            registry: rpc.registry().clone(),
            limits: rpc.cross_limits(),
            rpc,
            authorizer,
            info,
        }
    }

    /// The tonic service with message limits, zstd request decoding and [`AuthInterceptor`].
    pub fn into_service(self) -> InterceptedService<CrossSearchServer<Self>, AuthInterceptor> {
        let server = CrossSearchServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(tonic::codec::CompressionEncoding::Zstd);
        InterceptedService::new(server, AuthInterceptor)
    }

    fn check_client(&self, meta: &MetadataMap) -> Result<(), Status> {
        let client = match meta.get("x-asc-client") {
            None => None,
            Some(v) => {
                let text = v
                    .to_str()
                    .map_err(|_| Status::invalid_argument("malformed x-asc-client"))?;
                ClientVersion::from_header(text)?
            }
        };
        if let (Some(min), Some(got)) = (self.info.min_client, client)
            && got < min
        {
            let mut info = ErrorInfo::default();
            info.paths.push(format!("minimum client version is {min}"));
            return Err(rpc_status(&RpcError::ClientTooOld(info)));
        }
        Ok(())
    }

    /// The caller for `repo` when the credential may read it; `None` when it may not (or the
    /// repository does not exist: the two look the same).
    async fn readable(
        &self,
        credential: Option<&Credential>,
        repo: &RepoName,
    ) -> Result<Option<Caller>, Status> {
        match self
            .authorizer
            .authorize_credential(credential, repo.as_str(), Access::Read)
            .await
        {
            Ok(principal) => Ok(Some(self.caller(credential, principal, repo.clone()))),
            Err(AuthError::Forbidden | AuthError::Unauthenticated) => Ok(None),
            Err(e @ AuthError::Malformed) => Err(e.into()),
        }
    }

    fn caller(
        &self,
        credential: Option<&Credential>,
        principal: Principal,
        repo: RepoName,
    ) -> Caller {
        Caller::new(
            self.authorizer.clone(),
            credential.cloned(),
            principal,
            repo,
            IdMode::Asc,
        )
    }

    /// The repositories a target names, sorted and without duplicates, and whether the list was
    /// cut at [`MAX_CANDIDATES`]. Nothing here is checked against the caller: every one of them
    /// goes through [`Self::readable`] before it is looked at.
    async fn candidates(
        &self,
        target: Option<&wire::CrossSearchTarget>,
        from: &Cursor,
    ) -> Result<(Vec<RepoName>, bool), Status> {
        let (from, inclusive) = (from.at.as_str(), from.inclusive);
        let target = target.ok_or_else(|| Status::invalid_argument("missing target"))?;
        let mut cut = false;
        let mut names: Vec<String> = if !target.repos.is_empty() {
            if target.repos.len() > MAX_NAMED_REPOS {
                return Err(Status::invalid_argument(format!(
                    "at most {MAX_NAMED_REPOS} repositories may be named"
                )));
            }
            target.repos.clone()
        } else {
            let scope = target
                .scope
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("name repositories or a scope"))?;
            let kind =
                wire::OwnerKind::try_from(scope.kind).unwrap_or(wire::OwnerKind::Unspecified);
            let rows: Vec<String> = match kind {
                wire::OwnerKind::Unspecified => {
                    return Err(Status::invalid_argument("the scope has no kind"));
                }
                wire::OwnerKind::Instance => sqlx::query_scalar(
                    "SELECT name FROM repos WHERE name >= $1 ORDER BY name LIMIT $2",
                )
                .bind(from.to_owned())
                .bind(MAX_CANDIDATES as i64 + 1)
                .fetch_all(self.registry.pool())
                .await
                .map_err(internal)?,
                wire::OwnerKind::Repo => vec![scope.name.clone()],
                wire::OwnerKind::User | wire::OwnerKind::Org => {
                    if scope.name.is_empty() || scope.name.contains('/') {
                        return Err(Status::invalid_argument("invalid owner name"));
                    }
                    let prefix = format!("{}/", scope.name);
                    sqlx::query_scalar(
                        "SELECT name FROM repos WHERE substr(name, 1, $1) = $2 AND name >= $3 \
                         ORDER BY name LIMIT $4",
                    )
                    .bind(prefix.len() as i64)
                    .bind(prefix)
                    .bind(from.to_owned())
                    .bind(MAX_CANDIDATES as i64 + 1)
                    .fetch_all(self.registry.pool())
                    .await
                    .map_err(internal)?
                }
            };
            rows
        };
        names.sort_unstable();
        names.dedup();
        names.retain(|n| n.as_str() > from || (inclusive && n.as_str() == from));
        if names.len() > MAX_CANDIDATES {
            names.truncate(MAX_CANDIDATES);
            cut = true;
        }
        // A name that is not a repository name cannot be one the caller may read: it is dropped
        // for a scope and refused when the caller typed it.
        let explicit = !target.repos.is_empty();
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            match RepoName::parse(&n) {
                Ok(r) => out.push(r),
                Err(e) if explicit => return Err(e),
                Err(_) => {}
            }
        }
        Ok((out, cut))
    }

    /// Who the credential is, or nobody for a call that carries none or a refused one.
    async fn who(&self, credential: Option<&Credential>) -> String {
        self.authorizer
            .authenticate(credential)
            .await
            .map(|p| p.id)
            .unwrap_or_default()
    }

    async fn symbols(
        &self,
        caller: &Caller,
        req: wire::SymbolsRequest,
    ) -> Result<wire::SymbolsResponse, Status> {
        let req = asc_rpc::codeintel::SymbolsRequest::try_from(&req)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.rpc.symbols(caller, req).await
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        let key = blake3::derive_key("asc v1 cross search page encryption", &self.rpc.page_key());
        XChaCha20Poly1305::new(&key.into())
    }

    /// `cursor` as an opaque token for the question `digest` names.
    fn seal_page(&self, digest: &[u8; 32], cursor: Cursor) -> Result<String, Status> {
        let plain = postcard::to_stdvec(&Sealed {
            cursor,
            expires: now_secs() + PAGE_TTL_SECS,
        })
        .map_err(|_| Status::internal("internal error"))?;
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| Status::internal("internal error"))?;
        let ct = self
            .cipher()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plain,
                    aad: digest,
                },
            )
            .map_err(|_| Status::internal("internal error"))?;
        let mut raw = nonce.to_vec();
        raw.extend_from_slice(&ct);
        Ok(URL_SAFE_NO_PAD.encode(raw))
    }

    /// The cursor of `token`; every failure (garbage, a changed byte, another caller, another
    /// question, a late token) is the same refusal.
    fn open_page(&self, digest: &[u8; 32], token: &str) -> Result<Cursor, Status> {
        let raw = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid_token())?;
        if raw.len() < 24 + 16 {
            return Err(invalid_token());
        }
        let nonce: [u8; 24] = raw[..24].try_into().map_err(|_| invalid_token())?;
        let plain = self
            .cipher()
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &raw[24..],
                    aad: digest,
                },
            )
            .map_err(|_| invalid_token())?;
        let sealed: Sealed = postcard::from_bytes(&plain).map_err(|_| invalid_token())?;
        if sealed.expires < now_secs() {
            return Err(invalid_token());
        }
        Ok(sealed.cursor)
    }

    fn digest(&self, who: &str, op: &str, parts: &[&[u8]]) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key("asc v1 cross search page scope");
        let mut put = |b: &[u8]| {
            h.update(&(b.len() as u64).to_le_bytes());
            h.update(b);
        };
        put(op.as_bytes());
        put(who.as_bytes());
        for p in parts {
            put(p);
        }
        *h.finalize().as_bytes()
    }
}

/// The target of a request as bytes, so a token is good for the question it came from only.
fn target_key(t: Option<&wire::CrossSearchTarget>) -> Vec<u8> {
    let Some(t) = t else { return Vec::new() };
    let mut v = Vec::new();
    if let Some(s) = &t.scope {
        v.extend_from_slice(&s.kind.to_le_bytes());
        v.extend_from_slice(s.name.as_bytes());
    }
    v.push(0);
    for r in &t.repos {
        v.extend_from_slice(r.as_bytes());
        v.push(0);
    }
    v.extend_from_slice(t.rev.as_bytes());
    v
}

fn too_slow() -> Status {
    Status::deadline_exceeded("the search took too long; narrow it to fewer repositories")
}

fn internal(e: sqlx::Error) -> Status {
    tracing::error!(error = %e, "listing repositories for a cross search");
    Status::internal("internal error")
}

fn too_broad(why: &str) -> Status {
    rpc_status(&RpcError::SearchTooBroad(ErrorInfo {
        paths: vec![why.to_owned()],
        ..ErrorInfo::default()
    }))
}

/// A pattern shorter than [`MIN_PATTERN`] bytes, or a regular expression that matches the empty
/// string (and so every line), is not a search.
fn check_pattern(pattern: &str, mode: wire::SearchMode) -> Result<(), Status> {
    if pattern.len() > MAX_PATTERN {
        return Err(Status::invalid_argument(format!(
            "the pattern is longer than {MAX_PATTERN} bytes"
        )));
    }
    if pattern.len() < MIN_PATTERN {
        return Err(too_broad("the pattern is shorter than 3 bytes"));
    }
    if mode == wire::SearchMode::Regex {
        let re = regex::RegexBuilder::new(pattern)
            .size_limit(1 << 20)
            .build()
            .map_err(|_| {
                Status::invalid_argument("the pattern is not a valid regular expression")
            })?;
        if re.is_match("") {
            return Err(too_broad("the pattern matches every line"));
        }
    }
    Ok(())
}

fn scope_of(repo: &RepoName, rev: &str, filters: &Filters<'_>) -> wire::Scope {
    wire::Scope {
        repo: Some(wire::RepoRef {
            name: repo.to_string(),
            id_mode: IdMode::Asc.to_wire(),
        }),
        at: Some(wire::scope::At::Rev(if rev.is_empty() {
            "HEAD".to_owned()
        } else {
            rev.to_owned()
        })),
        paths: filters.paths.to_vec(),
        exclude_paths: filters.exclude.to_vec(),
        langs: filters.langs.to_vec(),
        ..wire::Scope::default()
    }
}

struct Filters<'a> {
    paths: &'a [String],
    exclude: &'a [String],
    langs: &'a [String],
}

/// Statuses that mean "this repository has nothing to search" (no such revision, no commits
/// yet, not ready) rather than a failure of the call.
fn is_empty_repo(s: &Status) -> bool {
    matches!(s.code(), tonic::Code::NotFound)
}

type SearchStream = Pin<
    Box<dyn futures::Stream<Item = Result<wire::CrossSearchResponse, Status>> + Send + 'static>,
>;

#[tonic::async_trait]
impl CrossSearch for CrossSearchService {
    type SearchStream = SearchStream;

    async fn search(
        &self,
        request: Request<wire::CrossSearchRequest>,
    ) -> Result<Response<Self::SearchStream>, Status> {
        let (meta, ext, msg) = request.into_parts();
        self.check_client(&meta)?;
        let credential = ext.get::<Credential>().cloned();
        let mode = wire::SearchMode::try_from(msg.mode)
            .map_err(|_| Status::invalid_argument("unknown search mode"))?;
        check_pattern(&msg.pattern, mode)?;
        let budget = msg.budget.unwrap_or_default();
        let answer = tokio::time::timeout(
            self.limits.timeout,
            self.walk(credential.as_ref(), &msg, mode, &budget),
        )
        .await;
        let resp = match answer {
            Ok(r) => r?,
            Err(_) => return Err(too_slow()),
        };
        Ok(Response::new(Box::pin(futures::stream::once(async move {
            Ok(resp)
        }))))
    }

    async fn lookup_symbol(
        &self,
        request: Request<wire::LookupSymbolRequest>,
    ) -> Result<Response<wire::LookupSymbolResponse>, Status> {
        tokio::time::timeout(self.limits.timeout, self.lookup(request))
            .await
            .map_err(|_| too_slow())?
    }

    async fn symbol_panel(
        &self,
        request: Request<wire::SymbolPanelRequest>,
    ) -> Result<Response<wire::SymbolPanelResponse>, Status> {
        tokio::time::timeout(self.limits.timeout, self.panel(request))
            .await
            .map_err(|_| too_slow())?
    }
}

impl CrossSearchService {
    async fn lookup(
        &self,
        request: Request<wire::LookupSymbolRequest>,
    ) -> Result<Response<wire::LookupSymbolResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        self.check_client(&meta)?;
        let credential = ext.get::<Credential>().cloned();
        if msg.name.is_empty() || msg.name.len() > MAX_SYMBOL_NAME {
            return Err(Status::invalid_argument("the name is 1 to 256 bytes"));
        }
        let kind = wire::SymbolMatch::try_from(msg.r#match)
            .map_err(|_| Status::invalid_argument("unknown match kind"))?;
        let limit = match msg.limit {
            0 => DEFAULT_SYMBOLS,
            n => n.min(MAX_SYMBOLS),
        } as usize;
        let rev = msg.target.as_ref().map_or("", |t| t.rev.as_str());
        let digest = self.digest(
            &self.who(credential.as_ref()).await,
            "lookup",
            &[
                msg.name.as_bytes(),
                rev.as_bytes(),
                &[kind as u8],
                &target_key(msg.target.as_ref()),
            ],
        );
        let mut hits: Vec<wire::SymbolHit> = Vec::new();
        let start = if msg.page_token.is_empty() {
            Cursor::default()
        } else {
            self.open_page(&digest, &msg.page_token)?
        };
        let (names, cut) = self.candidates(msg.target.as_ref(), &start).await?;
        let (mut visited, mut truncated) = (0usize, cut);
        let mut asks = Asks::new(&self.limits, self.limits.max_repos);
        // Where to go on if the walk stops before the repository in hand: after the last one
        // looked at. Never the repository in hand, which may be one the caller cannot read.
        let mut pos = start;
        let mut stopped = false;
        let mut prev: Option<&RepoName> = None;
        for repo in &names {
            if let Some(p) = prev.take() {
                pos = Cursor {
                    at: p.to_string(),
                    ..Cursor::default()
                };
            }
            prev = Some(repo);
            if hits.len() >= limit || !asks.take() {
                truncated = true;
                stopped = true;
                break;
            }
            let Some(caller) = self.readable(credential.as_ref(), repo).await? else {
                continue;
            };
            if visited >= self.limits.max_repos {
                truncated = true;
                stopped = true;
                break;
            }
            visited += 1;
            let req = wire::SymbolsRequest {
                scope: Some(scope_of(
                    repo,
                    rev,
                    &Filters {
                        paths: &[],
                        exclude: &[],
                        langs: &msg.langs,
                    },
                )),
                op: if kind == wire::SymbolMatch::Exact {
                    wire::SymbolOp::Def
                } else {
                    wire::SymbolOp::Find
                } as i32,
                name: msg.name.clone(),
                kinds: msg.kinds.clone(),
                budget: Some(wire::Budget {
                    max_results: (limit - hits.len()).min(u32::MAX as usize) as u32,
                    ..wire::Budget::default()
                }),
                ..wire::SymbolsRequest::default()
            };
            let resp = match self.symbols(&caller, req).await {
                Ok(r) => r,
                Err(s) if is_empty_repo(&s) => continue,
                Err(s) => return Err(s),
            };
            truncated |= resp.truncated;
            for s in resp.symbols {
                if msg.definitions_only && !s.is_def {
                    continue;
                }
                hits.push(wire::SymbolHit {
                    repo: Some(wire::RepoRef {
                        name: repo.to_string(),
                        id_mode: IdMode::Asc.to_wire(),
                    }),
                    rev: rev.to_owned(),
                    score: if kind == wire::SymbolMatch::Exact {
                        1.0
                    } else {
                        0.0
                    },
                    symbol: Some(s),
                });
            }
        }
        let next = if stopped {
            self.seal_page(&digest, pos)?
        } else {
            String::new()
        };
        Ok(Response::new(wire::LookupSymbolResponse {
            hits,
            truncated,
            next_page_token: next,
        }))
    }

    async fn panel(
        &self,
        request: Request<wire::SymbolPanelRequest>,
    ) -> Result<Response<wire::SymbolPanelResponse>, Status> {
        let (meta, ext, msg) = request.into_parts();
        self.check_client(&meta)?;
        let credential = ext.get::<Credential>().cloned();
        if msg.name.is_empty() || msg.name.len() > MAX_SYMBOL_NAME {
            return Err(Status::invalid_argument("the name is 1 to 256 bytes"));
        }
        let repo_ref = msg
            .repo
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing repo"))?;
        let repo = RepoName::parse(&repo_ref.name)?;
        // The panel's repository is the one the caller reads: refused as any read is.
        let principal = self
            .authorizer
            .authorize_credential(credential.as_ref(), repo.as_str(), Access::Read)
            .await?;
        let caller = self.caller(credential.as_ref(), principal, repo.clone());
        let filters = Filters {
            paths: &[],
            exclude: &[],
            langs: &[],
        };
        let ask = |op: wire::SymbolOp| wire::SymbolsRequest {
            scope: Some(scope_of(&repo, &msg.rev, &filters)),
            op: op as i32,
            name: msg.name.clone(),
            def_path: if msg.line > 0 {
                msg.path.clone()
            } else {
                Default::default()
            },
            def_line: msg.line,
            budget: msg.budget,
            ..wire::SymbolsRequest::default()
        };
        let defs = self.symbols(&caller, ask(wire::SymbolOp::Def)).await?;
        let refs = self.symbols(&caller, ask(wire::SymbolOp::Refs)).await?;
        let callers = self.symbols(&caller, ask(wire::SymbolOp::Callers)).await?;
        let here = |name: &str| {
            Some(wire::RepoRef {
                name: name.to_owned(),
                id_mode: repo_ref.id_mode,
            })
        };
        let group = |name: &str, symbols: Vec<wire::Symbol>| -> Vec<wire::SymbolReferences> {
            let mut by_path: BTreeMap<Vec<u8>, Vec<wire::Symbol>> = BTreeMap::new();
            for s in symbols {
                by_path.entry(s.path.to_vec()).or_default().push(s);
            }
            by_path
                .into_iter()
                .map(|(path, references)| wire::SymbolReferences {
                    repo: here(name),
                    path: path.into(),
                    references,
                })
                .collect()
        };
        let mut truncated = defs.truncated || refs.truncated || callers.truncated;
        let mut tokens = defs.est_tokens + refs.est_tokens + callers.est_tokens;
        let mut cross_repo = Vec::new();
        if msg.cross_repo {
            // References by name in the other repositories the caller can read; each is labelled
            // by the confidence the symbol engine gave it (`name_match` unless a precise index says
            // otherwise).
            // A bounded set: the panel repository's owner first, then the rest of the instance in
            // name order, and no more questions to the authorizer than eight per repository the
            // panel will look in.
            let (mut names, cut) = self
                .candidates(
                    Some(&wire::CrossSearchTarget {
                        scope: Some(wire::OwnerRef {
                            kind: wire::OwnerKind::Instance as i32,
                            name: String::new(),
                        }),
                        ..wire::CrossSearchTarget::default()
                    }),
                    &Cursor::default(),
                )
                .await?;
            truncated |= cut;
            let owner = format!("{}/", repo.as_str().split('/').next().unwrap_or(""));
            names.sort_by_key(|n| !n.as_str().starts_with(&owner));
            let mut visited = 0;
            let mut asks = Asks::new(&self.limits, PANEL_REPOS.min(self.limits.max_repos));
            for other in names.iter().filter(|n| **n != repo) {
                if !asks.take() {
                    truncated = true;
                    break;
                }
                let Some(c) = self.readable(credential.as_ref(), other).await? else {
                    continue;
                };
                if visited >= PANEL_REPOS.min(self.limits.max_repos) {
                    truncated = true;
                    break;
                }
                visited += 1;
                let req = wire::SymbolsRequest {
                    scope: Some(scope_of(other, "", &filters)),
                    op: wire::SymbolOp::Refs as i32,
                    name: msg.name.clone(),
                    budget: msg.budget,
                    ..wire::SymbolsRequest::default()
                };
                match self.symbols(&c, req).await {
                    Ok(r) => {
                        truncated |= r.truncated;
                        tokens += r.est_tokens;
                        cross_repo.extend(group(other.as_str(), r.symbols));
                    }
                    Err(s) if is_empty_repo(&s) => {}
                    Err(s) => return Err(s),
                }
            }
        }
        let definitions = defs
            .symbols
            .into_iter()
            .map(|s| wire::SymbolHit {
                repo: here(repo.as_str()),
                rev: msg.rev.clone(),
                symbol: Some(s),
                score: 1.0,
            })
            .collect();
        Ok(Response::new(wire::SymbolPanelResponse {
            scope: defs.scope,
            definitions,
            references: group(repo.as_str(), refs.symbols),
            callers: group(repo.as_str(), callers.symbols),
            cross_repo,
            truncated,
            est_tokens: tokens,
        }))
    }

    /// The walk of `Search`: visits the candidates in name order and stops when the budget is
    /// spent, the repository limit is reached or the walk is cut.
    async fn walk(
        &self,
        credential: Option<&Credential>,
        msg: &wire::CrossSearchRequest,
        mode: wire::SearchMode,
        budget: &wire::Budget,
    ) -> Result<wire::CrossSearchResponse, Status> {
        let rev = msg.target.as_ref().map_or("", |t| t.rev.as_str());
        let digest = self.digest(
            &self.who(credential).await,
            "search",
            &[
                msg.pattern.as_bytes(),
                &[mode as u8, u8::from(msg.case_sensitive)],
                rev.as_bytes(),
                &target_key(msg.target.as_ref()),
                &msg.paths.join("\0").into_bytes(),
                &msg.exclude_paths.join("\0").into_bytes(),
                &msg.langs.join("\0").into_bytes(),
            ],
        );
        let start = if msg.page_token.is_empty() {
            Cursor::default()
        } else {
            self.open_page(&digest, &msg.page_token)?
        };
        let (names, cut) = self.candidates(msg.target.as_ref(), &start).await?;
        let results = match budget.max_results {
            0 => asc_codeintel::render::DEFAULT_RESULTS,
            n => n.min(asc_codeintel::render::MAX_RESULTS),
        };
        let mut room = results;
        let full_tokens = match budget.max_tokens {
            0 => asc_codeintel::render::DEFAULT_TOKENS,
            n => n.min(asc_codeintel::render::MAX_TOKENS),
        };
        let mut tokens_left = full_tokens;
        let mut out = wire::CrossSearchResponse {
            truncated: cut,
            ..wire::CrossSearchResponse::default()
        };
        let filters = Filters {
            paths: &msg.paths,
            exclude: &msg.exclude_paths,
            langs: &msg.langs,
        };
        let mut next: Option<Cursor> = None;
        let mut visited = 0usize;
        let mut asks = Asks::new(&self.limits, self.limits.max_repos);
        // Where to go on if the walk stops before the repository in hand: after the last one
        // looked at. Never the repository in hand, which may be one the caller cannot read.
        let mut pos = start.clone();
        let mut prev: Option<&RepoName> = None;
        for repo in &names {
            if let Some(p) = prev.take() {
                pos = Cursor {
                    at: p.to_string(),
                    ..Cursor::default()
                };
            }
            prev = Some(repo);
            let resuming = start.inclusive && repo.as_str() == start.at;
            if !asks.take() {
                out.truncated = true;
                next = Some(pos.clone());
                break;
            }
            let Some(caller) = self.readable(credential, repo).await? else {
                continue;
            };
            if room == 0 || tokens_left < 20 || visited >= self.limits.max_repos {
                out.truncated = true;
                next = Some(pos.clone());
                break;
            }
            visited += 1;
            let sent = if resuming {
                start.inner.clone()
            } else {
                String::new()
            };
            let req = wire::SearchRequest {
                scope: Some(scope_of(repo, rev, &filters)),
                pattern: msg.pattern.clone(),
                mode: mode as i32,
                case_sensitive: msg.case_sensitive,
                context_lines: msg.context_lines.min(5),
                // Every repository is asked with the whole budget: a repository's hits are never
                // cut to the room left on a page, they go whole to the page that has room.
                budget: Some(wire::Budget {
                    max_results: results,
                    max_bytes: budget.max_bytes,
                    max_tokens: full_tokens,
                }),
                page_token: sent.clone(),
            };
            let req = asc_rpc::codeintel::SearchRequest::try_from(&req)
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let resp = match self.rpc.search(&caller, req).await {
                Ok(r) => r,
                Err(s) if is_empty_repo(&s) => continue,
                Err(s) => return Err(s),
            };
            if !out.repos.is_empty()
                && (resp.files.len() as u32 > room || resp.est_tokens > tokens_left)
            {
                // It is readable and has more than the page has room for: it goes whole to the
                // next page, which starts after the last repository looked at.
                out.truncated = true;
                next = Some(pos.clone());
                break;
            }
            out.searched_repos += 1;
            if resp.scope.as_ref().is_some_and(|s| s.coverage < 1.0) {
                out.unindexed_repos += 1;
            }
            out.est_tokens += resp.est_tokens;
            tokens_left = tokens_left.saturating_sub(resp.est_tokens);
            room = room.saturating_sub(resp.files.len() as u32);
            if !resp.files.is_empty() {
                out.repos.push(wire::RepoHits {
                    repo: Some(wire::RepoRef {
                        name: repo.to_string(),
                        id_mode: IdMode::Asc.to_wire(),
                    }),
                    rev: rev.to_owned(),
                    scope: resp.scope,
                    files: resp.files,
                });
            }
            if !resp.next_page_token.is_empty() {
                out.truncated = true;
                next = Some(Cursor {
                    at: repo.to_string(),
                    inclusive: true,
                    inner: resp.next_page_token,
                });
                break;
            }
            if resp.truncated {
                out.truncated = true;
            }
        }
        if let Some(cursor) = next {
            out.next_page_token = self.seal_page(&digest, cursor)?;
        }
        Ok(out)
    }
}
