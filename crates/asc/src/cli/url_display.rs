//! Display form of a remote URL, as git's `transport_anonymize_url`: the userinfo of a URL form
//! (`scheme://user:secret@host/...`) is dropped, and the password of an scp-like form
//! (`user:secret@host:path`) is dropped while the user is kept. Every place that prints or records
//! a remote URL (`From`, `To`, errors, reflog messages, merge messages) goes through here, so a
//! token embedded in a URL never reaches a terminal, a CI log or a ref log.

/// Query parameter names (lower case, substring match) whose values may hold a credential.
const SECRET_KEYS: [&str; 11] = [
    "secret",
    "token",
    "password",
    "passwd",
    "pwd",
    "key",
    "sig",
    "credential",
    "auth",
    "session",
    "x-amz-",
];

/// `url` with credentials removed, otherwise unchanged: userinfo, in the URL and in any URL
/// nested in its query, and the values of secret-bearing query parameters (replaced by `***`).
/// The host shown is always the host the transport parses; when the two could differ, or the URL
/// does not parse and its authority is suspect, the authority is shown as `***`.
pub fn anonymize(url: &str) -> String {
    match scheme_len(url) {
        Some(n) => anonymize_url(url, n),
        None => anonymize_scp(url),
    }
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

fn finish(base_and_tail: &str) -> String {
    let (base, tail) = match base_and_tail.find(['?', '#']) {
        Some(i) => base_and_tail.split_at(i),
        None => (base_and_tail, ""),
    };
    format!("{base}{}", anonymize_tail(tail))
}

fn anonymize_url(url: &str, n: usize) -> String {
    let (head, rest) = url.split_at(n);
    let Ok(parsed) = url::Url::parse(url) else {
        // The transport cannot parse it: never guess a host from text after an `@`.
        let region_end = rest.find(['/', char::from(92u8)]);
        let region = &rest[..region_end.unwrap_or(rest.len())];
        if region.contains(['@', '?', '#']) {
            return match region_end {
                Some(i) => format!("{head}***/{}", &rest[i + 1..]),
                None => format!("{head}***"),
            };
        }
        return finish(url);
    };
    let special = matches!(parsed.scheme(), "http" | "https" | "ftp" | "ws" | "wss" | "file");
    let end = (if special { rest.find(['/', char::from(92u8), '?', '#']) } else { rest.find(['/', '?', '#']) })
        .unwrap_or(rest.len());
    let auth = &rest[..end];
    let host_text = auth.rfind('@').map_or(auth, |at| &auth[at + 1..]);
    let bare = if host_text.ends_with(']') { host_text } else { host_text.rsplit_once(':').map_or(host_text, |(h, _)| h) };
    let agrees = parsed.host_str().is_some_and(|h| h.eq_ignore_ascii_case(bare)) || (parsed.host_str().is_none() && bare.is_empty());
    if agrees {
        return finish(&format!("{head}{host_text}{}", &rest[end..]));
    }
    // Display from the parsed parts, so it names exactly the host the transport uses.
    let mut clean = parsed;
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    finish(clean.as_str())
}

/// scp-like `user@host:path`, or a plain path.
fn anonymize_scp(url: &str) -> String {
    let slash = url.find('/').unwrap_or(url.len());
    let Some(at) = url[..slash].rfind('@') else {
        return url.to_owned();
    };
    let (user, after) = (&url[..at], &url[at + 1..]);
    if !user.contains([':', '?', '#']) {
        return url.to_owned();
    }
    match after.split_once(':') {
        Some((host, _)) if !host.is_empty() && !host.contains(['@', '?', '#']) => format!("***@{after}"),
        _ => "***".to_owned(),
    }
}

fn mask_fragment(f: &str) -> String {
    if f.contains('@') { "***".to_owned() } else { strip_nested(f) }
}

/// `?query#fragment`: secret values hidden, nested URLs stripped of userinfo.
fn anonymize_tail(tail: &str) -> String {
    if let Some(f) = tail.strip_prefix('#') {
        return format!("#{}", mask_fragment(f));
    }
    let Some(rest) = tail.strip_prefix('?') else {
        return tail.to_owned();
    };
    let (query, fragment) = rest
        .split_once('#')
        .map_or((rest, None), |(q, f)| (q, Some(f)));
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| {
            let Some((key, value)) = pair.split_once('=') else {
                return if pair.contains('@') { "***".to_owned() } else { pair.to_owned() };
            };
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
        out.push_str(&mask_fragment(f));
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
        assert_eq!(
            anonymize("https://user:TOKEN@host/repo.git"),
            "https://host/repo.git"
        );
        assert_eq!(
            anonymize("https://TOKEN@host:8443/repo"),
            "https://host:8443/repo"
        );
        assert_eq!(
            anonymize("s3://u:p%40ss@bucket/pre?endpoint=http://x"),
            "s3://bucket/pre?endpoint=http://x"
        );
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
        assert_eq!(anonymize("user:TOKEN@host:o/r.git"), "***@host:o/r.git");
    }

    #[test]
    fn file_and_plain_paths_are_unchanged() {
        assert_eq!(anonymize("file:///tmp/b"), "file:///tmp/b");
        assert_eq!(anonymize("/tmp/a@b/c"), "/tmp/a@b/c");
        assert_eq!(anonymize("C:/repos/x"), "C:/repos/x");
    }

    #[test]
    fn ipv6_host_is_kept() {
        assert_eq!(
            anonymize("https://user:TOKEN@[::1]:8080/r"),
            "https://[::1]:8080/r"
        );
        assert_eq!(
            anonymize("ssh://git@[2001:db8::1]/r"),
            "ssh://[2001:db8::1]/r"
        );
    }

    #[test]
    fn authority_ends_at_query_or_fragment() {
        assert_eq!(
            anonymize("s3://u:pw@bucket?region=x"),
            "s3://bucket?region=x"
        );
        assert_eq!(anonymize("https://u:pw@host#x"), "https://host#x");
        assert_eq!(anonymize("https://host/?a=b@c"), "https://host/?a=b@c");
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
        assert_eq!(
            anonymize("s3://b/p?endpoint=http%3A%2F%2Fk%3As%40h"),
            "s3://b/p?endpoint=***"
        );
    }

    #[test]
    fn secret_query_parameters_are_hidden() {
        assert_eq!(
            anonymize(
                "s3://b/p?access_key=AK&Secret=S&token=T&password=P&sig=G&X-Amz-Signature=Z&region=eu"
            ),
            "s3://b/p?access_key=***&Secret=***&token=***&password=***&sig=***&X-Amz-Signature=***&region=eu"
        );
        assert_eq!(
            anonymize("s3://b/p?path_style=true"),
            "s3://b/p?path_style=true"
        );
    }

    #[test]
    fn scp_like_path_containing_a_scheme_separator_still_hides_the_password() {
        assert_eq!(anonymize("user:pw@host:path/x://y"), "***@host:path/x://y");
        assert_eq!(anonymize("u:pw@host:a?b=c://d"), "***@host:a?b=c://d");
    }

    #[test]
    fn display_never_names_a_different_host_than_the_transport() {
        assert_eq!(anonymize("https://evil.com?@good.com/"), "https://evil.com?***");
        assert_eq!(anonymize("https://evil.com#@good.com/"), "https://evil.com#***");
    }

    #[test]
    fn unparsable_suspect_authorities_are_masked() {
        let shown = anonymize("https://u:p?w@host/");
        assert!(!shown.contains("p?w") && !shown.contains("u:p"), "{shown}");
        assert_eq!(shown, "https://***/");
        assert_eq!(anonymize("https://u:p#w@host/r"), "https://***/r");
        assert_eq!(anonymize("u:p?w@host:path"), "***@host:path");
        assert_eq!(anonymize("u:p@@x"), "***");
    }

    #[test]
    fn query_and_fragment_characters_inside_userinfo_do_not_leak() {
        for url in ["https://us?er:pw@host/r", "https://us#er:pw@host/r?x=1", "https://u:p?w@host/", "https://u:p#w@host/"] {
            let shown = anonymize(url);
            assert!(!shown.contains("pw") && !shown.contains("p?w") && !shown.contains("p#w"), "{url} -> {shown}");
        }
    }
}
