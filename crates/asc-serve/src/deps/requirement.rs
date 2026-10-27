//! Changing a version requirement as little as possible.
//!
//! A manifest says `^1.2.3` or `~> 1.2` or `==1.4.0`. To move a package to `2.0.1` the requirement has to
//! admit it. [`bump`] answers in three ways: the requirement already admits the version (nothing to
//! write), it is a plain one that can be rewritten (the same operator, the same spacing, the same number of
//! components), or it is something this code will not guess at (`>=1, <2`, `1.x`, `||`, a pre-release).

use super::version::{Scheme, Version};

/// What a bare version means in the ecosystem's manifests, and which operators exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// `1.2.3` is `^1.2.3` (Cargo).
    Caret,
    /// `1.2.3` is exactly that version (npm, Composer).
    Exact,
    /// Gem and Hex: `~> 1.2`, `>= 1.0`, `== 1.0`, and a bare version is exact.
    Gem,
    /// PEP 440: `==`, `===`, `~=`, `>=`; a requirement has an operator.
    Pep440,
}

impl Style {
    fn scheme(self) -> Scheme {
        match self {
            Self::Caret | Self::Exact => Scheme::Semver,
            Self::Gem => Scheme::Gem,
            Self::Pep440 => Scheme::Pep440,
        }
    }

    /// The operators of the style, longest first.
    fn operators(self) -> &'static [&'static str] {
        match self {
            Self::Caret | Self::Exact => &["^", "~", ">=", "="],
            Self::Gem => &["~>", ">=", "==", "="],
            Self::Pep440 => &["===", "==", "~=", ">="],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bump {
    /// The requirement admits the version already.
    Unchanged,
    /// The requirement to write instead.
    Changed(String),
    /// Not a plain requirement.
    Complex,
}

/// A requirement taken apart: the operator, the space after it and the release numbers.
struct Plain {
    op: &'static str,
    gap: String,
    base: Vec<u64>,
}

fn split(style: Style, req: &str) -> Option<Plain> {
    let r = req.trim();
    let (op, rest) = style
        .operators()
        .iter()
        .find_map(|o| r.strip_prefix(o).map(|rest| (*o, rest)))
        .unwrap_or(("", r));
    if op.is_empty() && style == Style::Pep440 {
        return None;
    }
    let number = rest.trim_start();
    let gap = rest[..rest.len() - number.len()].to_owned();
    let valid = !number.is_empty()
        && number.split('.').count() <= 6
        && number
            .split('.')
            .all(|p| !p.is_empty() && p.len() <= 18 && p.bytes().all(|b| b.is_ascii_digit()));
    if !valid {
        return None;
    }
    let base: Option<Vec<u64>> = number.split('.').map(|p| p.parse().ok()).collect();
    Some(Plain {
        op,
        gap,
        base: base?,
    })
}

fn exact_op(style: Style, op: &str) -> bool {
    matches!(op, "=" | "==" | "===")
        || (op.is_empty() && matches!(style, Style::Exact | Style::Gem))
}

/// The smallest version above everything the requirement admits, as release numbers, or `None` when it
/// admits all above its floor.
fn upper(style: Style, op: &str, base: &[u64]) -> Option<Vec<u64>> {
    let p = base.len();
    let bump_at = |i: usize| -> Vec<u64> {
        let mut v: Vec<u64> = base[..=i].to_vec();
        v[i] += 1;
        v
    };
    match op {
        ">=" => None,
        "^" => Some(bump_at(base.iter().position(|n| *n != 0).unwrap_or(p - 1))),
        "" if style == Style::Caret => {
            Some(bump_at(base.iter().position(|n| *n != 0).unwrap_or(p - 1)))
        }
        "~" => Some(bump_at(usize::from(p >= 2))),
        "~>" | "~=" => Some(bump_at(p.saturating_sub(2))),
        _ => None,
    }
}

fn text_of(parts: &[u64]) -> String {
    parts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// How `old` has to change to admit `to`.
pub fn bump(style: Style, old: &str, to: &str) -> Bump {
    let scheme = style.scheme();
    let Some(plain) = split(style, old) else {
        return Bump::Complex;
    };
    let Some(to_ver) = Version::parse(scheme, to) else {
        return Bump::Complex;
    };
    let to_parts: Vec<u64> = to
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map_while(|p| p.parse().ok())
        .collect();
    if to_ver.is_prerelease() || to_parts.is_empty() {
        return Bump::Complex;
    }
    let Some(floor) = Version::parse(scheme, &text_of(&plain.base)) else {
        return Bump::Complex;
    };
    let exact = exact_op(style, plain.op);
    let admitted = if exact {
        to_ver == floor
    } else {
        to_ver >= floor
            && upper(style, plain.op, &plain.base)
                .and_then(|u| Version::parse(scheme, &text_of(&u)))
                .is_none_or(|u| to_ver < u)
    };
    if admitted {
        return Bump::Unchanged;
    }
    // The same number of components as before; an exact requirement names the whole version.
    let want = plain.base.len();
    let mut parts = to_parts;
    if !exact {
        parts.truncate(want);
    }
    if parts.len() < want {
        parts.resize(want, 0);
    }
    Bump::Changed(format!("{}{}{}", plain.op, plain.gap, text_of(&parts)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(s: &str) -> Bump {
        Bump::Changed(s.to_owned())
    }

    #[test]
    fn caret_and_bare_cargo_requirements_admit_compatible_releases() {
        for old in ["1.2.3", "^1.2.3", "1.2", "1"] {
            assert_eq!(bump(Style::Caret, old, "1.9.0"), Bump::Unchanged, "{old}");
        }
        assert_eq!(bump(Style::Caret, "1.2.3", "2.0.0"), changed("2.0.0"));
        assert_eq!(bump(Style::Caret, "^1.2.3", "2.1.4"), changed("^2.1.4"));
        assert_eq!(bump(Style::Caret, "1.2", "2.1.4"), changed("2.1"));
        assert_eq!(bump(Style::Caret, "1", "3.4.5"), changed("3"));
        // 0.x: the minor number is the breaking one.
        assert_eq!(bump(Style::Caret, "0.3.1", "0.3.9"), Bump::Unchanged);
        assert_eq!(bump(Style::Caret, "0.3.1", "0.4.0"), changed("0.4.0"));
        assert_eq!(bump(Style::Caret, "0.3", "0.4.2"), changed("0.4"));
        assert_eq!(bump(Style::Caret, "0.0.3", "0.0.4"), changed("0.0.4"));
        assert_eq!(bump(Style::Caret, "0.0", "0.0.9"), Bump::Unchanged);
    }

    #[test]
    fn tilde_greater_equal_and_exact() {
        assert_eq!(bump(Style::Caret, "~1.2.3", "1.2.9"), Bump::Unchanged);
        assert_eq!(bump(Style::Caret, "~1.2.3", "1.3.0"), changed("~1.3.0"));
        assert_eq!(bump(Style::Caret, "~1", "1.9.0"), Bump::Unchanged);
        assert_eq!(bump(Style::Caret, "~1", "2.0.0"), changed("~2"));
        assert_eq!(bump(Style::Caret, ">=1.0.0", "7.0.0"), Bump::Unchanged);
        assert_eq!(bump(Style::Caret, "=1.2.3", "1.2.4"), changed("=1.2.4"));
        // npm and Composer: a bare version is exact.
        assert_eq!(bump(Style::Exact, "10.2.0", "10.2.0"), Bump::Unchanged);
        assert_eq!(bump(Style::Exact, "10.2.0", "10.3.1"), changed("10.3.1"));
        assert_eq!(bump(Style::Exact, "10", "11.2.3"), changed("11.2.3"));
        assert_eq!(bump(Style::Exact, "^10.2.0", "10.3.1"), Bump::Unchanged);
        assert_eq!(bump(Style::Exact, "~10.2", "10.3.1"), changed("~10.3"));
    }

    #[test]
    fn gem_and_hex_keep_the_space_after_the_operator() {
        let g = |old, to| bump(Style::Gem, old, to);
        assert_eq!(g("~> 1.2", "1.9"), Bump::Unchanged);
        assert_eq!(g("~> 1.2", "2.0.1"), changed("~> 2.0"));
        assert_eq!(g("~> 1.2.3", "1.2.9"), Bump::Unchanged);
        assert_eq!(g("~> 1.2.3", "1.3.0"), changed("~> 1.3.0"));
        assert_eq!(g("~>1.2", "2.0"), changed("~>2.0"));
        assert_eq!(g(">= 1.0", "9.9"), Bump::Unchanged);
        assert_eq!(g("1.2.3", "1.2.4"), changed("1.2.4"));
        assert_eq!(g("== 1.2.3", "1.2.4"), changed("== 1.2.4"));
        assert_eq!(g("~> 1", "2.0"), changed("~> 2"));
    }

    #[test]
    fn pep440_operators_need_one() {
        let p = |old, to| bump(Style::Pep440, old, to);
        assert_eq!(p("==3.2.0", "3.2.0"), Bump::Unchanged);
        assert_eq!(p("==3.2.0", "3.2.5"), changed("==3.2.5"));
        assert_eq!(p("===3.2.0", "4.0"), changed("===4.0.0"));
        assert_eq!(p("~=1.4.2", "1.4.9"), Bump::Unchanged);
        assert_eq!(p("~=1.4.2", "1.5.0"), changed("~=1.5.0"));
        assert_eq!(p("~=1.4", "1.9"), Bump::Unchanged);
        assert_eq!(p("~=1.4", "2.0"), changed("~=2.0"));
        assert_eq!(p(">=2.0", "3.0"), Bump::Unchanged);
        assert_eq!(
            p("3.2.0", "3.2.5"),
            Bump::Complex,
            "a requirement without an operator"
        );
    }

    #[test]
    fn what_is_not_plain_is_left_alone() {
        for old in [
            ">=1.0, <2",
            "1.x",
            "1.2.*",
            "^1.0 || ^2.0",
            "*",
            "",
            "latest",
            "^1.0.0-beta.1",
            "v1.2.3",
            ">1.0",
            "<2.0",
            "1..2",
        ] {
            assert_eq!(bump(Style::Caret, old, "5.0.0"), Bump::Complex, "{old:?}");
        }
        assert_eq!(
            bump(Style::Caret, "1.0.0", "2.0.0-rc.1"),
            Bump::Complex,
            "a pre-release target"
        );
        assert_eq!(bump(Style::Caret, "1.0.0", "main"), Bump::Complex);
        assert_eq!(
            bump(Style::Caret, &format!("1.{}", "9".repeat(30)), "2.0.0"),
            Bump::Complex
        );
    }
}
