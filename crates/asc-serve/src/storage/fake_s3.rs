//! A small S3-compatible server for the tests, in process on a loopback port.
//!
//! It checks the request signature (SigV4) against the one key pair it was started with, so a
//! test fails if the server signed with the wrong key, a stale key or the environment's key. It
//! speaks path-style requests: object `PUT`/`GET`/`HEAD`/`DELETE`, copy, list v2 with prefix,
//! delimiter, start-after and continuation, and the multi-object delete `object_store` uses.
//! Faults can be injected per method.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, Response, header};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// One request the server saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub key_id: Option<String>,
    pub status: u16,
}

#[derive(Default)]
struct Faults {
    /// `(method, skip, remaining, status)`: after `skip` requests with that method went
    /// through, the next `remaining` fail with `status`.
    fail: Vec<(String, usize, usize, u16)>,
    /// Milliseconds every request with the method waits first.
    delay: Vec<(String, u64)>,
    /// Every signed request is refused with 403.
    deny: bool,
}

struct Inner {
    bucket: String,
    region: String,
    access_key: Mutex<String>,
    secret: Mutex<String>,
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    log: Mutex<Vec<Seen>>,
    faults: Mutex<Faults>,
    page_size: Mutex<usize>,
}

/// The running server.
pub struct FakeS3 {
    inner: Arc<Inner>,
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeS3 {
    pub async fn start(bucket: &str, access_key: &str, secret: &str) -> Self {
        let inner = Arc::new(Inner {
            bucket: bucket.to_owned(),
            region: "us-east-1".into(),
            access_key: Mutex::new(access_key.to_owned()),
            secret: Mutex::new(secret.to_owned()),
            objects: Mutex::default(),
            log: Mutex::default(),
            faults: Mutex::default(),
            page_size: Mutex::new(1000),
        });
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Self { inner, addr, task }
    }

    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// Replaces the accepted key pair (a rotated key on the provider's side).
    pub fn set_credentials(&self, key: &str, secret: &str) {
        *self.inner.access_key.lock().unwrap() = key.to_owned();
        *self.inner.secret.lock().unwrap() = secret.to_owned();
    }

    pub fn set_page_size(&self, n: usize) {
        *self.inner.page_size.lock().unwrap() = n;
    }

    /// Makes the next `n` requests with `method` answer `status`.
    pub fn fail_next(&self, method: &str, n: usize, status: u16) {
        self.fail_after(method, 0, n, status);
    }

    /// Lets `skip` requests with `method` through, then fails the next `n` with `status`.
    pub fn fail_after(&self, method: &str, skip: usize, n: usize, status: u16) {
        self.inner
            .faults
            .lock()
            .unwrap()
            .fail
            .push((method.to_owned(), skip, n, status));
    }

    /// Makes every request with `method` wait `ms` milliseconds first.
    pub fn set_delay(&self, method: &str, ms: u64) {
        self.inner
            .faults
            .lock()
            .unwrap()
            .delay
            .push((method.to_owned(), ms));
    }

    /// Refuses every request with 403 until [`allow`](Self::allow).
    pub fn deny(&self) {
        self.inner.faults.lock().unwrap().deny = true;
    }

    pub fn allow(&self) {
        let mut f = self.inner.faults.lock().unwrap();
        f.deny = false;
        f.fail.clear();
    }

    /// Keys (without the bucket) that start with `prefix`.
    pub fn keys(&self, prefix: &str) -> Vec<String> {
        self.inner
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect()
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.objects.lock().unwrap().get(key).cloned()
    }

    pub fn put(&self, key: &str, data: &[u8]) {
        self.inner
            .objects
            .lock()
            .unwrap()
            .insert(key.to_owned(), data.to_vec());
    }

    pub fn remove(&self, key: &str) {
        self.inner.objects.lock().unwrap().remove(key);
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.inner.log.lock().unwrap().clone()
    }

    pub fn clear_requests(&self) {
        self.inner.log.lock().unwrap().clear();
    }
}

// ------------------------------------------------------------------------------ SigV4

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).unwrap();
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn strict_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Why a request was refused, as the S3 error code.
enum AuthFail {
    InvalidAccessKeyId,
    SignatureDoesNotMatch,
}

struct Parsed {
    key_id: String,
    scope: String,
    signed: Vec<String>,
    signature: String,
}

fn parse_auth(headers: &HeaderMap) -> Option<Parsed> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = v.strip_prefix("AWS4-HMAC-SHA256 ")?;
    let (mut cred, mut signed, mut sig) = (None, None, None);
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(c) = part.strip_prefix("Credential=") {
            cred = Some(c.to_owned());
        } else if let Some(s) = part.strip_prefix("SignedHeaders=") {
            signed = Some(s.to_owned());
        } else if let Some(s) = part.strip_prefix("Signature=") {
            sig = Some(s.to_owned());
        }
    }
    let cred = cred?;
    let (key_id, scope) = cred.split_once('/')?;
    Some(Parsed {
        key_id: key_id.to_owned(),
        scope: scope.to_owned(),
        signed: signed?.split(';').map(str::to_owned).collect(),
        signature: sig?,
    })
}

fn verify(
    inner: &Inner,
    req_parts: &axum::http::request::Parts,
    auth: &Parsed,
) -> Result<(), AuthFail> {
    if auth.key_id != *inner.access_key.lock().unwrap() {
        return Err(AuthFail::InvalidAccessKeyId);
    }
    let secret = inner.secret.lock().unwrap().clone();
    let query = req_parts.uri.query().unwrap_or("");
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (strict_encode(&k), strict_encode(&v)))
        .collect();
    pairs.sort();
    let canonical_query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut canonical_headers = String::new();
    for name in &auth.signed {
        let value = req_parts
            .headers
            .get_all(name.as_str())
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join(",");
        canonical_headers.push_str(&format!("{name}:{value}\n"));
    }
    let payload = req_parts
        .headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("UNSIGNED-PAYLOAD");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req_parts.method,
        req_parts.uri.path(),
        canonical_query,
        canonical_headers,
        auth.signed.join(";"),
        payload
    );
    let date = req_parts
        .headers
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{date}\n{}\n{}",
        auth.scope,
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let mut scope = auth.scope.split('/');
    let (day, region, service) = (
        scope.next().unwrap_or(""),
        scope.next().unwrap_or(""),
        scope.next().unwrap_or(""),
    );
    // A request signed for another region is refused, as a real provider refuses it.
    if region != inner.region {
        return Err(AuthFail::SignatureDoesNotMatch);
    }
    let k = hmac(format!("AWS4{secret}").as_bytes(), day.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    let expected = hex(&hmac(&k, to_sign.as_bytes()));
    if expected == auth.signature {
        Ok(())
    } else {
        Err(AuthFail::SignatureDoesNotMatch)
    }
}

// ------------------------------------------------------------------------------ handler

fn xml_error(status: u16, code: &str, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{msg}</Message></Error>"
        )))
        .unwrap()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn etag(data: &[u8]) -> String {
    format!("\"{}\"", hex(&Sha256::digest(data))[..32].to_owned())
}

async fn handle(State(inner): State<Arc<Inner>>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let path = parts.uri.path().to_owned();
    let auth = parse_auth(&parts.headers);
    let key_id = auth.as_ref().map(|a| a.key_id.clone());
    let response = respond(&inner, &parts, body, auth.as_ref()).await;
    inner.log.lock().unwrap().push(Seen {
        method: method.to_string(),
        path,
        key_id,
        status: response.status().as_u16(),
    });
    response
}

async fn respond(
    inner: &Inner,
    parts: &axum::http::request::Parts,
    body: Body,
    auth: Option<&Parsed>,
) -> Response<Body> {
    let path = parts.uri.path();
    // The bare endpoint answers anything: it is what the connect step looks at.
    if path == "/" && parts.method == Method::HEAD {
        return Response::new(Body::empty());
    }
    match auth {
        None => return xml_error(403, "AccessDenied", "no signature"),
        Some(a) => match verify(inner, parts, a) {
            Ok(()) => {}
            Err(AuthFail::InvalidAccessKeyId) => {
                return xml_error(403, "InvalidAccessKeyId", "unknown key");
            }
            Err(AuthFail::SignatureDoesNotMatch) => {
                return xml_error(403, "SignatureDoesNotMatch", "bad signature");
            }
        },
    }
    let delay: Option<u64>;
    {
        let mut f = inner.faults.lock().unwrap();
        if f.deny {
            return xml_error(403, "AccessDenied", "denied");
        }
        let m = parts.method.as_str().to_owned();
        for entry in f.fail.iter_mut().filter(|e| e.0 == m) {
            if entry.1 > 0 {
                entry.1 -= 1;
            } else if entry.2 > 0 {
                entry.2 -= 1;
                let status = entry.3;
                return xml_error(
                    status,
                    if status == 403 {
                        "AccessDenied"
                    } else {
                        "InternalError"
                    },
                    "injected",
                );
            }
        }
        delay = f.delay.iter().find(|(dm, _)| *dm == m).map(|(_, ms)| *ms);
    }
    if let Some(ms) = delay {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
    let decoded = percent_decode(path.trim_start_matches('/'));
    let (bucket, key) = decoded
        .split_once('/')
        .map_or((decoded.as_str(), ""), |(b, k)| (b, k));
    if bucket != inner.bucket {
        return xml_error(404, "NoSuchBucket", "no such bucket");
    }
    let query: BTreeMap<String, String> =
        url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let bytes = to_bytes(body, 256 * 1024 * 1024).await.unwrap_or_default();
    match (&parts.method, key.is_empty()) {
        (&Method::GET, true) if query.get("list-type").is_some_and(|v| v == "2") => {
            list(inner, &query)
        }
        (&Method::POST, true) if query.contains_key("delete") => multi_delete(inner, &bytes),
        (&Method::PUT, false) => put(inner, key, &parts.headers, bytes.to_vec()),
        (&Method::GET | &Method::HEAD, false) => {
            get(inner, key, &parts.headers, parts.method == Method::HEAD)
        }
        (&Method::DELETE, false) => {
            inner.objects.lock().unwrap().remove(key);
            Response::builder().status(204).body(Body::empty()).unwrap()
        }
        _ => xml_error(501, "NotImplemented", "not implemented by the test server"),
    }
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn put(inner: &Inner, key: &str, headers: &HeaderMap, data: Vec<u8>) -> Response<Body> {
    let mut objects = inner.objects.lock().unwrap();
    if let Some(src) = headers
        .get("x-amz-copy-source")
        .and_then(|v| v.to_str().ok())
    {
        let src = percent_decode(src.trim_start_matches('/'));
        let src_key = src.split_once('/').map_or("", |(_, k)| k);
        return match objects.get(src_key).cloned() {
            Some(d) => {
                let tag = etag(&d);
                objects.insert(key.to_owned(), d);
                Response::new(Body::from(format!(
                    "<CopyObjectResult><ETag>{tag}</ETag><LastModified>2026-01-01T00:00:00.000Z</LastModified></CopyObjectResult>"
                )))
            }
            None => xml_error(404, "NoSuchKey", "no such key"),
        };
    }
    if headers.get(header::IF_NONE_MATCH).is_some_and(|v| v == "*") && objects.contains_key(key) {
        return xml_error(412, "PreconditionFailed", "exists");
    }
    let tag = etag(&data);
    objects.insert(key.to_owned(), data);
    Response::builder()
        .status(200)
        .header(header::ETAG, tag)
        .body(Body::empty())
        .unwrap()
}

fn get(inner: &Inner, key: &str, headers: &HeaderMap, head: bool) -> Response<Body> {
    let objects = inner.objects.lock().unwrap();
    let Some(data) = objects.get(key) else {
        return xml_error(404, "NoSuchKey", "no such key");
    };
    let total = data.len();
    let mut builder = Response::builder()
        .header(header::ETAG, etag(data))
        .header(header::LAST_MODIFIED, "Thu, 01 Jan 2026 00:00:00 GMT")
        .header(header::ACCEPT_RANGES, "bytes");
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|r| {
            let (a, b) = r.split_once('-')?;
            let start: usize = a.parse().ok()?;
            let end: usize = if b.is_empty() {
                total.saturating_sub(1)
            } else {
                b.parse().ok()?
            };
            Some((start, end.min(total.saturating_sub(1))))
        });
    let (status, slice) = match range {
        Some((s, e)) if s <= e && s < total => {
            builder = builder.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{total}"));
            (206, data[s..=e].to_vec())
        }
        _ => (200, data.clone()),
    };
    builder = builder
        .status(status)
        .header(header::CONTENT_LENGTH, slice.len());
    if head {
        builder.body(Body::empty()).unwrap()
    } else {
        builder.body(Body::from(slice)).unwrap()
    }
}

fn list(inner: &Inner, q: &BTreeMap<String, String>) -> Response<Body> {
    let prefix = q.get("prefix").cloned().unwrap_or_default();
    let delimiter = q.get("delimiter").cloned().unwrap_or_default();
    let after = q
        .get("continuation-token")
        .or_else(|| q.get("start-after"))
        .cloned()
        .unwrap_or_default();
    let page = *inner.page_size.lock().unwrap();
    let max = q
        .get("max-keys")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1000)
        .min(page);
    let objects = inner.objects.lock().unwrap();
    let mut contents = Vec::new();
    let mut prefixes = BTreeSet::new();
    let mut count = 0;
    let mut truncated = false;
    let mut last = String::new();
    for (k, v) in objects
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix) && k.as_str() > after.as_str())
    {
        let rest = &k[prefix.len()..];
        if !delimiter.is_empty()
            && let Some(i) = rest.find(&delimiter)
        {
            let cp = format!("{prefix}{}", &rest[..i + delimiter.len()]);
            if prefixes.contains(&cp) {
                continue;
            }
            if count >= max {
                truncated = true;
                break;
            }
            prefixes.insert(cp);
            count += 1;
            last = k.clone();
            continue;
        }
        if count >= max {
            truncated = true;
            break;
        }
        contents.push((k.clone(), v.len(), etag(v)));
        count += 1;
        last = k.clone();
    }
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    xml.push_str(&format!(
        "<Name>{}</Name><Prefix>{}</Prefix><KeyCount>{count}</KeyCount><MaxKeys>{max}</MaxKeys><IsTruncated>{truncated}</IsTruncated>",
        esc(&inner.bucket),
        esc(&prefix)
    ));
    for (k, size, tag) in contents {
        xml.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified><ETag>{}</ETag><Size>{size}</Size><StorageClass>STANDARD</StorageClass></Contents>",
            esc(&k),
            esc(&tag)
        ));
    }
    for p in prefixes {
        xml.push_str(&format!(
            "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
            esc(&p)
        ));
    }
    if truncated {
        xml.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            esc(&last)
        ));
    }
    xml.push_str("</ListBucketResult>");
    Response::builder()
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(xml))
        .unwrap()
}

fn multi_delete(inner: &Inner, body: &[u8]) -> Response<Body> {
    let text = String::from_utf8_lossy(body);
    let mut deleted = String::new();
    let mut rest = text.as_ref();
    while let Some(i) = rest.find("<Key>") {
        let after = &rest[i + 5..];
        let Some(j) = after.find("</Key>") else { break };
        let key = after[..j]
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">");
        inner.objects.lock().unwrap().remove(&key);
        deleted.push_str(&format!("<Deleted><Key>{}</Key></Deleted>", esc(&key)));
        rest = &after[j + 6..];
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(format!(
            "<DeleteResult>{deleted}</DeleteResult>"
        )))
        .unwrap()
}
