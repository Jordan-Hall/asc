//! Does a requirement admit a version?
//!
//! The lockfile rewriters need to know whether a package that depends on `serde ^1.0` still accepts
//! `1.0.200`, or `2.0.0`. Each ecosystem writes its requirements in its own language; [`admits`] reads the
//! five that matter (npm, Cargo, PEP 440, RubyGems, Composer) into sets of bounds and compares with
//! [`Version`]. It answers `None` for text it does not understand (a tag, a URL, a branch, a marker it
//! cannot read), and callers treat that as "cannot be shown to be safe", never as a yes.

use super::model::Ecosystem;
use super::version::{Scheme, Version};

/// One bound.
#[derive(Clone, Debug)]
enum Bound {
    Any,
    Ge(Version),
    Gt(Version),
    Le(Version),
    Lt(Version),
    Eq(Version),
    Ne(Version),
    /// `===`: the text of the version, compared as written.
    Text(String),
}

/// A comparator: a bound, and whether its version text had a pre-release part.
#[derive(Clone, Debug)]
struct Comparator {
    bound: Bound,
    /// The `major.minor.patch` of a pre-release the comparator names, which lets versions with that
    /// triple and a pre-release through.
    pre: Option<Vec<u64>>,
}

type Set = Vec<Comparator>;

/// A partial version: `1`, `1.2`, `1.2.3`, `1.2.3-rc.1`, `1.x`, `*`.
#[derive(Clone, Debug)]
struct Partial {
    /// Release numbers present before the first wildcard.
    nums: Vec<u64>,
    pre: Option<String>,
}

fn parse_partial(s: &str) -> Option<Partial> {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V', '=']).unwrap_or(s).trim();
    if s.is_empty() || matches!(s, "*" | "x" | "X") {
        return Some(Partial {
            nums: Vec::new(),
            pre: None,
        });
    }
    let s = s.split('+').next().unwrap_or(s);
    let (rel, pre) = match s.split_once('-') {
        Some((r, p)) if !p.is_empty() => (r, Some(p.to_owned())),
        _ => (s, None),
    };
    let mut nums = Vec::new();
    for seg in rel.split('.') {
        if matches!(seg, "*" | "x" | "X") {
            if pre.is_some() {
                return None;
            }
            break;
        }
        if seg.is_empty() || seg.len() > 18 || !seg.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        nums.push(seg.parse().ok()?);
        if nums.len() > 6 {
            return None;
        }
    }
    Some(Partial { nums, pre })
}

fn text(nums: &[u64]) -> String {
    nums.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

fn filled(nums: &[u64], to: usize) -> Vec<u64> {
    let mut v = nums.to_vec();
    while v.len() < to {
        v.push(0);
    }
    v
}

fn cmp(
    scheme: Scheme,
    bound: impl FnOnce(Version) -> Bound,
    nums: &[u64],
    pre: Option<&str>,
) -> Option<Comparator> {
    let mut t = text(&filled(nums, 3));
    if let Some(p) = pre {
        t.push('-');
        t.push_str(p);
    }
    let v = Version::parse(scheme, &t)?;
    let pre_tuple = pre.map(|_| filled(nums, 3));
    Some(Comparator {
        bound: bound(v),
        pre: pre_tuple,
    })
}

fn any() -> Comparator {
    Comparator {
        bound: Bound::Any,
        pre: None,
    }
}

/// `nums` with its `at`-th number raised and the rest dropped (`[1, 2]` at 0 is `[2]`).
fn raised(nums: &[u64], at: usize) -> Vec<u64> {
    let mut v: Vec<u64> = filled(nums, at + 1)[..=at].to_vec();
    v[at] += 1;
    v
}

/// Reads one `op version` of the npm and Cargo languages into comparators.
fn desugar(scheme: Scheme, op: &str, p: &Partial, bare_is_caret: bool) -> Option<Vec<Comparator>> {
    let n = p.nums.len();
    let pre = p.pre.as_deref();
    let ge = |nums: &[u64], pre: Option<&str>| cmp(scheme, Bound::Ge, nums, pre);
    let lt = |nums: &[u64]| cmp(scheme, Bound::Lt, nums, None);
    let op = if op.is_empty() && bare_is_caret {
        "^"
    } else {
        op
    };
    if n == 0 {
        // `*`, `x`, empty: everything; with an operator that bounds, an unbounded side.
        return Some(vec![any()]);
    }
    Some(match op {
        "^" => {
            let at = p.nums.iter().position(|x| *x != 0).unwrap_or(n - 1);
            vec![ge(&p.nums, pre)?, lt(&raised(&p.nums, at))?]
        }
        "~" => {
            let at = usize::from(n >= 2);
            vec![ge(&p.nums, pre)?, lt(&raised(&p.nums, at))?]
        }
        "" | "=" => {
            if n >= 3 {
                vec![cmp(scheme, Bound::Eq, &p.nums, pre)?]
            } else {
                vec![ge(&p.nums, None)?, lt(&raised(&p.nums, n - 1))?]
            }
        }
        ">" => {
            if n >= 3 {
                vec![cmp(scheme, Bound::Gt, &p.nums, pre)?]
            } else {
                vec![ge(&raised(&p.nums, n - 1), None)?]
            }
        }
        ">=" => vec![ge(&p.nums, pre)?],
        "<" => vec![cmp(scheme, Bound::Lt, &p.nums, pre)?],
        "<=" => {
            if n >= 3 {
                vec![cmp(scheme, Bound::Le, &p.nums, pre)?]
            } else {
                vec![lt(&raised(&p.nums, n - 1))?]
            }
        }
        _ => return None,
    })
}

fn split_op<'a>(s: &'a str, ops: &[&'static str]) -> (&'static str, &'a str) {
    for o in ops {
        if let Some(rest) = s.strip_prefix(o) {
            return (o, rest.trim_start());
        }
    }
    ("", s)
}

const NPM_OPS: [&str; 7] = [">=", "<=", ">", "<", "=", "^", "~"];

/// `>= 1.2.3` is `>=1.2.3`: no space between an operator and its version.
fn normalise_ops(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if matches!(c, '>' | '<' | '=' | '^' | '~' | '!') {
            while chars.peek().is_some_and(|n| n.is_whitespace()) {
                chars.next();
            }
        }
    }
    out
}

/// npm and Cargo: `scheme` is Semver; npm's bare version is exact, Cargo's is a caret.
fn semver_sets(req: &str, cargo: bool) -> Option<Vec<Set>> {
    let scheme = Scheme::Semver;
    let mut sets = Vec::new();
    let alternatives: Vec<&str> = if cargo {
        vec![req]
    } else {
        req.split("||").collect()
    };
    for alt in alternatives {
        let alt = alt.trim();
        let mut set: Set = Vec::new();
        if alt.is_empty() {
            set.push(any());
            sets.push(set);
            continue;
        }
        // Hyphen ranges: `1.2.3 - 2.3.4`.
        if !cargo && let Some((lo, hi)) = alt.split_once(" - ") {
            let lo = parse_partial(lo)?;
            let hi = parse_partial(hi)?;
            set.extend(desugar(scheme, ">=", &lo, false)?);
            set.extend(desugar(scheme, "<=", &hi, false)?);
            sets.push(set);
            continue;
        }
        let flat = normalise_ops(alt);
        let parts: Vec<&str> = if cargo {
            flat.split(',').collect()
        } else {
            flat.split_whitespace().collect()
        };
        for part in parts {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (op, rest) = split_op(part, &NPM_OPS);
            // `~>` of old manifests is `~`.
            let rest = rest.strip_prefix('>').unwrap_or(rest);
            let p = parse_partial(rest)?;
            set.extend(desugar(scheme, op, &p, cargo)?);
        }
        if set.is_empty() {
            set.push(any());
        }
        sets.push(set);
    }
    Some(sets)
}

// ------------------------------------------------------------------------------ PEP 440

fn pep440_sets(req: &str) -> Option<Vec<Set>> {
    let scheme = Scheme::Pep440;
    let req = req.split(';').next().unwrap_or(req).trim();
    let req = req.trim_start_matches('(').trim_end_matches(')').trim();
    let mut set: Set = Vec::new();
    for spec in req.split(',') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        let (op, v) = split_op(spec, &["===", "==", "~=", "!=", "<=", ">=", "<", ">"]);
        if op.is_empty() {
            return None;
        }
        let v = v.trim();
        if op == "===" {
            set.push(Comparator {
                bound: Bound::Text(v.to_owned()),
                pre: None,
            });
            continue;
        }
        let (v, wild) = match v.strip_suffix(".*") {
            Some(r) => (r, true),
            None => (v, false),
        };
        let parsed = Version::parse(scheme, v)?;
        let pre = parsed.is_prerelease().then(|| triple(&parsed));
        let mk = |b: Bound, pre: Option<Vec<u64>>| Comparator { bound: b, pre };
        match (op, wild) {
            ("==", true) | ("!=", true) => {
                let nums = release_of(v)?;
                let lo = Version::parse(scheme, &text(&nums))?;
                let hi = Version::parse(scheme, &text(&raised(&nums, nums.len() - 1)))?;
                if op == "==" {
                    set.push(mk(Bound::Ge(lo), None));
                    set.push(mk(Bound::Lt(hi), None));
                } else {
                    // Not in [lo, hi): one comparator cannot say it; use a text-less pair marker.
                    return None;
                }
            }
            (_, true) => return None,
            ("==", false) => set.push(mk(Bound::Eq(parsed), pre)),
            ("!=", false) => set.push(mk(Bound::Ne(parsed), None)),
            ("<=", false) => set.push(mk(Bound::Le(parsed), pre)),
            (">=", false) => set.push(mk(Bound::Ge(parsed), pre)),
            ("<", false) => set.push(mk(Bound::Lt(parsed), pre)),
            (">", false) => set.push(mk(Bound::Gt(parsed), pre)),
            ("~=", false) => {
                let nums = release_of(v)?;
                if nums.len() < 2 {
                    return None;
                }
                let hi = Version::parse(scheme, &text(&raised(&nums, nums.len() - 2)))?;
                set.push(mk(Bound::Ge(parsed), pre));
                set.push(mk(Bound::Lt(hi), None));
            }
            _ => return None,
        }
    }
    if set.is_empty() {
        set.push(any());
    }
    Some(vec![set])
}

/// The leading release numbers of a version text.
fn release_of(v: &str) -> Option<Vec<u64>> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let mut out = Vec::new();
    for seg in v.split('.') {
        let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        out.push(digits.parse().ok()?);
        if digits.len() != seg.len() {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

fn triple(v: &Version) -> Vec<u64> {
    vec![v.major(), v.minor(), v.patch()]
}

// ------------------------------------------------------------------------------ RubyGems

fn gem_sets(req: &str) -> Option<Vec<Set>> {
    let scheme = Scheme::Gem;
    let mut set: Set = Vec::new();
    for spec in req.split(',') {
        let spec = spec.trim().trim_matches(['"', '\'']);
        if spec.is_empty() {
            continue;
        }
        let (op, v) = split_op(spec, &["~>", ">=", "<=", "!=", "=", ">", "<"]);
        let v = v.trim();
        let parsed = Version::parse(scheme, v)?;
        let pre = parsed.is_prerelease().then(|| triple(&parsed));
        let mk = |b: Bound, pre: Option<Vec<u64>>| Comparator { bound: b, pre };
        match op {
            "" | "=" => set.push(mk(Bound::Eq(parsed), pre)),
            "!=" => set.push(mk(Bound::Ne(parsed), None)),
            ">=" => set.push(mk(Bound::Ge(parsed), pre)),
            "<=" => set.push(mk(Bound::Le(parsed), pre)),
            ">" => set.push(mk(Bound::Gt(parsed), pre)),
            "<" => set.push(mk(Bound::Lt(parsed), pre)),
            "~>" => {
                let nums = release_of(v)?;
                let at = nums.len().saturating_sub(2);
                let hi = Version::parse(scheme, &text(&raised(&nums, at)))?;
                set.push(mk(Bound::Ge(parsed), pre));
                set.push(mk(Bound::Lt(hi), None));
            }
            _ => return None,
        }
    }
    if set.is_empty() {
        set.push(any());
    }
    Some(vec![set])
}

// ------------------------------------------------------------------------------ Composer

fn composer_sets(req: &str) -> Option<Vec<Set>> {
    let scheme = Scheme::Composer;
    let mut sets = Vec::new();
    // `|` and `||` separate alternatives.
    for alt in req.replace("||", "|").split('|') {
        let alt = alt.trim();
        let mut set: Set = Vec::new();
        if alt.is_empty() {
            set.push(any());
            sets.push(set);
            continue;
        }
        let alt = strip_flags(alt);
        if let Some((lo, hi)) = alt.split_once(" - ") {
            let lo = parse_partial(lo)?;
            let hi = parse_partial(hi)?;
            set.extend(composer_desugar(">=", &lo)?);
            set.extend(composer_desugar("<=", &hi)?);
            sets.push(set);
            continue;
        }
        let flat = normalise_ops(&alt.replace(',', " "));
        for part in flat.split_whitespace() {
            let (op, rest) = split_op(part, &["!=", ">=", "<=", "==", ">", "<", "=", "^", "~"]);
            if rest.starts_with("dev-")
                || rest.ends_with("-dev") && !rest.starts_with(|c: char| c.is_ascii_digit())
            {
                return None;
            }
            if op == "!=" {
                let p = parse_partial(rest)?;
                if p.nums.len() < 3 {
                    return None;
                }
                set.push(cmp(scheme, Bound::Ne, &p.nums, p.pre.as_deref())?);
                continue;
            }
            let p = parse_partial(rest)?;
            set.extend(composer_desugar(if op == "==" { "=" } else { op }, &p)?);
        }
        if set.is_empty() {
            set.push(any());
        }
        sets.push(set);
    }
    Some(sets)
}

/// Composer's `~1.2` is `>=1.2 <2.0`, and `~1.2.3` is `>=1.2.3 <1.3.0`: the last given number may grow.
fn composer_desugar(op: &str, p: &Partial) -> Option<Vec<Comparator>> {
    let scheme = Scheme::Composer;
    if op == "~" && !p.nums.is_empty() {
        let n = p.nums.len();
        let at = n.saturating_sub(2);
        return Some(vec![
            cmp(scheme, Bound::Ge, &p.nums, p.pre.as_deref())?,
            cmp(scheme, Bound::Lt, &raised(&p.nums, at), None)?,
        ]);
    }
    desugar(scheme, op, p, false)
}

fn strip_flags(s: &str) -> String {
    // `@dev`, `@stable` and `as 1.0` aliases are not part of the range.
    let s = s.split(" as ").next().unwrap_or(s);
    let mut out = Vec::new();
    for w in s.split_whitespace() {
        match w.split_once('@') {
            Some((a, _)) if !a.is_empty() => out.push(a),
            Some(_) => {}
            None => out.push(w),
        }
    }
    out.join(" ")
}

// ------------------------------------------------------------------------------ evaluation

fn holds(c: &Comparator, v: &Version, text: &str) -> bool {
    match &c.bound {
        Bound::Any => true,
        Bound::Ge(b) => v >= b,
        Bound::Gt(b) => v > b,
        Bound::Le(b) => v <= b,
        Bound::Lt(b) => v < b,
        Bound::Eq(b) => v == b,
        Bound::Ne(b) => v != b,
        Bound::Text(t) => t == text,
    }
}

fn set_admits(set: &Set, v: &Version, text: &str) -> bool {
    if !set.iter().all(|c| holds(c, v, text)) {
        return false;
    }
    if v.is_prerelease() {
        // A pre-release is admitted only by a comparator that names one of the same release.
        let t = triple(v);
        return set.iter().any(|c| c.pre.as_ref() == Some(&t));
    }
    true
}

/// Whether `requirement`, written in the language of `eco`, admits `version`. `None`: the requirement or
/// the version is not understood.
pub fn admits(eco: Ecosystem, requirement: &str, version: &str) -> Option<bool> {
    let req = requirement.trim();
    if req.len() > 512 {
        return None;
    }
    let scheme = eco.scheme();
    let v = Version::parse(scheme, version)?;
    let sets = match eco {
        Ecosystem::Npm => semver_sets(req, false)?,
        Ecosystem::CratesIo => semver_sets(req, true)?,
        Ecosystem::PyPi => pep440_sets(req)?,
        Ecosystem::RubyGems => gem_sets(req)?,
        Ecosystem::Packagist => composer_sets(req)?,
        _ => return None,
    };
    Some(sets.iter().any(|s| set_admits(s, &v, version)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn npm(r: &str, v: &str) -> Option<bool> {
        admits(Ecosystem::Npm, r, v)
    }
    fn cargo(r: &str, v: &str) -> Option<bool> {
        admits(Ecosystem::CratesIo, r, v)
    }

    #[test]
    fn npm_carets_tildes_and_partials() {
        assert_eq!(npm("^1.2.3", "1.9.0"), Some(true));
        assert_eq!(npm("^1.2.3", "2.0.0"), Some(false));
        assert_eq!(npm("^1.2.3", "1.2.2"), Some(false));
        assert_eq!(npm("^0.2.3", "0.2.9"), Some(true));
        assert_eq!(npm("^0.2.3", "0.3.0"), Some(false));
        assert_eq!(npm("^0.0.3", "0.0.4"), Some(false));
        assert_eq!(npm("^0.0.3", "0.0.3"), Some(true));
        assert_eq!(npm("~1.2.3", "1.2.9"), Some(true));
        assert_eq!(npm("~1.2.3", "1.3.0"), Some(false));
        assert_eq!(npm("~1", "1.9.9"), Some(true));
        assert_eq!(npm("1.2", "1.2.7"), Some(true));
        assert_eq!(npm("1.x", "1.4.0"), Some(true));
        assert_eq!(npm("1.x", "2.0.0"), Some(false));
        assert_eq!(npm("*", "9.0.0"), Some(true));
        assert_eq!(npm("", "9.0.0"), Some(true));
        assert_eq!(npm("1.2.3", "1.2.3"), Some(true));
        assert_eq!(npm("=1.2.3", "1.2.4"), Some(false));
    }

    #[test]
    fn npm_comparators_alternatives_and_hyphens() {
        assert_eq!(npm(">=1.2.0 <2.0.0", "1.5.0"), Some(true));
        assert_eq!(npm(">= 1.2.0 < 2.0.0", "2.0.0"), Some(false));
        assert_eq!(npm("<1.0.0 || >=2.0.0", "1.5.0"), Some(false));
        assert_eq!(npm("<1.0.0 || >=2.0.0", "3.0.0"), Some(true));
        assert_eq!(npm("1.2.3 - 2.3.4", "2.3.4"), Some(true));
        assert_eq!(npm("1.2.3 - 2.3", "2.3.9"), Some(true));
        assert_eq!(npm("1.2.3 - 2.3", "2.4.0"), Some(false));
        assert_eq!(npm(">1.2", "1.2.9"), Some(false));
        assert_eq!(npm(">1.2", "1.3.0"), Some(true));
        assert_eq!(npm("<=1.2", "1.2.9"), Some(true));
    }

    #[test]
    fn npm_prereleases_need_a_comparator_of_the_same_release() {
        assert_eq!(npm("^1.2.3", "1.5.0-beta.1"), Some(false));
        assert_eq!(npm("^1.2.3-beta.1", "1.2.3-beta.4"), Some(true));
        assert_eq!(npm("^1.2.3-beta.1", "1.2.4-beta.1"), Some(false));
    }

    #[test]
    fn npm_text_that_is_not_a_range_is_not_understood() {
        for r in [
            "latest",
            "github:a/b",
            "file:../x",
            "npm:other@1",
            "workspace:*",
            "1.2.3.4.5.6.7.8",
        ] {
            assert_eq!(npm(r, "1.0.0"), None, "{r}");
        }
    }

    #[test]
    fn cargo_requirements() {
        assert_eq!(cargo("1.0", "1.9.0"), Some(true));
        assert_eq!(cargo("1.0", "2.0.0"), Some(false));
        assert_eq!(cargo("0.3", "0.3.9"), Some(true));
        assert_eq!(cargo("0.3", "0.4.0"), Some(false));
        assert_eq!(cargo("=1.2.3", "1.2.4"), Some(false));
        assert_eq!(cargo(">=1.2, <1.5", "1.4.0"), Some(true));
        assert_eq!(cargo(">=1.2, <1.5", "1.5.0"), Some(false));
        assert_eq!(cargo("~1.2", "1.2.9"), Some(true));
        assert_eq!(cargo("~1.2", "1.3.0"), Some(false));
        assert_eq!(cargo("1.*", "1.9.0"), Some(true));
        assert_eq!(cargo("*", "5.0.0"), Some(true));
        assert_eq!(cargo("^0.0.3", "0.0.3"), Some(true));
        assert_eq!(cargo("^0", "0.9.0"), Some(true));
        assert_eq!(cargo("^0", "1.0.0"), Some(false));
    }

    #[test]
    fn pep440_specifiers() {
        let p = |r: &str, v: &str| admits(Ecosystem::PyPi, r, v);
        assert_eq!(p(">=1.21.1,<1.27", "1.26.9"), Some(true));
        assert_eq!(p(">=1.21.1,<1.27", "1.27.0"), Some(false));
        assert_eq!(p("==3.2.0", "3.2.0"), Some(true));
        assert_eq!(p("==3.2.0", "3.2.1"), Some(false));
        assert_eq!(p("==3.2.*", "3.2.9"), Some(true));
        assert_eq!(p("==3.2.*", "3.3.0"), Some(false));
        assert_eq!(p("~=1.4.5", "1.4.9"), Some(true));
        assert_eq!(p("~=1.4.5", "1.5.0"), Some(false));
        assert_eq!(p("~=2.2", "2.9"), Some(true));
        assert_eq!(p("~=2.2", "3.0"), Some(false));
        assert_eq!(p("!=1.5", "1.5"), Some(false));
        assert_eq!(p(">=1.0 ; python_version < '3.8'", "1.1"), Some(true));
        assert_eq!(p("(>=1.0)", "1.1"), Some(true));
        assert_eq!(p("", "1.1"), Some(true));
        assert_eq!(p(">=1.0", "2.0rc1"), Some(false));
        assert_eq!(p(">=2.0rc1", "2.0rc2"), Some(true));
        assert_eq!(p("garbage", "1.0"), None);
    }

    #[test]
    fn gem_requirements() {
        let g = |r: &str, v: &str| admits(Ecosystem::RubyGems, r, v);
        assert_eq!(g("~> 2.0, >= 2.2.4", "2.9.0"), Some(true));
        assert_eq!(g("~> 2.0, >= 2.2.4", "3.0.0"), Some(false));
        assert_eq!(g("~> 1.2.3", "1.2.9"), Some(true));
        assert_eq!(g("~> 1.2.3", "1.3.0"), Some(false));
        assert_eq!(g("= 7.0.4", "7.0.4"), Some(true));
        assert_eq!(g(">= 0", "9.9"), Some(true));
        assert_eq!(g("< 2", "2.0"), Some(false));
        assert_eq!(g("~> 1", "1.9"), Some(true));
        assert_eq!(g("~> 1", "2.0"), Some(false));
    }

    #[test]
    fn composer_constraints() {
        let c = |r: &str, v: &str| admits(Ecosystem::Packagist, r, v);
        assert_eq!(c("^2.8", "2.9.1"), Some(true));
        assert_eq!(c("^2.8", "3.0.0"), Some(false));
        assert_eq!(c("~1.2", "1.9.0"), Some(true));
        assert_eq!(c("~1.2", "2.0.0"), Some(false));
        assert_eq!(c("~1.2.3", "1.2.9"), Some(true));
        assert_eq!(c("~1.2.3", "1.3.0"), Some(false));
        assert_eq!(c("1.2.*", "1.2.9"), Some(true));
        assert_eq!(c(">=1.0 <2.0 || ^3.0", "3.4.0"), Some(true));
        assert_eq!(c(">=1.0,<2.0", "2.0.0"), Some(false));
        assert_eq!(c("1.0 - 2.0", "2.0.5"), Some(true));
        assert_eq!(c("^5.4@dev", "5.4.9"), Some(true));
        assert_eq!(c("dev-main", "5.4.9"), None);
        assert_eq!(c("*", "5.4.9"), Some(true));
    }

    #[test]
    fn ecosystems_without_a_range_language_are_not_understood() {
        assert_eq!(admits(Ecosystem::Go, ">=1", "1.0.0"), None);
        assert_eq!(admits(Ecosystem::Npm, "^1", "not a version"), None);
    }
}
