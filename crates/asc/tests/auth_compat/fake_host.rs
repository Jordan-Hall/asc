//! A test-only git host (SPEC §15.6): it issues git's exact flow and records every request.
//!
//! An unauthenticated request gets `401` with `WWW-Authenticate: Basic ...`; a request whose
//! `Authorization` value is one of the accepted ones gets a smart-HTTP advertisement of an
//! empty repository, so real `git ls-remote` succeeds. A host can also answer every request
//! with a redirect, or accept anonymous requests. Plain `tokio` sockets and `Connection:
//! close`, so no HTTP stack stands between the wire and the assertions.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// The path every test asks for.
pub const REPO_PATH: &str = "/org/repo.git";
/// The discovery request real git sends (and the redirect target must end with).
pub const INFO_REFS: &str = "/org/repo.git/info/refs?service=git-upload-pack";

/// One request as it arrived. Header names are lower-cased; values are verbatim.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub status: u16,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn authorization(&self) -> Option<&str> {
        self.header("authorization")
    }
}

#[derive(Default)]
struct Rules {
    /// Exact `Authorization` values that pass.
    accepted: Vec<String>,
    /// `WWW-Authenticate` lines of a 401, in order.
    challenges: Vec<String>,
    /// Answer every request with a 302 to this URL.
    redirect: Option<String>,
    /// Let a request without a credential through.
    open: bool,
}

struct Inner {
    rules: Rules,
    seen: Vec<Seen>,
}

/// A running fake host; dropping it stops the listener.
pub struct FakeHost {
    pub addr: SocketAddr,
    inner: Arc<Mutex<Inner>>,
    task: JoinHandle<()>,
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn pkt(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
}

/// The v0 advertisement of an empty repository.
fn advertisement() -> Vec<u8> {
    let mut body = Vec::new();
    pkt(&mut body, b"# service=git-upload-pack\n");
    body.extend_from_slice(b"0000");
    pkt(
        &mut body,
        b"0000000000000000000000000000000000000000 capabilities^{}\0multi_ack agent=fake-host\n",
    );
    body.extend_from_slice(b"0000");
    body
}

async fn read_request(
    sock: &mut tokio::net::TcpStream,
) -> Option<(String, String, Vec<(String, String)>)> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        got.extend_from_slice(&buf[..n]);
        if let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        if got.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&got[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    // Drain a body so the client is not reset mid-write.
    let want: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut have = got.len() - end;
    while have < want {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        have += n;
    }
    Some((method, target, headers))
}

impl FakeHost {
    /// A host that requires a credential and accepts none yet: call [`FakeHost::accept`].
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let inner = Arc::new(Mutex::new(Inner {
            rules: Rules {
                challenges: vec!["Basic realm=\"fake git host\"".to_owned()],
                ..Rules::default()
            },
            seen: Vec::new(),
        }));
        let shared = inner.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let Some((method, target, headers)) = read_request(&mut sock).await else {
                        return;
                    };
                    let (status, reply) = {
                        let mut guard = shared.lock().expect("host state");
                        let auth = headers
                            .iter()
                            .find(|(n, _)| n == "authorization")
                            .map(|(_, v)| v.as_str());
                        let (status, reply) = respond(&guard.rules, auth);
                        guard.seen.push(Seen {
                            method,
                            target,
                            headers,
                            status,
                        });
                        (status, reply)
                    };
                    let _ = status;
                    let _ = sock.write_all(&reply).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self { addr, inner, task }
    }

    fn rules(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("host state")
    }

    /// Accepts requests that carry exactly this `Authorization` value.
    pub fn accept(&self, authorization: &str) -> &Self {
        self.rules().rules.accepted.push(authorization.to_owned());
        self
    }

    /// `Authorization: Basic base64(user:pass)` as git sends it.
    pub fn accept_basic(&self, user: &str, pass: &str) -> &Self {
        self.accept(&format!(
            "Basic {}",
            base64(format!("{user}:{pass}").as_bytes())
        ))
    }

    /// Forgets every accepted credential: a revoked token.
    pub fn revoke_all(&self) {
        self.rules().rules.accepted.clear();
    }

    /// Replaces the `WWW-Authenticate` lines of a 401.
    pub fn challenge(&self, lines: &[&str]) -> &Self {
        self.rules().rules.challenges = lines.iter().map(|l| (*l).to_owned()).collect();
        self
    }

    /// Lets requests without a credential through.
    pub fn open(&self) -> &Self {
        self.rules().rules.open = true;
        self
    }

    /// Answers every request with `302 Location: <to>`.
    pub fn redirect_to(&self, to: &str) -> &Self {
        self.rules().rules.redirect = Some(to.to_owned());
        self
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.rules().seen.clone()
    }

    /// The origin, without a path.
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The remote URL real git and asc are pointed at.
    pub fn url(&self) -> String {
        format!("{}{REPO_PATH}", self.origin())
    }

    /// The `Authorization` of the last request this host accepted.
    pub fn accepted_authorization(&self) -> Option<String> {
        self.requests()
            .iter()
            .rev()
            .find(|r| r.status == 200)
            .and_then(|r| r.authorization().map(str::to_owned))
    }
}

fn respond(rules: &Rules, auth: Option<&str>) -> (u16, Vec<u8>) {
    if let Some(to) = &rules.redirect {
        let head = format!(
            "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return (302, head.into_bytes());
    }
    let allowed = rules.open || auth.is_some_and(|a| rules.accepted.iter().any(|ok| ok == a));
    if allowed {
        let body = advertisement();
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(&body);
        return (200, out);
    }
    let mut head = String::from("HTTP/1.1 401 Unauthorized\r\n");
    for line in &rules.challenges {
        head.push_str(&format!("WWW-Authenticate: {line}\r\n"));
    }
    head.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
    (401, head.into_bytes())
}

/// Standard base64, for `Authorization: Basic`.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
