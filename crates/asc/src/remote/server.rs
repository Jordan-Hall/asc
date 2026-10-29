//! Server remotes (SPEC §6.2, §6.3, §7, §15.6.1): `asc+https://` URLs and `https://` URLs whose
//! host publishes `/.well-known/asc`.
//!
//! [`connect`] turns a stored remote URL into a [`Connected`] server: discovery (or the
//! `remote.<n>.ascServer` override), the credential from the U77 resolver converted to
//! `asc_rpc::auth::Credential`, and an `RpcClient` with the `objects` tier list
//! (`remote.<n>.ascObjects` overrides the document). [`ServerRemote`] is the engine's [`Remote`]
//! over it: after a call it tells the credential helpers `approve` or `reject`, and one 401 is
//! answered with one re-ask. The `HistorySelector` and `RemoteObjectsSelector` registered by
//! [`register`] give history, blame and the remote object tier of a server repository.
//!
//! Host binding: the credential is for the server's origin. An `objects` tier on another origin
//! is used only when the credential is anonymous or came from `ASC_TOKEN` (which the spec allows
//! for the hosts the discovery document names); otherwise that tier is skipped and the primary
//! serves the objects, so no secret is ever sent to a second host.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::object::Change;
use asc_core::path::RepoPath;
use asc_core::transport_policy::{Policy, classify, classify_url};
use asc_engine::blame::{self, Blame, BlameApi};
use asc_engine::graph::{self, LogQuery};
use asc_engine::history::{GitMapping, HistoryApi, LogEntries, PageToken};
use asc_engine::sync::{self, Remote};
use asc_git::realgit::RealGit;
use asc_rpc::WireId as _;
use asc_rpc::auth::{BearerToken, Credential};
use asc_rpc::client::{CallError, PushMode as WireMode, RpcClient};
use asc_rpc::errors::RpcError;
use asc_rpc::server::{MAX_MESSAGE, RepoName, wire};
use asc_rpc::services::repo_client::RepoClient;
use asc_state::StateDb;
use asc_state::state::repos::RemoteKind;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{self, Objects};
use asc_store::refs::{RefConflict, RefEntry, RefError, RefUpdate};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use gix_config::File as Config;
use tonic::codec::CompressionEncoding;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use url::Url;
use uuid::Uuid;

use super::discover::{self, Document};
use super::url::{RefsMode, RemoteSpec};
use crate::cli::common::CliError;
use crate::credential::{self, Prompt, Resolved, Resolver, Scheme, Source};
use crate::ext::{self, HistorySelector, RemoteObjectsSelector};

type CliResult<T> = std::result::Result<T, CliError>;

// ------------------------------------------------------------------------------ configuration

/// The value of `key` (last wins), from a config file.
pub fn cfg_get(file: &Config, key: &str) -> Option<String> {
    file.string(key).map(|v| v.to_string())
}

/// Every value of `key`, in order.
pub fn cfg_all(file: &Config, key: &str) -> Vec<String> {
    file.strings(key)
        .unwrap_or_default()
        .iter()
        .map(|v| v.to_string())
        .collect()
}

// ------------------------------------------------------------------------------ resolution

/// Where a server repository lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// `scheme://host[:port]` of the gRPC endpoint.
    pub rpc_url: String,
    /// `owner/name`.
    pub repo: String,
    /// The `objects` tier list in use (override, else the document's).
    pub objects: Vec<String>,
    /// The seed base URL, when the document has one.
    pub seeds: Option<String>,
    /// The oldest client the server accepts.
    pub min_client: Option<String>,
}

/// `owner/name` from the last two path segments of `url` (`.git` stripped).
pub fn repo_of(url: &Url) -> Option<String> {
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let [.., owner, name] = segments.as_slice() else {
        return None;
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    let joined = format!("{owner}/{name}");
    RepoName::parse(&joined).ok().map(|_| joined)
}

fn transport_of(url: &Url) -> Option<String> {
    discover::origin_of(url)
}

/// What a stored remote URL resolves to.
pub enum Resolution {
    Server(Target),
    /// Not an asc server (or a mirror-mode repository): git-remote mode.
    Git,
}

fn bad_url(text: &str) -> CliError {
    CliError::Fatal(format!(
        "asc: '{}' is not a usable server URL",
        crate::cli::url_display::anonymize(text)
    ))
}

/// Resolves the stored URL `stored` of the remote `name` (SPEC §6.2).
pub async fn resolve(
    file: &Config,
    db: &StateDb,
    name: Option<&str>,
    stored: &str,
) -> CliResult<Resolution> {
    let spec = super::url::parse(stored, RefsMode::Local)
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
    let key = |k: &str| name.map(|n| format!("remote.{n}.{k}"));
    let override_server = key("ascServer").and_then(|k| cfg_get(file, &k));
    let override_objects: Vec<String> = key("ascObjects")
        .map(|k| cfg_all(file, &k))
        .unwrap_or_default();
    let (url, doc) = match spec {
        RemoteSpec::Server { url } => {
            let u = Url::parse(&url).map_err(|_| bad_url(stored))?;
            (u, None)
        }
        RemoteSpec::Discover { url } => {
            let u = Url::parse(&url).map_err(|_| bad_url(stored))?;
            match override_server.as_deref() {
                Some(_) => (u, None),
                None => match discover::lookup(db, &u)
                    .await
                    .map_err(|e| CliError::Fatal(e.to_string()))?
                {
                    Some(doc) => {
                        discover::check_origins(&u, &doc, !override_objects.is_empty())
                            .map_err(CliError::Fatal)?;
                        (u, Some(doc))
                    }
                    None => return Ok(Resolution::Git),
                },
            }
        }
        _ => return Ok(Resolution::Git),
    };
    let repo = repo_of(&url).ok_or_else(|| bad_url(stored))?;
    if let Some(doc) = &doc
        && doc.is_mirror(&repo)
    {
        // Mirror mode (SPEC §6.3): fetch and push use the forge's git path.
        return Ok(Resolution::Git);
    }
    let rpc_url = match (&override_server, &doc) {
        (Some(s), _) => {
            let u = Url::parse(s).map_err(|_| bad_url(s))?;
            transport_of(&u).ok_or_else(|| bad_url(s))?
        }
        (None, Some(d)) => d.server.clone(),
        (None, None) => transport_of(&url).ok_or_else(|| bad_url(stored))?,
    };
    let objects = if override_objects.is_empty() {
        doc.as_ref().map(|d| d.objects.clone()).unwrap_or_default()
    } else {
        let mut out = Vec::new();
        for o in override_objects.iter().take(discover::MAX_OBJECTS) {
            let u = Url::parse(o).map_err(|_| bad_url(o))?;
            out.push(transport_of(&u).ok_or_else(|| bad_url(o))?);
        }
        out
    };
    let Document {
        seeds, min_client, ..
    } = doc.unwrap_or_default();
    if let Some(min) = &min_client
        && discover::older_than(env!("CARGO_PKG_VERSION"), min)
    {
        return Err(too_old(min));
    }
    for endpoint in std::iter::once(&rpc_url).chain(&objects) {
        let u = Url::parse(endpoint).map_err(|_| bad_url(endpoint))?;
        if let Some(why) = transport_refusal(&u, Policy::current()) {
            return Err(CliError::Fatal(why));
        }
    }
    Ok(Resolution::Server(Target {
        rpc_url,
        repo,
        objects,
        seeds,
        min_client,
    }))
}

fn too_old(min: &str) -> CliError {
    CliError::Fatal(format!(
        "asc: this client ({}) is older than the server accepts ({min}); upgrade asc",
        env!("CARGO_PKG_VERSION")
    ))
}

// ------------------------------------------------------------------------------ credentials

fn auth_error(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: unusable credential: {e}"))
}

/// The wire credential of a resolved one.
pub fn to_rpc(resolved: &Resolved) -> CliResult<Option<Credential>> {
    let Some(p) = resolved.presented() else {
        return Ok(None);
    };
    let secret = p.secret.expose();
    let credential = match &p.scheme {
        Scheme::Basic => Credential::basic(&p.user, secret).map_err(auth_error)?,
        Scheme::Bearer => Credential::bearer(BearerToken::new(secret).map_err(auth_error)?),
        Scheme::Other(word) if word.eq_ignore_ascii_case("token") => {
            Credential::token(BearerToken::new(secret).map_err(auth_error)?)
        }
        Scheme::Other(word) => {
            return Err(CliError::Fatal(format!(
                "asc: the '{}' authorization scheme is not supported by asc servers",
                word.chars()
                    .filter(|c| c.is_ascii_graphic())
                    .take(32)
                    .collect::<String>()
            )));
        }
    };
    Ok(Some(credential))
}

/// How credentials are (re)asked for one server.
struct ReAsk {
    file: Config,
    real: Option<RealGit>,
    url: Url,
    tiers: Vec<Url>,
    prompt: Prompt,
}

impl ReAsk {
    fn resolver(&self) -> Resolver<'_> {
        let mut r = Resolver::new(&self.file);
        if let Some(real) = &self.real {
            r = r.with_real_git(real.clone());
        }
        r = r.allow_asc_token(&self.url);
        for t in &self.tiers {
            r = r.allow_asc_token(t);
        }
        r
    }

    /// The first try: no prompt, as git sends its first request without asking anyone.
    fn first(&self) -> CliResult<Resolved> {
        self.resolver()
            .resolve(&self.url, Prompt::Never)
            .map_err(|e| CliError::Fatal(format!("asc: {e}")))
    }

    /// After a 401: ask again under the command's own prompt policy.
    fn again(&self, old: &Resolved) -> CliResult<Resolved> {
        let r = self.resolver();
        let next = if old.is_anonymous() {
            r.resolve_with_challenge(&self.url, self.prompt, &[])
        } else {
            r.resolve_after(&self.url, self.prompt, old, &[])
        };
        next.map_err(|e| CliError::Fatal(format!("asc: {e}")))
    }

    fn failure(&self, refused: bool) -> String {
        credential::failure_message(&self.url, refused, self.prompt == Prompt::Never)
    }
}

// ------------------------------------------------------------------------------ the wire

const USER_AGENT: &str = concat!("asc/", env!("CARGO_PKG_VERSION"));

/// Why `url` may not carry credentials under `policy` (the U89 transport policy: https and
/// ssh, plain http to the local machine only behind the explicit opt-in), or `None` when it may.
pub fn transport_refusal(url: &Url, policy: Policy) -> Option<String> {
    classify_url(url, policy).err().map(|e| format!("asc: {e}"))
}

/// The transport policy for a URL as stored or typed (`git+…` and `asc+…` included), under the
/// process settings: an error that names the origin when plain http, `git://` or another scheme
/// would carry it.
pub fn check_transport(text: &str) -> CliResult<()> {
    classify(text, Policy::current())
        .map(drop)
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))
}

/// A lazily connected channel to `url`: https with the system roots, plain http only to the
/// local machine and only when [`LOOPBACK_ENV`] allows it.
pub fn channel(url: &str) -> CliResult<Channel> {
    channel_with(url, Policy::current())
}

fn channel_with(url: &str, policy: Policy) -> CliResult<Channel> {
    let parsed = Url::parse(url).map_err(|_| bad_url(url))?;
    if parsed.host_str().is_none() {
        return Err(bad_url(url));
    }
    if let Some(why) = transport_refusal(&parsed, policy) {
        return Err(CliError::Fatal(why));
    }
    let mut endpoint = Endpoint::from_shared(url.to_owned()).map_err(|_| bad_url(url))?;
    match parsed.scheme() {
        "https" => {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new().with_enabled_roots())
                .map_err(|e| CliError::Fatal(format!("asc: tls: {e}")))?;
        }
        "http" => {}
        _ => return Err(bad_url(url)),
    }
    Ok(endpoint
        .connect_timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_while_idle(true)
        .connect_lazy())
}

/// The raw connection of one repository: for the calls `RpcClient` has no method for
/// (`UpdateRefs` in LAND mode or with push options, and `asc api`).
#[derive(Clone)]
pub struct Wire {
    pub channel: Channel,
    pub repo: String,
    credential: Option<Credential>,
}

impl Wire {
    pub fn new(rpc_url: &str, repo: &str, credential: Option<Credential>) -> CliResult<Self> {
        Ok(Self {
            channel: channel(rpc_url)?,
            repo: repo.to_owned(),
            credential,
        })
    }

    /// `msg` with the common metadata of SPEC §7: credential, client version, operation id.
    pub fn request<T>(&self, msg: T, op: Option<Uuid>) -> tonic::Request<T> {
        let mut r = tonic::Request::new(msg);
        let meta = r.metadata_mut();
        if let Some(c) = &self.credential
            && let Ok(mut v) = c.header_value().parse::<MetadataValue<Ascii>>()
        {
            v.set_sensitive(true);
            meta.insert("authorization", v);
        }
        if let Ok(v) = format!(
            "{USER_AGENT} {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
        .parse::<MetadataValue<Ascii>>()
        {
            meta.insert("x-asc-client", v);
        }
        if let Some(id) = op
            && let Ok(v) = id.to_string().parse::<MetadataValue<Ascii>>()
        {
            meta.insert("x-asc-op-id", v);
        }
        r
    }

    fn repo_ref(&self) -> Option<wire::RepoRef> {
        Some(wire::RepoRef {
            name: self.repo.clone(),
            id_mode: asc_rpc::IdMode::Asc.to_wire(),
        })
    }

    fn stub(&self) -> RepoClient<Channel> {
        RepoClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE)
            .max_encoding_message_size(MAX_MESSAGE)
            .accept_compressed(CompressionEncoding::Zstd)
    }
}

// ------------------------------------------------------------------------------ connection

/// One authenticated connection: the client, what resolved its credential, and the raw wire.
pub struct Session {
    pub client: RpcClient,
    pub wire: Wire,
    pub resolved: Resolved,
}

/// A server reached through discovery or an `asc+https://` URL.
pub struct Connected {
    pub target: Target,
    session: Session,
    reask: ReAsk,
}

fn build_session(
    target: &Target,
    reask: &ReAsk,
    resolved: Resolved,
    remote_name: &str,
) -> CliResult<Session> {
    let credential = to_rpc(&resolved)?;
    let client = RpcClient::connect(&target.rpc_url, &target.repo, credential.clone())
        .map_err(|e| CliError::Fatal(format!("asc: {e}")))?
        .with_remote_name(remote_name);
    // A tier on another origin gets the credential only when that is allowed (module docs).
    let server_origin = credential::Origin::of(&reask.url);
    let usable: Vec<&str> = target
        .objects
        .iter()
        .zip(&reask.tiers)
        .filter(|(_, tier)| {
            credential::Origin::of(tier) == server_origin
                || resolved.is_anonymous()
                || resolved.source_kind() == Some(Source::AscToken)
        })
        .map(|(u, _)| u.as_str())
        .collect();
    let client = if usable.is_empty() {
        client
    } else {
        client
            .with_object_tiers(&usable)
            .map_err(|e| CliError::Fatal(format!("asc: {e}")))?
    };
    let wire = Wire::new(&target.rpc_url, &target.repo, credential)?;
    Ok(Session {
        client,
        wire,
        resolved,
    })
}

/// Opens the server behind the stored URL `stored`; `None` means git-remote mode.
pub async fn connect(
    file: &Config,
    db: &StateDb,
    real: Option<RealGit>,
    name: Option<&str>,
    stored: &str,
    prompt: Prompt,
) -> CliResult<Option<Connected>> {
    let Resolution::Server(target) = resolve(file, db, name, stored).await? else {
        return Ok(None);
    };
    let mut url = Url::parse(&target.rpc_url).map_err(|_| bad_url(&target.rpc_url))?;
    // Userinfo typed in the remote's own URL counts for its own origin only.
    if let Ok(typed) = Url::parse(stored.strip_prefix("asc+").unwrap_or(stored))
        && credential::same_origin(&typed, &url)
        && !typed.username().is_empty()
    {
        let _ = url.set_username(typed.username());
        let _ = url.set_password(typed.password());
    }
    let tiers: Vec<Url> = target
        .objects
        .iter()
        .filter_map(|o| Url::parse(o).ok())
        .collect();
    let reask = ReAsk {
        file: file.clone(),
        real,
        url,
        tiers,
        prompt,
    };
    let resolved = reask.first()?;
    let remote_name = name.map_or_else(|| target.repo.clone(), str::to_owned);
    let session = build_session(&target, &reask, resolved, &remote_name)?;
    Ok(Some(Connected {
        target,
        session,
        reask,
    }))
}

// ------------------------------------------------------------------------------ ServerRemote

/// How the next `update_refs` moves refs.
#[derive(Clone, Debug, Default)]
pub struct PushIntent {
    /// `--force`, a lease or a `+` refspec: the expectation alone decides.
    pub force: bool,
    /// `--land` or `-o land` (SPEC §20.5).
    pub land: bool,
    /// `--no-content-merge`, LAND only.
    pub no_content_merge: bool,
    /// `-o <option>` values other than `land`, passed to the server's hooks (AGit topics).
    pub options: Vec<String>,
}

/// What the server answered to an `UpdateRefs` that carried options or LAND.
#[derive(Clone, Debug, Default)]
pub struct UpdateOutcome {
    /// `(ref, old, new)` per updated ref.
    pub results: Vec<(String, Option<RefTarget>, Option<RefTarget>)>,
    /// Changesets the land rewrote: `(old, new)`.
    pub rewritten: Vec<(ChangesetId, ChangesetId)>,
    /// The server's message (the PR URL of an AGit push).
    pub message: String,
}

/// The engine's [`Remote`] for an asc server.
pub struct ServerRemote {
    name: String,
    session: Mutex<Arc<Session>>,
    target: Target,
    reask: ReAsk,
    intent: Mutex<PushIntent>,
    outcome: Mutex<Option<UpdateOutcome>>,
}

fn unauthenticated(e: &sync::Error) -> bool {
    match e {
        sync::Error::Graph(graph::Error::Remote(m)) | sync::Error::Remote(m) => {
            m.contains("Unauthenticated")
        }
        _ => false,
    }
}

/// A failed `GetObjects` as the engine's error (an unreachable server is the §6.4 offline error).
fn fetch_error(remote: &str, e: CallError) -> sync::Error {
    if e.is_unavailable() {
        return sync::Error::Graph(graph::Error::Offline {
            remote: remote.to_owned(),
            operation: "fetch",
            rpc: "GetObjects",
        });
    }
    too_old_error(sync::Error::Graph(graph::Error::Remote(e.to_string())))
}

fn too_old_error(e: sync::Error) -> sync::Error {
    let text = e.to_string();
    if text.contains("CLIENT_TOO_OLD") {
        sync::Error::Remote(format!(
            "this client ({}) is older than the server accepts; upgrade asc",
            env!("CARGO_PKG_VERSION")
        ))
    } else {
        e
    }
}

impl ServerRemote {
    pub fn new(name: &str, connected: Connected) -> Self {
        Self {
            name: name.to_owned(),
            session: Mutex::new(Arc::new(connected.session)),
            target: connected.target,
            reask: connected.reask,
            intent: Mutex::new(PushIntent::default()),
            outcome: Mutex::new(None),
        }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn session(&self) -> Arc<Session> {
        match self.session.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// The client of the current session.
    pub fn client(&self) -> RpcClient {
        self.session().client.clone()
    }

    /// Sets how the next `update_refs` moves refs.
    pub fn arm(&self, intent: PushIntent) {
        if let Ok(mut g) = self.intent.lock() {
            *g = intent;
        }
    }

    /// The server's answer to the last armed `update_refs`.
    pub fn take_outcome(&self) -> Option<UpdateOutcome> {
        self.outcome.lock().ok().and_then(|mut g| g.take())
    }

    /// Tells the helpers how the server answered the credential of `session`.
    fn report<T>(&self, session: &Session, result: &sync::Result<T>) {
        match result {
            Ok(_) => credential::report_result(&session.resolved, true),
            Err(e) if unauthenticated(e) => credential::report_result(&session.resolved, false),
            Err(_) => {}
        }
    }

    /// Asks for a credential again after a 401 and swaps in a client that uses it.
    pub fn reauth(&self, old: &Session) -> sync::Result<()> {
        credential::report_result(&old.resolved, false);
        let fail = |text: String| {
            sync::Error::Remote(text.strip_prefix("fatal: ").unwrap_or(&text).to_owned())
        };
        let next = self
            .reask
            .again(&old.resolved)
            .map_err(|e| sync::Error::Remote(e.to_string()))?;
        if next.is_anonymous() {
            return Err(fail(self.reask.failure(!old.resolved.is_anonymous())));
        }
        let session = build_session(&self.target, &self.reask, next, &self.name)
            .map_err(|e| sync::Error::Remote(e.to_string()))?;
        if let Ok(mut g) = self.session.lock() {
            *g = Arc::new(session);
        }
        Ok(())
    }

    fn map_update(&self, e: CallError, session: &Session) -> sync::Error {
        match e {
            CallError::Rpc(RpcError::RefConflict(info)) => {
                let name = info
                    .paths
                    .first()
                    .and_then(|p| asc_core::path::RefName::new_onelevel(p.as_bytes()).ok());
                let Some(name) = name else {
                    return sync::Error::Remote("ref conflict without a name".into());
                };
                let actual = if info.actual.is_empty() {
                    None
                } else {
                    match RefTarget::from_bytes(&info.actual) {
                        Ok(t) => Some(t),
                        Err(e) => return sync::Error::Remote(e.to_string()),
                    }
                };
                sync::Error::Refs(RefError::Conflict(RefConflict { name, actual }))
            }
            CallError::Rpc(RpcError::NotFastForward(info)) => {
                match info
                    .paths
                    .first()
                    .and_then(|p| asc_core::path::RefName::new_onelevel(p.as_bytes()).ok())
                {
                    Some(name) => sync::Error::Rejected {
                        name,
                        reason: "non-fast-forward",
                    },
                    None => sync::Error::Remote("rejected update without a name".into()),
                }
            }
            CallError::Rpc(RpcError::MirrorMode(_)) => sync::Error::Remote(
                "this repository is in mirror mode; push to the forge with git-remote mode".into(),
            ),
            CallError::Rpc(
                e @ (RpcError::LandConflict(_)
                | RpcError::LandNeedsClientRebase(_)
                | RpcError::LandNeedsContentMerge(_)
                | RpcError::HookRejected(_)),
            ) => sync::Error::Remote(land_message(&e)),
            other => {
                let e = sync::Error::Graph(graph::Error::Remote(other.to_string()));
                let _ = session;
                too_old_error(e)
            }
        }
    }

    /// `UpdateRefs` with options or in LAND mode, through the raw wire.
    async fn update_armed(
        &self,
        session: &Session,
        updates: &[RefUpdate],
        intent: &PushIntent,
    ) -> sync::Result<UpdateOutcome> {
        let mut wire_updates = Vec::with_capacity(updates.len());
        for u in updates {
            let name = String::from_utf8(u.name.as_bytes().to_vec())
                .map_err(|_| sync::Error::Remote("ref name is not UTF-8".into()))?;
            wire_updates.push(wire::RefUpdate {
                name,
                expect: Some(match &u.expect {
                    asc_store::refs::Expect::Any => wire::ref_update::Expect::Any(true),
                    asc_store::refs::Expect::Absent => wire::ref_update::Expect::Absent(true),
                    asc_store::refs::Expect::Value(t) => {
                        wire::ref_update::Expect::Value(wire::RefTarget::from(t))
                    }
                }),
                new: u.new.as_ref().map(wire::RefTarget::from),
                delete: u.new.is_none(),
            });
        }
        let mode = if intent.land {
            wire::UpdateMode::Land
        } else if intent.force {
            wire::UpdateMode::Force
        } else {
            wire::UpdateMode::FastForward
        };
        let req = wire::UpdateRefsRequest {
            repo: session.wire.repo_ref(),
            updates: wire_updates,
            mode: mode as i32,
            push_options: intent.options.clone(),
            no_content_merge: intent.no_content_merge,
            ..wire::UpdateRefsRequest::default()
        };
        let response = session
            .wire
            .stub()
            .update_refs(session.wire.request(req, Some(Uuid::now_v7())))
            .await
            .map_err(|s| self.map_update(CallError::from(s), session))?
            .into_inner();
        let mut outcome = UpdateOutcome {
            message: response
                .message
                .chars()
                .filter(|c| !c.is_control() || *c == '\n')
                .take(4096)
                .collect(),
            ..UpdateOutcome::default()
        };
        for r in response.results.iter().take(updates.len().max(1) * 4) {
            let conv = |t: &Option<wire::RefTarget>| -> sync::Result<Option<RefTarget>> {
                t.as_ref()
                    .map(RefTarget::try_from)
                    .transpose()
                    .map_err(|e| sync::Error::Remote(e.to_string()))
            };
            outcome
                .results
                .push((r.name.clone(), conv(&r.old)?, conv(&r.new)?));
        }
        for rw in response.rewritten.iter().take(100_000) {
            let old =
                ChangesetId::from_slice(&rw.old).map_err(|e| sync::Error::Remote(e.to_string()))?;
            let new =
                ChangesetId::from_slice(&rw.new).map_err(|e| sync::Error::Remote(e.to_string()))?;
            outcome.rewritten.push((old, new));
        }
        Ok(outcome)
    }
}

/// The user-facing text of a land or hook refusal.
pub fn land_message(e: &RpcError) -> String {
    let paths = &e.info().paths;
    let list = if paths.is_empty() {
        String::new()
    } else {
        let shown: Vec<String> = paths
            .iter()
            .take(10)
            .map(|p| p.chars().filter(|c| !c.is_control()).collect())
            .collect();
        format!(": {}", shown.join(", "))
    };
    match e {
        RpcError::LandConflict(_) => {
            format!("land conflict{list}; rebase onto the target and push again")
        }
        RpcError::LandNeedsClientRebase(_) => {
            format!("the server cannot rebase this change{list}; rebase it locally and push again")
        }
        RpcError::LandNeedsContentMerge(_) => {
            format!("landing needs a content merge{list}; merge locally or drop --no-content-merge")
        }
        RpcError::HookRejected(_) => format!("rejected by a server hook{list}"),
        other => other.to_string(),
    }
}

#[async_trait]
impl Remote for ServerRemote {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list_refs(&self) -> sync::Result<Vec<RefEntry>> {
        let s = self.session();
        let first = s.client.list_refs().await.map_err(too_old_error);
        self.report(&s, &first);
        match first {
            Err(e) if unauthenticated(&e) => {
                self.reauth(&s)?;
                let s = self.session();
                let again = s.client.list_refs().await.map_err(too_old_error);
                self.report(&s, &again);
                again
            }
            other => other,
        }
    }

    async fn default_branch(&self) -> sync::Result<Option<String>> {
        let s = self.session();
        s.client.default_branch().await.map_err(too_old_error)
    }

    async fn has_many(&self, keys: &[ObjKey]) -> sync::Result<Vec<bool>> {
        let s = self.session();
        let r = s.client.has_many(keys).await.map_err(too_old_error);
        self.report(&s, &r);
        r
    }

    fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, sync::Result<(ObjKey, Bytes)>> {
        let s = self.session();
        let name = self.name.clone();
        s.client
            .fetch(keys)
            .map(move |item| match item {
                Ok(found) => {
                    credential::report_result(&s.resolved, true);
                    Ok(found)
                }
                Err(e) => {
                    let e = fetch_error(&name, e);
                    if unauthenticated(&e) {
                        credential::report_result(&s.resolved, false);
                    }
                    Err(e)
                }
            })
            .boxed()
    }

    async fn put_many(&self, objects: Vec<(ObjKey, Bytes)>) -> sync::Result<()> {
        let s = self.session();
        let r = s.client.put_many(objects).await.map_err(too_old_error);
        self.report(&s, &r);
        r
    }

    async fn update_refs(&self, updates: &[RefUpdate]) -> sync::Result<()> {
        let s = self.session();
        let intent = self.intent.lock().map(|g| g.clone()).unwrap_or_default();
        let result = if intent.land || !intent.options.is_empty() {
            self.update_armed(&s, updates, &intent).await.map(|o| {
                if let Ok(mut slot) = self.outcome.lock() {
                    *slot = Some(o);
                }
            })
        } else {
            let mode = if intent.force {
                WireMode::Force
            } else {
                WireMode::FastForward
            };
            s.client
                .update_refs_with(updates, mode, Uuid::now_v7())
                .await
                .map_err(|e| self.map_update(e, &s))
        };
        self.report(&s, &result);
        result
    }

    async fn ingest(&self, tips: &[ChangesetId]) -> sync::Result<()> {
        let s = self.session();
        let r = s.client.ingest(tips).await.map_err(too_old_error);
        self.report(&s, &r);
        r
    }

    fn history(&self) -> Option<Arc<dyn HistoryApi>> {
        Some(Arc::new(self.client()))
    }
}

/// A shared [`ServerRemote`] as the engine's `Box<dyn Remote>`.
pub struct Shared(pub Arc<ServerRemote>);

#[async_trait]
impl Remote for Shared {
    fn name(&self) -> &str {
        self.0.name()
    }
    async fn list_refs(&self) -> sync::Result<Vec<RefEntry>> {
        self.0.list_refs().await
    }
    async fn default_branch(&self) -> sync::Result<Option<String>> {
        self.0.default_branch().await
    }
    async fn has_many(&self, keys: &[ObjKey]) -> sync::Result<Vec<bool>> {
        self.0.has_many(keys).await
    }
    fn get_many(&self, keys: Vec<ObjKey>) -> BoxStream<'_, sync::Result<(ObjKey, Bytes)>> {
        self.0.get_many(keys)
    }
    async fn put_many(&self, objects: Vec<(ObjKey, Bytes)>) -> sync::Result<()> {
        self.0.put_many(objects).await
    }
    async fn update_refs(&self, updates: &[RefUpdate]) -> sync::Result<()> {
        self.0.update_refs(updates).await
    }
    async fn ingest(&self, tips: &[ChangesetId]) -> sync::Result<()> {
        self.0.ingest(tips).await
    }
    fn history(&self) -> Option<Arc<dyn HistoryApi>> {
        self.0.history()
    }
}

/// A connected server as an [`Opened`](super::Opened) remote.
pub fn opened(
    name: String,
    stored: String,
    configured: bool,
    spec: RemoteSpec,
    connected: Connected,
) -> super::Opened {
    let remote = Arc::new(ServerRemote::new(&name, connected));
    super::Opened {
        name,
        configured,
        url: stored,
        spec,
        remote: Box::new(Shared(remote.clone())),
        server: Some(remote),
    }
}

// ------------------------------------------------------------------------------ selectors

/// What the selectors need and a `Ctx` knows: the config and `state.db` of this invocation.
pub(super) struct Bound {
    file: Config,
    db: StateDb,
}

fn bound_slot() -> &'static Mutex<Option<Arc<Bound>>> {
    static BOUND: OnceLock<Mutex<Option<Arc<Bound>>>> = OnceLock::new();
    BOUND.get_or_init(Default::default)
}

/// Remembers this invocation's config and `state.db` for the lazily connecting selectors.
pub fn bind(file: &Config, db: &StateDb) {
    if let Ok(mut g) = bound_slot().lock() {
        *g = Some(Arc::new(Bound {
            file: file.clone(),
            db: db.clone(),
        }));
    }
}

pub(super) fn bound() -> Option<Arc<Bound>> {
    bound_slot().lock().ok().and_then(|g| g.clone())
}

/// A server connection made on first use, because the selectors are synchronous and a
/// connection needs discovery (network) and the credential resolver.
pub(super) struct Lazy {
    bound: Arc<Bound>,
    url: String,
    cell: tokio::sync::OnceCell<Result<RpcClient, String>>,
}

impl Lazy {
    pub(super) fn new(bound: Arc<Bound>, url: String) -> Self {
        Self {
            bound,
            url,
            cell: tokio::sync::OnceCell::new(),
        }
    }

    pub(super) async fn client(&self) -> Result<&RpcClient, String> {
        let made = self
            .cell
            .get_or_init(|| async {
                match connect(
                    &self.bound.file,
                    &self.bound.db,
                    None,
                    None,
                    &self.url,
                    Prompt::Never,
                )
                .await
                {
                    Ok(Some(c)) => Ok(c.session.client.clone()),
                    Ok(None) => Err("the remote is not an asc server".to_owned()),
                    Err(e) => Err(e.to_string()),
                }
            })
            .await;
        made.as_ref().map_err(Clone::clone)
    }

    async fn graph(&self) -> graph::Result<&RpcClient> {
        self.client().await.map_err(graph::Error::Remote)
    }
}

#[async_trait]
impl HistoryApi for Lazy {
    async fn log(&self, q: &LogQuery, page: Option<&PageToken>) -> graph::Result<LogEntries> {
        HistoryApi::log(self.graph().await?, q, page).await
    }
    async fn merge_base(
        &self,
        a: ChangesetId,
        others: &[ChangesetId],
        all: bool,
    ) -> graph::Result<Vec<ChangesetId>> {
        HistoryApi::merge_base(self.graph().await?, a, others, all).await
    }
    async fn is_ancestor(&self, a: ChangesetId, b: ChangesetId) -> graph::Result<bool> {
        HistoryApi::is_ancestor(self.graph().await?, a, b).await
    }
    async fn diff(
        &self,
        a: ChangesetId,
        b: ChangesetId,
        paths: &[String],
    ) -> graph::Result<Vec<(RepoPath, Change)>> {
        HistoryApi::diff(self.graph().await?, a, b, paths).await
    }
    async fn resolve(&self, specs: &[String]) -> graph::Result<Vec<Option<ChangesetId>>> {
        HistoryApi::resolve(self.graph().await?, specs).await
    }
    async fn git_map(&self, asc: &[ChangesetId], git: &[GitOid]) -> graph::Result<GitMapping> {
        HistoryApi::git_map(self.graph().await?, asc, git).await
    }
}

#[async_trait]
impl BlameApi for Lazy {
    async fn blame(
        &self,
        rev: ChangesetId,
        path: &RepoPath,
        lines: Option<Range<u32>>,
    ) -> blame::Result<Blame> {
        let client = self
            .client()
            .await
            .map_err(|e| blame::Error::Derive(asc_engine::derive::Error::Store(store_error(e))))?;
        BlameApi::blame(client, rev, path, lines).await
    }
}

fn store_error(message: impl Into<String>) -> objects::Error {
    objects::Error::Store(object_store::Error::Generic {
        store: "asc-server",
        source: Box::new(std::io::Error::other(message.into())),
    })
}

/// The remote object tier of a server repository, connected on first use.
struct LazyObjects(Lazy);

impl LazyObjects {
    async fn objects(&self) -> objects::Result<asc_rpc::client::RemoteObjects> {
        self.0
            .client()
            .await
            .map(RpcClient::objects)
            .map_err(store_error)
    }
}

#[async_trait]
impl Objects for LazyObjects {
    async fn get(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        self.objects().await?.get(key).await
    }
    async fn get_stored(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        self.objects().await?.get_stored(key).await
    }
    async fn get_range(&self, key: &ObjKey, r: Range<u64>) -> objects::Result<Option<Bytes>> {
        self.objects().await?.get_range(key, r).await
    }
    async fn put(&self, key: &ObjKey, stored: Bytes) -> objects::Result<()> {
        self.objects().await?.put(key, stored).await
    }
    async fn has_many(&self, keys: &[ObjKey]) -> objects::Result<Vec<bool>> {
        self.objects().await?.has_many(keys).await
    }
    async fn delete(&self, key: &ObjKey) -> objects::Result<()> {
        self.objects().await?.delete(key).await
    }
    fn list(
        &self,
        _prefix: KeyPrefix,
    ) -> BoxStream<'_, objects::Result<(ObjKey, std::time::SystemTime)>> {
        futures::stream::once(async { Err(store_error("list is not available over RPC")) }).boxed()
    }
}

struct ServerObjects;

impl RemoteObjectsSelector for ServerObjects {
    fn objects(&self, spec: &ext::RemoteSpec) -> Option<Arc<dyn Objects>> {
        if spec.kind != RemoteKind::Server {
            return None;
        }
        let url = spec.url.clone()?;
        Some(Arc::new(LazyObjects(Lazy::new(bound()?, url))))
    }
}

struct ServerHistoryApis;

impl HistorySelector for ServerHistoryApis {
    /// The server answers history and blame when `asc.serverHistory` is `always`. With the
    /// default `auto` the local engine answers inside the history window and continues past
    /// its boundary through the server (U31); a server-first walk would not see local work
    /// that has not been pushed.
    fn select(&self, remote: &ext::RemoteSpec) -> Option<(Arc<dyn HistoryApi>, Arc<dyn BlameApi>)> {
        if remote.kind != RemoteKind::Server {
            return None;
        }
        let bound = bound()?;
        if cfg_get(&bound.file, "asc.serverHistory").as_deref() != Some("always") {
            return None;
        }
        let lazy = Arc::new(Lazy::new(bound, remote.url.clone()?));
        Some((lazy.clone(), lazy))
    }
}

/// Registers the server object source and the history selector (called once from `main`).
pub fn register() {
    ext::register_remote_objects(RemoteKind::Server, Arc::new(ServerObjects));
    let _ = ext::set_history_selector(Box::new(ServerHistoryApis));
}

// ------------------------------------------------------------------------------ helpers

/// The tier URLs that are the same origin as `server`, for tests and `asc auth status`.
pub fn same_origin_tiers(server: &str, tiers: &[String]) -> Vec<String> {
    let Ok(s) = Url::parse(server) else {
        return Vec::new();
    };
    let seen: HashSet<&String> = tiers.iter().collect();
    seen.into_iter()
        .filter(|t| Url::parse(t).is_ok_and(|u| credential::same_origin(&u, &s)))
        .cloned()
        .collect()
}

// ------------------------------------------------------------------------------ commands

/// Opens the server named `arg` (a configured remote or a URL; default `origin`) for a command
/// that works without pushing or fetching: `asc api`, `diff --remote`. It runs outside a
/// workspace too, so it opens `state.db` itself when there is none.
pub async fn open_for(
    ctx: &crate::cli::common::Ctx,
    arg: Option<&str>,
) -> CliResult<Arc<ServerRemote>> {
    let word = arg.unwrap_or("origin");
    let configured = cfg_get(ctx.config.file(), &format!("remote.{word}.url"));
    let (name, stored) = match configured {
        Some(url) => (Some(word.to_owned()), url),
        None if arg.is_some() => (None, word.to_owned()),
        None => {
            return Err(CliError::Fatal(
                "asc: no remote given and 'origin' is not configured; pass --remote <name|url>"
                    .into(),
            ));
        }
    };
    let db = match ctx.ws_opt() {
        Some(ws) => ws.db.clone(),
        None => {
            let home = asc_state::AscHome::locate()?;
            StateDb::open(&home).await?
        }
    };
    let real = ctx.real_git().ok().cloned();
    let connected = connect(
        ctx.config.file(),
        &db,
        real,
        name.as_deref(),
        &stored,
        credential::terminal_prompt(),
    )
    .await?
    .ok_or_else(|| {
        CliError::Fatal(format!(
            "asc: '{}' is not an asc server",
            crate::cli::url_display::anonymize(&stored)
        ))
    })?;
    let label = name.unwrap_or_else(|| connected.target.repo.clone());
    Ok(Arc::new(ServerRemote::new(&label, connected)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_come_from_the_last_two_segments() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(repo_of(&u("https://h/o/r.git")).as_deref(), Some("o/r"));
        assert_eq!(repo_of(&u("https://h:7/pre/o/r")).as_deref(), Some("o/r"));
        assert_eq!(repo_of(&u("https://h/r")), None);
        assert_eq!(repo_of(&u("https://h/o/.hidden")), None);
    }

    #[tokio::test]
    async fn plain_http_is_loopback_only() {
        let opted = Policy::LOOPBACK_HTTP;
        assert!(channel_with("http://127.0.0.1:1", opted).is_ok());
        assert!(channel_with("http://localhost:1", opted).is_ok());
        assert!(channel_with("http://127.example.com", opted).is_err());
        assert!(channel_with("http://localhost.evil", opted).is_err());
        assert!(channel_with("ftp://x", opted).is_err());
        // Off by default: not even the local machine.
        let err = channel_with("http://127.0.0.1:1", Policy::STRICT).unwrap_err();
        assert!(err.to_string().contains("refusing plain http"), "{err}");
    }

    #[test]
    fn land_messages_are_bounded_and_clean() {
        let info = asc_rpc::errors::ErrorInfo {
            paths: vec!["a\u{1b}[31mb".into()],
            ..Default::default()
        };
        let m = land_message(&RpcError::LandConflict(info));
        assert!(m.starts_with("land conflict: a[31mb"));
        assert!(!m.contains('\u{1b}'));
    }
}
