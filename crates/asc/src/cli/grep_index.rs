//! Index acceleration of `grep <rev>` (SPEC §23.4.2, unit I30). The code-intelligence index can
//! name a superset of the files of a revision that may match a pattern, so the grep reads those
//! files instead of the whole tree. The output stays byte-identical to the streaming path for
//! three reasons: the index only ever removes files from the candidate list the streaming path
//! walked (it can never add one), the survivors keep the streaming path's byte order, and every
//! survivor is verified by the same matcher. The index is used only when it covers the whole
//! revision (`coverage == 1`); otherwise, and for any query the index cannot prefilter, the
//! streaming path runs unchanged.

use std::collections::HashSet;

use asc_core::id::ChangesetId;
use asc_core::path::RepoPath;

/// Most candidate paths one answer may name; a larger answer is ignored (untrusted input).
pub const MAX_CANDIDATES: usize = 4 << 20;

/// What is asked of the index.
#[derive(Clone, Copy, Debug)]
pub struct Query<'a> {
    pub rev: ChangesetId,
    pub pattern: &'a str,
    pub fixed: bool,
    pub ignore_case: bool,
    pub word: bool,
}

/// The index's answer: the files of the revision that may match, and how much of the revision
/// the index covers.
#[derive(Clone, Debug)]
pub struct Answer {
    pub coverage: f64,
    pub candidates: Vec<RepoPath>,
}

/// A source of index answers. `None` means the index cannot answer this query.
pub trait IndexSource: Send + Sync {
    fn candidates(&self, q: &Query<'_>) -> Option<Answer>;
}

/// The index available to this process. The local helper (`asc codeintel worker`) is the only
/// source; none is attached when it is not running, and the streaming path then runs.
pub fn local_source() -> Option<Box<dyn IndexSource>> {
    None
}

/// Whether a query is one the index can prefilter: a single positive pattern.
pub fn prefilterable(patterns: usize, invert: bool) -> bool {
    patterns == 1 && !invert
}

/// Keeps of `files` (sorted by the caller) those the index lists. `files` is returned whole when
/// there is no source, no answer, a partial coverage or an oversized answer.
pub fn narrow(source: Option<&dyn IndexSource>, q: &Query<'_>, files: Vec<RepoPath>) -> Vec<RepoPath> {
    let Some(answer) = source.and_then(|s| s.candidates(q)) else { return files };
    if answer.coverage.is_nan() || answer.coverage < 1.0 || answer.candidates.len() > MAX_CANDIDATES {
        return files;
    }
    let keep: HashSet<&[u8]> = answer.candidates.iter().map(RepoPath::as_bytes).collect();
    files.into_iter().filter(|f| keep.contains(f.as_bytes())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Option<Answer>);
    impl IndexSource for Fixed {
        fn candidates(&self, _: &Query<'_>) -> Option<Answer> {
            self.0.clone()
        }
    }

    fn paths(v: &[&str]) -> Vec<RepoPath> {
        v.iter().map(|p| RepoPath::new(p.as_bytes().to_vec()).unwrap()).collect()
    }

    fn q() -> Query<'static> {
        Query { rev: ChangesetId::from_bytes([1; 32]), pattern: "x", fixed: true, ignore_case: false, word: false }
    }

    fn names(v: &[RepoPath]) -> Vec<&[u8]> {
        v.iter().map(RepoPath::as_bytes).collect()
    }

    #[test]
    fn full_coverage_narrows_and_keeps_order() {
        let src = Fixed(Some(Answer { coverage: 1.0, candidates: paths(&["c", "a", "zzz"]) }));
        let out = narrow(Some(&src), &q(), paths(&["a", "b", "c"]));
        assert_eq!(names(&out), [&b"a"[..], b"c"]);
    }

    #[test]
    fn partial_coverage_falls_back() {
        let src = Fixed(Some(Answer { coverage: 0.99, candidates: paths(&["a"]) }));
        assert_eq!(narrow(Some(&src), &q(), paths(&["a", "b"])).len(), 2);
        let nan = Fixed(Some(Answer { coverage: f64::NAN, candidates: paths(&["a"]) }));
        assert_eq!(narrow(Some(&nan), &q(), paths(&["a", "b"])).len(), 2);
    }

    #[test]
    fn no_source_or_answer_falls_back() {
        assert_eq!(narrow(None, &q(), paths(&["a", "b"])).len(), 2);
        assert_eq!(narrow(Some(&Fixed(None)), &q(), paths(&["a", "b"])).len(), 2);
    }

    #[test]
    fn only_a_single_positive_pattern_is_prefiltered() {
        assert!(prefilterable(1, false));
        assert!(!prefilterable(2, false));
        assert!(!prefilterable(1, true));
    }
}
