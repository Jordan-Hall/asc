//! Storage targets as the server holds them, and the rules every field must pass.
//!
//! Everything a caller types is checked here before it is stored or used: names, bucket, region,
//! prefix, credentials. The endpoint's own rules are the network guard's.

use std::fmt;

use zeroize::Zeroizing;

use crate::vault::SecretContext;

/// Most targets one owner may hold (`[storage] max_targets_per_owner` default).
pub const DEFAULT_MAX_TARGETS: u32 = 64;
/// Longest target name.
pub const MAX_NAME: usize = 40;
/// Longest key prefix, in bytes.
pub const MAX_PREFIX: usize = 256;
/// The directory under a target's prefix where verification writes its probe object.
pub const PROBE_DIR: &str = ".asc-probe";
/// The `kind` of the vault contexts of storage credentials.
pub const SECRET_KIND: &str = "storage";

/// Who a target belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnerKind {
    User,
    Org,
    Repo,
}

impl OwnerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Org => "org",
            Self::Repo => "repo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "org" => Some(Self::Org),
            "repo" => Some(Self::Repo),
            _ => None,
        }
    }
}

/// An owner: a user, an organisation or a repository (`owner/repo`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    pub kind: OwnerKind,
    pub name: String,
}

impl Owner {
    /// Validates and normalises: user and organisation names are lower case (hosts treat them
    /// case-insensitively), a repository is `owner/repo` as the server knows it.
    pub fn new(kind: OwnerKind, name: &str) -> Result<Self, String> {
        let ok_part = |p: &str| {
            !p.is_empty()
                && p.len() <= 100
                && !p.starts_with('.')
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        };
        let name = match kind {
            OwnerKind::User | OwnerKind::Org => {
                if !ok_part(name) {
                    return Err("invalid owner name".into());
                }
                name.to_ascii_lowercase()
            }
            OwnerKind::Repo => {
                let mut parts = name.split('/');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(a), Some(b), None) if ok_part(a) && ok_part(b) => name.to_owned(),
                    _ => return Err("invalid repository name".into()),
                }
            }
        };
        Ok(Self { kind, name })
    }

    /// `user:alice`, the text the vault binds a secret to.
    pub fn scope(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.name)
    }
}

/// The lifecycle of a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    Verified,
    Enabled,
    Disabled,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "verified" => Self::Verified,
            "enabled" => Self::Enabled,
            "disabled" => Self::Disabled,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// How the bucket is addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Addressing {
    Auto,
    Path,
    VirtualHost,
}

impl Addressing {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Path => "path",
            Self::VirtualHost => "virtual_host",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "path" => Self::Path,
            "virtual_host" => Self::VirtualHost,
            _ => return None,
        })
    }

    pub fn to_store(self) -> asc_store::s3::Addressing {
        match self {
            Self::Auto => asc_store::s3::Addressing::Auto,
            Self::Path => asc_store::s3::Addressing::Path,
            Self::VirtualHost => asc_store::s3::Addressing::VirtualHost,
        }
    }
}

/// A stored target, without its credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub owner: Owner,
    pub name: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// `""` or `a/b` (no leading or trailing slash).
    pub prefix: String,
    pub addressing: Addressing,
    pub access_key_hint: String,
    pub state: State,
    pub was_enabled: bool,
    pub last_error: String,
    pub verified_at: i64,
    pub key_gen: u32,
    pub version: i64,
    pub created: i64,
    pub updated: i64,
}

/// A key pair as typed. `Debug` prints nothing of it, and the memory is zeroed on drop.
pub struct Credentials {
    pub access_key_id: Zeroizing<String>,
    pub secret_access_key: Zeroizing<String>,
    pub session_token: Option<Zeroizing<String>>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Packed<'a> {
    #[serde(borrow)]
    k: std::borrow::Cow<'a, str>,
    #[serde(borrow)]
    s: std::borrow::Cow<'a, str>,
    #[serde(borrow, default, skip_serializing_if = "Option::is_none")]
    t: Option<std::borrow::Cow<'a, str>>,
}

impl Credentials {
    /// Checks lengths and characters. An empty key id or secret is an error.
    pub fn new(
        access_key_id: &str,
        secret_access_key: &str,
        session_token: &str,
    ) -> Result<Self, String> {
        let visible = |s: &str, max: usize| {
            !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_graphic())
        };
        if !visible(access_key_id, 128) {
            return Err("access_key_id must be 1 to 128 visible ASCII characters".into());
        }
        if !visible(secret_access_key, 256) {
            return Err("secret_access_key must be 1 to 256 visible ASCII characters".into());
        }
        if !session_token.is_empty() && !visible(session_token, 4096) {
            return Err("session_token must be at most 4096 visible ASCII characters".into());
        }
        Ok(Self {
            access_key_id: Zeroizing::new(access_key_id.to_owned()),
            secret_access_key: Zeroizing::new(secret_access_key.to_owned()),
            session_token: (!session_token.is_empty())
                .then(|| Zeroizing::new(session_token.to_owned())),
        })
    }

    /// What the target shows in place of the key id: first and last four characters of a long
    /// id, two and two of a medium one, nothing of a short one.
    pub fn hint(&self) -> String {
        hint_of(&self.access_key_id)
    }

    pub fn pack(&self) -> Result<Zeroizing<Vec<u8>>, String> {
        let p = Packed {
            k: (&*self.access_key_id).into(),
            s: (&*self.secret_access_key).into(),
            t: self.session_token.as_ref().map(|t| (&**t).into()),
        };
        serde_json::to_vec(&p)
            .map(Zeroizing::new)
            .map_err(|_| "credentials cannot be packed".to_owned())
    }

    pub fn unpack(bytes: &[u8]) -> Result<Self, String> {
        let p: Packed<'_> =
            serde_json::from_slice(bytes).map_err(|_| "credentials are damaged".to_owned())?;
        Ok(Self {
            access_key_id: Zeroizing::new(p.k.into_owned()),
            secret_access_key: Zeroizing::new(p.s.into_owned()),
            session_token: p.t.map(|t| Zeroizing::new(t.into_owned())),
        })
    }
}

/// See [`Credentials::hint`].
pub fn hint_of(id: &str) -> String {
    let n = id.len();
    if !id.is_ascii() {
        return String::new();
    }
    if n >= 12 {
        format!("{}...{}", &id[..4], &id[n - 4..])
    } else if n >= 8 {
        format!("{}...{}", &id[..2], &id[n - 2..])
    } else {
        String::new()
    }
}

/// The vault context of the credentials of target `id`.
pub fn secret_context<'a>(owner_scope: &'a str, id: &'a str) -> SecretContext<'a> {
    SecretContext {
        kind: SECRET_KIND,
        owner: owner_scope,
        id,
    }
}

/// A target name: `[a-z0-9-]`, 1 to 40 characters, starting with a letter or digit.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err("name must be 1 to 40 characters of a-z, 0-9 and '-', not starting with '-'".into())
    }
}

/// An S3 bucket name as S3-compatible stores accept it: 3 to 63 characters of letters, digits,
/// '.', '-' and '_', starting and ending with a letter or digit, no "..".
pub fn check_bucket(bucket: &str) -> Result<(), String> {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    let ok = (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && edge(bucket.chars().next())
        && edge(bucket.chars().last())
        && !bucket.contains("..");
    if ok {
        Ok(())
    } else {
        Err("bucket must be 3 to 63 characters of letters, digits, '.', '-' and '_'".into())
    }
}

/// A region as S3-compatible stores name it. Empty means `us-east-1`.
pub fn check_region(region: &str) -> Result<String, String> {
    if region.is_empty() {
        return Ok("us-east-1".into());
    }
    let ok = region.len() <= 63
        && region
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(region.to_owned())
    } else {
        Err("region must be at most 63 characters of letters, digits, '-', '_' and '.'".into())
    }
}

/// A key prefix. Accepts `""`, `a/b` and `a/b/`; returns it without a leading or trailing slash.
/// Refused: a leading slash, empty, `.` or `..` segments, characters outside
/// `[A-Za-z0-9._=+@-]`, and the probe directory name as a segment.
pub fn check_prefix(prefix: &str) -> Result<String, String> {
    if prefix.is_empty() {
        return Ok(String::new());
    }
    if prefix.len() > MAX_PREFIX || prefix.starts_with('/') {
        return Err("prefix must be at most 256 bytes and have no leading slash".into());
    }
    let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
    for seg in trimmed.split('/') {
        let ok = !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg != PROBE_DIR
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'=' | b'+' | b'@' | b'-'));
        if !ok {
            return Err("prefix segments must be non-empty, not '.' or '..', and use only letters, digits and ._=+@-".into());
        }
    }
    Ok(trimmed.to_owned())
}

/// The prefix as the API returns it: `""` or `a/b/`.
pub fn prefix_out(stored: &str) -> String {
    if stored.is_empty() {
        String::new()
    } else {
        format!("{stored}/")
    }
}

/// A fresh id: `st_` or `sm_` and 20 characters of `a-z2-7`.
pub fn new_id(prefix: &str) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut rnd = [0u8; 20];
    // The OS RNG does not fail on supported platforms; on failure the id still differs by time.
    if getrandom::fill(&mut rnd).is_err() {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        for (i, b) in rnd.iter_mut().enumerate() {
            *b = (t >> (i % 16)) as u8;
        }
    }
    let mut s = String::with_capacity(prefix.len() + 20);
    s.push_str(prefix);
    for b in rnd {
        s.push(char::from(ALPHABET[usize::from(b & 31)]));
    }
    s
}

/// Whether `id` has the shape of an id made by [`new_id`] with `prefix`.
pub fn is_id(prefix: &str, id: &str) -> bool {
    id.len() == prefix.len() + 20
        && id.starts_with(prefix)
        && id.bytes().skip(prefix.len()).all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["a", "my-bucket-1", &"a".repeat(40)] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-a", "A", "a_b", "a b", &"a".repeat(41), "a/b", "é"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn buckets() {
        for ok in ["abc", "my.bucket-1_x", &"a".repeat(63)] {
            assert!(check_bucket(ok).is_ok(), "{ok}");
        }
        for bad in ["ab", "-abc", "abc-", "a..b", "a/b", "a b", &"a".repeat(64), ".abc"] {
            assert!(check_bucket(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn regions() {
        assert_eq!(check_region("").unwrap(), "us-east-1");
        assert_eq!(check_region("eu-west-2").unwrap(), "eu-west-2");
        assert_eq!(check_region("garage").unwrap(), "garage");
        assert!(check_region("a b").is_err());
        assert!(check_region(&"a".repeat(64)).is_err());
    }

    #[test]
    fn prefixes() {
        assert_eq!(check_prefix("").unwrap(), "");
        assert_eq!(check_prefix("a").unwrap(), "a");
        assert_eq!(check_prefix("a/b/").unwrap(), "a/b");
        assert_eq!(check_prefix("a/b").unwrap(), "a/b");
        assert_eq!(prefix_out("a/b"), "a/b/");
        assert_eq!(prefix_out(""), "");
        for bad in [
            "/a", "a//b", "../a", "a/../b", "a/./b", "a/..", ".", "a%2e%2e", "a\\b", "a b", "a?b",
            ".asc-probe", "x/.asc-probe/y", &"a/".repeat(200),
        ] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn owners() {
        let u = Owner::new(OwnerKind::User, "Alice").unwrap();
        assert_eq!((u.name.as_str(), u.scope().as_str()), ("alice", "user:alice"));
        assert!(Owner::new(OwnerKind::Org, "Acme-Co").is_ok());
        let r = Owner::new(OwnerKind::Repo, "acme/app").unwrap();
        assert_eq!(r.scope(), "repo:acme/app");
        for (k, n) in [
            (OwnerKind::User, ""),
            (OwnerKind::User, "a/b"),
            (OwnerKind::User, ".hidden"),
            (OwnerKind::Org, "a b"),
            (OwnerKind::Repo, "acme"),
            (OwnerKind::Repo, "a/b/c"),
            (OwnerKind::Repo, "a/.b"),
        ] {
            assert!(Owner::new(k, n).is_err(), "{n}");
        }
    }

    #[test]
    fn credentials_pack_hint_and_redaction() {
        let c = Credentials::new("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "").unwrap();
        assert_eq!(c.hint(), "AKIA...MPLE");
        let back = Credentials::unpack(&c.pack().unwrap()).unwrap();
        assert_eq!(*back.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        assert!(back.session_token.is_none());
        let t = Credentials::new("ASIAXXXXXXXX", "secret", "tok").unwrap();
        let back = Credentials::unpack(&t.pack().unwrap()).unwrap();
        assert_eq!(back.session_token.as_deref().map(String::as_str), Some("tok"));
        let shown = format!("{c:?}{t:?}");
        assert!(!shown.contains("AKIA") && !shown.contains("secret") && !shown.contains("wJal"));
        assert_eq!(hint_of("12345678"), "12...78");
        assert_eq!(hint_of("1234567"), "");
        assert_eq!(hint_of(""), "");
        for (k, s, t) in [("", "s", ""), ("k", "", ""), ("k k", "s", ""), ("k", "s\n", ""), ("k", "s", "t t")] {
            assert!(Credentials::new(k, s, t).is_err(), "{k:?} {s:?} {t:?}");
        }
        assert!(Credentials::new(&"k".repeat(129), "s", "").is_err());
        assert!(Credentials::unpack(b"{").is_err());
    }

    #[test]
    fn ids() {
        let a = new_id("st_");
        assert!(is_id("st_", &a), "{a}");
        assert_ne!(a, new_id("st_"));
        assert!(!is_id("sm_", &a));
        assert!(!is_id("st_", "st_short"));
        assert!(!is_id("st_", &format!("st_{}", "A".repeat(20))));
    }
}
