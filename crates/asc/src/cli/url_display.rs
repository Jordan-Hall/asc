//! Display form of a remote URL, as git's `transport_anonymize_url`: the userinfo of a URL form
//! (`scheme://user:secret@host/...`) is dropped, and the password of an scp-like form
//! (`user:secret@host:path`) is dropped while the user is kept. Every place that prints or records
//! a remote URL (`From`, `To`, errors, reflog messages, merge messages) goes through here, so a
//! token embedded in a URL never reaches a terminal, a CI log or a ref log.

/// `url` with credentials removed, otherwise unchanged.
pub fn anonymize(url: &str) -> String {
    if let Some(i) = url.find("://") {
        let (head, rest) = url.split_at(i + 3);
        let authority_end = rest.find('/').unwrap_or(rest.len());
        return match rest[..authority_end].rfind('@') {
            Some(at) => format!("{head}{}", &rest[at + 1..]),
            None => url.to_owned(),
        };
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
}
