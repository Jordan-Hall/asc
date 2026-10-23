//! The pre-upload guard (SPEC §21.3.7).
//!
//! Before a draft is uploaded, the paths a changeset adds are checked against the ignore rules
//! and the deny list (`asc.cloud.denyPaths`), and the added blobs of at most [`MAX_SCAN_BLOB`]
//! are scanned for known token patterns and high-entropy secret assignments. A changeset with a
//! hit is held: it stays local until `asc cloud release`. The guard takes plain inputs and returns
//! a [`GuardVerdict`]; the caller persists holds. A verdict never carries the secret itself.
//!
//! Every bound is explicit: files per check, scanned bytes per check and holds reported.

use asc_core::path::RepoPath;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::bytes::Regex;

/// Largest blob the secret scan reads (SPEC §21.3.7); bigger blobs are skipped.
pub const MAX_SCAN_BLOB: usize = 1024 * 1024;
/// Most added files one check accepts.
pub const MAX_FILES: usize = 100_000;
/// Most blob bytes one check scans.
pub const MAX_SCAN_TOTAL: usize = 256 * 1024 * 1024;
/// Most holds a verdict lists; further holds only set [`GuardVerdict::Held::truncated`].
pub const MAX_HOLDS: usize = 64;
/// Most deny patterns accepted.
pub const MAX_DENY_PATTERNS: usize = 1024;
/// Longest deny pattern accepted, in bytes.
pub const MAX_PATTERN_LEN: usize = 512;

/// The default deny list (`asc.cloud.denyPaths`).
pub const DEFAULT_DENY: &[&str] = &[".env*", "*.pem", "*.key", "id_*", "*credentials*"];

/// Minimum entropy, in bits per character, of a secret assignment's value.
const MIN_ENTROPY: f64 = 3.5;

/// Failures from building or running the guard.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bad deny pattern {pattern:?}: {reason}")]
    Pattern { pattern: String, reason: String },
    #[error("{0} exceeds the limit of {1}")]
    TooLarge(&'static str, usize),
    #[error("secret pattern: {0}")]
    Regex(#[from] regex::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The kind of secret a scan found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    PrivateKey,
    AwsAccessKey,
    GithubToken,
    SlackToken,
    StripeKey,
    GoogleApiKey,
    /// A high-entropy value assigned to a secret-like name.
    HighEntropy,
}

/// Why a path holds a changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldReason {
    Ignored,
    Denied,
    Secret(SecretKind),
}

/// One held path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    pub path: RepoPath,
    pub reason: HoldReason,
}

/// The guard's answer for one changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardVerdict {
    Clear,
    Held {
        holds: Vec<Hold>,
        /// More holds existed than [`MAX_HOLDS`].
        truncated: bool,
    },
}

impl GuardVerdict {
    pub fn is_clear(&self) -> bool {
        matches!(self, GuardVerdict::Clear)
    }
}

/// A path a changeset adds, with the blob the scan reads. The caller passes an empty `content`
/// for a blob it did not load; blobs over [`MAX_SCAN_BLOB`] are skipped.
#[derive(Clone, Copy, Debug)]
pub struct AddedBlob<'a> {
    pub path: &'a RepoPath,
    pub content: &'a [u8],
}

/// Glob list of denied path names.
#[derive(Clone, Debug)]
pub struct DenyList(GlobSet);

impl DenyList {
    /// The default deny list.
    pub fn default_list() -> Result<Self> {
        Self::new(DEFAULT_DENY.iter().copied())
    }

    /// A pattern without `/` matches any path component; one with `/` matches the whole path.
    /// Matching ignores case, so `.ENV` is held on any file system.
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        let mut set = GlobSetBuilder::new();
        for (n, p) in patterns.into_iter().enumerate() {
            if n >= MAX_DENY_PATTERNS {
                return Err(Error::TooLarge("deny patterns", MAX_DENY_PATTERNS));
            }
            if p.len() > MAX_PATTERN_LEN {
                return Err(Error::TooLarge("deny pattern", MAX_PATTERN_LEN));
            }
            let bad = |e: globset::Error| Error::Pattern {
                pattern: p.to_owned(),
                reason: e.to_string(),
            };
            if p.contains('/') {
                set.add(
                    GlobBuilder::new(p.trim_start_matches('/'))
                        .literal_separator(true)
                        .case_insensitive(true)
                        .build()
                        .map_err(bad)?,
                );
            } else {
                set.add(
                    GlobBuilder::new(p)
                        .case_insensitive(true)
                        .build()
                        .map_err(bad)?,
                );
            }
        }
        Ok(Self(set.build().map_err(|e| Error::Pattern {
            pattern: String::new(),
            reason: e.to_string(),
        })?))
    }

    /// True when the path, or any of its components, is denied. Bytes that are not UTF-8 are
    /// matched lossily rather than skipped, so an invalid byte cannot hide a denied name.
    pub fn denies(&self, path: &RepoPath) -> bool {
        if self.0.is_empty() {
            return false;
        }
        if self.0.is_match(&*String::from_utf8_lossy(path.as_bytes())) {
            return true;
        }
        path.components()
            .any(|c| self.0.is_match(&*String::from_utf8_lossy(c)))
    }
}

/// The bounded secret scanner.
#[derive(Clone, Debug)]
pub struct SecretScanner {
    known: Vec<(SecretKind, Regex)>,
    assign: Regex,
}

impl SecretScanner {
    pub fn new() -> Result<Self> {
        let known = [
            (
                SecretKind::PrivateKey,
                r"-----BEGIN [A-Z ]{0,20}PRIVATE KEY( BLOCK)?-----",
            ),
            (SecretKind::AwsAccessKey, r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            (
                SecretKind::GithubToken,
                r"\b(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b",
            ),
            (SecretKind::SlackToken, r"\bxox[abprs]-[A-Za-z0-9-]{10,200}"),
            (SecretKind::StripeKey, r"\b[sr]k_live_[0-9a-zA-Z]{24,200}"),
            (SecretKind::GoogleApiKey, r"\bAIza[0-9A-Za-z_-]{35}\b"),
        ]
        .into_iter()
        .map(|(k, p)| Regex::new(p).map(|r| (k, r)))
        .collect::<Result<Vec<_>, _>>()?;
        let assign = Regex::new(
            r#"(?i)(?:secret|token|passw(?:or)?d|api[_-]?key|private[_-]?key|auth)[A-Za-z0-9_.-]{0,32}["']?\s{0,8}[:=]\s{0,8}["']([A-Za-z0-9+/=_-]{20,200})["']"#,
        )?;
        Ok(Self { known, assign })
    }

    /// The first secret found in `blob`, or `None`. A blob over [`MAX_SCAN_BLOB`] is not scanned.
    pub fn scan(&self, blob: &[u8]) -> Option<SecretKind> {
        if blob.len() > MAX_SCAN_BLOB {
            return None;
        }
        if let Some((kind, _)) = self.known.iter().find(|(_, re)| re.is_match(blob)) {
            return Some(*kind);
        }
        self.assign
            .captures_iter(blob)
            .filter_map(|c| c.get(1))
            .any(|m| entropy(m.as_bytes()) >= MIN_ENTROPY)
            .then_some(SecretKind::HighEntropy)
    }
}

/// Shannon entropy of `s`, in bits per byte.
fn entropy(s: &[u8]) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in s {
        counts[usize::from(b)] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// The pre-upload guard.
#[derive(Clone, Debug)]
pub struct Guard {
    deny: DenyList,
    scanner: SecretScanner,
}

impl Guard {
    pub fn new(deny: DenyList) -> Result<Self> {
        Ok(Self {
            deny,
            scanner: SecretScanner::new()?,
        })
    }

    /// The guard with the default deny list.
    pub fn with_defaults() -> Result<Self> {
        Self::new(DenyList::default_list()?)
    }

    /// Checks the paths a changeset adds. `ignored` answers the `.gitignore`/`.ascignore` rules.
    /// A path that is ignored or denied is held without a scan; the rest are scanned.
    pub fn check<'a>(
        &self,
        added: impl IntoIterator<Item = AddedBlob<'a>>,
        ignored: &dyn Fn(&RepoPath) -> bool,
    ) -> Result<GuardVerdict> {
        let mut holds = Vec::new();
        let mut truncated = false;
        let mut files = 0usize;
        let mut scanned = 0usize;
        for blob in added {
            files += 1;
            if files > MAX_FILES {
                return Err(Error::TooLarge("added files", MAX_FILES));
            }
            let reason = if ignored(blob.path) {
                Some(HoldReason::Ignored)
            } else if self.deny.denies(blob.path) {
                Some(HoldReason::Denied)
            } else if blob.content.len() <= MAX_SCAN_BLOB {
                scanned += blob.content.len();
                if scanned > MAX_SCAN_TOTAL {
                    return Err(Error::TooLarge("scanned bytes", MAX_SCAN_TOTAL));
                }
                self.scanner.scan(blob.content).map(HoldReason::Secret)
            } else {
                None
            };
            if let Some(reason) = reason {
                if holds.len() < MAX_HOLDS {
                    holds.push(Hold {
                        path: blob.path.clone(),
                        reason,
                    });
                } else {
                    truncated = true;
                }
            }
        }
        Ok(if holds.is_empty() {
            GuardVerdict::Clear
        } else {
            GuardVerdict::Held { holds, truncated }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn run(files: &[(&str, &[u8])], ign: &dyn Fn(&RepoPath) -> bool) -> GuardVerdict {
        let paths: Vec<RepoPath> = files.iter().map(|(n, _)| p(n)).collect();
        let g = Guard::with_defaults().unwrap();
        g.check(
            paths
                .iter()
                .zip(files)
                .map(|(path, (_, c))| AddedBlob { path, content: c }),
            ign,
        )
        .unwrap()
    }

    fn reasons(v: &GuardVerdict) -> Vec<HoldReason> {
        match v {
            GuardVerdict::Held { holds, .. } => holds.iter().map(|h| h.reason).collect(),
            GuardVerdict::Clear => vec![],
        }
    }

    #[test]
    fn env_change_is_denied() {
        let v = run(&[("app/.env", b"A=1"), ("app/.env.local", b"")], &|_| false);
        assert_eq!(reasons(&v), vec![HoldReason::Denied, HoldReason::Denied]);
    }

    #[test]
    fn default_deny_names() {
        let d = DenyList::default_list().unwrap();
        for n in [
            "a/server.pem",
            "k.key",
            "x/id_rsa",
            "aws-credentials.json",
            "d/.env/x",
        ] {
            assert!(d.denies(&p(n)), "{n}");
        }
        for n in ["src/main.rs", "environment.md", "identity.rs"] {
            assert!(!d.denies(&p(n)), "{n}");
        }
    }

    #[test]
    fn deny_ignores_case() {
        let d = DenyList::default_list().unwrap();
        for n in ["app/.ENV", "KEY.PEM", "x/ID_RSA", "AWS-Credentials.json"] {
            assert!(d.denies(&p(n)), "{n}");
        }
        assert!(
            DenyList::new(["secrets/*.txt"])
                .unwrap()
                .denies(&p("SECRETS/a.TXT"))
        );
    }

    #[test]
    fn non_utf8_path_is_still_denied() {
        let d = DenyList::default_list().unwrap();
        for raw in [&b"app/.env\xff"[..], b"\xfe/server.pem", b"k\xff/id_rsa"] {
            let path = RepoPath::new(raw.to_vec()).unwrap();
            assert!(d.denies(&path), "{raw:?}");
        }
    }

    #[test]
    fn slash_pattern_matches_whole_path() {
        let d = DenyList::new(["secrets/*.txt"]).unwrap();
        assert!(d.denies(&p("secrets/a.txt")));
        assert!(!d.denies(&p("x/secrets/a.txt")));
        assert!(!d.denies(&p("secrets/a/b.txt")));
    }

    #[test]
    fn ignored_path_is_held() {
        let v = run(&[("target/out.bin", b"x")], &|path| {
            path.as_bytes().starts_with(b"target/")
        });
        assert_eq!(reasons(&v), vec![HoldReason::Ignored]);
    }

    #[test]
    fn planted_tokens_are_held() {
        let cases: [(&[u8], SecretKind); 5] = [
            (b"key = AKIAIOSFODNN7EXAMPLE\n", SecretKind::AwsAccessKey),
            (
                b"t: ghp_abcdefghijklmnopqrstuvwxyz0123456789\n",
                SecretKind::GithubToken,
            ),
            (
                b"-----BEGIN RSA PRIVATE KEY-----\nMII",
                SecretKind::PrivateKey,
            ),
            (b"xoxb-1234567890-abcdefghij", SecretKind::SlackToken),
            (
                b"api_key = \"Zk3Qx9Lm2Vb7Nc4Rt8Yw1Ps6\"\n",
                SecretKind::HighEntropy,
            ),
        ];
        for (content, kind) in cases {
            let v = run(&[("src/a.txt", content)], &|_| false);
            assert_eq!(reasons(&v), vec![HoldReason::Secret(kind)]);
        }
    }

    #[test]
    fn plain_code_and_low_entropy_are_clear() {
        let v = run(
            &[
                ("a.rs", b"fn main() { let token_count = 3; }"),
                ("b.txt", b"password = \"aaaaaaaaaaaaaaaaaaaaaaaa\"\n"),
                ("c.txt", b"password = \"changeme\"\n"),
            ],
            &|_| false,
        );
        assert!(v.is_clear());
    }

    #[test]
    fn oversized_blob_is_not_scanned() {
        let mut big = vec![b' '; MAX_SCAN_BLOB + 1];
        big.extend_from_slice(b"AKIAIOSFODNN7EXAMPLE");
        let v = run(&[("big.txt", &big)], &|_| false);
        assert!(v.is_clear());
    }

    #[test]
    fn holds_are_bounded() {
        let names: Vec<String> = (0..MAX_HOLDS + 5).map(|i| format!("d/f{i}.pem")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        match run(&files, &|_| false) {
            GuardVerdict::Held { holds, truncated } => {
                assert_eq!(holds.len(), MAX_HOLDS);
                assert!(truncated);
            }
            GuardVerdict::Clear => panic!("expected held"),
        }
    }

    #[test]
    fn file_count_is_bounded() {
        let path = p("a.txt");
        let g = Guard::with_defaults().unwrap();
        let blob = AddedBlob {
            path: &path,
            content: b"",
        };
        let it = std::iter::repeat_n(blob, MAX_FILES + 1);
        assert!(matches!(g.check(it, &|_| false), Err(Error::TooLarge(..))));
    }

    #[test]
    fn bad_pattern_is_rejected() {
        assert!(matches!(DenyList::new(["[a"]), Err(Error::Pattern { .. })));
    }
}
