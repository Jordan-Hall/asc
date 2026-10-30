//! `asc search`, `asc sym`, `asc semantic` and `asc codeintel status` (SPEC §23.4.2): the client of
//! the `CodeIntel` service. Inside a workspace the default scope is the workspace, served by the
//! daemon (server answer at `HEAD` plus the dirty-file overlay, §23.2.5); otherwise it is
//! `master` on the server. `--rev`, `--change` and `--stack` name a server scope.
//!
//! Human output follows the layout of `git grep -n` (`path:line:text`) and the scope header goes
//! to stderr. `--json` prints each response message through the asc JSON profile (§22.4.2), one
//! object per line.
//!
//! Everything the server sends is file content, so it is untrusted: [`clean`] escapes control
//! characters (terminal escape sequences), line separators and bidirectional overrides before a
//! byte reaches the terminal.

pub mod scip;
pub mod search;
pub mod semantic;
pub mod status;
pub mod sym;

use std::future::Future;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use asc_core::change::ChangeId;
use asc_rpc::ipc::{WorkspaceClient, connect};
use asc_rpc::server::{MAX_MESSAGE, wire};
use prost_reflect::{DescriptorPool, DynamicMessage};
use tonic::transport::Channel;
use tonic::{Code, Status};
use url::Url;

use super::common::{CliError, Ctx, Result};
use super::json::JsonLines;
use crate::daemon::lifecycle;
use crate::remote::server::{ServerRemote, Session};

/// Most response messages read from one stream.
const MAX_STREAM: usize = 100_000;
/// Most encoded bytes read from one stream.
const MAX_STREAM_BYTES: usize = 256 << 20;
/// The scope of a query that names none, outside a workspace.
pub const DEFAULT_REV: &str = "master";

/// The options every query command shares.
#[derive(clap::Args, Debug, Default)]
pub struct ScopeOpts {
    /// Search this revision.
    #[arg(long, conflicts_with_all = ["change", "stack"])]
    pub rev: Option<String>,
    /// Search the head of this change (a full id, or a revision of one inside a workspace).
    #[arg(long, conflicts_with_all = ["rev", "stack"])]
    pub change: Option<String>,
    /// Search the top of the current stack.
    #[arg(long, conflicts_with_all = ["rev", "change"])]
    pub stack: bool,
    /// With `--stack`: only the files the stack changes.
    #[arg(long = "diff-only", requires = "stack")]
    pub diff_only: bool,
    /// Restrict to a language.
    #[arg(long = "lang", value_delimiter = ',')]
    pub langs: Vec<String>,
    /// The server to ask (a remote name or URL); default `origin`.
    #[arg(long)]
    pub remote: Option<String>,
    /// Print the responses in the asc JSON profile, one object per line.
    #[arg(long)]
    pub json: bool,
}

/// Escapes what must not reach a terminal: C0 and C1 controls (except tab), DEL, line and
/// paragraph separators, bidirectional controls and the byte order mark. Invalid UTF-8 becomes
/// U+FFFD.
pub fn clean(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let bad = (c.is_control() && c != '\t')
            || matches!(
                c,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{2028}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{feff}'
            );
        if bad {
            let n = u32::from(c);
            if n < 0x100 {
                out.push_str(&format!("\\x{n:02x}"));
            } else {
                out.push_str(&format!("\\u{{{n:x}}}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn clean_str(s: &str) -> String {
    clean(s.as_bytes())
}

fn fatal(text: impl std::fmt::Display) -> CliError {
    CliError::Fatal(format!("asc: {text}"))
}

fn status_err(s: &Status) -> CliError {
    let text = asc_rpc::server::status_rpc_error(s).map_or_else(
        || {
            let hint = if matches!(s.code(), Code::Unavailable | Code::DeadlineExceeded) {
                " (the server cannot be reached)"
            } else {
                ""
            };
            format!(
                "{:?}: {}{hint}",
                s.code(),
                clean_str(&s.message().chars().take(512).collect::<String>())
            )
        },
        |e| clean_str(&e.to_string()),
    );
    CliError::Fatal(format!("asc: {text}"))
}

/// Where a query is answered.
pub enum Backend {
    Server(Arc<ServerRemote>),
    Daemon {
        client: WorkspaceClient<Channel>,
        ws_id: String,
    },
}

/// The wire scope plus the backend that answers it.
pub struct Plan {
    pub backend: Backend,
    pub scope: wire::Scope,
}

fn repo_ref(name: &str) -> wire::RepoRef {
    wire::RepoRef {
        name: name.to_owned(),
        id_mode: wire::IdMode::Asc as i32,
    }
}

/// The repository the workspace's `origin` names.
fn origin_repo(ctx: &Ctx) -> Option<String> {
    let url = crate::remote::server::cfg_get(ctx.config.file(), "remote.origin.url")?;
    let url = Url::parse(url.strip_prefix("asc+").unwrap_or(&url)).ok()?;
    crate::remote::server::repo_of(&url)
}

async fn change_id(ctx: &Ctx, word: Option<&str>) -> Result<ChangeId> {
    if let Some(w) = word
        && let Ok(id) = w.parse::<ChangeId>()
    {
        return Ok(id);
    }
    if ctx.ws_opt().is_none() {
        return Err(fatal(
            "a change is named by its full 32-letter id outside a workspace",
        ));
    }
    use asc_engine::change::ChangeRows as _;
    let rev = word.unwrap_or("HEAD");
    let cs = crate::cli::restore::resolve_commit(ctx, rev)
        .await?
        .ok_or_else(|| fatal(format!("bad revision '{}'", clean_str(rev))))?;
    let rows = crate::stack::change_rows(ctx)?;
    let row = rows
        .get(cs)
        .await
        .map_err(fatal)?
        .ok_or_else(|| fatal(format!("'{}' is not a change", clean_str(rev))))?;
    Ok(row.change)
}

fn scope_of(repo: &str, at: wire::scope::At, o: &ScopeOpts, paths: Vec<String>) -> wire::Scope {
    wire::Scope {
        repo: Some(repo_ref(repo)),
        at: Some(at),
        stack_diff_only: o.diff_only,
        paths,
        exclude_paths: Vec::new(),
        langs: o.langs.clone(),
        include_excluded: false,
    }
}

async fn daemon_backend(ctx: &Ctx) -> Option<(Backend, String)> {
    let ws = ctx.ws_opt()?;
    if lifecycle::disabled(&|k: &str| std::env::var_os(k)) {
        return None;
    }
    let repo = origin_repo(ctx)?;
    let (cfg, proc) = super::daemon::system().ok()?;
    lifecycle::ensure_running(&cfg, &proc).await.ok()?;
    let ws_id = ws.workspace.ws.id.to_string();
    let mut client = WorkspaceClient::new(connect(&cfg.endpoint));
    client
        .mount(wire::MountRequest {
            ws_id: ws_id.clone(),
        })
        .await
        .ok()?;
    Some((Backend::Daemon { client, ws_id }, repo))
}

/// Chooses the backend and builds the scope for `o`; `paths` are the pathspecs.
pub async fn plan(ctx: &Ctx, o: &ScopeOpts, paths: Vec<String>) -> Result<Plan> {
    let explicit = o.rev.is_some() || o.change.is_some() || o.stack;
    if !explicit
        && o.remote.is_none()
        && let Some((backend, repo)) = daemon_backend(ctx).await
    {
        let scope = scope_of(&repo, wire::scope::At::Workspace(true), o, paths);
        return Ok(Plan { backend, scope });
    }
    if !explicit && o.remote.is_none() && ctx.ws_opt().is_some() {
        eprintln!(
            "asc: the daemon is not available; using {DEFAULT_REV} on the server (uncommitted changes are not searched)"
        );
    }
    let at = if let Some(r) = &o.rev {
        wire::scope::At::Rev(r.clone())
    } else if let Some(c) = &o.change {
        let id = change_id(ctx, Some(c)).await?;
        wire::scope::At::Change(id.as_bytes().to_vec().into())
    } else if o.stack {
        if ctx.ws_opt().is_none() {
            return Err(fatal("--stack needs a workspace"));
        }
        let id = change_id(ctx, None).await?;
        wire::scope::At::StackTop(id.as_bytes().to_vec().into())
    } else {
        wire::scope::At::Rev(DEFAULT_REV.to_owned())
    };
    let server = crate::remote::server::open_for(ctx, o.remote.as_deref()).await?;
    let repo = server.target().repo.clone();
    Ok(Plan {
        backend: Backend::Server(server),
        scope: scope_of(&repo, at, o, paths),
    })
}

async fn call_server<T, F, Fut>(server: &ServerRemote, mut f: F) -> Result<T>
where
    F: FnMut(Arc<Session>) -> Fut,
    Fut: Future<Output = std::result::Result<T, Status>>,
{
    let mut attempt = 0;
    loop {
        let session = server.session();
        match f(session.clone()).await {
            Ok(v) => {
                crate::credential::report_result(&session.resolved, true);
                return Ok(v);
            }
            Err(s) if s.code() == Code::Unauthenticated && attempt == 0 => {
                attempt += 1;
                server
                    .reauth(&session)
                    .await
                    .map_err(|e| CliError::Fatal(format!("asc: {e}")))?;
            }
            Err(s) => {
                if s.code() == Code::Unauthenticated {
                    crate::credential::report_result(&session.resolved, false);
                }
                return Err(status_err(&s));
            }
        }
    }
}

fn client(s: &Session) -> asc_rpc::services::code_intel_client::CodeIntelClient<Channel> {
    asc_rpc::services::code_intel_client::CodeIntelClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

impl Backend {
    pub async fn search(&self, q: wire::SearchRequest) -> Result<Vec<wire::SearchResponse>> {
        match self {
            Self::Server(server) => {
                let op = uuid::Uuid::now_v7();
                call_server(server, |s| {
                    let q = q.clone();
                    async move {
                        let mut stream = client(&s)
                            .search(s.wire.request(q, Some(op)))
                            .await?
                            .into_inner();
                        drain(&mut stream).await
                    }
                })
                .await
            }
            Self::Daemon { client, ws_id } => {
                let mut c = client.clone();
                let mut stream = c
                    .search(wire::WsSearchRequest {
                        ws_id: ws_id.clone(),
                        query: Some(q),
                    })
                    .await
                    .map_err(|s| status_err(&s))?
                    .into_inner();
                drain(&mut stream).await.map_err(|s| status_err(&s))
            }
        }
    }

    pub async fn symbols(&self, q: wire::SymbolsRequest) -> Result<wire::SymbolsResponse> {
        match self {
            Self::Server(server) => {
                let op = uuid::Uuid::now_v7();
                call_server(server, |s| {
                    let q = q.clone();
                    async move {
                        Ok(client(&s)
                            .symbols(s.wire.request(q, Some(op)))
                            .await?
                            .into_inner())
                    }
                })
                .await
            }
            Self::Daemon { client, ws_id } => client
                .clone()
                .symbols(wire::WsSymbolsRequest {
                    ws_id: ws_id.clone(),
                    query: Some(q),
                })
                .await
                .map(tonic::Response::into_inner)
                .map_err(|s| status_err(&s)),
        }
    }

    pub async fn semantic(&self, q: wire::SemanticRequest) -> Result<wire::SemanticResponse> {
        match self {
            Self::Server(server) => {
                let op = uuid::Uuid::now_v7();
                call_server(server, |s| {
                    let q = q.clone();
                    async move {
                        Ok(client(&s)
                            .semantic(s.wire.request(q, Some(op)))
                            .await?
                            .into_inner())
                    }
                })
                .await
            }
            Self::Daemon { client, ws_id } => client
                .clone()
                .semantic(wire::WsSemanticRequest {
                    ws_id: ws_id.clone(),
                    query: Some(q),
                })
                .await
                .map(tonic::Response::into_inner)
                .map_err(|s| status_err(&s)),
        }
    }

    pub async fn index_status(
        &self,
        q: wire::IndexStatusRequest,
    ) -> Result<wire::IndexStatusResponse> {
        match self {
            Self::Server(server) => {
                let op = uuid::Uuid::now_v7();
                call_server(server, |s| {
                    let q = q.clone();
                    async move {
                        Ok(client(&s)
                            .index_status(s.wire.request(q, Some(op)))
                            .await?
                            .into_inner())
                    }
                })
                .await
            }
            Self::Daemon { client, ws_id } => client
                .clone()
                .index_status(wire::WsIndexStatusRequest {
                    ws_id: ws_id.clone(),
                    query: Some(q),
                })
                .await
                .map(tonic::Response::into_inner)
                .map_err(|s| status_err(&s)),
        }
    }
}

/// Reads a response stream, bounded in messages and bytes.
async fn drain(
    stream: &mut tonic::Streaming<wire::SearchResponse>,
) -> std::result::Result<Vec<wire::SearchResponse>, Status> {
    use prost_reflect::prost::Message as _;
    let mut out = Vec::new();
    let mut bytes = 0usize;
    while let Some(m) = stream.message().await? {
        bytes = bytes.saturating_add(m.encoded_len());
        if out.len() >= MAX_STREAM || bytes > MAX_STREAM_BYTES {
            return Err(Status::resource_exhausted("the answer is too large"));
        }
        out.push(m);
    }
    Ok(out)
}

/// The scope line on stderr: what was searched and how much of it the index covers.
pub fn header(verb: &str, req: &wire::Scope, got: Option<&wire::ResolvedScope>) -> String {
    let at = match &req.at {
        Some(wire::scope::At::Rev(r)) => format!("rev {}", clean_str(r)),
        Some(wire::scope::At::Change(_)) => "change".to_owned(),
        Some(wire::scope::At::StackTop(_)) if req.stack_diff_only => "stack (diff only)".to_owned(),
        Some(wire::scope::At::StackTop(_)) => "stack".to_owned(),
        Some(wire::scope::At::Workspace(_)) => "workspace".to_owned(),
        None => "?".to_owned(),
    };
    let repo = req
        .repo
        .as_ref()
        .map_or_else(String::new, |r| clean_str(&r.name));
    let mut line = format!("asc: {verb}: {repo} @ {at}");
    if let Some(r) = got {
        if !r.changeset.is_empty() {
            let hex: String = r
                .changeset
                .iter()
                .take(6)
                .map(|b| format!("{b:02x}"))
                .collect();
            line.push_str(&format!(" ({hex})"));
        }
        line.push_str(&format!(", coverage {:.0}%", r.coverage * 100.0));
        if r.coverage < 1.0 {
            line.push_str(" (the index is incomplete; results may be missing)");
        }
    }
    line
}

/// One message as a line of the asc JSON profile.
pub fn emit_json<M: prost_reflect::prost::Message, W: Write>(
    lines: &mut JsonLines<W>,
    type_name: &str,
    msg: &M,
) -> Result<()> {
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).map_err(fatal)?;
    let desc = pool
        .get_message_by_name(type_name)
        .ok_or_else(|| fatal(format!("missing message {type_name}")))?;
    let mut dynamic = DynamicMessage::new(desc);
    dynamic.transcode_from(msg).map_err(fatal)?;
    lines.emit(&dynamic).map_err(fatal)
}

/// Runs `body` over a locked stdout; a closed pipe is the reader's choice, not an error.
pub fn with_stdout(body: impl FnOnce(&mut dyn Write) -> Result<ExitCode>) -> Result<ExitCode> {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let r = body(&mut out);
    match (r, out.flush()) {
        (Ok(code), Ok(())) => Ok(code),
        (Ok(code), Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(code),
        (Ok(_), Err(e)) => Err(CliError::Io(e)),
        (Err(CliError::Io(e)), _) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            Ok(ExitCode::SUCCESS)
        }
        (Err(e), _) => Err(e),
    }
}

/// Exit status of a query: 1 when it found nothing, as `git grep` does.
pub fn exit(found: bool) -> ExitCode {
    if found {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

pub fn notice_truncated(truncated: bool) {
    if truncated {
        eprintln!("asc: the answer was cut at the result budget; narrow the query to see the rest");
    }
}

/// Writes a JSON-profile line writer over `w`.
pub fn lines(w: &mut dyn Write) -> JsonLines<&mut dyn Write> {
    JsonLines::new(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_escapes_are_neutralised() {
        let s = clean(b"a\x1b[31mred\x07\x08\r\nx\ty\x7f\xc2\x9b");
        assert!(!s.contains('\x1b') && !s.contains('\x07') && !s.contains('\r'));
        assert!(s.contains("\\x1b[31m") && s.contains('\t') && s.contains("\\x9b"));
        assert_eq!(
            clean("a\u{202e}b\u{2028}".as_bytes()),
            "a\\u{202e}b\\u{2028}"
        );
        assert_eq!(clean(b"\xff"), "\u{fffd}");
    }
}
