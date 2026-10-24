//! `Alt-Svc` (RFC 7838) for the h3 upgrade: the parser, and the per-origin cache.
//!
//! Only `h3` is read (no draft ids), and only an alternative on the origin's own host: an
//! `Alt-Svc` that names another host is ignored, so a server can move its UDP port but can never
//! send a client's credentials to a different machine. The cache is bounded and every entry
//! expires; a failed h3 attempt is remembered too, so a network that drops UDP costs one
//! handshake timeout per backoff and not one per request.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use http::Uri;

/// Origins remembered at once. Past this the entry that expires first is dropped.
pub const MAX_ORIGINS: usize = 256;
/// The default lifetime of an advertisement without `ma` (RFC 7838 section 3.1).
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// The longest lifetime this client honours, whatever `ma` says.
pub const MAX_AGE_CAP: Duration = Duration::from_secs(24 * 3600);
/// How long an origin is not tried over h3 after an h3 failure.
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// Longest `Alt-Svc` value read; the rest is ignored.
const MAX_HEADER_BYTES: usize = 2048;

/// `https://host:port`, the key of the cache. Plain http has no origin here.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Origin {
    /// Lower case, IPv6 without brackets.
    pub host: String,
    pub port: u16,
}

impl Origin {
    /// The origin of `uri`, or `None` unless it is `https` with a host.
    pub fn of(uri: &Uri) -> Option<Self> {
        if uri.scheme_str() != Some("https") {
            return None;
        }
        let host = uri.host()?.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_ascii_lowercase(),
            port: uri.port_u16().unwrap_or(443),
        })
    }
}

/// What one `Alt-Svc` header says about h3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advert {
    /// `h3=":port"` on the origin's host.
    H3 { port: u16, max_age: Duration },
    /// `clear`, or `ma=0`: forget the origin's alternative.
    Clear,
    /// Nothing usable.
    None,
}

/// Reads an `Alt-Svc` value for `host`. The first usable `h3` entry wins; `clear` wins over all.
pub fn parse(value: &str, host: &str) -> Advert {
    let value = truncate(value);
    if value.trim().eq_ignore_ascii_case("clear") {
        return Advert::Clear;
    }
    let mut found = Advert::None;
    for entry in split_unquoted(value, ',') {
        let mut parts = split_unquoted(entry, ';').into_iter();
        let Some((proto, authority)) = parts.next().and_then(|p| p.split_once('=')) else {
            continue;
        };
        if proto.trim() != "h3" {
            continue;
        }
        let authority = authority.trim().trim_matches('"');
        let Some((alt_host, port)) = authority.rsplit_once(':') else {
            continue;
        };
        // An empty host means "the origin's own host"; anything else must equal it.
        let alt_host = alt_host.trim_start_matches('[').trim_end_matches(']');
        if !alt_host.is_empty() && !alt_host.eq_ignore_ascii_case(host) {
            continue;
        }
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        if port == 0 {
            continue;
        }
        let mut max_age = DEFAULT_MAX_AGE;
        for p in parts {
            if let Some((k, v)) = p.split_once('=')
                && k.trim() == "ma"
                && let Ok(secs) = v.trim().trim_matches('"').parse::<u64>()
            {
                max_age = Duration::from_secs(secs);
            }
        }
        if max_age.is_zero() {
            return Advert::Clear;
        }
        if found == Advert::None {
            found = Advert::H3 {
                port,
                max_age: max_age.min(MAX_AGE_CAP),
            };
        }
    }
    found
}

fn truncate(s: &str) -> &str {
    if s.len() <= MAX_HEADER_BYTES {
        return s;
    }
    let mut end = MAX_HEADER_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Splits at `sep` outside double quotes.
fn split_unquoted(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in s.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == sep && !quoted {
            out.push(&s[start..i]);
            start = i + 1;
        }
    }
    out.push(&s[start..]);
    out
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    port: u16,
    expires: Instant,
    /// No h3 attempt until then.
    failed_until: Option<Instant>,
}

/// Which origins advertised h3, and where. Bounded by [`MAX_ORIGINS`].
#[derive(Debug, Default)]
pub struct AltSvcCache {
    map: Mutex<HashMap<Origin, Entry>>,
}

impl AltSvcCache {
    /// Records what a response from `origin` said. `Clear` forgets the origin. A re-advertisement
    /// keeps a running failure backoff.
    pub fn note(&self, origin: &Origin, advert: Advert, now: Instant) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        match advert {
            Advert::None => {}
            Advert::Clear => {
                map.remove(origin);
            }
            Advert::H3 { port, max_age } => {
                let failed_until = map.get(origin).and_then(|e| e.failed_until);
                if !map.contains_key(origin) && map.len() >= MAX_ORIGINS {
                    map.retain(|_, e| e.expires > now);
                    if map.len() >= MAX_ORIGINS
                        && let Some(oldest) = map
                            .iter()
                            .min_by_key(|(_, e)| e.expires)
                            .map(|(o, _)| o.clone())
                    {
                        map.remove(&oldest);
                    }
                }
                map.insert(
                    origin.clone(),
                    Entry {
                        port,
                        expires: now + max_age,
                        failed_until,
                    },
                );
            }
        }
    }

    /// The UDP port to try for `origin`: advertised, unexpired and not in a failure backoff.
    pub fn lookup(&self, origin: &Origin, now: Instant) -> Option<u16> {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let e = *map.get(origin)?;
        if e.expires <= now {
            map.remove(origin);
            return None;
        }
        if e.failed_until.is_some_and(|t| t > now) {
            return None;
        }
        Some(e.port)
    }

    /// An h3 attempt to `origin` failed: no more attempts for [`FAILURE_BACKOFF`].
    pub fn failed(&self, origin: &Origin, now: Instant) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(e) = map.get_mut(origin) {
            e.failed_until = Some(now + FAILURE_BACKOFF);
        }
    }

    /// Origins held.
    pub fn len(&self) -> usize {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h3(port: u16, secs: u64) -> Advert {
        Advert::H3 {
            port,
            max_age: Duration::from_secs(secs),
        }
    }

    fn origin(host: &str) -> Origin {
        Origin {
            host: host.into(),
            port: 443,
        }
    }

    #[test]
    fn reads_h3_on_the_origins_own_host() {
        assert_eq!(parse("h3=\":443\"; ma=3600", "asc.example"), h3(443, 3600));
        assert_eq!(
            parse("h3=\"ASC.example:8443\"; ma=60", "asc.example"),
            h3(8443, 60)
        );
        assert_eq!(parse("h3=\":443\"", "asc.example"), h3(443, 86_400));
    }

    #[test]
    fn another_host_is_ignored() {
        assert_eq!(parse("h3=\"evil.example:443\"", "asc.example"), Advert::None);
        // The next entry for the right host is still used.
        assert_eq!(
            parse("h3=\"evil.example:443\", h3=\":444\"; ma=5", "asc.example"),
            h3(444, 5)
        );
    }

    #[test]
    fn drafts_other_protocols_and_junk_are_ignored() {
        assert_eq!(parse("h3-29=\":443\"", "h"), Advert::None);
        assert_eq!(parse("h2=\":443\"", "h"), Advert::None);
        assert_eq!(parse("h3=\":0\"", "h"), Advert::None);
        assert_eq!(parse("h3=\":99999\"", "h"), Advert::None);
        assert_eq!(parse("h3", "h"), Advert::None);
        assert_eq!(parse("", "h"), Advert::None);
    }

    #[test]
    fn clear_and_zero_age_forget() {
        assert_eq!(parse("clear", "h"), Advert::Clear);
        assert_eq!(parse("h3=\":443\"; ma=0", "h"), Advert::Clear);
    }

    #[test]
    fn age_is_capped() {
        assert_eq!(
            parse("h3=\":443\"; ma=99999999999", "h"),
            Advert::H3 {
                port: 443,
                max_age: MAX_AGE_CAP
            }
        );
    }

    #[test]
    fn commas_inside_quotes_do_not_split() {
        assert_eq!(parse("h2=\"a,b:1\", h3=\":443\"; ma=7", "h"), h3(443, 7));
    }

    #[test]
    fn only_https_has_an_origin() {
        let o = |s: &str| Origin::of(&s.parse().unwrap());
        assert_eq!(o("https://Asc.Example/x").unwrap().port, 443);
        assert_eq!(o("https://asc.example:8443/").unwrap().port, 8443);
        assert_eq!(o("https://asc.example/").unwrap().host, "asc.example");
        assert!(o("http://asc.example/").is_none());
        assert!(o("http://127.0.0.1:1/").is_none());
        assert!(o("/relative").is_none());
    }

    #[test]
    fn entries_expire_and_failures_back_off() {
        let c = AltSvcCache::default();
        let (o, t0) = (origin("a"), Instant::now());
        assert_eq!(c.lookup(&o, t0), None);
        c.note(&o, h3(443, 100), t0);
        assert_eq!(c.lookup(&o, t0), Some(443));
        assert_eq!(c.lookup(&o, t0 + Duration::from_secs(101)), None);
        assert!(c.is_empty(), "an expired entry is dropped on lookup");

        c.note(&o, h3(443, 10_000), t0);
        c.failed(&o, t0);
        assert_eq!(c.lookup(&o, t0 + Duration::from_secs(1)), None);
        // A fresh advertisement does not end the backoff.
        c.note(&o, h3(443, 10_000), t0 + Duration::from_secs(2));
        assert_eq!(c.lookup(&o, t0 + Duration::from_secs(3)), None);
        assert_eq!(
            c.lookup(&o, t0 + FAILURE_BACKOFF + Duration::from_secs(1)),
            Some(443)
        );
    }

    #[test]
    fn clear_removes_the_entry() {
        let c = AltSvcCache::default();
        let (o, t) = (origin("a"), Instant::now());
        c.note(&o, h3(443, 100), t);
        c.note(&o, Advert::Clear, t);
        assert_eq!(c.lookup(&o, t), None);
    }

    #[test]
    fn the_cache_is_bounded() {
        let c = AltSvcCache::default();
        let t = Instant::now();
        for i in 0..MAX_ORIGINS * 2 {
            c.note(&origin(&format!("h{i}")), h3(443, 100 + i as u64), t);
        }
        assert_eq!(c.len(), MAX_ORIGINS);
        // The soonest-to-expire went first; the latest is held.
        assert_eq!(c.lookup(&origin("h0"), t), None);
        assert_eq!(
            c.lookup(&origin(&format!("h{}", MAX_ORIGINS * 2 - 1)), t),
            Some(443)
        );
    }
}
