//! The planner never excludes a document that the regex matches (SPEC §23.3): the plan is
//! compared with brute force (`regex::Regex::is_match`) over generated patterns and documents.

use asc_codeintel::grams::distinct_trigrams;
use asc_codeintel::plan::{Plan, plan_literal, plan_regex};
use proptest::prelude::*;
use regex::Regex;

fn atom() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[a-d]{1,5}",
        1 => Just("abcd".to_string()),
        1 => Just("Google".to_string()),
        1 => Just("caf\u{e9}".to_string()),
        1 => Just("[ab]".to_string()),
        1 => Just("[^a]".to_string()),
        1 => Just(".".to_string()),
        1 => Just(r"\w".to_string()),
        1 => Just(r"\b".to_string()),
        1 => Just("^".to_string()),
        1 => Just("$".to_string()),
        1 => Just("(?i:ab)".to_string()),
        1 => Just("\u{e9}".to_string()),
    ]
}

fn pattern() -> impl Strategy<Value = String> {
    atom().prop_recursive(4, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 2..4).prop_map(|v| v.concat()),
            prop::collection::vec(inner.clone(), 2..4)
                .prop_map(|v| format!("(?:{})", v.join("|"))),
            inner.clone().prop_map(|s| format!("(?:{s})*")),
            inner.clone().prop_map(|s| format!("(?:{s})+")),
            inner.clone().prop_map(|s| format!("(?:{s})?")),
            inner.clone().prop_map(|s| format!("(?:{s}){{2,3}}")),
            inner.prop_map(|s| format!("({s})")),
        ]
    })
}

fn doc() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => prop::sample::select(vec!['a', 'b', 'c', 'd']),
            2 => prop::sample::select(vec!['A', 'B', 'G', 'o', 'l', 'e', 'g']),
            1 => prop::sample::select(vec!['\u{e9}', '\u{c9}', ' ', '\n', '_']),
        ],
        0..40,
    )
    .prop_map(|v| v.into_iter().collect())
}

fn check(re: &Regex, plan: &Plan, text: &str) -> Result<(), TestCaseError> {
    let grams = distinct_trigrams(text.as_bytes(), usize::MAX).unwrap_or_default();
    if re.is_match(text) {
        prop_assert!(
            plan.matches_sorted(&grams),
            "plan {plan:?} excludes {text:?} matched by {}",
            re.as_str()
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn plan_never_excludes_a_matching_doc(
        pat in pattern(),
        ci in any::<bool>(),
        docs in prop::collection::vec(doc(), 8),
    ) {
        let Ok(re) = regex::RegexBuilder::new(&pat).case_insensitive(ci).build() else {
            return Ok(());
        };
        let plan = plan_regex(&pat, ci).expect("a pattern that compiles must plan");
        for d in &docs {
            check(&re, &plan, d)?;
            // A doc spliced out of itself makes matches more likely.
            check(&re, &plan, &format!("{d}{d}"))?;
        }
    }

    #[test]
    fn literal_plan_never_excludes_a_containing_doc(
        needle in "[a-dA-D\u{e9}\u{c9}]{0,8}",
        ci in any::<bool>(),
        left in doc(),
        right in doc(),
    ) {
        let plan = plan_literal(&needle, ci).unwrap();
        let re = regex::RegexBuilder::new(&regex::escape(&needle))
            .case_insensitive(ci)
            .build()
            .unwrap();
        check(&re, &plan, &format!("{left}{needle}{right}"))?;
        check(&re, &plan, &format!("{left}{}{right}", needle.to_uppercase()))?;
    }
}

/// Matching docs are built from the pattern itself: a doc made of every literal run of the
/// pattern, so that most cases reach the interesting branch (match found).
#[test]
fn brute_force_on_fixed_patterns() {
    let cases: &[(&str, &str, bool)] = &[
        ("Google.*Search", "xx GOOGLE is a Search yy", true),
        ("foo(bar)?baz", "foobarbaz", false),
        ("(abc|abd)e+", "zzabdeee", false),
        ("a.c", "a\u{e9}c", false),
        ("\u{e9}t\u{e9}", "\u{c9}T\u{c9}", true),
        ("x{3}yz", "xxxyz", false),
        ("[ab]{2}cd", "bacd", false),
        ("^abc$", "abc", false),
    ];
    for (pat, text, ci) in cases {
        let re = regex::RegexBuilder::new(pat).case_insensitive(*ci).build().unwrap();
        assert!(re.is_match(text), "{pat} should match {text:?}");
        let plan = plan_regex(pat, *ci).unwrap();
        let grams = distinct_trigrams(text.as_bytes(), usize::MAX).unwrap();
        assert!(plan.matches_sorted(&grams), "{pat}: {plan:?} excludes {text:?}");
    }
}

#[test]
fn plan_prunes_non_matching_docs() {
    let plan = plan_regex("Google.*Search", false).unwrap();
    let grams = distinct_trigrams(b"nothing to see", usize::MAX).unwrap();
    assert!(!plan.matches_sorted(&grams));
}
