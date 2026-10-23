//! Regex-to-trigram planner: a port of codesearch `index/regexp.go` onto `regex-syntax`
//! HIR (SPEC §23.3, search row). The plan is a boolean query over folded trigrams that every
//! document containing a match must satisfy; it may admit non-matching documents (the
//! caller verifies candidates) but never excludes a matching one.
//!
//! Each HIR node is summarised by an `Info`:
//! * `exact`: when present, the complete set of strings the node can match;
//! * `prefix` / `suffix`: otherwise, sets such that every match starts / ends with one of
//!   their strings (the empty string means "unknown"; it is present when the node can match
//!   the empty string);
//! * `matched`: a query that any document containing a match satisfies.
//!
//! All strings are ASCII-folded, like the index. Input is untrusted, so the pattern length,
//! nesting depth and every set are bounded (`MAX_PATTERN`, 64 levels, `MAX_EXACT`, `MAX_SET`,
//! `MAX_EXACT_BYTES`).

use std::collections::BTreeSet;

use regex_syntax::ParserBuilder;
use regex_syntax::hir::{Class, Hir, HirKind};

use crate::grams::{Gram, grams_of};

/// Largest exact set kept before it is turned into prefix and suffix sets (codesearch `maxExact`).
pub const MAX_EXACT: usize = 7;
/// Largest prefix or suffix set kept (codesearch `maxSet`).
pub const MAX_SET: usize = 20;
/// A class with more characters than this matches "any" (codesearch: 100).
pub const MAX_CLASS: u64 = 100;
/// Longest accepted pattern, in bytes.
pub const MAX_PATTERN: usize = 4096;
/// Exact sets holding more bytes than this are converted early, to bound work.
const MAX_EXACT_BYTES: usize = 256;
const NEST_LIMIT: u32 = 64;

/// Why a pattern could not be planned.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("pattern longer than {MAX_PATTERN} bytes")]
    TooLong,
    #[error("invalid regex: {0}")]
    Syntax(String),
}

/// A boolean query over trigrams. `All` needs no trigram: the caller must scan the scope.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Plan {
    All,
    None,
    Gram(Gram),
    And(Vec<Plan>),
    Or(Vec<Plan>),
}

impl Plan {
    /// True when the plan gives no trigram to look up (a scan, SPEC §23.3).
    pub fn is_all(&self) -> bool {
        matches!(self, Plan::All)
    }

    /// Conjunction, flattened, sorted and deduplicated.
    pub fn and(self, other: Plan) -> Plan {
        match (self, other) {
            (Plan::None, _) | (_, Plan::None) => Plan::None,
            (Plan::All, x) | (x, Plan::All) => x,
            (a, b) => Plan::combine(a, b, true),
        }
    }

    /// Disjunction, flattened, sorted and deduplicated.
    pub fn or(self, other: Plan) -> Plan {
        match (self, other) {
            (Plan::All, _) | (_, Plan::All) => Plan::All,
            (Plan::None, x) | (x, Plan::None) => x,
            (a, b) => Plan::combine(a, b, false),
        }
    }

    fn combine(a: Plan, b: Plan, and: bool) -> Plan {
        let mut v = Vec::new();
        for p in [a, b] {
            match p {
                Plan::And(xs) if and => v.extend(xs),
                Plan::Or(xs) if !and => v.extend(xs),
                x => v.push(x),
            }
        }
        v.sort_unstable();
        v.dedup();
        if v.len() == 1 {
            return v.pop().unwrap_or(Plan::All);
        }
        if and { Plan::And(v) } else { Plan::Or(v) }
    }

    /// All trigrams of `s` ANDed; `All` when `s` is shorter than 3 bytes.
    fn of_string(s: &[u8]) -> Plan {
        grams_of(s).fold(Plan::All, |p, g| p.and(Plan::Gram(g)))
    }

    /// OR over the strings of the AND of their trigrams. `None` for the empty set.
    fn of_set(set: &Set) -> Plan {
        let mut out = Plan::None;
        for s in set {
            out = out.or(Plan::of_string(s));
            if out.is_all() {
                break;
            }
        }
        out
    }

    /// Evaluates the plan, with `has` telling which trigrams the document holds.
    pub fn eval(&self, has: &dyn Fn(Gram) -> bool) -> bool {
        match self {
            Plan::All => true,
            Plan::None => false,
            Plan::Gram(g) => has(*g),
            Plan::And(v) => v.iter().all(|p| p.eval(has)),
            Plan::Or(v) => v.iter().any(|p| p.eval(has)),
        }
    }

    /// Evaluates the plan against a sorted list of distinct document trigrams.
    pub fn matches_sorted(&self, doc_grams: &[Gram]) -> bool {
        self.eval(&|g| doc_grams.binary_search(&g).is_ok())
    }
}

/// Plans a regex. The index is ASCII-folded, so the plan is the same for case-sensitive and
/// insensitive searches; `case_insensitive` only adds the non-ASCII case variants.
pub fn plan_regex(pattern: &str, case_insensitive: bool) -> Result<Plan, PlanError> {
    if pattern.len() > MAX_PATTERN {
        return Err(PlanError::TooLong);
    }
    let hir = ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .nest_limit(NEST_LIMIT)
        .build()
        .parse(pattern)
        .map_err(|e| PlanError::Syntax(e.to_string()))?;
    Ok(plan_hir(&hir))
}

/// Plans a literal substring search.
pub fn plan_literal(text: &str, case_insensitive: bool) -> Result<Plan, PlanError> {
    if text.len() > MAX_PATTERN {
        return Err(PlanError::TooLong);
    }
    if !case_insensitive || text.is_ascii() {
        return Ok(Plan::of_string(text.as_bytes()));
    }
    plan_regex(&regex_syntax::escape(text), true)
}

/// Plans an already parsed HIR.
pub fn plan_hir(hir: &Hir) -> Plan {
    let mut info = analyze(hir);
    info.simplify(true);
    info.add_exact();
    info.matched
}

type Set = BTreeSet<Vec<u8>>;

#[derive(Clone)]
struct Info {
    exact: Option<Set>,
    prefix: Set,
    suffix: Set,
    matched: Plan,
}

fn fold(bytes: &[u8]) -> Vec<u8> {
    bytes.to_ascii_lowercase()
}

fn min_len(s: &Set) -> usize {
    s.iter().map(Vec::len).min().unwrap_or(0)
}

/// Drops strings that have a shorter member as prefix (or suffix): they add nothing.
fn clean(set: &mut Set, suffix: bool) {
    let all: Vec<Vec<u8>> = set.iter().cloned().collect();
    set.retain(|s| {
        !all.iter().any(|t| {
            t.len() < s.len() && if suffix { s.ends_with(t) } else { s.starts_with(t) }
        })
    });
}

fn cross(a: &Set, b: &Set) -> Set {
    let mut out = Set::new();
    for x in a {
        for y in b {
            let mut s = Vec::with_capacity(x.len() + y.len());
            s.extend_from_slice(x);
            s.extend_from_slice(y);
            out.insert(s);
        }
    }
    out
}

fn single(s: &[u8]) -> Set {
    Set::from([s.to_vec()])
}

impl Info {
    fn with_exact(exact: Set) -> Info {
        Info { exact: Some(exact), prefix: Set::new(), suffix: Set::new(), matched: Plan::All }
    }

    fn empty_string() -> Info {
        Info::with_exact(single(b""))
    }

    fn no_match() -> Info {
        Info::with_exact(Set::new())
    }

    /// Matches anything: no information at all.
    fn any_match() -> Info {
        Info { exact: None, prefix: single(b""), suffix: single(b""), matched: Plan::All }
    }

    fn is_dead(&self) -> bool {
        self.exact.as_ref().is_some_and(Set::is_empty)
    }

    /// Folds the exact set's trigrams into `matched` (the set stays).
    fn add_exact(&mut self) {
        if let Some(e) = &self.exact {
            let q = Plan::of_set(e);
            self.matched = std::mem::replace(&mut self.matched, Plan::All).and(q);
        }
    }

    /// Moves an oversized or long-enough exact set into prefix and suffix sets, then trims
    /// the prefix and suffix sets.
    fn simplify(&mut self, force: bool) {
        if let Some(e) = &self.exact {
            let min = min_len(e);
            let bytes: usize = e.iter().map(Vec::len).sum();
            if e.len() > MAX_EXACT
                || (min >= 3 && force)
                || min >= 4
                || bytes > MAX_EXACT_BYTES
            {
                self.add_exact();
                for s in self.exact.take().unwrap_or_default() {
                    if s.len() < 3 {
                        self.prefix.insert(s.clone());
                        self.suffix.insert(s);
                    } else {
                        self.prefix.insert(s[..2].to_vec());
                        self.suffix.insert(s[s.len() - 2..].to_vec());
                    }
                }
            }
        }
        if self.exact.is_none() {
            self.simplify_set(false);
            self.simplify_set(true);
        }
    }

    fn simplify_set(&mut self, suffix: bool) {
        let mut t = std::mem::take(if suffix { &mut self.suffix } else { &mut self.prefix });
        clean(&mut t, suffix);
        // Every match holds one of these strings, so their trigrams are required.
        if t.iter().any(|s| s.len() >= 3) {
            let q = Plan::of_set(&t);
            self.matched = std::mem::replace(&mut self.matched, Plan::All).and(q);
        }
        // Two bytes per side are enough to build every trigram across a join. Shorten to
        // one byte when the set is still too large, then give up.
        for n in [2usize, 1] {
            if n == 2 || t.len() > MAX_SET {
                t = t
                    .into_iter()
                    .map(|s| {
                        if s.len() <= n {
                            s
                        } else if suffix {
                            s[s.len() - n..].to_vec()
                        } else {
                            s[..n].to_vec()
                        }
                    })
                    .collect();
                clean(&mut t, suffix);
            }
        }
        if t.len() > MAX_SET {
            t = single(b"");
        }
        *(if suffix { &mut self.suffix } else { &mut self.prefix }) = t;
    }
}

fn concat(x: Info, y: Info) -> Info {
    if x.is_dead() || y.is_dead() {
        let mut d = Info::no_match();
        d.matched = Plan::None;
        return d;
    }
    let matched = x.matched.clone().and(y.matched.clone());
    let mut r = match (&x.exact, &y.exact) {
        (Some(a), Some(b)) => Info::with_exact(cross(a, b)),
        _ => {
            let mut prefix = match &x.exact {
                Some(a) => cross(a, &y.prefix),
                None => x.prefix.clone(),
            };
            clean(&mut prefix, false);
            let mut suffix = match &y.exact {
                Some(b) => cross(&x.suffix, b),
                None => y.suffix.clone(),
            };
            clean(&mut suffix, true);
            let mut m = Plan::All;
            // Both sides are open, so a match crosses the join through some suffix of x
            // followed by some prefix of y.
            if x.exact.is_none()
                && y.exact.is_none()
                && x.suffix.len() <= MAX_SET
                && y.prefix.len() <= MAX_SET
                && min_len(&x.suffix) + min_len(&y.prefix) >= 3
            {
                m = Plan::of_set(&cross(&x.suffix, &y.prefix));
            }
            Info { exact: None, prefix, suffix, matched: m }
        }
    };
    r.matched = matched.and(std::mem::replace(&mut r.matched, Plan::All));
    r.simplify(false);
    r
}

fn alternate(mut x: Info, mut y: Info) -> Info {
    let mut r = match (&x.exact, &y.exact) {
        (Some(a), Some(b)) => Info::with_exact(a.union(b).cloned().collect()),
        (Some(a), None) => {
            let prefix = a.union(&y.prefix).cloned().collect();
            let suffix = a.union(&y.suffix).cloned().collect();
            x.add_exact();
            Info { exact: None, prefix, suffix, matched: Plan::All }
        }
        (None, Some(b)) => {
            let prefix = x.prefix.union(b).cloned().collect();
            let suffix = x.suffix.union(b).cloned().collect();
            y.add_exact();
            Info { exact: None, prefix, suffix, matched: Plan::All }
        }
        (None, None) => Info {
            exact: None,
            prefix: x.prefix.union(&y.prefix).cloned().collect(),
            suffix: x.suffix.union(&y.suffix).cloned().collect(),
            matched: Plan::All,
        },
    };
    r.matched = x.matched.or(y.matched);
    r.simplify(false);
    r
}

/// `x+`: at least one `x`, so prefixes and suffixes stay; an exact `x` no longer is.
fn plus(mut x: Info) -> Info {
    if let Some(e) = x.exact.take() {
        x.prefix = e.clone();
        x.suffix = e;
    }
    x.simplify(false);
    x
}

fn class_info(class: &Class) -> Info {
    let mut set = Set::new();
    match class {
        Class::Unicode(c) => {
            let n: u64 = c.iter().map(|r| u64::from(r.end() as u32 - r.start() as u32) + 1).sum();
            if n > MAX_CLASS {
                return Info::any_match();
            }
            for r in c.iter() {
                for cp in r.start() as u32..=r.end() as u32 {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut buf = [0u8; 4];
                        set.insert(fold(ch.encode_utf8(&mut buf).as_bytes()));
                    }
                }
            }
        }
        Class::Bytes(c) => {
            let n: u64 = c.iter().map(|r| u64::from(r.end() - r.start()) + 1).sum();
            if n > MAX_CLASS {
                return Info::any_match();
            }
            for r in c.iter() {
                for b in r.start()..=r.end() {
                    set.insert(fold(&[b]));
                }
            }
        }
    }
    Info::with_exact(set)
}

fn analyze(hir: &Hir) -> Info {
    let mut info = match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::empty_string(),
        HirKind::Literal(l) => Info::with_exact(single(&fold(&l.0))),
        HirKind::Class(c) => class_info(c),
        HirKind::Capture(c) => analyze(&c.sub),
        HirKind::Repetition(r) => {
            if r.max == Some(0) {
                Info::empty_string()
            } else if r.min == 0 {
                if r.max == Some(1) {
                    alternate(analyze(&r.sub), Info::empty_string())
                } else {
                    Info::any_match()
                }
            } else if r.min == 1 && r.max == Some(1) {
                analyze(&r.sub)
            } else {
                plus(analyze(&r.sub))
            }
        }
        HirKind::Concat(subs) => {
            subs.iter().fold(Info::empty_string(), |acc, s| concat(acc, analyze(s)))
        }
        HirKind::Alternation(subs) => {
            let mut it = subs.iter();
            match it.next() {
                None => Info::no_match(),
                Some(first) => it.fold(analyze(first), |acc, s| alternate(acc, analyze(s))),
            }
        }
    };
    info.simplify(false);
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grams::distinct_trigrams;

    fn g(s: &str) -> Plan {
        Plan::of_string(s.as_bytes())
    }

    #[test]
    fn google_search_is_the_eight_trigram_and() {
        let p = plan_regex("Google.*Search", false).unwrap();
        assert_eq!(p, g("google").and(g("search")));
        match &p {
            Plan::And(v) => assert_eq!(v.len(), 8),
            other => panic!("not an AND: {other:?}"),
        }
    }

    #[test]
    fn case_folding_gives_the_same_plan() {
        let a = plan_regex("(?i)GOOGLE", true).unwrap();
        assert_eq!(a, plan_regex("google", false).unwrap());
        assert_eq!(plan_literal("GooGle", false).unwrap(), g("google"));
        assert_eq!(plan_literal("GooGle", true).unwrap(), g("google"));
    }

    #[test]
    fn non_ascii_case_variants_are_alternatives() {
        let p = plan_literal("caf\u{e9}s", true).unwrap();
        let lower = distinct_trigrams("caf\u{e9}s".as_bytes(), 100).unwrap();
        let upper = distinct_trigrams("caf\u{c9}s".as_bytes(), 100).unwrap();
        assert!(p.matches_sorted(&lower) && p.matches_sorted(&upper));
        assert!(!p.matches_sorted(&[]));
    }

    #[test]
    fn wide_class_is_match_all() {
        assert!(plan_regex("[a-z]", false).unwrap().is_all());
        assert!(plan_regex(r"\w+", false).unwrap().is_all());
        assert!(plan_regex(r"[\x00-\x{ff}]{3}", false).unwrap().is_all());
        assert_eq!(plan_regex("[ab]cd", false).unwrap(), g("acd").or(g("bcd")));
    }

    #[test]
    fn class_limit_is_one_hundred_characters() {
        let parse = |p: &str| ParserBuilder::new().build().parse(p).unwrap();
        // 100 characters are enumerated (two lead bytes survive), 101 are "any".
        let hundred = analyze(&parse("[\\x{100}-\\x{163}]"));
        let wide = analyze(&parse("[\\x{100}-\\x{164}]"));
        assert_eq!(hundred.prefix, Set::from([vec![0xc4], vec![0xc5]]));
        assert_eq!(wide.prefix, single(b""));
    }

    #[test]
    fn short_and_open_patterns_have_no_trigram() {
        for p in ["", "ab", ".*", "a|bcd", "(abc)?", "x*abc*"] {
            assert!(plan_regex(p, false).unwrap().is_all(), "{p}");
        }
    }

    #[test]
    fn alternation_and_repeats() {
        assert_eq!(plan_regex("abc|abd", false).unwrap(), g("abc").or(g("abd")));
        assert_eq!(plan_regex("(abc)+", false).unwrap(), g("abc"));
        assert_eq!(plan_regex("abcd{2,}", false).unwrap(), g("abcd"));
        assert_eq!(
            plan_regex("foo(bar)?baz", false).unwrap(),
            g("foobaz").or(g("foobarbaz"))
        );
    }

    #[test]
    fn empty_class_never_matches() {
        assert_eq!(plan_regex("a[^\\x00-\\x{10FFFF}]b", false).unwrap(), Plan::None);
    }

    #[test]
    fn bounds_and_errors() {
        assert_eq!(plan_regex(&"a".repeat(MAX_PATTERN + 1), false), Err(PlanError::TooLong));
        assert!(matches!(plan_regex("(", false), Err(PlanError::Syntax(_))));
        let deep = format!("{}a{}", "(".repeat(200), ")".repeat(200));
        assert!(matches!(plan_regex(&deep, false), Err(PlanError::Syntax(_))));
        let big = "(a|b|c|d|e|f|g|h|i|j){20}[a-c]{99}".repeat(100);
        assert!(plan_regex(&big, false).is_ok());
    }
}
