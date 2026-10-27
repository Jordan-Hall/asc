//! Which version a package moves to.
//!
//! Pure decisions over a package's current version, the versions its registry lists and the advisories
//! that affect it. Nothing here asks the network.
//!
//! * An ordinary update goes to the newest stable version whose step from the current one is no bigger
//!   than the limit (`max_kind`); when only bigger ones exist the update is *blocked*, not forgotten, and
//!   says what it would take.
//! * A security update goes to the *smallest* version that no advisory with a fix still affects, judged
//!   by each advisory's own ranges (not by "the smallest fixed version above the current one": an
//!   advisory with a backport line affects versions of another release line above that). If every
//!   version within the limit is still affected the update is blocked, and says by which advisories.
//! * Whatever advisory still affects the chosen version is `unfixed`, never `fixes`; an advisory that
//!   names no fix is always unfixed.
//! * An advisory that states no severity counts as moderate (an answer that cannot be shown to be minor
//!   is not minor), against the repository's severity threshold and the policy's `block_severity`.

use asc_core::policy::DepSeverity;

use super::model::{Advisory, Ecosystem};
use super::version::{UpdateKind, Version};

/// The severity an advisory counts as.
pub fn effective(a: &Advisory) -> DepSeverity {
    a.severity.unwrap_or(DepSeverity::Moderate)
}

/// Whether `candidate` is still affected by `a`, for a package now at `current`. By the advisory's ranges;
/// without ranges, by its fixed versions; an advisory with no fix above `current` affects everything.
pub fn still_affected(eco: Ecosystem, current: &str, a: &Advisory, candidate: &str) -> bool {
    if let Some(yes) = a.affects(eco, candidate) {
        return yes;
    }
    let scheme = eco.scheme();
    match (
        a.fixed_after(scheme, current)
            .and_then(|f| Version::parse(scheme, &f)),
        Version::parse(scheme, candidate),
    ) {
        (Some(fixed), Some(c)) => c < fixed,
        _ => true,
    }
}

/// Whether `a` names a fix above `current`.
pub fn fixable(eco: Ecosystem, current: &str, a: &Advisory) -> bool {
    a.fixed_after(eco.scheme(), current).is_some()
}

/// The ids of the advisories that no longer affect `target`.
pub fn fixes(eco: Ecosystem, current: &str, target: &str, advisories: &[&Advisory]) -> Vec<String> {
    let mut ids: Vec<String> = advisories
        .iter()
        .filter(|a| !still_affected(eco, current, a, target))
        .map(|a| a.id.clone())
        .collect();
    ids.sort();
    ids
}

/// The ids of the advisories that still affect `target`.
pub fn unfixed(
    eco: Ecosystem,
    current: &str,
    target: &str,
    advisories: &[&Advisory],
) -> Vec<String> {
    let mut ids: Vec<String> = advisories
        .iter()
        .filter(|a| still_affected(eco, current, a, target))
        .map(|a| a.id.clone())
        .collect();
    ids.sort();
    ids
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Move to this version.
    Target(String),
    /// There is nothing to move to.
    Nothing,
    /// There is something, but not within the limits; the reason says what.
    Blocked(String),
}

/// The versions a security update could move to, smallest first: newer than `current`, within `max_kind`,
/// and not affected by any advisory that has a fix. `Err` says why there are none.
pub fn security_candidates(
    eco: Ecosystem,
    current: &str,
    versions: &[String],
    advisories: &[&Advisory],
    max_kind: UpdateKind,
) -> Result<Vec<String>, String> {
    let scheme = eco.scheme();
    let Some(cur) = Version::parse(scheme, current) else {
        return Err(format!("the current version {current:?} cannot be ordered"));
    };
    let mut newer: Vec<(Version, &String)> = versions
        .iter()
        .filter_map(|s| Version::parse(scheme, s).map(|v| (v, s)))
        .filter(|(v, _)| *v > cur)
        .collect();
    newer.sort_by(|a, b| a.0.cmp(&b.0));
    let wanting: Vec<&Advisory> = advisories
        .iter()
        .copied()
        .filter(|a| fixable(eco, current, a))
        .collect();
    let clean: Vec<&(Version, &String)> = newer
        .iter()
        .filter(|(_, s)| wanting.iter().all(|a| !still_affected(eco, current, a, s)))
        .collect();
    let within: Vec<String> = clean
        .iter()
        .filter(|(v, _)| UpdateKind::between(&cur, v).is_some_and(|k| k <= max_kind))
        .map(|(_, s)| (*s).clone())
        .collect();
    if !within.is_empty() {
        return Ok(within);
    }
    if let Some((v, s)) = clean.first() {
        let kind = UpdateKind::between(&cur, v).unwrap_or(UpdateKind::Major);
        return Err(format!(
            "the smallest version free of the advisories ({s}) is a {kind} update; the limit is {max_kind}"
        ));
    }
    // Every listed version is still affected: say by what, at the newest.
    let blame = newer
        .last()
        .map(|(_, s)| unfixed(eco, current, s, &wanting))
        .unwrap_or_default();
    if newer.is_empty() {
        return Err("no newer version is listed".into());
    }
    Err(format!(
        "no listed version is free of {}",
        if blame.is_empty() {
            "the advisories".to_owned()
        } else {
            blame.join(", ")
        }
    ))
}

/// The version to move `current` to. `versions` is the registry's stable versions.
pub fn choose(
    eco: Ecosystem,
    current: &str,
    versions: &[String],
    advisories: &[&Advisory],
    security_only: bool,
    max_kind: UpdateKind,
) -> Pick {
    let scheme = eco.scheme();
    let Some(cur) = Version::parse(scheme, current) else {
        return Pick::Blocked(format!("the current version {current:?} cannot be ordered"));
    };
    let mut newer: Vec<(Version, &String)> = versions
        .iter()
        .filter_map(|s| Version::parse(scheme, s).map(|v| (v, s)))
        .filter(|(v, _)| *v > cur)
        .collect();
    newer.sort_by(|a, b| a.0.cmp(&b.0));
    let Some((newest, newest_s)) = newer.last().map(|(v, s)| (v.clone(), (*s).clone())) else {
        return Pick::Nothing;
    };
    if security_only {
        if !advisories.iter().any(|a| fixable(eco, current, a)) {
            return Pick::Nothing;
        }
        return match security_candidates(eco, current, versions, advisories, max_kind) {
            Ok(list) => Pick::Target(list[0].clone()),
            Err(why) => Pick::Blocked(why),
        };
    }
    match newer
        .iter()
        .rev()
        .find(|(v, _)| UpdateKind::between(&cur, v).is_some_and(|k| k <= max_kind))
    {
        Some((_, s)) => Pick::Target((*s).clone()),
        None => {
            let kind = UpdateKind::between(&cur, &newest).unwrap_or(UpdateKind::Major);
            Pick::Blocked(format!(
                "the newest version ({newest_s}) is a {kind} update; the limit is {max_kind}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::advisory::Record;
    use serde_json::json;

    const NPM: Ecosystem = Ecosystem::Npm;

    /// An advisory from an OSV record that affects `ranges` of the package `p`.
    fn adv(id: &str, sev: Option<&str>, ranges: &[&[(&str, &str)]]) -> Advisory {
        let ranges: Vec<_> = ranges
            .iter()
            .map(|events| {
                let events: Vec<serde_json::Value> = events
                    .iter()
                    .map(|(k, v)| {
                        let mut m = serde_json::Map::new();
                        m.insert((*k).to_owned(), json!(*v));
                        serde_json::Value::Object(m)
                    })
                    .collect();
                json!({"type": "SEMVER", "events": events})
            })
            .collect();
        let mut rec = json!({
            "id": id,
            "affected": [{"package": {"name": "p", "ecosystem": "npm"}, "ranges": ranges}]
        });
        if let Some(s) = sev {
            rec["database_specific"] = json!({"severity": s});
        }
        serde_json::from_value::<Record>(rec)
            .unwrap()
            .advisory(NPM, "p")
    }

    fn vs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_ordinary_update_goes_to_the_newest_version_within_the_limit() {
        let all = vs(&["1.0.0", "1.0.1", "1.1.0", "1.2.0", "2.0.0", "2.1.0"]);
        let pick = |cur, max| choose(NPM, cur, &all, &[], false, max);
        assert_eq!(
            pick("1.0.0", UpdateKind::Patch),
            Pick::Target("1.0.1".into())
        );
        assert_eq!(
            pick("1.0.0", UpdateKind::Minor),
            Pick::Target("1.2.0".into())
        );
        assert_eq!(
            pick("1.0.0", UpdateKind::Major),
            Pick::Target("2.1.0".into())
        );
        assert_eq!(pick("2.1.0", UpdateKind::Major), Pick::Nothing);
        match pick("1.2.0", UpdateKind::Minor) {
            Pick::Blocked(r) => assert!(
                r.contains("2.1.0") && r.contains("major") && r.contains("minor"),
                "{r}"
            ),
            other => panic!("{other:?}"),
        }
        match choose(NPM, "banana", &all, &[], false, UpdateKind::Major) {
            Pick::Blocked(r) => assert!(r.contains("cannot be ordered")),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            choose(NPM, "1.0.0", &[], &[], false, UpdateKind::Major),
            Pick::Nothing
        );
    }

    #[test]
    fn a_security_update_goes_to_the_smallest_version_no_fixable_advisory_still_affects() {
        let a = adv(
            "A",
            Some("HIGH"),
            &[&[("introduced", "0"), ("fixed", "1.0.5")]],
        );
        let all = vs(&["1.0.0", "1.0.5", "1.1.0", "1.1.4", "2.0.0"]);
        let sec = |cur, max| choose(NPM, cur, &all, &[&a], true, max);
        assert_eq!(
            sec("1.0.0", UpdateKind::Patch),
            Pick::Target("1.0.5".into())
        );
        // The fix is 1.0.5, which is not listed in this registry: the next listed version above it.
        let sparse = vs(&["1.0.0", "1.1.0", "1.1.4"]);
        assert_eq!(
            choose(NPM, "1.0.0", &sparse, &[&a], true, UpdateKind::Minor),
            Pick::Target("1.1.0".into())
        );
        match choose(NPM, "1.0.0", &sparse, &[&a], true, UpdateKind::Patch) {
            Pick::Blocked(r) => assert!(r.contains("minor") && r.contains("patch"), "{r}"),
            other => panic!("{other:?}"),
        }
        // Nothing to fix, nothing newer.
        assert_eq!(
            choose(NPM, "1.0.0", &all, &[], true, UpdateKind::Major),
            Pick::Nothing
        );
        assert_eq!(
            choose(NPM, "2.0.0", &all, &[&a], true, UpdateKind::Major),
            Pick::Nothing
        );
    }

    #[test]
    fn a_fix_in_one_release_line_does_not_clear_a_backport_line_above_it() {
        // Affected: everything below 1.5.0, and 1.6.0 up to (not including) 1.8.0.
        let a = adv(
            "GHSA-line",
            Some("CRITICAL"),
            &[
                &[("introduced", "0"), ("fixed", "1.5.0")],
                &[("introduced", "1.6.0"), ("fixed", "1.8.0")],
            ],
        );
        assert_eq!(
            a.fixed_after(NPM.scheme(), "1.4.0").as_deref(),
            Some("1.5.0")
        );
        // The smallest fixed version above 1.4.0 is 1.5.0, which is not listed; 1.6.0 is the next
        // version and is still affected. The target is 1.8.0.
        let all = vs(&["1.4.0", "1.6.0", "1.7.0", "1.8.0"]);
        assert_eq!(
            choose(NPM, "1.4.0", &all, &[&a], true, UpdateKind::Minor),
            Pick::Target("1.8.0".into())
        );
        assert_eq!(
            security_candidates(NPM, "1.4.0", &all, &[&a], UpdateKind::Minor).unwrap(),
            vs(&["1.8.0"])
        );
        // Within a patch limit there is no such version, and the reason says so.
        match choose(NPM, "1.4.0", &all, &[&a], true, UpdateKind::Patch) {
            Pick::Blocked(r) => assert!(r.contains("1.8.0") && r.contains("minor"), "{r}"),
            other => panic!("{other:?}"),
        }
        // When no listed version is free, the advisory is named.
        let none = vs(&["1.4.0", "1.6.0", "1.7.0"]);
        match choose(NPM, "1.4.0", &none, &[&a], true, UpdateKind::Major) {
            Pick::Blocked(r) => assert!(r.contains("GHSA-line"), "{r}"),
            other => panic!("{other:?}"),
        }
        // Moving from the second line goes to its own fix.
        let line2 = vs(&["1.6.0", "1.7.0", "1.8.0", "2.0.0"]);
        assert_eq!(
            choose(NPM, "1.6.0", &line2, &[&a], true, UpdateKind::Minor),
            Pick::Target("1.8.0".into())
        );
        // An ordinary update to the newest version in the limit is reported as fixing it or not.
        let mid = vs(&["1.4.0", "1.7.0"]);
        let Pick::Target(t) = choose(NPM, "1.4.0", &mid, &[&a], false, UpdateKind::Minor) else {
            panic!("a target")
        };
        assert_eq!(t, "1.7.0");
        assert!(
            fixes(NPM, "1.4.0", &t, &[&a]).is_empty(),
            "1.7.0 is in the second range"
        );
        assert_eq!(unfixed(NPM, "1.4.0", &t, &[&a]), ["GHSA-line"]);
    }

    #[test]
    fn two_advisories_both_have_to_be_cleared() {
        let a = adv("A", None, &[&[("introduced", "0"), ("fixed", "1.0.5")]]);
        let b = adv(
            "B",
            Some("LOW"),
            &[&[("introduced", "1.0.0"), ("fixed", "1.0.9")]],
        );
        let all = vs(&["1.0.0", "1.0.5", "1.0.9", "1.1.0"]);
        assert_eq!(
            choose(NPM, "1.0.0", &all, &[&a, &b], true, UpdateKind::Patch),
            Pick::Target("1.0.9".into()),
            "1.0.5 fixes A but is inside B"
        );
        assert_eq!(fixes(NPM, "1.0.0", "1.0.9", &[&a, &b]), ["A", "B"]);
        assert_eq!(fixes(NPM, "1.0.0", "1.0.5", &[&a, &b]), ["A"]);
        assert_eq!(unfixed(NPM, "1.0.0", "1.0.5", &[&a, &b]), ["B"]);
        let without = vs(&["1.0.0", "1.0.5", "1.1.0"]);
        assert_eq!(
            choose(NPM, "1.0.0", &without, &[&a, &b], true, UpdateKind::Minor),
            Pick::Target("1.1.0".into())
        );
        match choose(
            NPM,
            "1.0.0",
            &vs(&["1.0.0", "1.0.5"]),
            &[&a, &b],
            true,
            UpdateKind::Major,
        ) {
            Pick::Blocked(r) => assert!(r.contains('B'), "{r}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_advisory_with_no_fix_is_never_fixed_and_does_not_hold_up_the_others() {
        let open = adv("OPEN", Some("HIGH"), &[&[("introduced", "0")]]);
        let a = adv(
            "A",
            Some("HIGH"),
            &[&[("introduced", "0"), ("fixed", "1.0.5")]],
        );
        assert!(!fixable(NPM, "1.0.0", &open));
        assert!(fixable(NPM, "1.0.0", &a));
        let all = vs(&["1.0.0", "1.0.5", "1.1.0"]);
        assert_eq!(
            choose(NPM, "1.0.0", &all, &[&open, &a], true, UpdateKind::Patch),
            Pick::Target("1.0.5".into())
        );
        assert_eq!(fixes(NPM, "1.0.0", "1.0.5", &[&open, &a]), ["A"]);
        assert_eq!(unfixed(NPM, "1.0.0", "1.0.5", &[&open, &a]), ["OPEN"]);
        assert_eq!(
            choose(NPM, "1.0.0", &all, &[&open], true, UpdateKind::Major),
            Pick::Nothing,
            "nothing fixable"
        );
    }

    #[test]
    fn without_ranges_the_fixed_versions_decide() {
        let mut a = adv("A", None, &[&[("introduced", "0"), ("fixed", "1.0.5")]]);
        a.affected.clear();
        assert_eq!(a.affects(NPM, "1.0.0"), None);
        assert!(still_affected(NPM, "1.0.0", &a, "1.0.4"));
        assert!(!still_affected(NPM, "1.0.0", &a, "1.0.5"));
        let mut open = adv("O", None, &[&[("introduced", "0")]]);
        open.affected.clear();
        assert!(
            still_affected(NPM, "1.0.0", &open, "9.9.9"),
            "no fix: affected for good"
        );
        assert!(
            still_affected(NPM, "1.0.0", &a, "garbage"),
            "an unreadable version is not clean"
        );
    }

    #[test]
    fn an_advisory_with_no_severity_counts_as_moderate() {
        assert_eq!(effective(&adv("A", None, &[])), DepSeverity::Moderate);
        assert_eq!(effective(&adv("A", Some("LOW"), &[])), DepSeverity::Low);
    }
}
