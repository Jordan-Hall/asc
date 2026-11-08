//! AWS Signature Version 4 for the one bucket request `object_store` does not make: reading the
//! bucket's versioning setting (`GET /?versioning`) during a verification. The request has no body
//! and goes through the network guard's pinned client like every other request to the target.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use asc_store::s3::TargetConfig;

type HmacSha256 = Hmac<Sha256>;

/// The SHA-256 of an empty body, hex.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC takes a key of any length.
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// RFC 3986 unreserved characters kept, everything else percent-encoded (SigV4 URI encoding).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `(YYYYMMDD, YYYYMMDDTHHMMSSZ)` in UTC of a unix time.
fn stamps(unix: i64) -> (String, String) {
    let (y, m, d) = crate::ai::clock::ymd(unix);
    let s = unix.rem_euclid(86_400);
    let day = format!("{y:04}{m:02}{d:02}");
    let at = format!("{day}T{:02}{:02}{:02}Z", s / 3600, s / 60 % 60, s % 60);
    (day, at)
}

/// A signed `GET` of the bucket's versioning setting: the URL and the headers to send with it.
pub struct Signed {
    pub url: url::Url,
    pub headers: Vec<(&'static str, String)>,
}

/// Signs `GET <bucket>?versioning` for `cfg` at the unix time `now`. `endpoint` is the effective
/// endpoint (the bucket subdomain for virtual-host style).
pub fn versioning_request(
    cfg: &TargetConfig,
    endpoint: &str,
    now: i64,
) -> Result<Signed, url::ParseError> {
    let virtual_host = asc_store::s3::uses_virtual_host(cfg);
    let path = if virtual_host {
        "/".to_owned()
    } else {
        format!("/{}", encode(&cfg.bucket))
    };
    let mut url = url::Url::parse(endpoint)?;
    url.set_path(&path);
    url.set_query(Some("versioning"));
    let host = url[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
    let (day, at) = stamps(now);
    let mut headers: Vec<(&'static str, String)> = vec![
        ("host", host),
        ("x-amz-content-sha256", EMPTY_SHA256.to_owned()),
        ("x-amz-date", at.clone()),
    ];
    if let Some(token) = &cfg.credentials.session_token {
        headers.push(("x-amz-security-token", token.clone()));
    }
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");
    let canonical =
        format!("GET\n{path}\nversioning=\n{canonical_headers}\n{signed}\n{EMPTY_SHA256}");
    let scope = format!("{day}/{}/s3/aws4_request", cfg.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{at}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let k = hmac(
        format!("AWS4{}", cfg.credentials.secret_access_key).as_bytes(),
        day.as_bytes(),
    );
    let k = hmac(&k, cfg.region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    let signature = hex(&hmac(&k, to_sign.as_bytes()));
    headers.push((
        "authorization",
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            cfg.credentials.access_key_id
        ),
    ));
    // reqwest sets Host from the URL; the value signed is the same.
    headers.retain(|(k, _)| *k != "host");
    Ok(Signed { url, headers })
}

/// The versioning status in a `GetBucketVersioning` answer: `Enabled`, `Suspended`, or `None`
/// when the bucket never had versioning.
pub fn versioning_status(body: &str) -> Option<&str> {
    let start = body.find("<Status>")? + "<Status>".len();
    let end = body[start..].find("</Status>")? + start;
    Some(body[start..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_store::s3::{Addressing, TargetCredentials};

    fn cfg(addressing: Addressing) -> TargetConfig {
        TargetConfig {
            endpoint: "https://s3.amazonaws.com".into(),
            region: "us-east-1".into(),
            bucket: "examplebucket".into(),
            prefix: String::new(),
            addressing,
            credentials: TargetCredentials {
                access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
                session_token: None,
            },
            allow_http: false,
        }
    }

    #[test]
    fn dates_are_utc_basic_format() {
        assert_eq!(
            stamps(1_369_353_600),
            ("20130524".to_owned(), "20130524T000000Z".to_owned())
        );
        assert_eq!(stamps(1_700_000_000).1, "20231114T221320Z");
    }

    #[test]
    fn the_request_names_the_bucket_and_is_signed_for_its_region() {
        let c = cfg(Addressing::Path);
        let s = versioning_request(&c, "https://s3.example.org:9000", 1_369_353_600).unwrap();
        assert_eq!(
            s.url.as_str(),
            "https://s3.example.org:9000/examplebucket?versioning"
        );
        let auth = &s
            .headers
            .iter()
            .find(|(k, _)| *k == "authorization")
            .unwrap()
            .1;
        assert!(
            auth.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="
            ),
            "{auth}"
        );
        assert!(!auth.contains("wJalr"), "the secret is not sent");
        let v = cfg(Addressing::VirtualHost);
        let s = versioning_request(&v, "https://examplebucket.s3.amazonaws.com", 0).unwrap();
        assert_eq!(
            s.url.as_str(),
            "https://examplebucket.s3.amazonaws.com/?versioning"
        );
        let mut t = cfg(Addressing::Path);
        t.credentials.session_token = Some("tok".into());
        let s = versioning_request(&t, "https://s3.example.org", 0).unwrap();
        assert!(
            s.headers
                .iter()
                .any(|(k, v)| *k == "x-amz-security-token" && v == "tok")
        );
        assert!(
            s.headers
                .iter()
                .any(|(k, v)| *k == "authorization" && v.contains("x-amz-security-token"))
        );
    }

    #[test]
    fn the_status_is_read_from_the_answer() {
        let on = r#"<?xml version="1.0"?><VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#;
        assert_eq!(versioning_status(on), Some("Enabled"));
        assert_eq!(
            versioning_status(
                "<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>"
            ),
            Some("Suspended")
        );
        assert_eq!(versioning_status("<VersioningConfiguration/>"), None);
    }
}
