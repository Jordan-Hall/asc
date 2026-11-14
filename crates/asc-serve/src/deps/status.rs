//! One security status of a repository (docs/DEPENDENCIES.md "Status"): is the trunk scan fresh and
//! complete, are there open findings, and which package moves would close them.
//!
//! The rules, in order: no recorded scan, a lookup that was not complete, or a scan older than twice the
//! rescan interval is `Unknown` (never clean); any open finding is `Findings`; otherwise `Clean`. A
//! dismissed or fixed finding does not count. The functions here are pure; [`super::hub::DepsHub::status`]
//! reads the rows.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use asc_core::workflow::Cron;

use super::model::Ecosystem;
use super::store::{FindingRow, ScanRow, State};
use super::version::{self, Scheme, Version};

/// Most moves in one status.
pub const MAX_MOVES: usize = 100;
/// How long a scan stays fresh when the repository sets no schedule, in seconds.
pub const DEFAULT_STALE_SECS: i64 = 48 * 3600;
/// `ScanRow::lookup` of a scan that matched every package (`DepLookupStatus::Complete`).
const LOOKUP_COMPLETE: i64 = 1;

/// The verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Unknown,
    Clean,
    Findings,
}

/// Why the verdict is `Unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    None,
    NotScanned,
    LookupPartial,
    Stale,
}

impl Reason {
    /// The text clients show; empty when the verdict is known.
    pub fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::NotScanned => "not scanned",
            Self::LookupPartial => "lookup partial",
            Self::Stale => "stale",
        }
    }
}

/// One package version with open findings and the version that closes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Move {
    /// The OSV ecosystem name.
    pub ecosystem: String,
    pub name: String,
    pub from: String,
    /// The highest fixed version of its findings; empty: no fix yet.
    pub to: String,
}

/// The status of a repository at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoStatus {
    pub verdict: Verdict,
    pub reason: Reason,
    pub open_by_severity: BTreeMap<String, u32>,
    pub moves: Vec<Move>,
    /// When the trunk scan ran; 0 when none did.
    pub scanned_at: i64,
    /// The scanned changeset, as stored; empty when none.
    pub scanned: Vec<u8>,
}

/// How old a scan may be before it is stale: twice the shortest gap of the repository's cron schedule, or
/// [`DEFAULT_STALE_SECS`] when there is none.
pub fn stale_after(schedule: Option<&str>) -> i64 {
    schedule
        .and_then(|s| Cron::parse(s).ok())
        .and_then(|c| c.min_interval_secs())
        .and_then(|gap| i64::try_from(gap).ok())
        .map_or(DEFAULT_STALE_SECS, |gap| gap.saturating_mul(2))
}

/// The status from the open findings (`open`: state open only), their counts per severity, and the last
/// trunk scan.
pub fn repo_status(
    open: &[FindingRow],
    open_by_severity: BTreeMap<String, u32>,
    last: Option<&ScanRow>,
    now: i64,
    stale_after: i64,
) -> RepoStatus {
    let (verdict, reason) = match last {
        None => (Verdict::Unknown, Reason::NotScanned),
        Some(l) if l.lookup != LOOKUP_COMPLETE => (Verdict::Unknown, Reason::LookupPartial),
        Some(l) if now.saturating_sub(l.at) > stale_after => (Verdict::Unknown, Reason::Stale),
        Some(_) => {
            let any = !open.is_empty() || open_by_severity.values().any(|n| *n > 0);
            (
                if any {
                    Verdict::Findings
                } else {
                    Verdict::Clean
                },
                Reason::None,
            )
        }
    };
    RepoStatus {
        verdict,
        reason,
        open_by_severity,
        moves: moves(open),
        scanned_at: last.map_or(0, |l| l.at),
        scanned: last.map(|l| l.rev.clone()).unwrap_or_default(),
    }
}

/// The package moves that close the open findings in `findings`: one per (ecosystem, name, version), `to`
/// the highest fixed version among its findings under the ecosystem's ordering. Findings that are not
/// open are skipped. Sorted by ecosystem, name and version; at most [`MAX_MOVES`].
pub fn moves(findings: &[FindingRow]) -> Vec<Move> {
    let mut by: BTreeMap<(&str, &str, &str), &str> = BTreeMap::new();
    for f in findings.iter().filter(|f| f.state == State::Open) {
        let scheme = Ecosystem::parse(&f.ecosystem).map_or(Scheme::Semver, Ecosystem::scheme);
        let best = by.entry((&f.ecosystem, &f.name, &f.version)).or_insert("");
        if higher(scheme, &f.fixed_in, best) {
            *best = &f.fixed_in;
        }
    }
    by.into_iter()
        .take(MAX_MOVES)
        .map(|((ecosystem, name, from), to)| Move {
            ecosystem: ecosystem.to_owned(),
            name: name.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
        })
        .collect()
}

/// Whether `a` is a higher version than `b`. Empty is lowest; a version that does not parse is lower
/// than one that does, and two of those order as text.
fn higher(scheme: Scheme, a: &str, b: &str) -> bool {
    if a.is_empty() {
        return false;
    }
    if b.is_empty() {
        return true;
    }
    match version::compare(scheme, a, b) {
        Some(o) => o == Ordering::Greater,
        None => {
            let a_ok = Version::parse(scheme, a).is_some();
            let b_ok = Version::parse(scheme, b).is_some();
            match (a_ok, b_ok) {
                (true, false) => true,
                (false, true) => false,
                _ => a > b,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::Advisory;
    use super::*;

    fn row(eco: &str, name: &str, version: &str, fixed: &str, state: State) -> FindingRow {
        FindingRow {
            id: 1,
            ecosystem: eco.into(),
            name: name.into(),
            version: version.into(),
            path: "Cargo.lock".into(),
            direct: true,
            advisory: Advisory {
                id: "A".into(),
                aliases: vec![],
                summary: String::new(),
                severity: None,
                cvss: 0.0,
                fixed: vec![],
                url: String::new(),
                affected: vec![],
            },
            state,
            fixed_in: fixed.into(),
            first_seen: 0,
            last_seen: 0,
            dismissed_by: String::new(),
            dismissed_at: 0,
            dismiss_reason: 0,
            note: String::new(),
        }
    }

    fn scan(at: i64, lookup: i64) -> ScanRow {
        ScanRow {
            at,
            rev: vec![1, 2],
            lookup,
            packages: 3,
            unknown: 0,
            by_ecosystem: BTreeMap::new(),
        }
    }

    const NOW: i64 = 1_000_000;

    fn status(open: &[FindingRow], last: Option<&ScanRow>) -> RepoStatus {
        let mut sev = BTreeMap::new();
        if !open.is_empty() {
            sev.insert("high".to_owned(), open.len() as u32);
        }
        repo_status(open, sev, last, NOW, 1000)
    }

    #[test]
    fn no_scan_is_unknown_not_scanned() {
        let s = status(&[], None);
        assert_eq!(
            (s.verdict, s.reason),
            (Verdict::Unknown, Reason::NotScanned)
        );
        assert_eq!(s.reason.text(), "not scanned");
        assert_eq!((s.scanned_at, s.scanned.len()), (0, 0));
    }

    #[test]
    fn a_partial_or_failed_lookup_is_unknown_even_with_no_findings() {
        for lookup in [2, 3, 0] {
            let s = status(&[], Some(&scan(NOW, lookup)));
            assert_eq!(
                (s.verdict, s.reason),
                (Verdict::Unknown, Reason::LookupPartial)
            );
        }
    }

    #[test]
    fn an_old_scan_is_stale_and_the_edge_is_fresh() {
        let s = status(&[], Some(&scan(NOW - 1001, 1)));
        assert_eq!((s.verdict, s.reason), (Verdict::Unknown, Reason::Stale));
        let s = status(&[], Some(&scan(NOW - 1000, 1)));
        assert_eq!((s.verdict, s.reason), (Verdict::Clean, Reason::None));
        assert_eq!(s.scanned, vec![1, 2]);
    }

    #[test]
    fn open_findings_make_findings_and_dismissed_ones_do_not() {
        let open = [row("crates.io", "a", "1.0.0", "1.0.1", State::Open)];
        let s = status(&open, Some(&scan(NOW, 1)));
        assert_eq!(s.verdict, Verdict::Findings);
        assert_eq!(s.open_by_severity["high"], 1);
        assert_eq!(s.moves.len(), 1);
        let none = status(&[], Some(&scan(NOW, 1)));
        assert_eq!(none.verdict, Verdict::Clean);
        let dismissed = [row("crates.io", "a", "1.0.0", "1.0.1", State::Dismissed)];
        assert!(moves(&dismissed).is_empty());
    }

    #[test]
    fn the_highest_fixed_version_is_picked_by_the_ecosystem_order() {
        let f = [
            row("crates.io", "a", "1.0.0", "1.9.0", State::Open),
            row("crates.io", "a", "1.0.0", "1.10.0", State::Open),
            row("crates.io", "a", "1.0.0", "", State::Open),
        ];
        let m = moves(&f);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].from.as_str(), m[0].to.as_str()), ("1.0.0", "1.10.0"));
        let p = [
            row("PyPI", "p", "1.0", "1.0rc1", State::Open),
            row("PyPI", "p", "1.0", "1.0", State::Open),
            row("PyPI", "p", "1.0", "1.0.post1", State::Open),
        ];
        assert_eq!(moves(&p)[0].to, "1.0.post1");
        let mv = [
            row("Maven", "m", "1.0", "1.0-alpha", State::Open),
            row("Maven", "m", "1.0", "1.0.1", State::Open),
        ];
        assert_eq!(moves(&mv)[0].to, "1.0.1");
    }

    #[test]
    fn no_fix_yet_is_an_empty_target_and_versions_move_apart() {
        let f = [
            row("npm", "x", "1.0.0", "", State::Open),
            row("npm", "x", "2.0.0", "2.0.1", State::Open),
            row("crates.io", "x", "1.0.0", "1.0.1", State::Open),
        ];
        let m = moves(&f);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].ecosystem, "crates.io");
        let npm1 = m
            .iter()
            .find(|m| m.from == "1.0.0" && m.ecosystem == "npm")
            .map(|m| m.to.as_str());
        assert_eq!(npm1, Some(""));
    }

    #[test]
    fn moves_are_capped_at_one_hundred() {
        let f: Vec<FindingRow> = (0..150)
            .map(|i| row("npm", &format!("p{i:03}"), "1.0.0", "1.0.1", State::Open))
            .collect();
        let m = moves(&f);
        assert_eq!(m.len(), MAX_MOVES);
        assert_eq!(m[0].name, "p000");
    }

    #[test]
    fn staleness_follows_the_schedule_or_two_days() {
        assert_eq!(stale_after(None), 48 * 3600);
        assert_eq!(stale_after(Some("garbage")), 48 * 3600);
        assert_eq!(stale_after(Some("0 4 * * *")), 2 * 86_400);
        assert_eq!(stale_after(Some("0 4 * * 1")), 2 * 7 * 86_400);
    }
}
