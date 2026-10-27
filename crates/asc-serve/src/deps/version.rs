//! Versions and how they order.
//!
//! Each ecosystem has its own idea of a version. [`Version`] reads the five that matter into one shape
//! (an epoch, a list of release numbers, a pre- or post-release phase and a list of qualifiers) whose
//! derived order is the ecosystem's:
//!
//! * [`Scheme::Semver`]: semantic versions, also Go (`v1.2.3`, pseudo-versions, `+incompatible`), NuGet
//!   (a fourth number) and Hex, Pub and SwiftPM. Anything after the first `-` is a pre-release, compared
//!   identifier by identifier, numbers below words.
//! * [`Scheme::Pep440`]: `1.0a1 < 1.0b2 < 1.0rc1 < 1.0 < 1.0.post1`, `1.0.dev1` below them all, epochs.
//! * [`Scheme::Maven`]: `alpha < beta < milestone < rc < snapshot < (release) < sp`.
//! * [`Scheme::Gem`]: RubyGems and CocoaPods; a segment with letters makes a pre-release.
//! * [`Scheme::Composer`]: `-dev`, `-alpha`, `-beta`, `-RC`, `-patch`.
//!
//! A string that has no leading release number (`dev-master`, `latest`) is not a version.

use std::cmp::Ordering;
use std::fmt;
use std::sync::LazyLock;

use regex::Regex;

/// How an ecosystem orders its versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    Semver,
    Pep440,
    Maven,
    Gem,
    Composer,
}

/// One qualifier after the release numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    Num(u64),
    Word(String),
}

impl Tok {
    fn cmp_tok(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Num(a), Self::Num(b)) => a.cmp(b),
            (Self::Num(_), Self::Word(_)) => Ordering::Less,
            (Self::Word(_), Self::Num(_)) => Ordering::Greater,
            (Self::Word(a), Self::Word(b)) => a.cmp(b),
        }
    }
}

/// The phase of a pre-release generic to semantic versions (all identifiers compare by the rules).
const SEMVER_PRE: i32 = -100;

/// A parsed version.
#[derive(Clone, Debug)]
pub struct Version {
    epoch: u64,
    parts: Vec<u64>,
    phase: i32,
    qual: Vec<Tok>,
}

fn number(s: &str) -> u64 {
    s.parse::<u64>().unwrap_or(u64::MAX)
}

/// The phase a qualifier word puts a version in, and whether the word carries no more information
/// than the phase (`final`, `ga`).
fn word_phase(scheme: Scheme, w: &str) -> i32 {
    match w {
        "dev" => -50,
        "alpha" | "a" => -40,
        "beta" | "b" => -30,
        "milestone" | "m" => -25,
        "rc" | "cr" | "c" | "preview" | "pre" => -10,
        "snapshot" => -5,
        "ga" | "final" | "release" | "stable" | "" => 0,
        "post" | "rev" | "r" | "sp" | "p" | "pl" | "patch" => 10,
        _ if scheme == Scheme::Maven => 5,
        _ => -20,
    }
}

impl Version {
    /// Reads `s` under `scheme`, or `None` when it has no leading release number or holds a character
    /// no version has.
    pub fn parse(scheme: Scheme, s: &str) -> Option<Self> {
        let mut s = s.trim();
        if s.is_empty() || s.len() > 256 {
            return None;
        }
        if let Some(r) = s.strip_prefix(['v', 'V'])
            && r.starts_with(|c: char| c.is_ascii_digit())
        {
            s = r;
        }
        let mut epoch = 0;
        if scheme == Scheme::Pep440
            && let Some((e, r)) = s.split_once('!')
            && let Ok(n) = e.parse::<u64>()
        {
            epoch = n;
            s = r;
        }
        if let Some((r, _)) = s.split_once('+') {
            s = r;
        }
        let b = s.as_bytes();
        let mut parts = Vec::new();
        let mut i = 0;
        loop {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if start == i {
                break;
            }
            parts.push(number(&s[start..i]));
            if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                i += 1;
            } else {
                break;
            }
        }
        if parts.is_empty() || parts.len() > 16 {
            return None;
        }
        let rest = &s[i..];
        if !rest
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        {
            return None;
        }
        let toks = tokens(rest);
        let (phase, qual) = if toks.is_empty() {
            (0, Vec::new())
        } else if scheme == Scheme::Semver {
            (SEMVER_PRE, toks)
        } else {
            match &toks[0] {
                Tok::Word(w) => (word_phase(scheme, w), toks[1..].to_vec()),
                Tok::Num(_) if scheme == Scheme::Pep440 => (10, toks),
                Tok::Num(_) => (1, toks),
            }
        };
        Some(Self {
            epoch,
            parts,
            phase,
            qual,
        })
    }

    /// A version before the release it leads to (an alpha, a release candidate, a snapshot).
    pub fn is_prerelease(&self) -> bool {
        self.phase < 0
    }

    pub fn major(&self) -> u64 {
        self.parts[0]
    }

    pub fn minor(&self) -> u64 {
        self.parts.get(1).copied().unwrap_or(0)
    }

    pub fn patch(&self) -> u64 {
        self.parts.get(2).copied().unwrap_or(0)
    }

    /// The number of release numbers the text had (`1.2` has two).
    pub fn precision(&self) -> usize {
        self.parts.len()
    }
}

fn tokens(rest: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut digits = false;
    let flush = |cur: &mut String, digits: bool, out: &mut Vec<Tok>| {
        if cur.is_empty() {
            return;
        }
        out.push(if digits {
            Tok::Num(number(cur))
        } else {
            Tok::Word(cur.to_ascii_lowercase())
        });
        cur.clear();
    };
    for c in rest.chars() {
        if matches!(c, '.' | '-' | '_') {
            flush(&mut cur, digits, &mut out);
            continue;
        }
        let d = c.is_ascii_digit();
        if !cur.is_empty() && d != digits {
            flush(&mut cur, digits, &mut out);
        }
        digits = d;
        cur.push(c);
    }
    flush(&mut cur, digits, &mut out);
    out
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| {
                let n = self.parts.len().max(other.parts.len());
                for i in 0..n {
                    let a = self.parts.get(i).copied().unwrap_or(0);
                    let b = other.parts.get(i).copied().unwrap_or(0);
                    match a.cmp(&b) {
                        Ordering::Equal => {}
                        o => return o,
                    }
                }
                Ordering::Equal
            })
            .then(self.phase.cmp(&other.phase))
            .then_with(|| {
                for (a, b) in self.qual.iter().zip(&other.qual) {
                    match a.cmp_tok(b) {
                        Ordering::Equal => {}
                        o => return o,
                    }
                }
                self.qual.len().cmp(&other.qual.len())
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

/// The order of two version strings, or `None` when either is not a version.
pub fn compare(scheme: Scheme, a: &str, b: &str) -> Option<Ordering> {
    Some(Version::parse(scheme, a)?.cmp(&Version::parse(scheme, b)?))
}

/// How big an update is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UpdateKind {
    Patch,
    Minor,
    Major,
}

impl UpdateKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "patch" => Some(Self::Patch),
            "minor" => Some(Self::Minor),
            "major" => Some(Self::Major),
            _ => None,
        }
    }

    /// The kind of the step from `from` to `to`, or `None` when `to` is not newer.
    pub fn between(from: &Version, to: &Version) -> Option<Self> {
        if to <= from {
            return None;
        }
        Some(if to.epoch != from.epoch || to.major() != from.major() {
            Self::Major
        } else if to.minor() != from.minor() {
            Self::Minor
        } else {
            Self::Patch
        })
    }
}

impl fmt::Display for UpdateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One comparator of a requirement text: an optional operator and a version or wildcard.
static COMPARATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<op>>=|<=|~>|~=|===|==|!=|\^|~|=|>|<)?\s*[vV]?(?P<ver>\d[0-9A-Za-z.*+_\-]*|[xX*])",
    )
    .unwrap()
});

/// The lowest version a requirement text admits, as written by a manifest: `^1.2.3`, `~> 1.2`,
/// `>=1.0, <2`, `[1.0,2.0)`, `1.x`. `None` when the text names no version (`*`, `latest`, a git or
/// path source, a workspace reference, an upper bound only).
pub fn requirement_floor(req: &str) -> Option<String> {
    let r = req.trim();
    if r.is_empty() || r.len() > 512 {
        return None;
    }
    let lower = r.to_ascii_lowercase();
    for p in [
        "git",
        "http",
        "file:",
        "link:",
        "workspace:",
        "github:",
        "gitlab:",
        "npm:",
        "path",
        "latest",
        "next",
        "dev-",
        "catalog:",
        "portal:",
        "patch:",
        "jsr:",
        "ssh:",
        "bitbucket:",
        "*",
    ] {
        if lower.starts_with(p) {
            return None;
        }
    }
    if r.starts_with("(,") {
        return None;
    }
    let first = r.split("||").next().unwrap_or(r);
    for m in COMPARATOR.captures_iter(first) {
        let op = m.name("op").map_or("", |o| o.as_str());
        if matches!(op, "<" | "<=" | "!=") {
            continue;
        }
        let ver = m.name("ver")?.as_str();
        let ver = ver.trim_end_matches(['.', '-', '_']);
        let mut out = Vec::new();
        for (i, seg) in ver.split('.').enumerate() {
            if matches!(seg, "x" | "X" | "*") {
                if i == 0 {
                    return None;
                }
                out.push("0".to_owned());
                continue;
            }
            out.push(seg.to_owned());
        }
        let floor = out.join(".");
        if Version::parse(Scheme::Maven, &floor).is_some() {
            return Some(floor);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::{Equal, Greater, Less};

    fn cmp(s: Scheme, a: &str, b: &str) -> Ordering {
        compare(s, a, b).unwrap_or_else(|| panic!("{a} or {b} is not a version"))
    }

    #[test]
    fn semver_orders_numbers_before_words_and_releases_after_pre_releases() {
        use Scheme::Semver as S;
        assert_eq!(cmp(S, "1.0.0", "1.0.0"), Equal);
        assert_eq!(cmp(S, "1.0", "1.0.0"), Equal, "missing parts are zero");
        assert_eq!(cmp(S, "1.9.0", "1.10.0"), Less);
        assert_eq!(cmp(S, "1.0.0-alpha", "1.0.0"), Less);
        assert_eq!(cmp(S, "1.0.0-alpha", "1.0.0-alpha.1"), Less);
        assert_eq!(cmp(S, "1.0.0-alpha.1", "1.0.0-alpha.beta"), Less);
        assert_eq!(cmp(S, "1.0.0-alpha.beta", "1.0.0-beta"), Less);
        assert_eq!(cmp(S, "1.0.0-beta.2", "1.0.0-beta.11"), Less);
        assert_eq!(cmp(S, "1.0.0-beta.11", "1.0.0-rc.1"), Less);
        assert_eq!(cmp(S, "1.0.0-rc.1", "1.0.0"), Less);
        assert_eq!(cmp(S, "1.0.0+build5", "1.0.0+build9"), Equal);
        assert_eq!(cmp(S, "v1.2.3", "1.2.3"), Equal);
        assert_eq!(cmp(S, "1.2.3.4", "1.2.3"), Greater, "NuGet's fourth number");
    }

    #[test]
    fn go_pseudo_versions_sit_below_the_release_they_lead_to() {
        use Scheme::Semver as S;
        assert_eq!(cmp(S, "v0.0.0-20210101000000-abcdef123456", "v0.0.1"), Less);
        assert_eq!(
            cmp(
                S,
                "v0.0.0-20210101000000-abcdef123456",
                "v0.0.0-20220101000000-abcdef123456"
            ),
            Less
        );
        assert_eq!(cmp(S, "v2.0.0+incompatible", "v2.0.0"), Equal);
        assert!(
            Version::parse(S, "v1.2.4-0.20210101000000-abcdef123456")
                .unwrap()
                .is_prerelease()
        );
    }

    #[test]
    fn pep440_orders_dev_pre_release_final_and_post() {
        use Scheme::Pep440 as P;
        let order = [
            "1.0.dev1",
            "1.0a1",
            "1.0a2",
            "1.0b1",
            "1.0rc1",
            "1.0",
            "1.0.post1",
            "1.0.1",
            "1!0.5",
        ];
        for w in order.windows(2) {
            assert_eq!(cmp(P, w[0], w[1]), Less, "{} < {}", w[0], w[1]);
        }
        assert_eq!(cmp(P, "1.0", "1.0.0"), Equal);
        assert_eq!(cmp(P, "1.0rc1", "1.0.rc1"), Equal);
        assert_eq!(cmp(P, "2.0.0+local.1", "2.0.0"), Equal);
    }

    #[test]
    fn maven_qualifiers_follow_the_documented_order() {
        use Scheme::Maven as M;
        let order = [
            "1.0-alpha-1",
            "1.0-beta-2",
            "1.0-milestone-1",
            "1.0-rc1",
            "1.0-SNAPSHOT",
            "1.0",
            "1.0-sp1",
            "1.0.1",
        ];
        for w in order.windows(2) {
            assert_eq!(cmp(M, w[0], w[1]), Less, "{} < {}", w[0], w[1]);
        }
        assert_eq!(cmp(M, "1.0.RELEASE", "1.0"), Equal);
        assert_eq!(cmp(M, "1.0-Final", "1.0"), Equal);
        assert_eq!(cmp(M, "1.0.0.GA", "1.0"), Equal);
        assert_eq!(cmp(M, "1.0-1", "1.0"), Greater);
    }

    #[test]
    fn gem_and_composer_treat_letters_as_pre_release() {
        assert_eq!(cmp(Scheme::Gem, "1.0.0.pre", "1.0.0"), Less);
        assert_eq!(cmp(Scheme::Gem, "1.0.0.rc1", "1.0.0"), Less);
        assert_eq!(cmp(Scheme::Gem, "1.0.0.beta", "1.0.0.rc1"), Less);
        assert_eq!(cmp(Scheme::Gem, "7.0.4", "7.0.10"), Less);
        assert_eq!(cmp(Scheme::Composer, "v3.0.0-RC1", "3.0.0"), Less);
        assert_eq!(cmp(Scheme::Composer, "3.0.0-beta1", "3.0.0-RC1"), Less);
        assert_eq!(cmp(Scheme::Composer, "3.0.0", "3.0.0-patch1"), Less);
    }

    #[test]
    fn things_that_are_not_versions_are_not_parsed() {
        for s in ["", "latest", "dev-master", "x.y", "1.0 beta", "a1", "v"] {
            assert!(Version::parse(Scheme::Semver, s).is_none(), "{s:?}");
        }
        assert!(Version::parse(Scheme::Semver, &"1.".repeat(40)).is_none());
        assert!(compare(Scheme::Semver, "1.0", "dev-master").is_none());
    }

    #[test]
    fn update_kinds_split_on_the_first_number_that_changes() {
        let v = |s: &str| Version::parse(Scheme::Semver, s).unwrap();
        let k = |a: &str, b: &str| UpdateKind::between(&v(a), &v(b));
        assert_eq!(k("1.2.3", "1.2.4"), Some(UpdateKind::Patch));
        assert_eq!(k("1.2.3", "1.3.0"), Some(UpdateKind::Minor));
        assert_eq!(k("1.2.3", "2.0.0"), Some(UpdateKind::Major));
        assert_eq!(k("0.2.3", "0.3.0"), Some(UpdateKind::Minor));
        assert_eq!(k("1.2.3", "1.2.3"), None);
        assert_eq!(k("1.2.3", "1.2.2"), None);
        assert_eq!(k("1.0.0-rc.1", "1.0.0"), Some(UpdateKind::Patch));
        assert!(UpdateKind::Patch < UpdateKind::Minor && UpdateKind::Minor < UpdateKind::Major);
        assert_eq!(UpdateKind::parse("MAJOR"), Some(UpdateKind::Major));
        let p = |s: &str| Version::parse(Scheme::Pep440, s).unwrap();
        assert_eq!(
            UpdateKind::between(&p("1!1.0"), &p("2.0")),
            None,
            "an epoch outranks a major number"
        );
        assert_eq!(
            UpdateKind::between(&p("2.0"), &p("1!1.0")),
            Some(UpdateKind::Major)
        );
    }

    #[test]
    fn a_requirement_has_a_floor() {
        let f = requirement_floor;
        assert_eq!(f("^1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(f("~1.2").as_deref(), Some("1.2"));
        assert_eq!(f("~> 1.2").as_deref(), Some("1.2"));
        assert_eq!(f(">=1.0.0 <2.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(f(">=1.0, <2").as_deref(), Some("1.0"));
        assert_eq!(f("<2.0, >=1.5").as_deref(), Some("1.5"));
        assert_eq!(f("1.x").as_deref(), Some("1.0"));
        assert_eq!(f("1.2.*").as_deref(), Some("1.2.0"));
        assert_eq!(f("[1.0,2.0)").as_deref(), Some("1.0"));
        assert_eq!(f("[13.0.1, )").as_deref(), Some("13.0.1"));
        assert_eq!(f("==2.31.0").as_deref(), Some("2.31.0"));
        assert_eq!(f("~=1.4.2").as_deref(), Some("1.4.2"));
        assert_eq!(f("1.2.3 - 2.0.0").as_deref(), Some("1.2.3"));
        assert_eq!(f("^1.0.0 || ^2.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(f("v1.4.0").as_deref(), Some("1.4.0"));
        assert_eq!(f("1.0.0-beta.1").as_deref(), Some("1.0.0-beta.1"));
        for none in [
            "*",
            "latest",
            "",
            "x",
            "<2.0",
            "(,1.0]",
            "git+https://github.com/a/b",
            "file:../a",
            "workspace:*",
            "npm:other@1.0.0",
            "dev-master",
            "github:a/b",
        ] {
            assert_eq!(f(none), None, "{none:?}");
        }
    }
}
