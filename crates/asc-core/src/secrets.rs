//! Secret detection: the pure engine behind push-time scanning and `SecretScan.Scan`.
//!
//! [`RuleSet`] holds the built-in detectors and the custom rules of a repository policy
//! (`[secrets.rules.<id>]`). [`RuleSet::scan`] finds secrets in the bytes of one file and returns
//! where they are ([`Hit`]: rule, line, column and the byte range of the value). It knows nothing
//! about repositories, stores or refusals; the caller decides what a hit means.
//!
//! # What a hit never holds
//!
//! A [`Hit`] points into the scanned bytes and does not copy the value. [`preview`] is the only
//! text derived from a value that may leave the process: at most two leading characters and then
//! `***`, and nothing at all for a short value. Fingerprints are computed by the caller, keyed
//! (`HMAC(repository key, rule id || value)`), so they cannot be reversed or compared across
//! repositories.
//!
//! # Detectors
//!
//! * `prefix`: tokens with a known issuer prefix and shape (cloud, forge, payment and AI provider
//!   keys, private key blocks, JWTs, connection strings with a password).
//! * `entropy`: an assignment to a name that says it is a secret (`password`, `api_key`, ...) of a
//!   value that is long enough, has the Shannon entropy of a random string, mixes character
//!   classes and is not a placeholder.
//! * `regex`: custom rules from the policy.
//!
//! Every pattern runs on the `regex` crate, whose matching time is linear in the input, and every
//! compiled pattern has a size limit. Binary files and files over [`MAX_SCAN_FILE`] are skipped by
//! [`RuleSet::scan`]. When two hits overlap, the more specific rule wins and the other is dropped.
//!
//! Policy `allow_paths` is applied by the caller; there is deliberately no inline "ignore this
//! line" marker, because a bypass that any author can write is not a control.

use std::ops::Range;
use std::sync::OnceLock;

use regex::bytes::{Regex, RegexBuilder};

/// Largest file [`RuleSet::scan`] reads; a larger one is skipped (and the caller counts it).
pub const MAX_SCAN_FILE: usize = 1024 * 1024;
/// Most custom rules a policy defines.
pub const MAX_CUSTOM_RULES: usize = 32;
/// Longest custom pattern, in bytes.
pub const MAX_PATTERN_LEN: usize = 1024;
/// Longest rule description, in bytes.
pub const MAX_DESCRIPTION_LEN: usize = 256;
/// Longest rule id, in bytes (without the `custom.` prefix).
pub const MAX_RULE_ID_LEN: usize = 48;
/// Compiled-size limit of one pattern, in bytes.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// Most hits one file reports; a file with more is cut here (the rest is the same problem).
pub const MAX_HITS_PER_FILE: usize = 64;
/// The byte window searched for a NUL when deciding that a file is binary.
const BINARY_WINDOW: usize = 8000;
/// Entropy (bits per character) an assigned value needs to count as random.
const ENTROPY_FLOOR: f64 = 3.3;

/// How serious a leaked secret of a rule is. The numbers are the wire values of `SecretSeverity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Severity {
    Low = 1,
    Medium = 2,
    High = 3,
    Critical = 4,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "critical" => Self::Critical,
            _ => return None,
        })
    }

    /// The wire number.
    pub fn number(self) -> i32 {
        self as i32
    }

    pub fn from_number(n: i32) -> Option<Self> {
        Some(match n {
            1 => Self::Low,
            2 => Self::Medium,
            3 => Self::High,
            4 => Self::Critical,
            _ => return None,
        })
    }
}

/// Why a custom rule was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RuleError(pub String);

fn bad<T>(m: impl Into<String>) -> Result<T, RuleError> {
    Err(RuleError(m.into()))
}

/// A rule as the policy file writes it: `[secrets.rules.<id>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomRule {
    /// The id without the `custom.` prefix.
    pub id: String,
    pub pattern: String,
    pub severity: Severity,
    pub description: String,
}

impl CustomRule {
    /// Checks the id, the lengths and that the pattern compiles within the size limit and is not
    /// able to match the empty string (a rule that matches everything).
    pub fn validate(&self) -> Result<(), RuleError> {
        let id_ok = !self.id.is_empty()
            && self.id.len() <= MAX_RULE_ID_LEN
            && self.id.as_bytes()[0].is_ascii_lowercase()
            && self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !id_ok {
            return bad(format!(
                "secret rule id {:?} is not lower-case letters, digits, - and _",
                self.id
            ));
        }
        if self.pattern.is_empty() || self.pattern.len() > MAX_PATTERN_LEN {
            return bad(format!(
                "secret rule {:?}: the pattern is empty or longer than {MAX_PATTERN_LEN} bytes",
                self.id
            ));
        }
        if self.description.len() > MAX_DESCRIPTION_LEN
            || self.description.chars().any(char::is_control)
        {
            return bad(format!(
                "secret rule {:?}: the description is too long or holds a control character",
                self.id
            ));
        }
        let re = compile(&self.pattern, false).map_err(|e| {
            RuleError(format!(
                "secret rule {:?}: the pattern does not compile: {e}",
                self.id
            ))
        })?;
        if re.is_match(b"") {
            return bad(format!(
                "secret rule {:?}: the pattern matches the empty string",
                self.id
            ));
        }
        Ok(())
    }
}

fn compile(pattern: &str, case_insensitive: bool) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .unicode(false)
        .case_insensitive(case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(8 << 20)
        .build()
}

/// What `ListRules` shows about a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleInfo {
    /// `aws-access-key-id`, `private-key-block`, `custom.<id>`, ...
    pub id: String,
    pub name: String,
    pub description: String,
    pub severity: Severity,
    pub builtin: bool,
    /// `prefix`, `entropy` or `regex`.
    pub detector: &'static str,
}

/// Extra conditions on a match of a built-in pattern.
#[derive(Clone, Copy)]
enum Check {
    /// The match stands as it is.
    None,
    /// The value is not a placeholder.
    Real,
    /// An assignment: the value is long, random, mixed and not a placeholder.
    Assigned,
    /// A connection string: the password is not a placeholder.
    Password,
}

struct Rule {
    info: RuleInfo,
    re: Regex,
    /// The capture group that is the secret (0 = the whole match).
    group: usize,
    check: Check,
}

struct Spec {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    severity: Severity,
    detector: &'static str,
    pattern: &'static str,
    case_insensitive: bool,
    group: usize,
    check: Check,
}

const fn spec(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    severity: Severity,
    pattern: &'static str,
) -> Spec {
    Spec {
        id,
        name,
        description,
        severity,
        detector: "prefix",
        pattern,
        case_insensitive: false,
        group: 0,
        check: Check::None,
    }
}

/// The names that say a variable holds a secret, for the assignment detector.
const SECRET_NAMES: &str = r"(?:secret|token|passw(?:or)?d|passwd|pwd|api[_\-.]?key|apikey|private[_\-.]?key|access[_\-.]?key|auth[_\-.]?key|signing[_\-.]?key|client[_\-.]?secret|credential)";

fn builtin_specs() -> Vec<Spec> {
    use Severity::*;
    vec![
        Spec {
            // The header and the first line of the body: documentation that only names the
            // header does not trip it. A JSON string holds the body after a literal `\n`.
            check: Check::None,
            ..spec(
                "private-key-block",
                "Private key",
                "A PEM or OpenSSH private key block (RSA, EC, DSA, OpenSSH, PGP, PKCS#8).",
                Critical,
                r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----(?:\s|\\r|\\n)*[A-Za-z0-9+/=]{16,}",
            )
        },
        spec(
            "aws-access-key-id",
            "AWS access key id",
            "An AWS access key id (long-term AKIA or short-term ASIA).",
            High,
            r"\b(?:AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16}\b",
        ),
        Spec {
            detector: "entropy",
            group: 1,
            check: Check::Real,
            case_insensitive: true,
            ..spec(
                "aws-secret-access-key",
                "AWS secret access key",
                "A 40 character AWS secret access key assigned to a name that says so.",
                Critical,
                r#"aws[_\-.]?secret[_\-.]?(?:access[_\-.]?)?key["']?\s*[:=]{1,2}>?\s*["']?([A-Za-z0-9/+=]{40})\b"#,
            )
        },
        spec(
            "github-token",
            "GitHub token",
            "A GitHub personal access, OAuth, user-to-server, server-to-server or refresh token.",
            Critical,
            r"\bgh[pousr]_[A-Za-z0-9]{36,251}\b",
        ),
        spec(
            "github-fine-grained-token",
            "GitHub fine-grained token",
            "A GitHub fine-grained personal access token.",
            Critical,
            r"\bgithub_pat_[A-Za-z0-9_]{22,255}\b",
        ),
        spec(
            "gitlab-token",
            "GitLab token",
            "A GitLab personal, project, group, deploy or runner token.",
            Critical,
            r"\b(?:glpat|gldt|glrt|glcbt|glptt|glft|glimt|glagent|gloas)-[A-Za-z0-9_\-]{20,}\b",
        ),
        spec(
            "slack-token",
            "Slack token",
            "A Slack bot, user, app or refresh token.",
            High,
            r"\bxox[abeoprs]-[0-9A-Za-z\-]{10,}\b",
        ),
        spec(
            "slack-webhook",
            "Slack webhook URL",
            "A Slack incoming webhook URL, which lets anyone post to the channel.",
            Medium,
            r"https://hooks\.slack\.com/(?:services|workflows|triggers)/[A-Za-z0-9/_\-]{20,}",
        ),
        spec(
            "stripe-key",
            "Stripe live key",
            "A Stripe live secret or restricted key.",
            Critical,
            r"\b[rs]k_live_[0-9A-Za-z]{24,}\b",
        ),
        spec(
            "google-api-key",
            "Google API key",
            "A Google Cloud API key.",
            High,
            r"\bAIza[0-9A-Za-z_\-]{35}\b",
        ),
        spec(
            "npm-token",
            "npm access token",
            "An npm automation or publish token.",
            Critical,
            r"\bnpm_[A-Za-z0-9]{36}\b",
        ),
        spec(
            "pypi-token",
            "PyPI upload token",
            "A PyPI API token.",
            Critical,
            r"\bpypi-AgE[A-Za-z0-9_\-]{50,}",
        ),
        spec(
            "sendgrid-key",
            "SendGrid API key",
            "A SendGrid API key.",
            High,
            r"\bSG\.[A-Za-z0-9_\-]{22}\.[A-Za-z0-9_\-]{43}\b",
        ),
        spec(
            "anthropic-key",
            "Anthropic API key",
            "An Anthropic API or admin key.",
            Critical,
            r"\bsk-ant-(?:api|admin)[0-9]{2}-[A-Za-z0-9_\-]{60,}",
        ),
        spec(
            "openai-key",
            "OpenAI API key",
            "An OpenAI API key (classic, project or service-account).",
            Critical,
            r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}T3BlbkFJ[A-Za-z0-9_\-]{20,}",
        ),
        spec(
            "huggingface-token",
            "Hugging Face token",
            "A Hugging Face access token.",
            High,
            r"\bhf_[A-Za-z0-9]{34,}\b",
        ),
        spec(
            "jwt",
            "JSON Web Token",
            "A signed JSON Web Token; it may grant access until it expires.",
            Medium,
            r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\b",
        ),
        Spec {
            group: 1,
            check: Check::Password,
            ..spec(
                "connection-string-password",
                "Password in a connection string",
                "A database or broker URL that carries a password.",
                High,
                r#"\b(?:postgres(?:ql)?|mysql|mariadb|mongodb(?:\+srv)?|redis|rediss|amqps?|mssql|sqlserver)://[^\s:/@"'<>]{1,128}:([^\s@/"'<>]{4,128})@[^\s"'<>]+"#,
            )
        },
        Spec {
            detector: "entropy",
            group: 1,
            check: Check::Assigned,
            ..spec(
                "generic-secret-assignment",
                "High-entropy secret assignment",
                "A value that looks random assigned to a name such as password, token or api_key.",
                Medium,
                // name, separator, then a quoted value or a bare token; the value is group 1
                // (quoted, no space) or group 2 (bare); `Spec::group` is fixed up below.
                "",
            )
        },
    ]
}

/// The pattern of the assignment detector. One group holds the value, whichever form it takes:
/// the alternation is flattened with an inline group so `captures` finds a single index.
fn assignment_pattern() -> String {
    format!(
        r#"{SECRET_NAMES}[A-Za-z0-9_.\-]*["']?\s*(?:[:=]{{1,2}}>?|=>)\s*["']?([A-Za-z0-9+/_\-=.~!@#$%^&*]{{16,200}})"#
    )
}

fn build_builtin() -> Vec<Rule> {
    builtin_specs()
        .into_iter()
        .map(|s| {
            let pattern = if s.id == "generic-secret-assignment" {
                assignment_pattern()
            } else {
                s.pattern.to_owned()
            };
            let insensitive = s.case_insensitive || s.id == "generic-secret-assignment";
            let re = compile(&pattern, insensitive)
                .unwrap_or_else(|e| panic!("built-in secret rule {} does not compile: {e}", s.id));
            Rule {
                info: RuleInfo {
                    id: s.id.to_owned(),
                    name: s.name.to_owned(),
                    description: s.description.to_owned(),
                    severity: s.severity,
                    builtin: true,
                    detector: s.detector,
                },
                re,
                group: s.group,
                check: s.check,
            }
        })
        .collect()
}

fn builtin() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(build_builtin)
}

/// One secret found in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Index into [`RuleSet::infos`].
    pub rule: usize,
    pub severity: Severity,
    /// 1-based line.
    pub line: u32,
    /// 1-based byte column of the start of the value.
    pub column: u32,
    /// The value, as a range of the scanned bytes.
    pub value: Range<usize>,
}

/// Why a file was not scanned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    Binary,
    TooLarge,
}

/// The result of scanning one file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scanned {
    pub hits: Vec<Hit>,
    pub skipped: Option<Skipped>,
}

/// The built-in rules plus a policy's custom rules.
pub struct RuleSet {
    custom: Vec<Rule>,
    infos: Vec<RuleInfo>,
}

impl RuleSet {
    /// Only the built-in rules.
    pub fn builtin() -> Self {
        Self::with_custom(&[]).expect("no custom rules cannot be invalid")
    }

    /// The built-in rules and `custom`, which must each validate and number at most
    /// [`MAX_CUSTOM_RULES`] with distinct ids.
    pub fn with_custom(custom: &[CustomRule]) -> Result<Self, RuleError> {
        if custom.len() > MAX_CUSTOM_RULES {
            return bad(format!("more than {MAX_CUSTOM_RULES} secret rules"));
        }
        let mut rules = Vec::with_capacity(custom.len());
        for (i, c) in custom.iter().enumerate() {
            c.validate()?;
            if custom[..i].iter().any(|o| o.id == c.id) {
                return bad(format!("secret rule {:?} is defined twice", c.id));
            }
            let re = compile(&c.pattern, false)
                .map_err(|e| RuleError(format!("secret rule {:?}: {e}", c.id)))?;
            let group = usize::from(re.captures_len() > 1);
            rules.push(Rule {
                info: RuleInfo {
                    id: format!("custom.{}", c.id),
                    name: c.id.clone(),
                    description: c.description.clone(),
                    severity: c.severity,
                    builtin: false,
                    detector: "regex",
                },
                re,
                group,
                check: Check::None,
            });
        }
        let mut infos: Vec<RuleInfo> = builtin().iter().map(|r| r.info.clone()).collect();
        infos.extend(rules.iter().map(|r| r.info.clone()));
        Ok(Self {
            custom: rules,
            infos,
        })
    }

    /// Every rule, built-in first.
    pub fn infos(&self) -> &[RuleInfo] {
        &self.infos
    }

    /// The index of rule `id` in [`infos`](Self::infos).
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.infos.iter().position(|r| r.id == id)
    }

    fn rules(&self) -> impl Iterator<Item = &Rule> {
        builtin().iter().chain(self.custom.iter())
    }

    /// Scans one file's bytes. A binary or oversized file reports `skipped` and no hits.
    pub fn scan(&self, data: &[u8]) -> Scanned {
        if data.len() > MAX_SCAN_FILE {
            return Scanned {
                hits: Vec::new(),
                skipped: Some(Skipped::TooLarge),
            };
        }
        if is_binary(data) {
            return Scanned {
                hits: Vec::new(),
                skipped: Some(Skipped::Binary),
            };
        }
        // (specificity, hit): the assignment detector is the least specific.
        let mut found: Vec<(u8, Range<usize>, usize, Severity)> = Vec::new();
        for (idx, rule) in self.rules().enumerate() {
            let rank = u8::from(!rule.check_assigned());
            for caps in rule.re.captures_iter(data) {
                let m = caps.get(rule.group).or_else(|| caps.get(0));
                let Some(m) = m else { continue };
                let value = &data[m.range()];
                if !rule.accepts(value) {
                    continue;
                }
                found.push((rank, m.range(), idx, rule.info.severity));
                if found.len() > MAX_HITS_PER_FILE * 8 {
                    break;
                }
            }
        }
        // Specific rules first, then higher severity, then position; drop overlaps.
        found.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(b.3.cmp(&a.3))
                .then(a.1.start.cmp(&b.1.start))
        });
        let mut kept: Vec<(u8, Range<usize>, usize, Severity)> = Vec::new();
        for f in found {
            if kept
                .iter()
                .any(|k| k.1.start < f.1.end && f.1.start < k.1.end)
            {
                continue;
            }
            kept.push(f);
        }
        kept.sort_by_key(|k| k.1.start);
        kept.truncate(MAX_HITS_PER_FILE);
        let lines = LineIndex::new(data);
        let hits = kept
            .into_iter()
            .map(|(_, value, rule, severity)| {
                let (line, column) = lines.locate(value.start);
                Hit {
                    rule,
                    severity,
                    line,
                    column,
                    value,
                }
            })
            .collect();
        Scanned {
            hits,
            skipped: None,
        }
    }
}

impl Rule {
    fn check_assigned(&self) -> bool {
        matches!(self.check, Check::Assigned)
    }

    fn accepts(&self, value: &[u8]) -> bool {
        match self.check {
            Check::None => true,
            Check::Real | Check::Password => !is_placeholder(value),
            Check::Assigned => {
                let Ok(text) = std::str::from_utf8(value) else {
                    return false;
                };
                looks_random(text)
            }
        }
    }
}

/// A NUL in the first bytes marks a file as binary, as git does.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_WINDOW)].contains(&0)
}

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(data: &[u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            data.iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    /// 1-based line and byte column of `offset`.
    fn locate(&self, offset: usize) -> (u32, u32) {
        let line = self.starts.partition_point(|s| *s <= offset);
        let start = self.starts[line - 1];
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(offset - start + 1).unwrap_or(u32::MAX),
        )
    }
}

/// Shannon entropy of `s` in bits per byte.
pub fn entropy(s: &[u8]) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in s {
        counts[usize::from(b)] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = f64::from(*c) / n;
            -p * p.log2()
        })
        .sum()
}

/// Words and shapes that mark a value as an example rather than a credential.
const PLACEHOLDER_WORDS: &[&str] = &[
    "example",
    "sample",
    "placeholder",
    "changeme",
    "change_me",
    "change-me",
    "your_",
    "your-",
    "yourkey",
    "dummy",
    "redacted",
    "fake",
    "insert",
    "replace",
    "notasecret",
    "not-a-secret",
    "xxxxxxxx",
    "********",
    "<",
    ">",
    "${",
    "{{",
    "%(",
    "$(",
];

/// Whether `value` reads as a placeholder: an obvious word, one repeated character, or a counting
/// sequence.
pub fn is_placeholder(value: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(value) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    if PLACEHOLDER_WORDS.iter().any(|w| lower.contains(w)) {
        return true;
    }
    let bytes = lower.as_bytes();
    if bytes.is_empty() {
        return true;
    }
    if bytes.iter().all(|b| *b == bytes[0]) {
        return true;
    }
    // 0123456789 / abcdefgh style runs.
    let ascending = bytes
        .windows(2)
        .filter(|w| w[1] == w[0].wrapping_add(1))
        .count();
    ascending * 4 >= (bytes.len() - 1) * 3
}

/// The assignment detector's test of a value: long, random, mixed and not a placeholder, path,
/// URL or plain word.
fn looks_random(value: &str) -> bool {
    if value.len() < 16 || value.starts_with('$') || value.starts_with('%') {
        return false;
    }
    if is_placeholder(value.as_bytes()) {
        return false;
    }
    if value.contains("://") || value.starts_with('/') || value.starts_with("./") {
        return false;
    }
    let has_lower = value.bytes().any(|b| b.is_ascii_lowercase());
    let has_upper = value.bytes().any(|b| b.is_ascii_uppercase());
    let has_digit = value.bytes().any(|b| b.is_ascii_digit());
    let has_other = value
        .bytes()
        .any(|b| !b.is_ascii_alphanumeric() && b != b'_' && b != b'-' && b != b'.');
    let classes = [has_lower, has_upper, has_digit, has_other]
        .iter()
        .filter(|c| **c)
        .count();
    // A snake_case or kebab-case identifier is a name, not a secret.
    let identifier = value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'-' || b == b'.');
    if identifier || classes < 2 {
        return false;
    }
    entropy(value.as_bytes()) >= ENTROPY_FLOOR
}

/// The text of a value that may be shown: at most its first two characters, then `***`; nothing
/// but `***` for a value under eight bytes or one that does not start with printable ASCII.
pub fn preview(value: &[u8]) -> String {
    if value.len() < 8 {
        return "***".to_owned();
    }
    let head: String = value
        .iter()
        .take(2)
        .take_while(|b| b.is_ascii_graphic())
        .map(|b| char::from(*b))
        .collect();
    if head.len() < 2 {
        return "***".to_owned();
    }
    format!("{head}***")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str) -> Vec<(String, u32, u32)> {
        let set = RuleSet::builtin();
        set.scan(text.as_bytes())
            .hits
            .into_iter()
            .map(|h| (set.infos()[h.rule].id.clone(), h.line, h.column))
            .collect()
    }

    fn ids(text: &str) -> Vec<String> {
        scan(text).into_iter().map(|h| h.0).collect()
    }

    /// Builds `parts` into one string, so this file holds no complete token.
    fn join(parts: &[&str]) -> String {
        parts.concat()
    }

    fn token(prefix: &str, len: usize) -> String {
        // Mixed, non-repeating characters.
        const ALPHABET: &[u8] = b"q7Zk3MxR9vTb2LpWd8NcYf4HgJa6SeUo5Ei1XrB0";
        let body: String = (0..len)
            .map(|i| char::from(ALPHABET[(i * 7 + i / 3) % ALPHABET.len()]))
            .collect();
        format!("{prefix}{body}")
    }

    #[test]
    fn a_private_key_block_is_found_with_its_body_and_not_a_bare_header() {
        let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7";
        let key = join(&[
            "-----BEGIN ",
            "PRIVATE KEY-----\n",
            body,
            "\n-----END ",
            "PRIVATE KEY-----\n",
        ]);
        assert_eq!(scan(&key), [("private-key-block".to_owned(), 1, 1)]);
        for kind in ["RSA ", "EC ", "OPENSSH ", "DSA ", "ENCRYPTED "] {
            let k = join(&["-----BEGIN ", kind, "PRIVATE KEY-----\n", body, "\n"]);
            assert_eq!(ids(&k), ["private-key-block"], "{kind}");
        }
        let pgp = join(&["-----BEGIN PGP ", "PRIVATE KEY BLOCK-----\n\n", body, "\n"]);
        assert_eq!(ids(&pgp), ["private-key-block"]);
        // A JSON string carries the body after an escaped newline.
        let json = join(&[
            "{\"private_key\": \"-----BEGIN ",
            "PRIVATE KEY-----\\n",
            body,
            "\\n\"}",
        ]);
        assert_eq!(ids(&json), ["private-key-block"]);
        // Documentation that names the header only is not a key.
        let doc = join(&[
            "Run: openssl genrsa. A file starting with -----BEGIN ",
            "PRIVATE KEY-----\n\nis secret.\n",
        ]);
        assert!(ids(&doc).is_empty(), "{:?}", ids(&doc));
        // A public key is not a secret.
        assert!(ids(&join(&["-----BEGIN PUBLIC ", "KEY-----\n", body, "\n"])).is_empty());
    }

    #[test]
    fn cloud_and_forge_tokens_are_found_by_shape() {
        let cases: Vec<(String, &str)> = vec![
            (join(&["AK", "IA", "Q2WE4RT6YUIO3PAS"]), "aws-access-key-id"),
            (join(&["AS", "IA", "Q2WE4RT6YUIO3PAS"]), "aws-access-key-id"),
            (token("ghp_", 36), "github-token"),
            (token("gho_", 40), "github-token"),
            (token("ghs_", 36), "github-token"),
            (token("github_pat_", 60), "github-fine-grained-token"),
            (token("glpat-", 24), "gitlab-token"),
            (
                join(&["xo", "xb-", "1234567890-abcdefghijkl"]),
                "slack-token",
            ),
            (
                join(&[
                    "https://hooks.",
                    "slack.com/services/T000/B000/",
                    "abcdefghijklmnopqrstuvwx",
                ]),
                "slack-webhook",
            ),
            (
                join(&["sk_", "live_", "abcdefghijklmnopqrstuvwx12"]),
                "stripe-key",
            ),
            (
                join(&["rk_", "live_", "abcdefghijklmnopqrstuvwx12"]),
                "stripe-key",
            ),
            (token("AIza", 35), "google-api-key"),
            (token("npm_", 36), "npm-token"),
            (token("pypi-AgEIcHlwaS5vcmc", 60), "pypi-token"),
            (
                join(&["SG", ".", &token("", 22), ".", &token("", 43)]),
                "sendgrid-key",
            ),
            (token("sk-ant-api03-", 90), "anthropic-key"),
            (
                join(&["sk-", &token("", 24), "T3Blbk", "FJ", &token("", 24)]),
                "openai-key",
            ),
            (token("hf_", 36), "huggingface-token"),
        ];
        for (secret, rule) in cases {
            let text = format!("key = {secret}\n");
            let got = ids(&text);
            assert!(got.iter().any(|g| g == rule), "{rule}: got {got:?}");
        }
    }

    #[test]
    fn near_misses_are_not_tokens() {
        // Wrong length, wrong prefix, a letter inside a word.
        let cases = [
            join(&["AK", "IA", "SHORT"]),
            join(&["AK", "IA", "q2we4rt6yu8io3pa"]),
            token("ghp_", 10),
            token("xxghp_", 36),
            token("glpat-", 5),
            join(&["sk_", "test_", "abcdefghijklmnopqrstuvwx12"]),
            token("AIza", 20),
            "the npm_ prefix is for npm tokens".to_owned(),
        ];
        for c in cases {
            assert!(ids(&c).is_empty(), "{c}: {:?}", ids(&c));
        }
    }

    #[test]
    fn a_jwt_and_a_connection_string_password_are_found() {
        let jwt = join(&[
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.",
            "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4ifQ.",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        ]);
        assert_eq!(ids(&format!("Authorization: Bearer {jwt}")), ["jwt"]);
        assert!(ids("eyJhbGciOiJIUzI1NiJ9.short").is_empty());

        let url = join(&["postgres://app:", "s3kr1tPassw0rd", "@db.internal:5432/app"]);
        assert_eq!(
            ids(&format!("DATABASE_URL={url}")),
            ["connection-string-password"]
        );
        // The value is the password, so the preview and fingerprint never include the host.
        let set = RuleSet::builtin();
        let text = format!("DATABASE_URL={url}");
        let hit = &set.scan(text.as_bytes()).hits[0];
        assert_eq!(&text.as_bytes()[hit.value.clone()], b"s3kr1tPassw0rd");
        // A URL without a password, with a placeholder, or with a variable is fine.
        for ok in [
            "postgres://db.internal/app",
            "postgres://app@db.internal/app",
            "postgres://app:password-placeholder@db/app",
            "redis://user:${REDIS_PASSWORD}@cache:6379",
            "mongodb://user:<password>@cluster0.example.net/test",
        ] {
            assert!(ids(ok).is_empty(), "{ok}: {:?}", ids(ok));
        }
    }

    #[test]
    fn aws_secret_keys_need_the_name_and_forty_characters() {
        let secret = token("", 40);
        assert_eq!(
            ids(&format!("aws_secret_access_key = \"{secret}\"")),
            ["aws-secret-access-key"]
        );
        assert_eq!(
            ids(&format!("AWS_SECRET_ACCESS_KEY={secret}")),
            ["aws-secret-access-key"]
        );
        assert!(ids(&format!("aws_secret_access_key = \"{}\"", "A".repeat(40))).is_empty());
        assert!(ids(&format!("sha = \"{secret}\"")).is_empty());
    }

    #[test]
    fn entropy_rules_separate_secrets_from_names_and_placeholders() {
        let random = token("", 32);
        let positives = [
            format!("api_key = \"{random}\""),
            format!("password: '{random}'"),
            format!("export CLIENT_SECRET={random}"),
            format!("\"auth_token\": \"{random}\","),
            format!("db.passwd => \"{random}\""),
        ];
        for p in positives {
            assert_eq!(ids(&p), ["generic-secret-assignment"], "{p}");
        }
        let negatives = [
            "password = \"correct horse battery staple\"".to_owned(), // spaces
            "password = \"changeme-changeme-changeme\"".to_owned(),
            "api_key = \"your_api_key_goes_here_please\"".to_owned(),
            "token = \"${GITHUB_TOKEN_FOR_THE_JOB}\"".to_owned(),
            "secret_name = \"database_password_secret\"".to_owned(), // an identifier
            "token = \"aaaaaaaaaaaaaaaaaaaaaaaa\"".to_owned(),
            "api_key = \"0123456789abcdefghij\"".to_owned(), // a counting run
            "password = \"short\"".to_owned(),
            "password = \"https://example.org/some/long/path/here\"".to_owned(),
            "token_type = \"bearer\"".to_owned(),
            "let secret = read_secret_from_the_vault();".to_owned(),
            format!("author = \"{random}\""), // the name does not say secret
        ];
        for n in negatives {
            assert!(ids(&n).is_empty(), "{n}: {:?}", ids(&n));
        }
    }

    #[test]
    fn entropy_is_shannon_bits_per_byte() {
        assert_eq!(entropy(b""), 0.0);
        assert_eq!(entropy(b"aaaa"), 0.0);
        assert!((entropy(b"abab") - 1.0).abs() < 1e-9);
        assert!((entropy(b"abcd") - 2.0).abs() < 1e-9);
        let all: Vec<u8> = (0..=255u8).collect();
        assert!((entropy(&all) - 8.0).abs() < 1e-9);
    }

    #[test]
    fn a_specific_rule_wins_over_the_generic_one_and_overlaps_are_dropped() {
        let t = token("ghp_", 36);
        let hits = scan(&format!("github_token = \"{t}\"\n"));
        assert_eq!(hits, [("github-token".to_owned(), 1, 17)]);
    }

    #[test]
    fn lines_and_columns_are_one_based() {
        let t = token("npm_", 36);
        let text = format!("# header\n\n   token: {t}\nlast\n");
        let hits = scan(&text);
        assert_eq!(hits, [("npm-token".to_owned(), 3, 11)]);
        // CRLF files count the same lines.
        let crlf = format!("a\r\nb\r\nx = {t}\r\n");
        assert_eq!(scan(&crlf)[0].1, 3);
    }

    #[test]
    fn previews_show_at_most_two_characters() {
        assert_eq!(
            preview(b"ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
            "gh***"
        );
        assert_eq!(preview(b"short"), "***");
        assert_eq!(preview(b""), "***");
        assert_eq!(preview(&[0xff, 0xfe, 1, 2, 3, 4, 5, 6, 7, 8]), "***");
        for len in 0..40 {
            let v = vec![b'a'; len];
            assert!(preview(&v).len() <= 5, "{len}");
        }
    }

    #[test]
    fn binary_and_large_files_are_skipped_and_say_so() {
        let set = RuleSet::builtin();
        let mut bin = token("npm_", 36).into_bytes();
        bin.push(0);
        let r = set.scan(&bin);
        assert!(r.hits.is_empty());
        assert_eq!(r.skipped, Some(Skipped::Binary));
        let big = vec![b'a'; MAX_SCAN_FILE + 1];
        assert_eq!(set.scan(&big).skipped, Some(Skipped::TooLarge));
        let edge = vec![b'a'; MAX_SCAN_FILE];
        assert_eq!(set.scan(&edge).skipped, None);
        assert_eq!(set.scan(b"").skipped, None);
    }

    #[test]
    fn work_is_bounded_on_adversarial_input() {
        let set = RuleSet::builtin();
        // Backtracking bombs for a backtracking engine; linear here.
        let evil = format!("{}!", "a".repeat(500_000));
        let start = std::time::Instant::now();
        assert!(set.scan(evil.as_bytes()).hits.is_empty());
        let spaces = format!("password{}=", " ".repeat(200_000));
        assert!(set.scan(spaces.as_bytes()).hits.is_empty());
        let dots = "token=".to_owned() + &"=".repeat(300_000);
        assert!(set.scan(dots.as_bytes()).hits.is_empty());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            start.elapsed()
        );
        // A file full of tokens reports a bounded number of hits.
        let t = token("npm_", 36);
        let many = format!("{t}\n").repeat(10_000);
        assert_eq!(set.scan(many.as_bytes()).hits.len(), MAX_HITS_PER_FILE);
    }

    #[test]
    fn custom_rules_are_validated_and_scanned() {
        let rule = |id: &str, pattern: &str| CustomRule {
            id: id.into(),
            pattern: pattern.into(),
            severity: Severity::High,
            description: "internal key".into(),
        };
        let set = RuleSet::with_custom(&[
            rule("acme-key", r"ACME-[0-9A-F]{16}"),
            rule("acme-secret", r"acme_secret=([a-z0-9]{12})"),
        ])
        .unwrap();
        assert_eq!(set.infos().last().unwrap().id, "custom.acme-secret");
        assert!(!set.infos().last().unwrap().builtin);
        let text = "id ACME-0123456789ABCDEF\nacme_secret=abcdefghij12\n";
        let r = set.scan(text.as_bytes());
        assert_eq!(r.hits.len(), 2);
        assert_eq!(set.infos()[r.hits[0].rule].id, "custom.acme-key");
        // A capture group is the value, so the label is not in it.
        assert_eq!(&text.as_bytes()[r.hits[1].value.clone()], b"abcdefghij12");
        assert_eq!(r.hits[1].line, 2);

        for (id, pattern, why) in [
            ("Bad", "x", "id"),
            ("", "x", "empty id"),
            ("a", "", "empty pattern"),
            ("a", "(", "does not compile"),
            ("a", "x*", "matches the empty string"),
            ("a", "(a+)+$\\", "bad escape"),
        ] {
            assert!(RuleSet::with_custom(&[rule(id, pattern)]).is_err(), "{why}");
        }
        assert!(RuleSet::with_custom(&[rule("a", "x"), rule("a", "y")]).is_err());
        let long = "a".repeat(MAX_PATTERN_LEN + 1);
        assert!(RuleSet::with_custom(&[rule("a", &long)]).is_err());
        let huge = "(((a{1000}){1000}){1000})";
        assert!(
            RuleSet::with_custom(&[rule("a", huge)]).is_err(),
            "size limit"
        );
        let many: Vec<CustomRule> = (0..=MAX_CUSTOM_RULES)
            .map(|i| rule(&format!("r{i}"), "x"))
            .collect();
        assert!(RuleSet::with_custom(&many).is_err());
    }

    #[test]
    fn severity_round_trips() {
        for s in [
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            assert_eq!(Severity::parse(s.as_str()), Some(s));
            assert_eq!(Severity::from_number(s.number()), Some(s));
        }
        assert_eq!(Severity::parse("severe"), None);
        assert_eq!(Severity::from_number(0), None);
        assert!(Severity::Critical > Severity::Low);
    }

    #[test]
    fn rule_ids_are_unique_and_documented() {
        let set = RuleSet::builtin();
        let mut ids: Vec<&str> = set.infos().iter().map(|r| r.id.as_str()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n);
        for r in set.infos() {
            assert!(!r.description.is_empty() && !r.name.is_empty(), "{}", r.id);
            assert!(r.builtin);
        }
        assert_eq!(
            set.index_of("jwt").map(|i| &set.infos()[i].id[..]),
            Some("jwt")
        );
        assert_eq!(set.index_of("nope"), None);
    }
}
