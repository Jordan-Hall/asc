//! Discovery of an asc server behind an `https://` remote (SPEC §6.2).
//!
//! `GET <origin>/.well-known/asc` returns the server's address, the optional `objects` tier list
//! and `seeds` URL, the per-repository `mode` and the oldest client the server accepts. The
//! answer is untrusted: the body is bounded, every URL is parsed and checked, and a document
//! that does not validate counts as "no asc server here" (git-remote mode), never as a partial
//! answer. The result is cached in `state.db` for [`TTL`] (a 404 for [`MISS_TTL`]); a request
//! that fails to reach the host is not cached and falls back to git, so git reports the
//! network failure itself.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asc_state::StateDb;
use url::Url;

/// How long a discovery document is trusted (SPEC §6.2: one hour).
pub const TTL: Duration = Duration::from_secs(3600);
/// How long "this host has no asc server" is remembered.
pub const MISS_TTL: Duration = Duration::from_secs(60);
/// Largest document read.
pub const MAX_BODY: usize = 64 * 1024;
/// Most `objects` tier URLs accepted.
pub const MAX_OBJECTS: usize = 8;
/// Longest URL accepted in a document.
const MAX_URL: usize = 2048;
/// Most `mode` entries kept.
const MAX_MODES: usize = 10_000;

/// A validated `/.well-known/asc` document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    /// The gRPC endpoint: scheme, host and port only.
    pub server: String,
    /// Object tier URLs, in order (may be empty).
    pub objects: Vec<String>,
    /// The seed download base, when the server has one.
    pub seeds: Option<String>,
    /// `<owner>/<repo>` to `mirror` or `primary`.
    pub mode: BTreeMap<String, String>,
    /// The oldest client version the server accepts.
    pub min_client: Option<String>,
}

impl Document {
    /// Whether `repo` (`owner/name`) is served in mirror mode (pushes go to the forge).
    pub fn is_mirror(&self, repo: &str) -> bool {
        self.mode.get(repo).is_some_and(|m| m == "mirror")
    }
}

/// Why a document was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiscoverError {
    #[error("the discovery document is larger than {MAX_BODY} bytes")]
    TooLarge,
    #[error("the discovery document is not valid JSON")]
    NotJson,
    #[error("the discovery document has no usable '{0}'")]
    Field(&'static str),
}

/// A URL the document names, reduced to what a client may use: http(s), a host, no userinfo,
/// no query or fragment.
fn clean_url(text: &str, keep_path: bool) -> Option<String> {
    if text.len() > MAX_URL || text.chars().any(char::is_control) {
        return None;
    }
    let mut u = Url::parse(text).ok()?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
        return None;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    u.set_query(None);
    u.set_fragment(None);
    if !keep_path {
        u.set_path("");
    }
    Some(u.as_str().trim_end_matches('/').to_owned())
}

/// Parses and validates a document.
pub fn parse(body: &[u8]) -> Result<Document, DiscoverError> {
    if body.len() > MAX_BODY {
        return Err(DiscoverError::TooLarge);
    }
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| DiscoverError::NotJson)?;
    let obj = v.as_object().ok_or(DiscoverError::NotJson)?;
    let server = obj
        .get("server")
        .and_then(|s| s.as_str())
        .and_then(|s| clean_url(s, false))
        .ok_or(DiscoverError::Field("server"))?;
    let mut objects = Vec::new();
    match obj.get("objects") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Array(items)) => {
            if items.len() > MAX_OBJECTS {
                return Err(DiscoverError::Field("objects"));
            }
            for item in items {
                let url = item
                    .as_str()
                    .and_then(|s| clean_url(s, false))
                    .ok_or(DiscoverError::Field("objects"))?;
                objects.push(url);
            }
        }
        Some(_) => return Err(DiscoverError::Field("objects")),
    }
    let seeds = match obj.get("seeds") {
        None | Some(serde_json::Value::Null) => None,
        Some(s) => Some(
            s.as_str()
                .and_then(|s| clean_url(s, true))
                .ok_or(DiscoverError::Field("seeds"))?,
        ),
    };
    let mut mode = BTreeMap::new();
    if let Some(m) = obj.get("mode") {
        let map = m.as_object().ok_or(DiscoverError::Field("mode"))?;
        for (repo, value) in map.iter().take(MAX_MODES) {
            if let Some(word) = value.as_str()
                && matches!(word, "mirror" | "primary")
            {
                mode.insert(repo.clone(), word.to_owned());
            }
        }
    }
    let min_client = match obj.get("min_client") {
        None | Some(serde_json::Value::Null) => None,
        Some(s) => {
            let s = s.as_str().ok_or(DiscoverError::Field("min_client"))?;
            if s.len() > 64 || !s.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return Err(DiscoverError::Field("min_client"));
            }
            Some(s.to_owned())
        }
    };
    Ok(Document {
        server,
        objects,
        seeds,
        mode,
        min_client,
    })
}

/// Whether `ours` (`x.y.z`) is older than `min`.
pub fn older_than(ours: &str, min: &str) -> bool {
    let parts = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(ours), parts(min));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    false
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

async fn ensure_table(db: &StateDb) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS discovery_cache (\
         origin TEXT PRIMARY KEY, fetched_at INTEGER NOT NULL, body TEXT NOT NULL)",
    )
    .execute(db.pool())
    .await
    .map(|_| ())
}

/// A cached answer: `Some(None)` is a remembered miss, `None` is nothing usable.
async fn cached(db: &StateDb, origin: &str) -> Option<Option<Document>> {
    ensure_table(db).await.ok()?;
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT fetched_at, body FROM discovery_cache WHERE origin = $1")
            .bind(origin.to_owned())
            .fetch_optional(db.pool())
            .await
            .ok()?;
    let (at, body) = row?;
    let age = now().saturating_sub(at);
    if age < 0 {
        return None;
    }
    let age = Duration::from_secs(age.unsigned_abs());
    if body.is_empty() {
        return (age < MISS_TTL).then_some(None);
    }
    if age >= TTL {
        return None;
    }
    parse(body.as_bytes()).ok().map(Some)
}

async fn store(db: &StateDb, origin: &str, body: &str) {
    if ensure_table(db).await.is_err() {
        return;
    }
    let _ = sqlx::query(
        "INSERT INTO discovery_cache (origin, fetched_at, body) VALUES ($1, $2, $3) \
         ON CONFLICT (origin) DO UPDATE SET fetched_at = excluded.fetched_at, body = excluded.body",
    )
    .bind(origin.to_owned())
    .bind(now())
    .bind(body.to_owned())
    .execute(db.pool())
    .await;
}

/// Reads at most [`MAX_BODY`] bytes of a response.
async fn bounded(mut response: reqwest::Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len() + chunk.len() > MAX_BODY {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

/// The origin (`scheme://host[:port]`) of `url`, as the cache key and the discovery host.
pub fn origin_of(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{host}{port}", url.scheme()))
}

/// The discovery document of `url`'s host: cached, else fetched. `None` means no asc server
/// (a 4xx answer, an unreadable document, or a host that could not be reached).
pub async fn lookup(db: &StateDb, url: &Url) -> Option<Document> {
    let origin = origin_of(url)?;
    if let Some(hit) = cached(db, &origin).await {
        return hit;
    }
    let client = reqwest::Client::builder()
        .redirect(crate::credential::redirect_policy())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("asc/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let response = client
        .get(format!("{origin}/.well-known/asc"))
        .send()
        .await
        .ok()?;
    let status = response.status();
    if status.is_client_error() {
        store(db, &origin, "").await;
        return None;
    }
    if !status.is_success() {
        return None;
    }
    let body = bounded(response).await?;
    let doc = parse(&body).ok()?;
    store(db, &origin, std::str::from_utf8(&body).ok()?).await;
    Some(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_spec_document() {
        let doc = parse(
            br#"{"server":"https://asc.example:443","objects":["https://c1.example:7890"],
            "seeds":"https://seeds.example/s","repos":"*",
            "mode":{"o/r":"mirror","o/p":"primary","o/x":"weird"},"min_client":"0.1.0"}"#,
        )
        .unwrap();
        assert_eq!(doc.server, "https://asc.example");
        assert_eq!(doc.objects, ["https://c1.example:7890"]);
        assert_eq!(doc.seeds.as_deref(), Some("https://seeds.example/s"));
        assert!(doc.is_mirror("o/r"));
        assert!(!doc.is_mirror("o/p"));
        assert!(!doc.is_mirror("o/x"));
        assert_eq!(doc.min_client.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn refuses_hostile_documents() {
        for bad in [
            r#"{"server":"ftp://x"}"#,
            r#"{"server":"https://u:p@x"}"#,
            r#"{"server":"https://x","objects":"https://y"}"#,
            r#"{"server":"https://x","objects":["javascript:1"]}"#,
            r#"{"server":"https://x","min_client":"1.0; rm"}"#,
            r#"{"server":"https://x","seeds":5}"#,
            r#"[1]"#,
            r#"nope"#,
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad}");
        }
        let many = format!(
            r#"{{"server":"https://x","objects":[{}]}}"#,
            [r#""https://y""#; MAX_OBJECTS + 1].join(",")
        );
        assert!(parse(many.as_bytes()).is_err());
        assert_eq!(
            parse(&vec![b' '; MAX_BODY + 1]),
            Err(DiscoverError::TooLarge)
        );
    }

    #[test]
    fn compares_versions() {
        assert!(older_than("0.1.0", "0.2.0"));
        assert!(older_than("1.9", "1.10.0"));
        assert!(!older_than("1.0.0", "1.0.0"));
        assert!(!older_than("2.0.0", "1.9.9"));
    }
}
