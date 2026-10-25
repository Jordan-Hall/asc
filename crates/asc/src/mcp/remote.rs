//! `asc mcp --remote <url>`: stdio towards the agent host, Streamable HTTP towards an asc server
//! (SPEC 22.2, 22.7.3, M-D12; unit M31).
//!
//! A thin proxy with no workspace tools. Each JSON-RPC line on standard input becomes one `POST`
//! to the server's `/mcp`; each reply (a JSON body, or the `data:` events of an SSE stream, which
//! carry progress) becomes one line on standard output. Standard output carries protocol and
//! nothing else (the same channel guard as `asc mcp`); logs go to standard error.
//!
//! **Credentials** come from the one resolver `asc fetch` uses ([`crate::credential`], SPEC
//! 15.6.1) with [`Prompt::Never`]: `ASC_TOKEN` for this origin, `http.extraHeader`, `.netrc`, then
//! `git credential fill`. There is no login of our own. The credential is bound to the origin of
//! the given URL: redirects are never followed (a 3xx is reported to the agent as an error), so
//! it cannot reach another origin. A `401` rejects the credential to the helpers and asks them
//! once more; a second `401`, or no credential left, ends the session with
//! `run git credential approve for <host>`. The credential is approved to the helpers after the
//! first accepted request. Plain `http` is accepted for a loopback host only under the transport
//! policy's explicit opt-in.
//!
//! **Narrowing.** `--read-only`, `--toolsets` and `--tools` can only remove tools: they filter
//! the `tools/list` reply and refuse a `tools/call` of a hidden tool before it leaves the
//! process. `--max-tokens` lowers the per-call cap. The server's own scope filter still applies.
//! **Cancellation.** The server is stateless, so `notifications/cancelled` closes the in-flight
//! request's connection, which the server treats as the cancel.

use std::collections::{BTreeSet, HashMap};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asc_core::transport_policy::{self, Policy, Transport};
use asc_git::realgit::RealGit;
use asc_mcp::catalogue::{self, Toolset};
use asc_mcp::config::Flags;
use asc_mcp::schema::Surface;
use gix_config::File as Config;
use reqwest::header::{ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use url::Url;

use crate::cli::common::{CliError, Ctx, Result};
use crate::credential::{self, Prompt, Resolved, Resolver};

/// The largest line read from the agent host.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// The largest reply (a JSON body, or one SSE event) accepted from the server.
const MAX_REPLY: usize = 32 * 1024 * 1024;
/// The largest error body looked at.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Requests in flight at once.
const MAX_IN_FLIGHT: usize = 32;
/// How long in-flight requests get to finish once standard input is closed.
const DRAIN: Duration = Duration::from_secs(10);

/// The JSON-RPC code for a failure of the proxy itself.
const PROXY_ERROR: i64 = -32603;

/// The `/mcp` endpoint for `raw`: `https` (or loopback `http` under the opt-in), no userinfo,
/// query or fragment, and a path ending in `/mcp` (appended when missing).
///
/// # Errors
/// The text for the user.
pub fn endpoint(raw: &str, policy: Policy) -> std::result::Result<Url, String> {
    let mut url = Url::parse(raw).map_err(|_| "--remote is not a valid URL".to_owned())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("--remote must not carry credentials; use the git credential helper".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("--remote must not carry a query or fragment".into());
    }
    match transport_policy::classify_url(&url, policy) {
        Ok(Transport::Https | Transport::LoopbackHttp) => {}
        Ok(_) => return Err("--remote must be an https URL".into()),
        Err(e) => return Err(e.to_string()),
    }
    let path = url.path().trim_end_matches('/').to_owned();
    if !path.ends_with("/mcp") {
        url.set_path(&format!("{path}/mcp"));
    }
    Ok(url)
}

/// `host[:port]`, as `git credential` names the host.
fn host_label(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    }
}

// ------------------------------------------------------------------ credentials

/// The current credential and how it is asked for again.
struct Creds {
    file: Config,
    real: Option<RealGit>,
    url: Url,
    /// A counter that grows with every re-ask, so concurrent requests refused with the same
    /// credential ask the helpers once.
    state: Mutex<(u64, Arc<Resolved>)>,
}

impl Creds {
    fn resolver(&self) -> Resolver<'_> {
        let mut r = Resolver::new(&self.file);
        if let Some(real) = &self.real {
            r = r.with_real_git(real.clone());
        }
        r.allow_asc_token(&self.url)
    }

    fn open(file: Config, real: Option<RealGit>, url: Url) -> Result<Self> {
        let first = {
            let mut r = Resolver::new(&file);
            if let Some(real) = &real {
                r = r.with_real_git(real.clone());
            }
            r.allow_asc_token(&url)
                .resolve(&url, Prompt::Never)
                .map_err(|e| CliError::Fatal(format!("asc mcp: {e}")))?
        };
        Ok(Self {
            file,
            real,
            url,
            state: Mutex::new((0, Arc::new(first))),
        })
    }

    fn current(&self) -> (u64, Arc<Resolved>) {
        let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (g.0, g.1.clone())
    }

    /// After a `401` for generation `generation`: reject that credential and ask again, once
    /// per generation. `None` when no credential is left.
    fn reask(&self, generation: u64, www: &[String]) -> Option<(u64, Arc<Resolved>)> {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if g.0 != generation {
            // Another request already asked; use what it found.
            return (!g.1.is_anonymous()).then(|| (g.0, g.1.clone()));
        }
        let r = self.resolver();
        let next = if g.1.is_anonymous() {
            r.resolve_with_challenge(&self.url, Prompt::Never, www)
        } else {
            r.resolve_after(&self.url, Prompt::Never, &g.1, www)
        }
        .ok()?;
        if next.is_anonymous() {
            return None;
        }
        *g = (generation + 1, Arc::new(next));
        Some((g.0, g.1.clone()))
    }
}

// ------------------------------------------------------------------ narrowing

/// What the flags remove from the server's tool list.
#[derive(Debug, Default)]
struct Narrow {
    read_only: bool,
    toolsets: Option<BTreeSet<Toolset>>,
    tools: Option<BTreeSet<String>>,
}

impl Narrow {
    fn new(flags: &Flags) -> Self {
        Self {
            read_only: flags.read_only,
            toolsets: flags.toolsets.clone(),
            tools: flags.tools.clone(),
        }
    }

    fn active(&self) -> bool {
        self.read_only || self.toolsets.is_some() || self.tools.is_some()
    }

    /// Whether tool `name` may be listed and called. Without a narrowing flag the server decides.
    fn allows(&self, name: &str) -> bool {
        if !self.active() {
            return true;
        }
        let Some(def) = catalogue::find(name, Surface::Http) else {
            return false;
        };
        if def.toolset == Toolset::Write {
            if self.read_only {
                return false;
            }
        } else if self.toolsets.as_ref().is_some_and(|t| !t.contains(&def.toolset)) {
            return false;
        }
        self.tools.as_ref().is_none_or(|t| t.contains(name))
    }

    fn filter_list(&self, reply: &mut Value) {
        if !self.active() {
            return;
        }
        if let Some(tools) = reply.pointer_mut("/result/tools").and_then(Value::as_array_mut) {
            tools.retain(|t| t.get("name").and_then(Value::as_str).is_some_and(|n| self.allows(n)));
        }
    }
}

// ------------------------------------------------------------------ the proxy

struct Proxy {
    client: reqwest::Client,
    url: Url,
    host: String,
    creds: Arc<Creds>,
    narrow: Narrow,
    max_tokens: Option<u64>,
    out: mpsc::Sender<String>,
    /// A fatal message for the main loop; the first one wins.
    fatal: mpsc::UnboundedSender<String>,
}

impl Proxy {
    async fn emit(&self, v: &Value) {
        let _ = self.out.send(v.to_string()).await;
    }

    async fn error_reply(&self, id: Option<&Value>, code: i64, message: &str) {
        // A notification has no id and gets no reply.
        if let Some(id) = id.filter(|i| !i.is_null()) {
            self.emit(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
                .await;
        }
    }

    async fn fail_auth(&self, id: Option<&Value>, refused: bool) {
        let host = &self.host;
        let text = if refused {
            format!("the server at {host} refused the credential; run git credential approve for {host}")
        } else {
            format!("the server at {host} needs a credential and none was found; run git credential approve for {host}")
        };
        self.error_reply(id, -32001, &text).await;
        let _ = self.fatal.send(format!("asc mcp: {text}"));
    }

    fn post(&self, resolved: &Resolved, msg: &Value, body: String) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body);
        // Headers that gateways route and rate-limit on; the server checks them against the body.
        let method = msg.get("method").and_then(Value::as_str);
        if let Some(m) = method {
            req = req.header("Mcp-Method", m);
            let named = match m {
                "tools/call" | "prompts/get" => msg.pointer("/params/name"),
                "resources/read" => msg.pointer("/params/uri"),
                _ => None,
            };
            if let Some(n) = named.and_then(Value::as_str).filter(|n| header_safe(n)) {
                req = req.header("Mcp-Name", n);
            }
        }
        if let Some(r) = msg
            .pointer("/params/arguments/repo")
            .and_then(Value::as_str)
            .filter(|r| header_safe(r))
        {
            req = req.header("Mcp-Param-Repo", r);
        }
        resolved.apply(req, &self.url)
    }

    /// Sends one message and relays the reply. Never returns an error: failures become a
    /// JSON-RPC error for the request, and a lost credential also ends the session.
    async fn forward(&self, mut msg: Value) {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        if method.as_deref() == Some("tools/call") {
            let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
            if !self.narrow.allows(name) {
                self.error_reply(id.as_ref(), -32602, &format!("unknown tool: {name}")).await;
                return;
            }
            if let Some(cap) = self.max_tokens {
                let args = msg
                    .pointer_mut("/params")
                    .and_then(Value::as_object_mut)
                    .map(|p| p.entry("arguments").or_insert_with(|| json!({})));
                if let Some(args) = args.and_then(Value::as_object_mut) {
                    let cur = args.get("max_tokens").and_then(Value::as_u64).unwrap_or(cap);
                    args.insert("max_tokens".into(), json!(cur.min(cap)));
                }
            }
        }
        let body = msg.to_string();
        let (mut generation, mut resolved) = self.creds.current();
        for attempt in 0..2 {
            let resp = match self.post(&resolved, &msg, body.clone()).send().await {
                Ok(r) => r,
                Err(e) => {
                    let why = e.without_url().to_string();
                    self.error_reply(id.as_ref(), PROXY_ERROR, &format!("cannot reach the server: {why}")).await;
                    return;
                }
            };
            let status = resp.status();
            if status.as_u16() == 401 {
                let www: Vec<String> = resp
                    .headers()
                    .get_all(WWW_AUTHENTICATE)
                    .iter()
                    .filter_map(|v| v.to_str().ok().map(str::to_owned))
                    .collect();
                // The helpers are child processes: keep them off the async threads.
                let next = if attempt == 0 {
                    let creds = self.creds.clone();
                    tokio::task::spawn_blocking(move || creds.reask(generation, &www))
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                if let Some(next) = next {
                    (generation, resolved) = next;
                    continue;
                }
                credential::report_result(&resolved, false);
                self.fail_auth(id.as_ref(), !resolved.is_anonymous()).await;
                return;
            }
            if status.is_redirection() {
                self.error_reply(
                    id.as_ref(),
                    PROXY_ERROR,
                    "the server redirected the request; redirects are not followed so the credential stays on its origin",
                )
                .await;
                return;
            }
            if !status.is_success() {
                self.relay_error(resp, id.as_ref()).await;
                return;
            }
            credential::report_result(&resolved, true);
            self.relay_success(resp, method.as_deref(), id.as_ref()).await;
            return;
        }
    }

    async fn relay_error(&self, mut resp: reqwest::Response, id: Option<&Value>) {
        let status = resp.status();
        let insufficient = resp
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .any(|v| v.to_str().is_ok_and(|v| v.contains("insufficient_scope")));
        let mut body = Vec::new();
        while body.len() < MAX_ERROR_BODY {
            match resp.chunk().await {
                Ok(Some(c)) => body.extend_from_slice(&c),
                _ => break,
            }
        }
        // The server answers header mismatches and the like as JSON-RPC errors: pass those on.
        if let Ok(v) = serde_json::from_slice::<Value>(&body)
            && v.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
            && v.get("error").is_some()
        {
            self.emit(&v).await;
            return;
        }
        let mut text = format!("the server answered HTTP {}", status.as_u16());
        if insufficient {
            text.push_str(" (insufficient_scope)");
        }
        self.error_reply(id, PROXY_ERROR, &text).await;
    }

    async fn relay_success(&self, mut resp: reqwest::Response, method: Option<&str>, id: Option<&Value>) {
        let list = method == Some("tools/list");
        let ctype = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let sse = ctype.starts_with("text/event-stream");
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    let why = e.without_url().to_string();
                    self.error_reply(id, PROXY_ERROR, &format!("the server's reply broke off: {why}")).await;
                    return;
                }
            };
            buf.extend_from_slice(&chunk);
            if buf.len() > MAX_REPLY {
                self.error_reply(id, PROXY_ERROR, "the server's reply is too large").await;
                return;
            }
            if sse {
                for data in drain_events(&mut buf) {
                    self.relay_value(&data, list).await;
                }
            }
        }
        if sse {
            // A final event without its blank line.
            buf.extend_from_slice(b"\n\n");
            for data in drain_events(&mut buf) {
                self.relay_value(&data, list).await;
            }
        } else if !buf.iter().all(u8::is_ascii_whitespace) {
            self.relay_value(&buf, list).await;
        } else if id.is_some() && !matches!(method, Some(m) if m.starts_with("notifications/")) {
            // 202 or an empty body for a request that expects an answer.
            if method.is_some() {
                self.error_reply(id, PROXY_ERROR, "the server sent no reply").await;
            }
        }
    }

    async fn relay_value(&self, data: &[u8], list: bool) {
        match serde_json::from_slice::<Value>(data) {
            Ok(mut v) if v.is_object() => {
                if list {
                    self.narrow.filter_list(&mut v);
                }
                self.emit(&v).await;
            }
            _ => eprintln!("asc mcp: dropped a reply that is not a JSON-RPC message"),
        }
    }
}

/// Header values must be visible ASCII, or `reqwest` would refuse the request.
fn header_safe(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512 && s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Splits complete SSE events off the front of `buf` and returns the `data` of each.
fn drain_events(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let text = String::from_utf8_lossy(buf).replace("\r\n", "\n").replace('\r', "\n");
        let Some(end) = text.find("\n\n") else {
            return out;
        };
        let (event, rest) = (&text[..end], &text[end + 2..]);
        let data: Vec<&str> = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.strip_prefix(' ').unwrap_or(d))
            .collect();
        if !data.is_empty() {
            out.push(data.join("\n").into_bytes());
        }
        *buf = rest.as_bytes().to_vec();
    }
}

// ------------------------------------------------------------------ the session

/// Reads JSON-RPC lines from `input` and proxies them until it closes or a credential is lost.
async fn pump<R>(proxy: Arc<Proxy>, mut input: BufReader<R>, mut fatal_rx: mpsc::UnboundedReceiver<String>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut tasks: JoinSet<()> = JoinSet::new();
    let inflight: Arc<Mutex<HashMap<String, AbortHandle>>> = Arc::default();
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    let mut buf = Vec::new();
    let outcome = loop {
        buf.clear();
        let read = {
            let mut limited = (&mut input).take(MAX_LINE as u64 + 1);
            tokio::select! {
                r = limited.read_until(b'\n', &mut buf) => r,
                f = fatal_rx.recv() => break f.map_or(Ok(()), |m| Err(CliError::Fatal(m))),
            }
        };
        match read {
            Ok(0) => break Ok(()),
            Ok(_) if buf.len() > MAX_LINE => {
                break Err(CliError::Fatal("asc mcp: a message from the host is too large".into()));
            }
            Ok(_) => {}
            Err(e) => break Err(CliError::Fatal(format!("asc mcp: cannot read standard input: {e}"))),
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                proxy.error_reply(Some(&Value::Null), -32700, "parse error").await;
                continue;
            }
        };
        if !msg.is_object() {
            proxy.error_reply(Some(&Value::Null), -32600, "one JSON-RPC message per line").await;
            continue;
        }
        let method = msg.get("method").and_then(Value::as_str);
        if method == Some("notifications/cancelled") {
            if let Some(rid) = msg.pointer("/params/requestId") {
                let handle = inflight.lock().unwrap_or_else(|p| p.into_inner()).remove(&rid.to_string());
                if let Some(h) = handle {
                    h.abort();
                }
            }
            continue;
        }
        let key = msg.get("id").filter(|i| !i.is_null()).map(Value::to_string);
        let Ok(permit) = permits.clone().acquire_owned().await else {
            break Ok(());
        };
        let p = proxy.clone();
        let map = inflight.clone();
        let key2 = key.clone();
        let handle = tasks.spawn(async move {
            p.forward(msg).await;
            if let Some(k) = key2 {
                map.lock().unwrap_or_else(|e| e.into_inner()).remove(&k);
            }
            drop(permit);
        });
        if let Some(k) = key {
            inflight.lock().unwrap_or_else(|p| p.into_inner()).insert(k, handle);
        }
    };
    if outcome.is_ok() {
        let drain = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(DRAIN, drain).await.is_err() {
            tasks.abort_all();
        }
    } else {
        // Let the error reply of the failing request reach the host, then stop the rest.
        tokio::task::yield_now().await;
        tasks.abort_all();
    }
    outcome
}

/// Serves MCP on standard input and output, forwarding to the asc server at `raw_url`.
///
/// # Errors
/// An unusable URL, a credential error before the first request, or the loss of the credential
/// during the session.
pub async fn serve(ctx: &Ctx, raw_url: &str, flags: Flags) -> Result<ExitCode> {
    let url = endpoint(raw_url, Policy::current()).map_err(|e| CliError::Fatal(format!("asc mcp: {e}")))?;
    let real_out = super::guard::take_stdout()
        .map_err(|e| CliError::Fatal(format!("asc mcp: cannot protect standard output: {e}")))?;
    let client = reqwest::Client::builder()
        .redirect(credential::rpc_redirect_policy())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(300))
        .user_agent(concat!("asc/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| CliError::Fatal(format!("asc mcp: http client: {e}")))?;
    let real = ctx.real_git().ok().cloned();
    let file = ctx.config.file().clone();
    let creds = {
        let url = url.clone();
        tokio::task::spawn_blocking(move || Creds::open(file, real, url))
            .await
            .map_err(|e| CliError::Fatal(format!("asc mcp: {e}")))??
    };
    let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
    let (fatal_tx, fatal_rx) = mpsc::unbounded_channel::<String>();
    let proxy = Arc::new(Proxy {
        client,
        host: host_label(&url),
        url,
        creds: Arc::new(creds),
        narrow: Narrow::new(&flags),
        max_tokens: flags.max_tokens,
        out: out_tx,
        fatal: fatal_tx,
    });
    let mut stdout = tokio::fs::File::from_std(real_out);
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });
    let result = pump(proxy.clone(), BufReader::new(tokio::io::stdin()), fatal_rx).await;
    drop(proxy);
    let _ = writer.await;
    result.map(|()| ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(s: &str) -> std::result::Result<String, String> {
        endpoint(s, Policy::STRICT).map(|u| u.to_string())
    }

    #[test]
    fn endpoint_rules() {
        assert_eq!(ep("https://h.example").unwrap(), "https://h.example/mcp");
        assert_eq!(ep("https://h.example/").unwrap(), "https://h.example/mcp");
        assert_eq!(ep("https://h.example/mcp").unwrap(), "https://h.example/mcp");
        assert_eq!(ep("https://h.example/asc/").unwrap(), "https://h.example/asc/mcp");
        assert!(ep("http://h.example").is_err());
        assert!(ep("http://127.0.0.1:1").is_err(), "loopback http needs the opt-in");
        assert_eq!(
            endpoint("http://127.0.0.1:8080", Policy::LOOPBACK_HTTP).unwrap().as_str(),
            "http://127.0.0.1:8080/mcp"
        );
        assert!(endpoint("http://h.example", Policy::LOOPBACK_HTTP).is_err());
        assert!(ep("https://u:p@h.example").is_err());
        assert!(ep("https://h.example/?a=1").is_err());
        assert!(ep("ftp://h.example").is_err());
        assert!(ep("nope").is_err());
    }

    #[test]
    fn sse_events_split_and_join_data_lines() {
        let mut b = b"event: message\ndata: {\"a\":1}\n\ndata: x\r\ndata: y\r\n\r\n: ping\n\ndata: part".to_vec();
        let got = drain_events(&mut b);
        assert_eq!(got, vec![b"{\"a\":1}".to_vec(), b"x\ny".to_vec()]);
        assert_eq!(b, b"data: part");
    }

    #[test]
    fn narrowing_removes_only() {
        let none = Narrow::default();
        assert!(none.allows("anything_the_server_lists"));
        let ro = Narrow { read_only: true, ..Narrow::default() };
        assert!(ro.allows("log"));
        assert!(!ro.allows("commit"));
        assert!(!ro.allows("not_a_known_tool"));
        let only = Narrow { tools: Some(BTreeSet::from(["log".to_owned()])), ..Narrow::default() };
        assert!(only.allows("log") && !only.allows("status"));
        let mut list = json!({ "result": { "tools": [{ "name": "log" }, { "name": "commit" }, { "name": "show" }] } });
        ro.filter_list(&mut list);
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["log", "show"]);
    }
}
