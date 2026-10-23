//! Display form of a remote URL, in the spirit of git's `transport_anonymize_url`, but built so
//! that it cannot show a secret or name a host the transport would not use.
//!
//! * A `scheme://` URL is parsed with `url::Url` (the parser the transport uses) and rebuilt
//!   only from its parsed parts: scheme, host, port, path, query and fragment. Raw authority text
//!   and the username and password are never copied. A URL that does not parse shows as
//!   `scheme://***`.
//! * Only `region=` and `path_style=` query pairs are shown (and only if the value carries no `@`,
//!   `%40` or `://`); every other pair, and any fragment, is shown as `***`.
//! * An scp-like `user[:pw]@host:path` shows as `host:path`; an ambiguous host shows as `***`.
//! * `file://` URLs without credentials and plain paths are unchanged.
//!
//! Every place that prints or records a remote URL (`From`, `To`, errors, reflog messages, merge
//! messages) goes through [`anonymize`].

/// `url` in a form that is safe to print or record.
pub fn anonymize(url: &str) -> String {
    match scheme_len(url) {
        Some(n) => anonymize_url(url, n),
        None => anonymize_scp(url),
    }
}

/// The length of a leading `scheme://`, if `url` starts with one.
fn scheme_len(url: &str) -> Option<usize> {
    let i = url.find("://")?;
    let mut chars = url[..i].chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
        .then_some(i + 3)
}

fn suspect(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains('@') || lower.contains("%40") || lower.contains("://") || lower.contains("%2f%2f")
}

fn anonymize_url(url: &str, n: usize) -> String {
    let scheme = &url[..n - 3];
    let Ok(parsed) = url::Url::parse(url) else {
        return format!("{scheme}://***");
    };
    let has_credentials = !parsed.username().is_empty() || parsed.password().is_some();
    if parsed.scheme() == "file" && !has_credentials && parsed.query().is_none() && parsed.fragment().is_none() {
        return url.to_owned();
    }
    let mut out = format!("{}://", parsed.scheme());
    if let Some(host) = parsed.host_str() {
        out.push_str(host);
    }
    if let Some(port) = parsed.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str(parsed.path());
    if let Some(query) = parsed.query() {
        let pairs: Vec<String> = query.split('&').map(mask_pair).collect();
        out.push('?');
        out.push_str(&pairs.join("&"));
    }
    if parsed.fragment().is_some() {
        out.push_str("#***");
    }
    out
}

/// Keys the bucket URL parser (`asc-store` `s3::open`) accepts whose values are never secret.
/// `endpoint` is deliberately absent: its value is a URL and always shows as `***`.
const SAFE_KEYS: [&str; 2] = ["region", "path_style"];

/// Only `safe-key=value` pairs are shown; every other pair is `***`.
fn mask_pair(pair: &str) -> String {
    match pair.split_once('=') {
        Some((key, value)) if SAFE_KEYS.contains(&key) && !suspect(value) => pair.to_owned(),
        _ => "***".to_owned(),
    }
}

/// scp-like `user[:password]@host:path` becomes `host:path`; anything without an `@` before the
/// first `/` is a plain path and stays as typed.
fn anonymize_scp(url: &str) -> String {
    let slash = url.find('/').unwrap_or(url.len());
    let Some(at) = url[..slash].rfind('@') else {
        return url.to_owned();
    };
    let after = &url[at + 1..];
    let host = after.split_once(':').map_or(after, |(h, _)| h);
    if host.is_empty() || host.contains(['@', '?', '#', char::from(92u8)]) {
        return "***".to_owned();
    }
    after.to_owned()
}

#[cfg(test)]
mod tests {
    use super::anonymize;

    /// The host a URL displays as, or None when it is masked.
    fn shown_host(url: &str) -> Option<String> {
        url::Url::parse(&anonymize(url)).ok().and_then(|u| u.host_str().map(str::to_owned))
    }

    #[test]
    fn strips_user_and_password_from_url_forms() {
        assert_eq!(anonymize("https://user:TOKEN@host/repo.git"), "https://host/repo.git");
        assert_eq!(anonymize("https://TOKEN@host:8443/repo"), "https://host:8443/repo");
        assert_eq!(anonymize("https://user:pa@ss@host/r"), "https://host/r");
        assert_eq!(anonymize("https://host/path@x"), "https://host/path@x");
    }

    #[test]
    fn authority_ends_at_query_or_fragment() {
        assert_eq!(anonymize("s3://u:pw@bucket?region=x"), "s3://bucket?region=x");
        assert_eq!(anonymize("https://u:pw@host#x"), "https://host/#***");
    }

    #[test]
    fn query_at_sign_never_becomes_the_host() {
        assert_eq!(anonymize("https://host?a=b@c"), "https://host/?***");
        assert_eq!(shown_host("https://host?a=b@c").as_deref(), Some("host"));
        assert_eq!(shown_host("https://evil.com?@good.com/").as_deref(), Some("evil.com"));
        assert_eq!(shown_host("https://evil.com#@good.com/").as_deref(), Some("evil.com"));
        assert_eq!(anonymize("https://evil.com#@good.com/"), "https://evil.com/#***");
    }

    #[test]
    fn unparsable_urls_show_only_the_scheme() {
        for url in ["https://u:p?w@host/", "https://u:p#w@host/r", "https://u:p@w@host:bad/"] {
            assert_eq!(anonymize(url), "https://***", "{url}");
        }
    }

    #[test]
    fn question_mark_and_hash_in_userinfo_do_not_leak() {
        for url in [
            "https://us?er:pw@host/r",
            "https://us#er:pw@host/r?x=1",
            "https://u:p?w@host/",
            "https://u:p#w@host/",
        ] {
            let shown = anonymize(url);
            assert!(!shown.contains("pw") && !shown.contains("p?w") && !shown.contains("p#w"), "{url} -> {shown}");
        }
    }

    #[test]
    fn nested_urls_and_encoded_userinfo_in_the_query_are_masked() {
        assert_eq!(
            anonymize("s3://bucket/p?endpoint=https://key:secret@minio:9000&region=eu"),
            "s3://bucket/p?***&region=eu"
        );
        assert_eq!(anonymize("s3://b/p?endpoint=http%3A%2F%2Fk%3As%40h"), "s3://b/p?***");
        assert_eq!(anonymize("s3://b/p?endpoint=http://127.0.0.1:9000"), "s3://b/p?***");
    }

    #[test]
    fn secret_query_parameters_are_hidden() {
        assert_eq!(
            anonymize("s3://b/p?access_key=AK&Secret=S&token=T&password=P&sig=G&X-Amz-Signature=Z&region=eu"),
            "s3://b/p?***&***&***&***&***&***&region=eu"
        );
        assert_eq!(anonymize("s3://b/p?path_style=true"), "s3://b/p?path_style=true");
    }

    #[test]
    fn a_query_key_can_carry_credentials() {
        assert_eq!(anonymize("s3://b/p?https://u:p@x=1"), "s3://b/p?***");
        assert_eq!(anonymize("s3://b/p?u:p%40x=1&region=eu"), "s3://b/p?***&region=eu");
        assert_eq!(anonymize("s3://b/p?region=http://u:p@h"), "s3://b/p?***");
        assert_eq!(anonymize("s3://b/p?anything=1"), "s3://b/p?***");
    }

    #[test]
    fn scp_like_forms_show_host_and_path_only() {
        assert_eq!(anonymize("git@host:o/r.git"), "host:o/r.git");
        assert_eq!(anonymize("user:TOKEN@host:o/r.git"), "host:o/r.git");
        assert_eq!(anonymize("user:pw@host:path/x://y"), "host:path/x://y");
        assert_eq!(anonymize("u:pw@host:a?b=c://d"), "host:a?b=c://d");
        assert_eq!(anonymize("u:p?w@host:path"), "host:path");
        assert_eq!(anonymize("u@:path"), "***");
        assert_eq!(anonymize("u@h?x:path"), "***");
    }

    #[test]
    fn file_and_plain_paths_are_unchanged() {
        assert_eq!(anonymize("file:///tmp/b"), "file:///tmp/b");
        assert_eq!(anonymize("/tmp/a@b/c"), "/tmp/a@b/c");
        assert_eq!(anonymize("C:/repos/x"), "C:/repos/x");
        assert_eq!(anonymize("host:path"), "host:path");
    }

    #[test]
    fn ipv6_host_is_kept() {
        assert_eq!(anonymize("https://user:TOKEN@[::1]:8080/r"), "https://[::1]:8080/r");
        assert_eq!(anonymize("ssh://git@[2001:db8::1]/r"), "ssh://[2001:db8::1]/r");
    }
}
