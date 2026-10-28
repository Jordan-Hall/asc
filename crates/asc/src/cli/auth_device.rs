//! The device flow of `asc auth login` (RFC 8628 shape, as the hub serves it).
//!
//! * `POST {hub}/login/device/code` (form: `client_name`, optional) answers JSON `device_code`,
//!   `user_code` (`XXXX-XXXX`), `verification_uri`, `verification_uri_complete`, `expires_in`
//!   (600) and `interval` (5).
//! * The person opens `verification_uri_complete` in a browser, signs in and approves the code.
//! * The CLI polls `POST {hub}/login/device/token` (form: `grant_type =
//!   urn:ietf:params:oauth:grant-type:device_code`, `device_code`) every `interval` seconds, five
//!   seconds more after `slow_down`. `200` carries `access_token`, a normal personal access token;
//!   `400` carries `error`: `authorization_pending` (keep polling), `slow_down`, `access_denied`,
//!   `expired_token`, `invalid_grant`.
//!
//! Everything the hub answers is untrusted: bodies are bounded, the URLs it names must be on the
//! hub's own origin, and the code shown to the person is cleaned of control characters. The token
//! never reaches argv or the terminal; the caller stores it with the credential helpers.

use std::future::Future;
use std::time::{Duration, Instant};

use url::Url;

/// Longest response read.
const MAX_BODY: usize = 64 * 1024;
/// Most a poll interval may grow to, in seconds.
const MAX_INTERVAL: u64 = 60;
/// Least and most life of a device code the CLI waits for, in seconds.
const MAX_EXPIRES: u64 = 3600;
/// Seconds added after `slow_down`.
const SLOW_DOWN_STEP: u64 = 5;
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Why the flow ended without a token.
#[derive(Debug, PartialEq, Eq)]
pub enum DeviceError {
    Denied,
    Expired,
    Invalid,
    Hub(String),
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied => f.write_str("the sign-in was denied in the browser"),
            Self::Expired => f.write_str("the code expired before it was approved; run the command again"),
            Self::Invalid => f.write_str("the hub no longer knows this sign-in; run the command again"),
            Self::Hub(m) => write!(f, "{m}"),
        }
    }
}

/// The answer of `/login/device/code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// One poll.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    Token(String),
    Pending,
    SlowDown,
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(512).collect()
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

async fn post(client: &reqwest::Client, url: Url, body: String) -> Result<(u16, serde_json::Value), DeviceError> {
    let resp = client
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| DeviceError::Hub(format!("the hub could not be reached: {}", clean(&e.to_string()))))?;
    let status = resp.status().as_u16();
    let mut bytes = Vec::new();
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await.map_err(|e| DeviceError::Hub(clean(&e.to_string())))? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_BODY {
            return Err(DeviceError::Hub("the hub answered with too much data".into()));
        }
    }
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Ok((status, v))
}

/// The same origin as `hub`: a verification link the hub names must not lead elsewhere.
fn on_origin(hub: &Url, link: &str) -> bool {
    Url::parse(link).is_ok_and(|u| u.origin() == hub.origin() && u.username().is_empty() && u.password().is_none())
}

/// Starts the flow.
pub async fn start(client: &reqwest::Client, hub: &Url, client_name: &str) -> Result<Grant, DeviceError> {
    let url = hub.join("/login/device/code").map_err(|_| DeviceError::Hub("the hub URL is not usable".into()))?;
    let body = if client_name.is_empty() { String::new() } else { form(&[("client_name", client_name)]) };
    let (status, v) = post(client, url, body).await?;
    if status != 200 {
        return Err(DeviceError::Hub(format!("the hub refused to start a sign-in (HTTP {status})")));
    }
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64);
    let (Some(device_code), Some(user_code), Some(uri)) = (s("device_code"), s("user_code"), s("verification_uri")) else {
        return Err(DeviceError::Hub("the hub's answer is missing the device code".into()));
    };
    let complete = s("verification_uri_complete").unwrap_or_else(|| uri.clone());
    if !on_origin(hub, &uri) || !on_origin(hub, &complete) {
        return Err(DeviceError::Hub("the hub named a verification link on another site".into()));
    }
    if device_code.is_empty() || device_code.len() > 1024 || user_code.is_empty() || user_code.len() > 64 {
        return Err(DeviceError::Hub("the hub's device code is not usable".into()));
    }
    Ok(Grant {
        device_code,
        user_code: clean(&user_code),
        verification_uri: uri,
        verification_uri_complete: complete,
        expires_in: n("expires_in").unwrap_or(600).clamp(1, MAX_EXPIRES),
        interval: n("interval").unwrap_or(5).clamp(1, MAX_INTERVAL),
    })
}

/// Asks once whether the person has approved.
pub async fn poll_once(client: &reqwest::Client, hub: &Url, device_code: &str) -> Result<Poll, DeviceError> {
    let url = hub.join("/login/device/token").map_err(|_| DeviceError::Hub("the hub URL is not usable".into()))?;
    let (status, v) = post(client, url, form(&[("grant_type", GRANT), ("device_code", device_code)])).await?;
    if status == 200 {
        return match v.get("access_token").and_then(|t| t.as_str()) {
            Some(t) if !t.is_empty() && t.len() <= 4096 && !t.chars().any(char::is_control) && !t.contains(char::is_whitespace) => {
                Ok(Poll::Token(t.to_owned()))
            }
            _ => Err(DeviceError::Hub("the hub's answer holds no usable token".into())),
        };
    }
    match v.get("error").and_then(|e| e.as_str()) {
        Some("authorization_pending") => Ok(Poll::Pending),
        Some("slow_down") => Ok(Poll::SlowDown),
        Some("access_denied") => Err(DeviceError::Denied),
        Some("expired_token") => Err(DeviceError::Expired),
        Some("invalid_grant") => Err(DeviceError::Invalid),
        _ => Err(DeviceError::Hub(format!("the hub answered HTTP {status}"))),
    }
}

/// Runs the whole flow: `show` tells the person where to go, `sleep` waits (a test passes a fake).
pub async fn login<S, Fut>(
    client: &reqwest::Client,
    hub: &Url,
    client_name: &str,
    show: impl Fn(&Grant),
    sleep: S,
) -> Result<String, DeviceError>
where
    S: Fn(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    let grant = start(client, hub, client_name).await?;
    show(&grant);
    let started = Instant::now();
    let mut interval = grant.interval;
    let mut waited = 0u64;
    loop {
        sleep(Duration::from_secs(interval)).await;
        waited += interval;
        if waited > grant.expires_in + interval || started.elapsed() > Duration::from_secs(grant.expires_in + MAX_INTERVAL) {
            return Err(DeviceError::Expired);
        }
        match poll_once(client, hub, &grant.device_code).await? {
            Poll::Token(t) => return Ok(t),
            Poll::Pending => {}
            Poll::SlowDown => interval = (interval + SLOW_DOWN_STEP).min(MAX_INTERVAL),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    fn grant_json(base: &Url) -> String {
        format!(
            "{{\"device_code\":\"dc123\",\"user_code\":\"ABCD-EFGH\",\"verification_uri\":\"{b}login/device\",\"verification_uri_complete\":\"{b}login/device?user_code=ABCD-EFGH\",\"expires_in\":600,\"interval\":5}}",
            b = base
        )
    }

    fn err(e: &str) -> (u16, String) {
        (400, format!("{{\"error\":\"{e}\"}}"))
    }

    #[tokio::test]
    async fn the_flow_waits_slows_down_and_returns_the_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let script = vec![
            (200, grant_json(&base)),
            err("authorization_pending"),
            err("slow_down"),
            err("authorization_pending"),
            (200, "{\"access_token\":\"tok_abc\",\"token_type\":\"bearer\"}".to_owned()),
        ];
        let seen = serve(listener, script);
        let slept = Arc::new(Mutex::new(Vec::new()));
        let s2 = slept.clone();
        let shown = Arc::new(Mutex::new(None));
        let sh = shown.clone();
        let token = login(
            &reqwest::Client::new(),
            &base,
            "asc cli",
            move |g| *sh.lock().unwrap() = Some(g.clone()),
            move |d| {
                s2.lock().unwrap().push(d.as_secs());
                std::future::ready(())
            },
        )
        .await
        .unwrap();
        assert_eq!(token, "tok_abc");
        assert_eq!(*slept.lock().unwrap(), [5, 5, 10, 10], "5 seconds, then 5 more after slow_down");
        let g = shown.lock().unwrap().clone().unwrap();
        assert_eq!((g.user_code.as_str(), g.expires_in, g.interval), ("ABCD-EFGH", 600, 5));
        assert!(g.verification_uri_complete.ends_with("user_code=ABCD-EFGH"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/login/device/code");
        assert_eq!(seen[0].1, "client_name=asc+cli");
        assert_eq!(seen[1].0, "/login/device/token");
        assert!(seen[1].1.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"), "{}", seen[1].1);
        assert!(seen[1].1.contains("device_code=dc123"));
    }

    fn serve(l: TcpListener, script: Vec<(u16, String)>) -> Arc<Mutex<Vec<(String, String)>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in script {
                let Ok((mut c, _)) = l.accept() else { return };
                let mut buf = vec![0u8; 16 * 1024];
                let mut n = 0;
                loop {
                    let r = c.read(&mut buf[n..]).unwrap_or(0);
                    n += r;
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if n >= h + 4 + len || r == 0 {
                            let path = text.split_whitespace().nth(1).unwrap_or("").to_owned();
                            s2.lock().unwrap().push((path, text[h + 4..].to_owned()));
                            break;
                        }
                    }
                    if r == 0 {
                        break;
                    }
                }
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = c.write_all(resp.as_bytes());
            }
        });
        seen
    }

    async fn run_with(script_after_grant: Vec<(u16, String)>) -> Result<String, DeviceError> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let mut script = vec![(200, grant_json(&base))];
        script.extend(script_after_grant);
        serve(listener, script);
        login(&reqwest::Client::new(), &base, "", |_| {}, |_| std::future::ready(())).await
    }

    #[tokio::test]
    async fn every_error_the_hub_names_ends_the_flow_with_its_own_message() {
        assert_eq!(run_with(vec![err("access_denied")]).await, Err(DeviceError::Denied));
        assert_eq!(run_with(vec![err("expired_token")]).await, Err(DeviceError::Expired));
        assert_eq!(run_with(vec![err("invalid_grant")]).await, Err(DeviceError::Invalid));
        assert!(matches!(run_with(vec![(500, "boom".into())]).await, Err(DeviceError::Hub(_))));
        assert!(matches!(run_with(vec![(200, "{\"access_token\":\"has space\"}".into())]).await, Err(DeviceError::Hub(_))));
        assert!(matches!(run_with(vec![(200, "{}".into())]).await, Err(DeviceError::Hub(_))));
    }

    #[tokio::test]
    async fn a_hub_that_never_approves_ends_when_the_code_expires() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let mut script = vec![(
            200,
            grant_json(&base).replace("\"expires_in\":600,\"interval\":5", "\"expires_in\":12,\"interval\":5"),
        )];
        script.extend((0..6).map(|_| err("authorization_pending")));
        serve(listener, script);
        let r = login(&reqwest::Client::new(), &base, "", |_| {}, |_| std::future::ready(())).await;
        assert_eq!(r, Err(DeviceError::Expired));
    }

    #[tokio::test]
    async fn a_verification_link_on_another_site_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let body = "{\"device_code\":\"d\",\"user_code\":\"AAAA-BBBB\",\"verification_uri\":\"https://evil.example/login/device\",\"verification_uri_complete\":\"https://evil.example/x\",\"expires_in\":600,\"interval\":5}";
        serve(listener, vec![(200, body.to_owned())]);
        let r = start(&reqwest::Client::new(), &base, "asc").await;
        assert!(matches!(r, Err(DeviceError::Hub(m)) if m.contains("another site")));
    }

    #[tokio::test]
    async fn a_start_the_hub_refuses_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        serve(listener, vec![(404, "{}".to_owned())]);
        assert!(matches!(start(&reqwest::Client::new(), &base, "").await, Err(DeviceError::Hub(m)) if m.contains("404")));
    }
}
