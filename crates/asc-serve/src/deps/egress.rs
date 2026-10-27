//! The one way the scanner talks to the network.
//!
//! A request passes three gates before a byte leaves: the host must be on the egress allowlist
//! (`[deps] egress_allow`, by default the hosts of the configured services), the network guard must
//! accept the address it resolves to (no loopback, private, link-local or metadata address, https only,
//! the addresses pinned so a second lookup cannot answer differently), and the call carries its own
//! timeout and a cap on the size of the answer. Redirects are not followed: a redirect is an answer the
//! scanner treats as an error. Errors never carry the URL.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::config::host_matches;
use crate::netguard::{Guard, Profile, Shape};

/// How long a pinned client is reused before the host is resolved again.
const CLIENT_TTL: Duration = Duration::from_secs(60);

/// What one call may use.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub timeout: Duration,
    pub max_bytes: usize,
}

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The host is not on the allowlist.
    NotAllowed,
    /// The network guard refused the address (the slug of the reason).
    Blocked(&'static str),
    Timeout,
    /// The answer was longer than the limit.
    TooLarge,
    /// A status other than success (a redirect is one).
    Status(u16),
    Transport(String),
}

impl FetchError {
    /// Worth asking again once: the other side or the network may recover.
    pub fn transient(&self) -> bool {
        matches!(self, Self::Timeout | Self::Transport(_))
            || matches!(self, Self::Status(s) if *s == 429 || *s >= 500)
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAllowed => f.write_str("the host is not on the egress allowlist"),
            Self::Blocked(r) => write!(f, "the network guard refused the address ({r})"),
            Self::Timeout => f.write_str("the request timed out"),
            Self::TooLarge => f.write_str("the answer is larger than the limit"),
            Self::Status(s) => write!(f, "the server answered {s}"),
            Self::Transport(m) => write!(f, "the request failed ({m})"),
        }
    }
}

pub struct Egress {
    guard: Guard,
    allow: Vec<String>,
    clients: Mutex<HashMap<String, (Instant, reqwest::Client)>>,
}

impl Egress {
    pub fn new(guard: Guard, allow: Vec<String>) -> Self {
        Self {
            guard,
            allow,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the allowlist admits `host`.
    pub fn allows(&self, host: &str) -> bool {
        self.allow.iter().any(|p| host_matches(p, host))
    }

    async fn client(&self, url: &str) -> Result<reqwest::Client, FetchError> {
        let parsed = url::Url::parse(url).map_err(|_| FetchError::Blocked("malformed"))?;
        let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        if !self.allows(&host) {
            return Err(FetchError::NotAllowed);
        }
        let key = format!(
            "{}://{host}:{}",
            parsed.scheme(),
            parsed.port_or_known_default().unwrap_or(0)
        );
        if let Some((at, c)) = self
            .clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            && at.elapsed() < CLIENT_TTL
        {
            return Ok(c.clone());
        }
        let pinned = self
            .guard
            .pin(url, Shape::Url)
            .await
            .map_err(|b| FetchError::Blocked(b.reason().slug()))?;
        let client = pinned
            .client(Profile::Api)
            .map_err(|e| FetchError::Transport(e.without_url().to_string()))?;
        let mut map = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if map.len() > 64 {
            map.clear();
        }
        map.insert(key, (Instant::now(), client.clone()));
        Ok(client)
    }

    async fn send(
        &self,
        url: &str,
        lim: &Limits,
        build: impl FnOnce(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Result<Vec<u8>, FetchError> {
        let client = self.client(url).await?;
        let mut resp = build(&client)
            .timeout(lim.timeout)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    FetchError::Timeout
                } else {
                    FetchError::Transport(e.without_url().to_string())
                }
            })?;
        if !resp.status().is_success() {
            return Err(FetchError::Status(resp.status().as_u16()));
        }
        if resp
            .content_length()
            .is_some_and(|n| n > lim.max_bytes as u64)
        {
            return Err(FetchError::TooLarge);
        }
        let mut body = Vec::new();
        loop {
            let chunk = resp.chunk().await.map_err(|e| {
                if e.is_timeout() {
                    FetchError::Timeout
                } else {
                    FetchError::Transport(e.without_url().to_string())
                }
            })?;
            let Some(chunk) = chunk else { break };
            if body.len() + chunk.len() > lim.max_bytes {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// `GET url`, the body of a success.
    pub async fn get(
        &self,
        url: &str,
        lim: &Limits,
        accept: Option<&str>,
    ) -> Result<Vec<u8>, FetchError> {
        self.send(url, lim, |c| {
            let r = c.get(url);
            match accept {
                Some(a) => r.header(reqwest::header::ACCEPT, a),
                None => r,
            }
        })
        .await
    }

    /// `POST url` with a JSON body.
    pub async fn post_json(
        &self,
        url: &str,
        body: Vec<u8>,
        lim: &Limits,
    ) -> Result<Vec<u8>, FetchError> {
        self.send(url, lim, |c| {
            c.post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
        })
        .await
    }

    /// `get`, asked once more when the failure may pass.
    pub async fn get_retry(
        &self,
        url: &str,
        lim: &Limits,
        accept: Option<&str>,
    ) -> Result<Vec<u8>, FetchError> {
        match self.get(url, lim, accept).await {
            Err(e) if e.transient() => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                self.get(url, lim, accept).await
            }
            other => other,
        }
    }

    /// `post_json`, asked once more when the failure may pass.
    pub async fn post_retry(
        &self,
        url: &str,
        body: &[u8],
        lim: &Limits,
    ) -> Result<Vec<u8>, FetchError> {
        match self.post_json(url, body.to_vec(), lim).await {
            Err(e) if e.transient() => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                self.post_json(url, body.to_vec(), lim).await
            }
            other => other,
        }
    }
}
