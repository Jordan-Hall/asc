//! `.asc/deps.toml`: what a repository wants of dependency scanning and update proposals.
//!
//! The file lives on trunk, so a change to it is reviewed like code, and the server reads the version on
//! the trunk tip. It holds choices only: which ecosystems, how often, what to leave alone, how to group
//! updates. It never names an address; the endpoints the server calls are the operator's `[deps]`
//! section.
//!
//! ```toml
//! version = 1
//! ecosystems = ["cargo", "npm"]    # OSV names or common aliases; empty: every ecosystem found
//! schedule = "0 4 * * 1"           # five-field cron, UTC; empty: no scheduled run
//! security_only = false            # propose only updates that fix an open advisory
//! severity_threshold = "high"      # advisories below this are reported but not proposed for
//! max_kind = "minor"               # the largest update a proposal makes: patch | minor | major
//! max_proposals = 5                # changesets one run records, at most 50
//! grouping = "ecosystem"           # none | ecosystem | kind | all
//!
//! [[group]]                        # checked before `grouping`
//! name = "lint-tools"
//! ecosystem = "npm"
//! patterns = ["eslint*", "prettier"]
//! kinds = ["patch", "minor"]
//!
//! [[ignore]]
//! ecosystem = "npm"
//! name = "left-pad"
//! kinds = ["major"]                # update kinds not proposed; empty: no update at all
//! advisories = ["GHSA-xxxx"]       # advisories not reported for this package
//! until = "2027-01-01"             # the entry lapses on this day (UTC); empty: never
//! reason = "pinned by the vendor"
//! ```
//!
//! Unknown keys, a second `[[group]]` of one name, an unknown ecosystem and a schedule more frequent than
//! once an hour are errors; an invalid file is never read in part.

use serde::Deserialize;

use asc_core::policy::DepSeverity;
use asc_core::workflow::Cron;

use super::model::Ecosystem;
use super::version::UpdateKind;

/// Where the file lives.
pub const PATH: &str = ".asc/deps.toml";
/// The one schema version this code reads.
pub const VERSION: u32 = 1;
/// Largest file.
pub const MAX_BYTES: usize = 64 * 1024;
/// Most groups, ignore entries and patterns per list.
pub const MAX_ENTRIES: usize = 128;
/// Most changesets one run may record.
pub const MAX_PROPOSALS: u32 = 50;
/// The default of `max_proposals`.
pub const DEFAULT_PROPOSALS: u32 = 5;
/// The shortest gap between two scheduled runs.
pub const MIN_SCHEDULE_SECS: u64 = 3600;
/// Longest name, pattern and reason.
const MAX_TEXT: usize = 256;

/// How updates are put into changesets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouping {
    /// One changeset per package.
    None,
    /// One per ecosystem.
    Ecosystem,
    /// One per ecosystem and update kind.
    Kind,
    /// All in one.
    All,
}

impl Grouping {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ecosystem => "ecosystem",
            Self::Kind => "kind",
            Self::All => "all",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" | "" => Some(Self::None),
            "ecosystem" => Some(Self::Ecosystem),
            "kind" => Some(Self::Kind),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub ecosystem: Option<Ecosystem>,
    pub patterns: Vec<String>,
    pub kinds: Vec<UpdateKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ignore {
    pub ecosystem: Option<Ecosystem>,
    pub name: String,
    pub kinds: Vec<UpdateKind>,
    pub advisories: Vec<String>,
    /// `YYYY-MM-DD`.
    pub until: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub ecosystems: Vec<Ecosystem>,
    pub schedule: Option<String>,
    pub security_only: bool,
    pub severity_threshold: Option<DepSeverity>,
    pub max_kind: UpdateKind,
    pub max_proposals: u32,
    pub grouping: Grouping,
    pub groups: Vec<Group>,
    pub ignore: Vec<Ignore>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ecosystems: Vec::new(),
            schedule: None,
            security_only: false,
            severity_threshold: None,
            max_kind: UpdateKind::Minor,
            max_proposals: DEFAULT_PROPOSALS,
            grouping: Grouping::None,
            groups: Vec::new(),
            ignore: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    version: u32,
    #[serde(default)]
    ecosystems: Vec<String>,
    #[serde(default)]
    schedule: String,
    #[serde(default)]
    security_only: bool,
    #[serde(default)]
    severity_threshold: String,
    #[serde(default)]
    max_kind: String,
    max_proposals: Option<u32>,
    #[serde(default)]
    grouping: String,
    #[serde(default)]
    group: Vec<RawGroup>,
    #[serde(default)]
    ignore: Vec<RawIgnore>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    name: String,
    #[serde(default)]
    ecosystem: String,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    kinds: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIgnore {
    #[serde(default)]
    ecosystem: String,
    name: String,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    advisories: Vec<String>,
    #[serde(default)]
    until: String,
    #[serde(default)]
    reason: String,
}

fn kinds(list: &[String], what: &str) -> Result<Vec<UpdateKind>, String> {
    let mut out = Vec::new();
    for k in list {
        let kind = UpdateKind::parse(k)
            .ok_or_else(|| format!("{what}: {k:?} is not patch, minor or major"))?;
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    Ok(out)
}

fn ecosystem(s: &str, what: &str) -> Result<Option<Ecosystem>, String> {
    if s.is_empty() {
        return Ok(None);
    }
    Ecosystem::parse(s)
        .map(Some)
        .ok_or_else(|| format!("{what}: {s:?} is not a known ecosystem"))
}

fn plain(s: &str, what: &str) -> Result<(), String> {
    if s.len() > MAX_TEXT || s.chars().any(char::is_control) {
        return Err(format!("{what} is too long or holds a control character"));
    }
    Ok(())
}

/// A name for a group or a ref: lower-case letters, digits, `-`, `.` and `_`, at most 60 bytes.
pub fn valid_group_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 60
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'_')
        })
}

/// A `YYYY-MM-DD` that names a day.
pub fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<u32>().ok();
    let (Some(y), Some(m), Some(d)) = (num(0..4), num(5..7), num(8..10)) else {
        return false;
    };
    if !(1..=12).contains(&m) || d == 0 {
        return false;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        2 => 28 + u32::from(leap),
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    d <= days && s.bytes().filter(u8::is_ascii_digit).count() == 8
}

/// The UTC day of `unix_secs` as `YYYY-MM-DD`.
pub fn date_of(unix_secs: i64) -> String {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = unix_secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Whether `text` matches `pattern`, where `*` matches any run of characters. Case does not matter.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

impl Config {
    /// Parses and validates a file. The error names the key.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err(format!("the file is larger than {MAX_BYTES} bytes"));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "the file is not UTF-8".to_owned())?;
        let raw: Raw = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        if raw.version != VERSION {
            return Err(format!("version must be {VERSION}"));
        }
        if raw.ecosystems.len() > 32
            || raw.group.len() > MAX_ENTRIES
            || raw.ignore.len() > MAX_ENTRIES
        {
            return Err("too many ecosystems, groups or ignore entries".into());
        }
        let mut ecosystems = Vec::new();
        for e in &raw.ecosystems {
            let eco = Ecosystem::parse(e)
                .ok_or_else(|| format!("ecosystems: {e:?} is not a known ecosystem"))?;
            if !ecosystems.contains(&eco) {
                ecosystems.push(eco);
            }
        }
        let schedule = if raw.schedule.trim().is_empty() {
            None
        } else {
            let cron = Cron::parse(raw.schedule.trim()).map_err(|e| format!("schedule: {e}"))?;
            if cron
                .min_interval_secs()
                .is_some_and(|gap| gap < MIN_SCHEDULE_SECS)
            {
                return Err("schedule: a scan runs at most once an hour".into());
            }
            Some(cron.as_str().to_owned())
        };
        let severity_threshold = if raw.severity_threshold.is_empty() {
            None
        } else {
            Some(DepSeverity::parse(&raw.severity_threshold).ok_or_else(|| {
                "severity_threshold is not low, moderate, high or critical".to_owned()
            })?)
        };
        let max_kind = if raw.max_kind.is_empty() {
            UpdateKind::Minor
        } else {
            UpdateKind::parse(&raw.max_kind)
                .ok_or_else(|| "max_kind is not patch, minor or major".to_owned())?
        };
        let max_proposals = raw.max_proposals.unwrap_or(DEFAULT_PROPOSALS);
        if max_proposals == 0 || max_proposals > MAX_PROPOSALS {
            return Err(format!("max_proposals must be 1 to {MAX_PROPOSALS}"));
        }
        let grouping = Grouping::parse(&raw.grouping)
            .ok_or_else(|| "grouping is not none, ecosystem, kind or all".to_owned())?;
        let mut groups: Vec<Group> = Vec::new();
        for g in &raw.group {
            if !valid_group_name(&g.name) {
                return Err(format!(
                    "group {:?}: a name is lower-case letters, digits, `-`, `.` and `_`, up to 60",
                    g.name
                ));
            }
            if groups.iter().any(|x| x.name == g.name) {
                return Err(format!("group {:?} is named twice", g.name));
            }
            if g.patterns.is_empty() || g.patterns.len() > MAX_ENTRIES {
                return Err(format!("group {:?} needs `patterns`", g.name));
            }
            for p in &g.patterns {
                plain(p, "a pattern")?;
                if p.is_empty() {
                    return Err(format!("group {:?} has an empty pattern", g.name));
                }
            }
            groups.push(Group {
                name: g.name.clone(),
                ecosystem: ecosystem(&g.ecosystem, &format!("group {:?}", g.name))?,
                patterns: g.patterns.clone(),
                kinds: kinds(&g.kinds, &format!("group {:?}", g.name))?,
            });
        }
        let mut ignore = Vec::new();
        for i in &raw.ignore {
            plain(&i.name, "an ignore name")?;
            plain(&i.reason, "an ignore reason")?;
            if i.name.is_empty() {
                return Err("an ignore entry needs `name`".into());
            }
            if !i.until.is_empty() && !valid_date(&i.until) {
                return Err(format!(
                    "ignore {:?}: until is not a YYYY-MM-DD date",
                    i.name
                ));
            }
            if i.advisories.len() > MAX_ENTRIES {
                return Err(format!("ignore {:?}: too many advisories", i.name));
            }
            for a in &i.advisories {
                plain(a, "an advisory id")?;
            }
            ignore.push(Ignore {
                ecosystem: ecosystem(&i.ecosystem, &format!("ignore {:?}", i.name))?,
                name: i.name.clone(),
                kinds: kinds(&i.kinds, &format!("ignore {:?}", i.name))?,
                advisories: i.advisories.clone(),
                until: i.until.clone(),
                reason: i.reason.clone(),
            });
        }
        Ok(Self {
            ecosystems,
            schedule,
            security_only: raw.security_only,
            severity_threshold,
            max_kind,
            max_proposals,
            grouping,
            groups,
            ignore,
        })
    }

    /// Whether `eco` is wanted.
    pub fn wants(&self, eco: Ecosystem) -> bool {
        self.ecosystems.is_empty() || self.ecosystems.contains(&eco)
    }

    fn entry_matches(i: &Ignore, eco: Ecosystem, name: &str, today: &str) -> bool {
        (i.until.is_empty() || i.until.as_str() >= today)
            && i.ecosystem.is_none_or(|e| e == eco)
            && (glob(&i.name, name) || glob(&i.name, &eco.canonical(name)))
    }

    /// Whether updates of `kind` for the package are left alone on `today`.
    pub fn update_ignored(
        &self,
        eco: Ecosystem,
        name: &str,
        kind: UpdateKind,
        today: &str,
    ) -> bool {
        self.ignore.iter().any(|i| {
            Self::entry_matches(i, eco, name, today)
                && (i.advisories.is_empty() || !i.kinds.is_empty())
                && (i.kinds.is_empty() || i.kinds.contains(&kind))
        })
    }

    /// Whether the advisory (by id or alias) is not reported for the package on `today`.
    pub fn advisory_ignored(&self, eco: Ecosystem, name: &str, ids: &[&str], today: &str) -> bool {
        self.ignore.iter().any(|i| {
            Self::entry_matches(i, eco, name, today)
                && i.advisories
                    .iter()
                    .any(|a| ids.iter().any(|id| id.eq_ignore_ascii_case(a)))
        })
    }

    /// The group an update belongs to: the first `[[group]]` that matches, else the one `grouping`
    /// gives. The result is a valid group name.
    pub fn group_of(&self, eco: Ecosystem, name: &str, kind: UpdateKind) -> String {
        for g in &self.groups {
            if g.ecosystem.is_none_or(|e| e == eco)
                && (g.kinds.is_empty() || g.kinds.contains(&kind))
                && g.patterns
                    .iter()
                    .any(|p| glob(p, name) || glob(p, &eco.canonical(name)))
            {
                return g.name.clone();
            }
        }
        let slug = eco_slug(eco);
        match self.grouping {
            Grouping::None => slugify(&format!("{slug}-{name}")),
            Grouping::Ecosystem => slug.to_owned(),
            Grouping::Kind => format!("{slug}-{kind}"),
            Grouping::All => "all".to_owned(),
        }
    }

    /// The canonical text of the file.
    pub fn render(&self) -> String {
        let q = |s: &str| format!("{s:?}");
        let list = |v: Vec<String>| format!("[{}]", v.join(", "));
        let mut out = String::from(
            "# Dependency scanning and update proposals (docs/DEPENDENCIES.md).\nversion = 1\n",
        );
        out.push_str(&format!(
            "ecosystems = {}\n",
            list(self.ecosystems.iter().map(|e| q(e.osv_name())).collect())
        ));
        if let Some(s) = &self.schedule {
            out.push_str(&format!("schedule = {}\n", q(s)));
        }
        out.push_str(&format!("security_only = {}\n", self.security_only));
        if let Some(s) = self.severity_threshold {
            out.push_str(&format!("severity_threshold = {}\n", q(s.name())));
        }
        out.push_str(&format!("max_kind = {}\n", q(self.max_kind.name())));
        out.push_str(&format!("max_proposals = {}\n", self.max_proposals));
        out.push_str(&format!("grouping = {}\n", q(self.grouping.name())));
        let kind_list = |k: &[UpdateKind]| list(k.iter().map(|k| q(k.name())).collect());
        for g in &self.groups {
            out.push_str("\n[[group]]\n");
            out.push_str(&format!("name = {}\n", q(&g.name)));
            if let Some(e) = g.ecosystem {
                out.push_str(&format!("ecosystem = {}\n", q(e.osv_name())));
            }
            out.push_str(&format!(
                "patterns = {}\n",
                list(g.patterns.iter().map(|p| q(p)).collect())
            ));
            if !g.kinds.is_empty() {
                out.push_str(&format!("kinds = {}\n", kind_list(&g.kinds)));
            }
        }
        for i in &self.ignore {
            out.push_str("\n[[ignore]]\n");
            if let Some(e) = i.ecosystem {
                out.push_str(&format!("ecosystem = {}\n", q(e.osv_name())));
            }
            out.push_str(&format!("name = {}\n", q(&i.name)));
            if !i.kinds.is_empty() {
                out.push_str(&format!("kinds = {}\n", kind_list(&i.kinds)));
            }
            if !i.advisories.is_empty() {
                out.push_str(&format!(
                    "advisories = {}\n",
                    list(i.advisories.iter().map(|a| q(a)).collect())
                ));
            }
            if !i.until.is_empty() {
                out.push_str(&format!("until = {}\n", q(&i.until)));
            }
            if !i.reason.is_empty() {
                out.push_str(&format!("reason = {}\n", q(&i.reason)));
            }
        }
        out
    }
}

/// The short name of an ecosystem in refs and group names.
pub fn eco_slug(eco: Ecosystem) -> &'static str {
    match eco {
        Ecosystem::CratesIo => "cargo",
        Ecosystem::Npm => "npm",
        Ecosystem::PyPi => "pip",
        Ecosystem::Go => "go",
        Ecosystem::Maven => "maven",
        Ecosystem::NuGet => "nuget",
        Ecosystem::Packagist => "composer",
        Ecosystem::RubyGems => "bundler",
        Ecosystem::Hex => "hex",
        Ecosystem::Pub => "pub",
        Ecosystem::SwiftUrl => "swift",
        Ecosystem::CocoaPods => "pods",
    }
}

/// `s` as a valid group name: lower case, anything else becomes `-`, at most 60 bytes.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_');
        let c = if ok { c } else { '-' };
        if c == '-' && (out.is_empty() || out.ends_with('-')) {
            continue;
        }
        out.push(c);
    }
    let mut out: String = out.chars().take(60).collect();
    while out.ends_with(['-', '.']) {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("dep");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
version = 1
ecosystems = ["cargo", "NPM", "crates.io"]
schedule = "0  4 * * 1"
security_only = true
severity_threshold = "High"
max_kind = "major"
max_proposals = 7
grouping = "kind"

[[group]]
name = "lint-tools"
ecosystem = "npm"
patterns = ["eslint*", "prettier"]
kinds = ["patch", "minor", "patch"]

[[ignore]]
ecosystem = "npm"
name = "left-pad"
kinds = ["major"]
until = "2027-01-01"
reason = "pinned by the vendor"

[[ignore]]
name = "Django*"
advisories = ["GHSA-aaaa"]
"#;

    #[test]
    fn a_full_file_parses_and_round_trips_through_render() {
        let c = Config::parse(FULL.as_bytes()).unwrap();
        assert_eq!(c.ecosystems, [Ecosystem::CratesIo, Ecosystem::Npm]);
        assert_eq!(c.schedule.as_deref(), Some("0 4 * * 1"));
        assert!(c.security_only);
        assert_eq!(c.severity_threshold, Some(DepSeverity::High));
        assert_eq!(c.max_kind, UpdateKind::Major);
        assert_eq!(c.max_proposals, 7);
        assert_eq!(c.grouping, Grouping::Kind);
        assert_eq!(c.groups[0].kinds, [UpdateKind::Patch, UpdateKind::Minor]);
        assert_eq!(c.ignore.len(), 2);
        let again = Config::parse(c.render().as_bytes()).unwrap();
        assert_eq!(again, c);
        assert_eq!(again.render(), c.render());
    }

    #[test]
    fn a_minimal_file_has_the_documented_defaults() {
        let c = Config::parse(b"version = 1\n").unwrap();
        assert_eq!(c, Config::default());
        assert!(c.wants(Ecosystem::Go));
        assert_eq!(c.max_kind, UpdateKind::Minor);
        assert_eq!(c.max_proposals, 5);
        assert_eq!(c.grouping, Grouping::None);
        assert_eq!(Config::parse(c.render().as_bytes()).unwrap(), c);
    }

    #[test]
    fn invalid_files_are_errors_that_name_the_key() {
        let head = "version = 1\n";
        for (text, want) in [
            ("".to_owned(), "version"),
            ("version = 2\n".to_owned(), "version must be 1"),
            (format!("{head}bogus = 1\n"), "bogus"),
            (format!("{head}ecosystems = [\"cobol\"]\n"), "cobol"),
            (format!("{head}schedule = \"* * * * *\"\n"), "once an hour"),
            (
                format!("{head}schedule = \"*/30 * * * *\"\n"),
                "once an hour",
            ),
            (format!("{head}schedule = \"nonsense\"\n"), "schedule"),
            (
                format!("{head}severity_threshold = \"severe\"\n"),
                "severity_threshold",
            ),
            (format!("{head}max_kind = \"huge\"\n"), "max_kind"),
            (format!("{head}max_proposals = 0\n"), "max_proposals"),
            (format!("{head}max_proposals = 51\n"), "max_proposals"),
            (format!("{head}grouping = \"weekly\"\n"), "grouping"),
            (
                format!("{head}[[group]]\nname = \"A B\"\npatterns = [\"x\"]\n"),
                "group",
            ),
            (format!("{head}[[group]]\nname = \"a\"\n"), "patterns"),
            (
                format!(
                    "{head}[[group]]\nname = \"a\"\npatterns = [\"x\"]\n[[group]]\nname = \"a\"\npatterns = [\"y\"]\n"
                ),
                "twice",
            ),
            (
                format!("{head}[[ignore]]\nname = \"x\"\nuntil = \"2027-02-30\"\n"),
                "until",
            ),
            (
                format!("{head}[[ignore]]\nname = \"x\"\nkinds = [\"giant\"]\n"),
                "giant",
            ),
            (
                format!("{head}[[ignore]]\nname = \"x\"\necosystem = \"cobol\"\n"),
                "cobol",
            ),
            (format!("{head}[[ignore]]\nreason = \"no name\"\n"), "name"),
        ] {
            let e = Config::parse(text.as_bytes()).unwrap_err();
            assert!(e.contains(want), "{text:?}: {e}");
        }
        assert!(Config::parse(&vec![b' '; MAX_BYTES + 1]).is_err());
        assert!(Config::parse(b"\xff").is_err());
    }

    #[test]
    fn dates_are_valid_and_computed_in_utc() {
        for ok in ["2027-01-01", "2028-02-29", "1999-12-31"] {
            assert!(valid_date(ok), "{ok}");
        }
        for bad in [
            "2027-02-29",
            "2027-13-01",
            "2027-00-10",
            "27-01-01",
            "2027/01/01",
            "2027-1-01",
            "+027-01-01",
            "",
        ] {
            assert!(!valid_date(bad), "{bad}");
        }
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(86_399), "1970-01-01");
        assert_eq!(date_of(86_400), "1970-01-02");
        assert_eq!(date_of(1_709_164_800), "2024-02-29");
        assert_eq!(date_of(1_793_000_000), "2026-10-26");
        assert_eq!(date_of(-1), "1969-12-31");
    }

    #[test]
    fn globs_match_stars_and_ignore_case() {
        assert!(glob("eslint*", "eslint-plugin-react"));
        assert!(glob("*", "anything"));
        assert!(glob("@types/*", "@types/node"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(glob("PRETTIER", "prettier"));
        assert!(!glob("eslint", "eslint-plugin"));
        assert!(!glob("a*b", "ac"));
        assert!(glob("", ""));
        assert!(!glob("", "x"));
    }

    #[test]
    fn ignore_entries_apply_by_package_kind_advisory_and_day() {
        let c = Config::parse(FULL.as_bytes()).unwrap();
        let npm = Ecosystem::Npm;
        let today = "2026-10-27";
        // `left-pad`: major updates only, until the end of 2026.
        assert!(c.update_ignored(npm, "left-pad", UpdateKind::Major, today));
        assert!(!c.update_ignored(npm, "left-pad", UpdateKind::Minor, today));
        assert!(!c.update_ignored(Ecosystem::CratesIo, "left-pad", UpdateKind::Major, today));
        assert!(c.update_ignored(npm, "left-pad", UpdateKind::Major, "2027-01-01"));
        assert!(
            !c.update_ignored(npm, "left-pad", UpdateKind::Major, "2027-01-02"),
            "lapsed"
        );
        // `Django*`: only advisories, so updates are not ignored.
        let py = Ecosystem::PyPi;
        assert!(!c.update_ignored(py, "django", UpdateKind::Patch, today));
        assert!(c.advisory_ignored(py, "Django", &["GHSA-AAAA"], today));
        assert!(c.advisory_ignored(py, "django-rest", &["CVE-1", "ghsa-aaaa"], today));
        assert!(!c.advisory_ignored(py, "django", &["GHSA-bbbb"], today));
        assert!(!c.advisory_ignored(npm, "left-pad", &["GHSA-aaaa"], today));
    }

    #[test]
    fn updates_are_grouped_by_the_first_matching_group_then_by_the_strategy() {
        let mut c = Config::parse(FULL.as_bytes()).unwrap();
        let g = |c: &Config, e, n, k| c.group_of(e, n, k);
        assert_eq!(
            g(&c, Ecosystem::Npm, "eslint-plugin-react", UpdateKind::Minor),
            "lint-tools"
        );
        assert_eq!(
            g(&c, Ecosystem::Npm, "eslint", UpdateKind::Major),
            "npm-major",
            "kind not in the group"
        );
        assert_eq!(
            g(&c, Ecosystem::CratesIo, "eslint", UpdateKind::Minor),
            "cargo-minor"
        );
        c.grouping = Grouping::None;
        assert_eq!(
            g(&c, Ecosystem::Npm, "@Scope/Pkg", UpdateKind::Patch),
            "npm-scope-pkg"
        );
        c.grouping = Grouping::Ecosystem;
        assert_eq!(g(&c, Ecosystem::PyPi, "x", UpdateKind::Patch), "pip");
        c.grouping = Grouping::All;
        assert_eq!(g(&c, Ecosystem::PyPi, "x", UpdateKind::Patch), "all");
        for name in ["@Scope/Pkg", "..", "a b c", "ÜBER", &"x".repeat(200)] {
            c.grouping = Grouping::None;
            let s = c.group_of(Ecosystem::Npm, name, UpdateKind::Patch);
            assert!(valid_group_name(&s), "{name:?} -> {s:?}");
        }
        assert_eq!(slugify(""), "dep");
        assert_eq!(slugify("--"), "dep");
    }
}
