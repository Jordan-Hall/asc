//! Display form of a remote URL, as git's `transport_anonymize_url`: the userinfo of a URL form
//! (`scheme://user:secret@host/...`) is dropped, and the password of an scp-like form
//! (`user:secret@host:path`) is dropped while the user is kept. Every place that prints or records
//! a remote URL (`From`, `To`, errors, reflog messages, merge messages) goes through here, so a
//! token embedded in a URL never reaches a terminal, a CI log or a ref log.

/// Query parameter names (lower case, substring match) whose values may hold a credential.
const SECRET_KEYS: [&str; 11] =
    ["secret", "token", "password", "passwd", "pwd", "key", "sig", "credential", "auth", "session", "x-amz-"];

/// `url` with credentials removed, otherwise unchanged: userinfo, in the URL and in any URL
/// nested in its query, and the values of secret-bearing query parameters (replaced by `***`).
pub fn anonymize(url: &str) -> String {
    let (base, tail) = match url.find(['?', '#']) {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    let mut out = anonymize_base(base);
    out.push_str(&anonymize_tail(tail));
    out
}

/// The length of a leading `scheme://`, if `url` starts with one.
fn scheme_len(url: &str) -> Option<usize> {
    let i = url.find("://")?;
    let scheme = &url[..i];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')))
        .then_some(i + 3)
}

/// The part before any `?` or `#`.
fn anonymize_base(url: &str) -> String {
    if let Some(n) = scheme_len(url) {
        let (head, rest) = url.split_at(n);
        let authority_end = rest.find(['/', char::from(92u8)]).unwrap_or(rest.len());
        let out = match rest[..authority_end].rfind('@') {
            Some(at) => format!("{head}{}", &rest[at + 1..]),
            None => url.to_owned(),
        };
        // The transport's parser must agree: if it still sees credentials, let it strip them.
        if let Ok(mut parsed) = url::Url::parse(&out)
            && (!parsed.username().is_empty() || parsed.password().is_some())
            && parsed.set_username("").is_ok()
            && parsed.set_password(None).is_ok()
        {
            return parsed.to_string();
        }
        return out;
    }
    // scp-like `user[:password]@host:path`, or a plain path.
    let slash = url.find('/').unwrap_or(url.len());
    let Some(at) = url[..slash].rfind('@') else {
        return url.to_owned();
    };
    let user = &url[..at];
    let user = user.split_once(':').map_or(user, |(u, _)| u);
    format!("{user}{}", &url[at..])
}

/// `?query#fragment`: secret values hidden, nested URLs stripped of userinfo.
fn anonymize_tail(tail: &str) -> String {
    let Some(rest) = tail.strip_prefix('?') else {
        return tail.to_owned();
    };
    let (query, fragment) = rest.split_once('#').map_or((rest, None), |(q, f)| (q, Some(f)));
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let lower = key.to_ascii_lowercase();
            if SECRET_KEYS.iter().any(|k| lower.contains(k)) {
                return format!("{key}=***");
            }
            let encoded = value.to_ascii_lowercase();
            if encoded.contains("%40") || encoded.contains("%2f%2f") {
                return format!("{key}=***");
            }
            format!("{key}={}", strip_nested(value))
        })
        .collect();
    let mut out = format!("?{}", pairs.join("&"));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(&strip_nested(f));
    }
    out
}

/// Drops the userinfo of every `scheme://user:pw@host` substring of `text`.
fn strip_nested(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
        let at = tail[..end].rfind('@').map_or(0, |a| a + 1);
        out.push_str(&tail[at..end]);
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::anonymize;

    #[test]
    fn strips_user_and_password_from_url_forms() {
        assert_eq!(anonymize("https://user:TOKEN@host/repo.git"), "https://host/repo.git");
        assert_eq!(anonymize("https://TOKEN@host:8443/repo"), "https://host:8443/repo");
        assert_eq!(anonymize("s3://u:p%40ss@bucket/pre?endpoint=http://x"), "s3://bucket/pre?endpoint=http://x");
    }

    #[test]
    fn password_with_raw_at_is_fully_removed() {
        assert_eq!(anonymize("https://user:pa@ss@host/r"), "https://host/r");
    }

    #[test]
    fn at_sign_after_the_authority_is_kept() {
        assert_eq!(anonymize("https://host/path@x"), "https://host/path@x");
    }

    #[test]
    fn scp_like_keeps_user_drops_password() {
        assert_eq!(anonymize("git@host:o/r.git"), "git@host:o/r.git");
        assert_eq!(anonymize("user:TOKEN@host:o/r.git"), "user@host:o/r.git");
    }

    #[test]
    fn file_and_plain_paths_are_unchanged() {
        assert_eq!(anonymize("file:///tmp/b"), "file:///tmp/b");
        assert_eq!(anonymize("/tmp/a@b/c"), "/tmp/a@b/c");
        assert_eq!(anonymize("C:/repos/x"), "C:/repos/x");
    }

    #[test]
    fn ipv6_host_is_kept() {
        assert_eq!(anonymize("https://user:TOKEN@[::1]:8080/r"), "https://[::1]:8080/r");
        assert_eq!(anonymize("ssh://git@[2001:db8::1]/r"), "ssh://[2001:db8::1]/r");
    }

    #[test]
    fn authority_ends_at_query_or_fragment() {
        assert_eq!(anonymize("s3://u:pw@bucket?region=x"), "s3://bucket?region=x");
        assert_eq!(anonymize("https://u:pw@host#x"), "https://host#x");
        assert_eq!(anonymize("https://host?a=b@c"), "https://host?a=b@c");
    }

    #[test]
    fn nested_urls_in_the_query_lose_their_userinfo() {
        assert_eq!(
            anonymize("s3://bucket/p?endpoint=https://key:secret@minio:9000&region=eu"),
            "s3://bucket/p?endpoint=https://minio:9000&region=eu"
        );
        assert_eq!(
            anonymize("s3://b/p?endpoint=http://k:s@h:1/x?y=z"),
            "s3://b/p?endpoint=http://h:1/x?y=z"
        );
        assert_eq!(anonymize("s3://b/p?endpoint=http%3A%2F%2Fk%3As%40h"), "s3://b/p?endpoint=***");
    }

    #[test]
    fn secret_query_parameters_are_hidden() {
        assert_eq!(
            anonymize("s3://b/p?access_key=AK&Secret=S&token=T&password=P&sig=G&X-Amz-Signature=Z&region=eu"),
            "s3://b/p?access_key=***&Secret=***&token=***&password=***&sig=***&X-Amz-Signature=***&region=eu"
        );
        assert_eq!(anonymize("s3://b/p?path_style=true"), "s3://b/p?path_style=true");
    }

    #[test]
    fn scp_like_path_containing_a_scheme_separator_still_hides_the_password() {
        assert_eq!(anonymize("user:pw@host:path/x://y"), "user@host:path/x://y");
        assert_eq!(anonymize("u:pw@host:a?b=c://d"), "u@host:a?b=c://d");
    }
}
