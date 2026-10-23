//! Git's URL matching for `http.<url>.*` and `credential.<url>.*` (git's `urlmatch.c`).
//!
//! A config subsection names a URL prefix. It applies to a request URL when the scheme is
//! equal, the user (if the prefix has one) is equal, the host is equal or matches with `*`
//! standing for exactly one label (`*.example.com`), the port is equal (a default port and
//! no port are the same) and the prefix path is a path-segment prefix of the request path.
//! The most specific match wins: the longer matched host, then the longer matched path, then
//! a match that includes the user.

use std::cmp::Ordering;

/// A URL reduced to the parts git compares, normalised the way git does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlInfo {
    scheme: String,
    user: Option<String>,
    host: String,
    /// `None` for no port and for the scheme's default port.
    port: Option<String>,
    /// Starts with `/`; dot segments removed, escapes normalised, no query or fragment.
    path: String,
}

/// How specifically a prefix matched; the derived order is git's `cmp_matches`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity {
    host_len: usize,
    path_len: usize,
    user_matched: bool,
}

fn default_port(scheme: &str) -> Option<&'static str> {
    match scheme {
        "http" => Some("80"),
        "https" => Some("443"),
        "ssh" => Some("22"),
        "ftp" => Some("21"),
        _ => None,
    }
}

/// Uppercases `%xx` escapes, decodes escapes of unreserved characters and refuses a bad
/// escape, as git's `url_normalize` does.
fn normalise_escapes(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            let value = u8::from_str_radix(hex, 16).ok()?;
            if value.is_ascii_alphanumeric() || matches!(value, b'-' | b'.' | b'_' | b'~') {
                out.push(value as char);
            } else {
                out.push_str(&format!("%{value:02X}"));
            }
            i += 3;
        } else {
            let c = text[i..].chars().next()?;
            out.push(c);
            i += c.len_utf8();
        }
    }
    Some(out)
}

/// Removes `.` and `..` segments from an absolute path.
fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let trailing = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    for seg in path.split('/').skip(1) {
        match seg {
            "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let mut joined = String::from("/");
    joined.push_str(&out.join("/"));
    if trailing && !joined.ends_with('/') {
        joined.push('/');
    }
    joined
}

impl UrlInfo {
    /// Normalises `url`; `None` when it has no scheme or host or a bad escape. A `*` is
    /// allowed in the host so config prefixes parse; a request URL never contains one.
    pub fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return None;
        }
        let rest = rest.split(['?', '#']).next().unwrap_or("");
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (userinfo, hostport) = match authority.rfind('@') {
            Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
            None => (None, authority),
        };
        let user = match userinfo {
            Some(u) => {
                let name = u.split(':').next().unwrap_or("");
                Some(normalise_escapes(name)?).filter(|n| !n.is_empty())
            }
            None => None,
        };
        let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
            let (inner, after) = v6.split_once(']')?;
            (format!("[{inner}]"), after.strip_prefix(':'))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p)),
                None => (hostport.to_owned(), None),
            }
        };
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c == '\\') {
            return None;
        }
        let scheme = scheme.to_ascii_lowercase();
        let port = match port {
            Some(p) if !p.is_empty() => {
                if !p.chars().all(|c| c.is_ascii_digit()) || p.len() > 5 {
                    return None;
                }
                let trimmed = p.trim_start_matches('0');
                let p = if trimmed.is_empty() { "0" } else { trimmed };
                (default_port(&scheme) != Some(p)).then(|| p.to_owned())
            }
            _ => None,
        };
        let path = remove_dot_segments(&normalise_escapes(path)?);
        Some(Self {
            scheme,
            user,
            host: host.to_ascii_lowercase(),
            port,
            path,
        })
    }

    fn host_wild(&self) -> bool {
        self.host.contains('*')
    }
}

/// Matches whole labels: a `*` label in the prefix matches exactly one label of the request.
fn host_matches(request: &str, prefix: &str) -> bool {
    let mut req = request.split('.');
    let mut pat = prefix.split('.');
    loop {
        match (req.next(), pat.next()) {
            (None, None) => return true,
            (Some(r), Some(p)) if p == "*" || p == r => {}
            _ => return false,
        }
    }
}

/// Whether `prefix` applies to `request`, and how specifically.
pub fn match_url(request: &UrlInfo, prefix: &UrlInfo) -> Option<Specificity> {
    if request.scheme != prefix.scheme {
        return None;
    }
    let user_matched = match &prefix.user {
        Some(u) if request.user.as_ref() != Some(u) => return None,
        Some(_) => true,
        None => false,
    };
    let host_len = if request.host == prefix.host {
        prefix.host.len()
    } else if prefix.host_wild() && host_matches(&request.host, &prefix.host) {
        prefix.host.chars().filter(|c| *c != '*').count()
    } else {
        return None;
    };
    if request.port != prefix.port {
        return None;
    }
    let prefix_path = match prefix.path.as_str() {
        "/" => "",
        p => p.strip_suffix('/').unwrap_or(p),
    };
    let rest = request.path.strip_prefix(prefix_path)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    Some(Specificity {
        host_len,
        path_len: prefix_path.len().max(1),
        user_matched,
    })
}

/// git's cascade for one variable name: feed every entry of that name in config order and
/// it says whether the entry applies. An entry without a URL always applies. An entry for a
/// URL applies when it matches and is at least as specific as every match accepted so far
/// for this variable, so a later, less specific entry does not undo a more specific one.
#[derive(Debug)]
pub struct Cascade {
    request: Option<UrlInfo>,
    best: Option<Specificity>,
}

impl Cascade {
    /// A cascade for requests to `request_url`; one that is not a URL matches no prefix.
    pub fn new(request_url: &str) -> Self {
        Self {
            request: UrlInfo::parse(request_url),
            best: None,
        }
    }

    /// Offers the next entry; `prefix` is the config subsection (`None` for `http.var`).
    pub fn offer(&mut self, prefix: Option<&str>) -> bool {
        let Some(prefix) = prefix else {
            return true;
        };
        let (Some(request), Some(prefix)) = (&self.request, UrlInfo::parse(prefix)) else {
            return false;
        };
        let Some(found) = match_url(request, &prefix) else {
            return false;
        };
        if self.best.is_some_and(|b| found.cmp(&b) == Ordering::Less) {
            return false;
        }
        self.best = Some(found);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(request: &str, prefix: &str) -> Option<Specificity> {
        match_url(&UrlInfo::parse(request).unwrap(), &UrlInfo::parse(prefix)?)
    }

    #[test]
    fn scheme_and_host_must_be_equal_case_insensitively() {
        assert!(matches("https://Example.com/a", "HTTPS://example.COM").is_some());
        assert!(matches("https://example.com/", "http://example.com").is_none());
        assert!(matches("https://example.com/", "https://other.com").is_none());
        assert!(matches("https://sub.example.com/", "https://example.com").is_none());
    }

    #[test]
    fn default_ports_are_dropped_and_other_ports_must_match() {
        assert!(matches("https://h:443/x", "https://h").is_some());
        assert!(matches("https://h/x", "https://h:443").is_some());
        assert!(matches("https://h:8443/x", "https://h:8443").is_some());
        assert!(matches("https://h:8443/x", "https://h").is_none());
        assert!(matches("https://h/x", "https://h:8443").is_none());
        assert!(matches("http://h:80/", "http://h").is_some());
        assert!(UrlInfo::parse("https://h:99999999/").is_none());
        assert!(UrlInfo::parse("https://h:ab/").is_none());
    }

    #[test]
    fn path_prefix_is_by_segment() {
        assert!(matches("https://h/org/repo", "https://h/org").is_some());
        assert!(matches("https://h/org/repo", "https://h/org/").is_some());
        assert!(matches("https://h/org", "https://h/org").is_some());
        assert!(matches("https://h/organisation", "https://h/org").is_none());
        assert!(matches("https://h/a", "https://h/a/b").is_none());
        assert!(matches("https://h/anything", "https://h/").is_some());
        assert!(matches("https://h/a/../b/./c", "https://h/b").is_some());
        assert!(matches("https://h/%7Euser/x", "https://h/~user").is_some());
        assert!(matches("https://h/a%2fb", "https://h/a").is_none());
        assert!(matches("https://h/a?q=1#f", "https://h/a").is_some());
    }

    #[test]
    fn user_in_the_prefix_must_equal_the_request_user() {
        assert!(matches("https://alice@h/x", "https://alice@h").is_some());
        assert!(matches("https://h/x", "https://alice@h").is_none());
        assert!(matches("https://bob@h/x", "https://alice@h").is_none());
        assert!(matches("https://alice:pw@h/x", "https://alice@h").is_some());
        // A prefix without a user matches a request with one.
        assert!(matches("https://alice@h/x", "https://h").is_some());
    }

    #[test]
    fn star_stands_for_one_whole_label() {
        assert!(matches("https://a.example.com/", "https://*.example.com").is_some());
        assert!(matches("https://example.com/", "https://*.example.com").is_none());
        assert!(matches("https://a.b.example.com/", "https://*.example.com").is_none());
        assert!(matches("https://a.b.example.com/", "https://*.*.example.com").is_some());
        assert!(matches("https://a.example.com/", "https://a.*.com").is_some());
        assert!(matches("https://b.example.com/", "https://a.*.com").is_none());
        assert!(matches("https://abc.example.com/", "https://a*.example.com").is_none());
    }

    #[test]
    fn specificity_follows_git() {
        let req = UrlInfo::parse("https://alice@git.example.com/org/repo").unwrap();
        let rank = |p: &str| match_url(&req, &UrlInfo::parse(p).unwrap()).unwrap();
        assert!(rank("https://git.example.com") > rank("https://*.example.com"));
        assert!(rank("https://git.example.com/org") > rank("https://git.example.com"));
        assert!(rank("https://git.example.com/org/repo") > rank("https://git.example.com/org"));
        assert!(rank("https://alice@git.example.com") > rank("https://git.example.com"));
        // Host beats path: an exact host with the root path beats a wildcard host with a path.
        assert!(rank("https://git.example.com") > rank("https://*.example.com/org/repo"));
    }

    #[test]
    fn invalid_prefixes_never_match() {
        for bad in [
            "example.com",
            "https://",
            "://h",
            "https://h/%zz",
            "https://h:x/",
            "",
        ] {
            assert!(UrlInfo::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn cascade_applies_plain_entries_and_the_best_url_entries() {
        let mut c = Cascade::new("https://git.example.com/org/repo");
        assert!(c.offer(None));
        assert!(!c.offer(Some("https://example.org")));
        assert!(c.offer(Some("https://*.example.com")));
        assert!(c.offer(Some("https://git.example.com")));
        // Less specific than what was accepted: ignored.
        assert!(!c.offer(Some("https://*.example.com")));
        // Equal specificity: applies too (multi-valued variables collect them all).
        assert!(c.offer(Some("https://git.example.com/")));
        assert!(c.offer(Some("https://git.example.com/org")));
        assert!(!c.offer(Some("https://git.example.com")));
        assert!(c.offer(None));
    }

    #[test]
    fn a_request_that_is_not_a_url_matches_no_prefix() {
        let mut c = Cascade::new("not a url");
        assert!(c.offer(None));
        assert!(!c.offer(Some("https://h")));
    }
}
