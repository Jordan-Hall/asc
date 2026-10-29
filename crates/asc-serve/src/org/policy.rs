//! The organisation's default merge requirements and how a repository's own policy sits on top.
//!
//! A repository inherits the defaults and can only tighten them. [`tighten`] is the one rule;
//! [`apply`] is the same rule on the requirements the land gate reads from a repository's policy.

use asc_core::policy::{LandRequirements, Strategy};
use serde::{Deserialize, Serialize};

/// Most required check names.
pub const MAX_CHECKS: usize = 32;
/// Longest check name, in bytes.
pub const MAX_CHECK_LEN: usize = 100;
/// Most approvals a default may demand.
pub const MAX_APPROVALS: u32 = 100;

/// The defaults of an organisation. An unset default (`set` false) requires nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeDefaults {
    pub require_approvals: u32,
    pub require_checks: Vec<String>,
    pub allow_force: bool,
    pub require_signed: bool,
    pub human_approval: bool,
    pub author_may_approve: bool,
    pub queue: bool,
    pub revalidate: bool,
    /// `""` leaves the strategy to the repository.
    pub strategy: String,
}

fn strategy_ok(s: &str) -> bool {
    matches!(s, "" | "rebase" | "rebase-disjoint" | "fast-forward")
}

impl MergeDefaults {
    /// Bounds and spelling; the message names the field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.require_approvals > MAX_APPROVALS {
            return Err("require_approvals is at most 100");
        }
        if self.require_checks.len() > MAX_CHECKS {
            return Err("require_checks has at most 32 names");
        }
        for c in &self.require_checks {
            if c.is_empty() || c.len() > MAX_CHECK_LEN || c.chars().any(char::is_control) {
                return Err("a check name is 1 to 100 bytes without control characters");
            }
        }
        if !strategy_ok(&self.strategy) {
            return Err("strategy is rebase, rebase-disjoint or fast-forward");
        }
        Ok(())
    }

    /// The checks, sorted and without duplicates.
    pub fn normalised(mut self) -> Self {
        self.require_checks.sort();
        self.require_checks.dedup();
        self
    }
}

/// The effective values of a repository whose own values are `repo` under the organisation's
/// `org`, and the fields where the organisation's value overrode the repository's.
pub fn tighten(org: &MergeDefaults, repo: &MergeDefaults) -> (MergeDefaults, Vec<&'static str>) {
    let mut e = repo.clone();
    let mut tightened = Vec::new();
    if org.require_approvals > e.require_approvals {
        e.require_approvals = org.require_approvals;
        tightened.push("require_approvals");
    }
    let before = e.require_checks.len();
    for c in &org.require_checks {
        if !e.require_checks.contains(c) {
            e.require_checks.push(c.clone());
        }
    }
    e.require_checks.sort();
    e.require_checks.dedup();
    if e.require_checks.len() != before {
        tightened.push("require_checks");
    }
    let mut and = |name: &'static str, org_v: bool, v: &mut bool| {
        if *v && !org_v {
            *v = false;
            tightened.push(name);
        }
    };
    and("allow_force", org.allow_force, &mut e.allow_force);
    and(
        "author_may_approve",
        org.author_may_approve,
        &mut e.author_may_approve,
    );
    let mut or = |name: &'static str, org_v: bool, v: &mut bool| {
        if org_v && !*v {
            *v = true;
            tightened.push(name);
        }
    };
    or("require_signed", org.require_signed, &mut e.require_signed);
    or("human_approval", org.human_approval, &mut e.human_approval);
    or("queue", org.queue, &mut e.queue);
    or("revalidate", org.revalidate, &mut e.revalidate);
    if !org.strategy.is_empty() && org.strategy != e.strategy {
        e.strategy.clone_from(&org.strategy);
        tightened.push("strategy");
    }
    (e, tightened)
}

fn strategy_of(s: &str) -> Option<Strategy> {
    match s {
        "rebase" => Some(Strategy::Rebase),
        "rebase-disjoint" => Some(Strategy::RebaseDisjoint),
        "fast-forward" => Some(Strategy::FastForward),
        _ => None,
    }
}

/// The land requirements of a repository under the organisation's defaults: more approvals,
/// more checks, a signature if either asks, and the organisation's strategy when it sets one.
/// `defaults` is `None` when the organisation has set none: the requirements stand.
pub fn apply(defaults: Option<&MergeDefaults>, own: LandRequirements) -> LandRequirements {
    let Some(d) = defaults else {
        return own;
    };
    let mut checks = own.checks;
    for c in &d.require_checks {
        if !checks.contains(c) {
            checks.push(c.clone());
        }
    }
    LandRequirements {
        approvals: own.approvals.max(d.require_approvals),
        checks,
        signed: own.signed || d.require_signed,
        strategy: strategy_of(&d.strategy).unwrap_or(own.strategy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d() -> MergeDefaults {
        MergeDefaults::default()
    }

    #[test]
    fn a_repository_can_only_tighten_what_the_organisation_requires() {
        let org = MergeDefaults {
            require_approvals: 2,
            require_checks: vec!["build".into(), "lint".into()],
            require_signed: true,
            human_approval: true,
            strategy: "rebase-disjoint".into(),
            ..d()
        };
        // A repository that asks for less gets the organisation's floor.
        let loose = MergeDefaults {
            require_approvals: 1,
            require_checks: vec!["test".into()],
            allow_force: true,
            author_may_approve: true,
            strategy: "rebase".into(),
            ..d()
        };
        let (eff, why) = tighten(&org, &loose);
        assert_eq!(eff.require_approvals, 2);
        assert_eq!(eff.require_checks, ["build", "lint", "test"]);
        assert!(
            eff.require_signed && eff.human_approval && !eff.allow_force && !eff.author_may_approve
        );
        assert_eq!(eff.strategy, "rebase-disjoint");
        for f in [
            "require_approvals",
            "require_checks",
            "allow_force",
            "author_may_approve",
            "require_signed",
            "human_approval",
            "strategy",
        ] {
            assert!(why.contains(&f), "{f}: {why:?}");
        }
        // A repository that asks for more keeps it.
        let strict = MergeDefaults {
            require_approvals: 3,
            require_signed: true,
            human_approval: true,
            queue: true,
            strategy: "rebase-disjoint".into(),
            require_checks: vec!["build".into(), "lint".into()],
            ..d()
        };
        let (eff, why) = tighten(&org, &strict);
        assert_eq!((eff.require_approvals, eff.queue), (3, true));
        assert!(why.is_empty(), "{why:?}");
    }

    #[test]
    fn tightening_never_loosens() {
        // Property over a grid of values: the effective policy is at least as strict as both sides.
        for oa in 0..3 {
            for ra in 0..3 {
                for (of, rf) in [(false, false), (false, true), (true, false), (true, true)] {
                    let org = MergeDefaults {
                        require_approvals: oa,
                        allow_force: of,
                        ..d()
                    };
                    let repo = MergeDefaults {
                        require_approvals: ra,
                        allow_force: rf,
                        ..d()
                    };
                    let (e, _) = tighten(&org, &repo);
                    assert!(e.require_approvals >= oa.max(ra));
                    assert!(!e.allow_force || (of && rf));
                }
            }
        }
    }

    #[test]
    fn land_requirements_take_the_stricter_of_both() {
        let own = LandRequirements {
            approvals: 1,
            checks: vec!["a".into()],
            signed: false,
            strategy: Strategy::Rebase,
        };
        assert_eq!(apply(None, own.clone()), own);
        let org = MergeDefaults {
            require_approvals: 2,
            require_checks: vec!["b".into()],
            require_signed: true,
            strategy: "fast-forward".into(),
            ..d()
        };
        let got = apply(Some(&org), own);
        assert_eq!(
            (got.approvals, got.signed, got.strategy),
            (2, true, Strategy::FastForward)
        );
        assert_eq!(got.checks, ["a", "b"]);
    }

    #[test]
    fn defaults_are_bounded() {
        assert!(d().validate().is_ok());
        for bad in [
            MergeDefaults {
                require_approvals: 101,
                ..d()
            },
            MergeDefaults {
                require_checks: vec![String::new()],
                ..d()
            },
            MergeDefaults {
                require_checks: vec!["a\nb".into()],
                ..d()
            },
            MergeDefaults {
                require_checks: vec!["x".into(); 33],
                ..d()
            },
            MergeDefaults {
                strategy: "squash".into(),
                ..d()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
