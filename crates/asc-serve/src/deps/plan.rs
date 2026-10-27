//! Which version a package moves to.
//!
//! Pure decisions over a package's current version, the versions its registry lists and the advisories
//! that affect it. Nothing here asks the network.
//!
//! * An ordinary update goes to the newest stable version whose step from the current one is no bigger
//!   than the limit (`max_kind`); when only bigger ones exist the update is *blocked*, not forgotten, and
//!   says what it would take.
//! * A security update goes to the *smallest* version that fixes every advisory that has a fix, never
//!   beyond what the fix needs; if that step is bigger than the limit it is blocked.
//! * An advisory that states no severity counts as moderate (an answer that cannot be shown to be minor
//!   is not minor), against the repository's severity threshold and the policy's `block_severity`.

use asc_core::policy::DepSeverity;

use super::model::Advisory;
use super::version::{Scheme, UpdateKind, Version};

/// The severity an advisory counts as.
pub fn effective(a: &Advisory) -> DepSeverity {
    a.severity.unwrap_or(DepSeverity::Moderate)
}

/// What the fixes of a set of advisories ask of a package at `current`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Required {
    /// The smallest version that has every fix that exists.
    pub version: Option<String>,
    /// Advisories that name no fixed version above `current`.
    pub unfixable: Vec<String>,
}

pub fn required(scheme: Scheme, current: &str, advisories: &[&Advisory]) -> Required {
    let mut out = Required::default();
    let mut best: Option<(Version, String)> = None;
    for a in advisories {
        match a.fixed_after(scheme, current) {
            Some(f) => {
                if let Some(v) = Version::parse(scheme, &f)
                    && best.as_ref().is_none_or(|(b, _)| v > *b)
                {
                    best = Some((v, f));
                }
            }
            None => out.unfixable.push(a.id.clone()),
        }
    }
    out.version = best.map(|(_, s)| s);
    out
}

/// The ids of the advisories that `target` fixes: those with a fixed version above `current` that
/// `target` has reached.
pub fn fixes(scheme: Scheme, current: &str, target: &str, advisories: &[&Advisory]) -> Vec<String> {
    let Some(t) = Version::parse(scheme, target) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = advisories
        .iter()
        .filter(|a| {
            a.fixed_after(scheme, current)
                .and_then(|f| Version::parse(scheme, &f))
                .is_some_and(|f| t >= f)
        })
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

/// The version to move `current` to.
///
/// `versions` is the registry's stable versions, oldest first. `need` is the smallest version that fixes
/// what has a fix, when the update is a security one.
pub fn choose(
    scheme: Scheme,
    current: &str,
    versions: &[String],
    need: Option<&str>,
    security_only: bool,
    max_kind: UpdateKind,
) -> Pick {
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
        let Some(need) = need else {
            return Pick::Nothing;
        };
        let Some(need_v) = Version::parse(scheme, need) else {
            return Pick::Blocked(format!("the fixed version {need:?} cannot be ordered"));
        };
        let Some((v, s)) = newer.iter().find(|(v, _)| *v >= need_v) else {
            return Pick::Blocked(format!(
                "the fix needs {need}, which the registry does not list"
            ));
        };
        return match UpdateKind::between(&cur, v) {
            Some(k) if k > max_kind => Pick::Blocked(format!(
                "the fix ({s}) is a {k} update; the limit is {max_kind}"
            )),
            _ => Pick::Target((*s).clone()),
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

    fn adv(id: &str, sev: Option<DepSeverity>, fixed: &[&str]) -> Advisory {
        Advisory {
            id: id.into(),
            aliases: vec![],
            summary: String::new(),
            severity: sev,
            cvss: 0.0,
            fixed: fixed.iter().map(|s| (*s).to_owned()).collect(),
            url: String::new(),
        }
    }

    fn vs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    const S: Scheme = Scheme::Semver;

    #[test]
    fn an_ordinary_update_goes_to_the_newest_version_within_the_limit() {
        let all = vs(&["1.0.0", "1.0.1", "1.1.0", "1.2.0", "2.0.0", "2.1.0"]);
        let pick = |cur, max| choose(S, cur, &all, None, false, max);
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
        // Only a major update exists: blocked, with the reason.
        match pick("1.2.0", UpdateKind::Minor) {
            Pick::Blocked(r) => assert!(
                r.contains("2.1.0") && r.contains("major") && r.contains("minor"),
                "{r}"
            ),
            other => panic!("{other:?}"),
        }
        match choose(S, "banana", &all, None, false, UpdateKind::Major) {
            Pick::Blocked(r) => assert!(r.contains("cannot be ordered")),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            choose(S, "1.0.0", &[], None, false, UpdateKind::Major),
            Pick::Nothing
        );
    }

    #[test]
    fn a_security_update_goes_to_the_smallest_version_that_fixes() {
        let all = vs(&["1.0.0", "1.0.5", "1.1.0", "1.1.4", "2.0.0"]);
        let sec = |cur, need, max| choose(S, cur, &all, need, true, max);
        assert_eq!(
            sec("1.0.0", Some("1.0.5"), UpdateKind::Patch),
            Pick::Target("1.0.5".into())
        );
        // The fix is 1.0.3, which is not listed; the next listed version above it is taken.
        assert_eq!(
            sec("1.0.0", Some("1.0.3"), UpdateKind::Patch),
            Pick::Target("1.0.5".into())
        );
        assert_eq!(
            sec("1.0.0", Some("1.1.2"), UpdateKind::Minor),
            Pick::Target("1.1.4".into())
        );
        match sec("1.0.0", Some("1.1.2"), UpdateKind::Patch) {
            Pick::Blocked(r) => assert!(r.contains("minor") && r.contains("patch"), "{r}"),
            other => panic!("{other:?}"),
        }
        match sec("1.0.0", Some("9.0.0"), UpdateKind::Major) {
            Pick::Blocked(r) => assert!(r.contains("does not list"), "{r}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            sec("1.0.0", None, UpdateKind::Major),
            Pick::Nothing,
            "nothing to fix"
        );
        assert_eq!(
            sec("2.0.0", Some("2.0.0"), UpdateKind::Major),
            Pick::Nothing
        );
    }

    #[test]
    fn the_required_version_is_the_largest_of_the_fixes() {
        let a = adv("A", Some(DepSeverity::High), &["1.0.5", "2.0.1"]);
        let b = adv("B", None, &["1.1.0"]);
        let c = adv("C", Some(DepSeverity::Low), &[]);
        let d = adv("D", None, &["0.9.0"]);
        let r = required(S, "1.0.0", &[&a, &b, &c, &d]);
        assert_eq!(r.version.as_deref(), Some("1.1.0"));
        assert_eq!(r.unfixable, ["C", "D"], "no fix above the current version");
        assert_eq!(required(S, "1.0.0", &[]), Required::default());
        assert_eq!(fixes(S, "1.0.0", "1.1.0", &[&a, &b, &c, &d]), ["A", "B"]);
        assert_eq!(fixes(S, "1.0.0", "1.0.5", &[&a, &b]), ["A"]);
        assert_eq!(fixes(S, "1.0.0", "1.0.1", &[&a, &b]), Vec::<String>::new());
        assert_eq!(fixes(S, "1.0.0", "nonsense", &[&a]), Vec::<String>::new());
    }

    #[test]
    fn an_advisory_with_no_severity_counts_as_moderate() {
        assert_eq!(effective(&adv("A", None, &[])), DepSeverity::Moderate);
        assert_eq!(
            effective(&adv("A", Some(DepSeverity::Low), &[])),
            DepSeverity::Low
        );
    }
}
