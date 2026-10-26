//! Flaky test detection from a test's recent results (docs/BUILDOUT.md section 8).
//!
//! A test is flaky when the same code gives it both verdicts. Two results belong to the same code
//! when they were made for the same head, or by retries inside one run. The detector sorts a
//! test's results by time and counts a *flip* each time the verdict changes between two
//! consecutive results of one head. A test that failed on a new head after it passed on the old
//! one did not flip: the code changed.
//!
//! The rate is flips over runs in the window: the latest [`DEFAULT_LAST_RUNS`] runs (or the number
//! the caller names). A test is flaky when it flipped at least once and the rate is at or above
//! the threshold ([`DEFAULT_THRESHOLD`], policy `[ci] flaky_threshold`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Runs the verdict looks at when the caller names no number.
pub const DEFAULT_LAST_RUNS: usize = 100;
/// The flip rate at which a test is flaky when the policy names none.
pub const DEFAULT_THRESHOLD: f64 = 0.02;
/// Seconds in a day bucket.
pub const DAY: i64 = 86_400;

/// One result of one test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The run that reported it.
    pub run: u64,
    /// Unix seconds.
    pub at: i64,
    /// 1 for the first try, higher for a retry inside the run.
    pub attempt: u32,
    /// True for a pass, false for a failure.
    pub passed: bool,
    /// The code the test ran against (a changeset or commit id). Empty when unknown: the result
    /// then only compares with other results of its own run.
    pub head: Vec<u8>,
}

/// The totals of one UTC day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bucket {
    /// Unix seconds of 00:00 UTC.
    pub start: i64,
    pub runs: u32,
    pub failures: u32,
    pub flips: u32,
}

/// What the detector found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlakyStats {
    /// Runs in the window.
    pub runs: u32,
    /// Failed results in the window.
    pub failures: u32,
    /// Verdict changes between results of one head.
    pub flips: u32,
    /// `flips / runs`, 0 when there are no runs.
    pub rate: f64,
    pub flaky: bool,
    pub first_flip: Option<i64>,
    pub last_flip: Option<i64>,
    /// Per UTC day, oldest first, over the window.
    pub buckets: Vec<Bucket>,
}

/// The start of the UTC day of `at`.
pub fn day_start(at: i64) -> i64 {
    at - at.rem_euclid(DAY)
}

/// Analyses the results of one test. `last_runs` is the window in runs (0 means
/// [`DEFAULT_LAST_RUNS`]); `threshold` is the flip rate that makes a test flaky (a value that is
/// not finite or not above zero means [`DEFAULT_THRESHOLD`]).
pub fn analyse(observations: &[Observation], last_runs: usize, threshold: f64) -> FlakyStats {
    let last_runs = if last_runs == 0 {
        DEFAULT_LAST_RUNS
    } else {
        last_runs
    };
    let threshold = if threshold.is_finite() && threshold > 0.0 {
        threshold
    } else {
        DEFAULT_THRESHOLD
    };

    // The window: the newest `last_runs` runs, a run being as new as its newest result.
    let mut newest: HashMap<u64, i64> = HashMap::new();
    for o in observations {
        let t = newest.entry(o.run).or_insert(o.at);
        *t = (*t).max(o.at);
    }
    let mut runs: Vec<(u64, i64)> = newest.into_iter().collect();
    runs.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    runs.truncate(last_runs);
    let in_window: BTreeSet<u64> = runs.iter().map(|(r, _)| *r).collect();

    let mut window: Vec<&Observation> = observations
        .iter()
        .filter(|o| in_window.contains(&o.run))
        .collect();
    window.sort_by_key(|o| (o.at, o.run, o.attempt));

    // Verdict changes, per head. A result with no head compares with its own run only.
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum Key<'a> {
        Head(&'a [u8]),
        Run(u64),
    }
    let mut last: BTreeMap<Key<'_>, bool> = BTreeMap::new();
    let mut flip_times: Vec<i64> = Vec::new();
    for o in &window {
        let key = if o.head.is_empty() {
            Key::Run(o.run)
        } else {
            Key::Head(&o.head)
        };
        if let Some(prev) = last.insert(key, o.passed)
            && prev != o.passed
        {
            flip_times.push(o.at);
        }
    }

    let mut days: BTreeMap<i64, (BTreeSet<u64>, u32, u32)> = BTreeMap::new();
    for o in &window {
        let d = days.entry(day_start(o.at)).or_default();
        d.0.insert(o.run);
        if !o.passed {
            d.1 += 1;
        }
    }
    for t in &flip_times {
        days.entry(day_start(*t)).or_default().2 += 1;
    }
    let buckets: Vec<Bucket> = days
        .into_iter()
        .map(|(start, (runs, failures, flips))| Bucket {
            start,
            runs: u32::try_from(runs.len()).unwrap_or(u32::MAX),
            failures,
            flips,
        })
        .collect();

    let runs_n = u32::try_from(in_window.len()).unwrap_or(u32::MAX);
    let flips = u32::try_from(flip_times.len()).unwrap_or(u32::MAX);
    let rate = if runs_n == 0 {
        0.0
    } else {
        f64::from(flips) / f64::from(runs_n)
    };
    FlakyStats {
        runs: runs_n,
        failures: u32::try_from(window.iter().filter(|o| !o.passed).count()).unwrap_or(u32::MAX),
        flips,
        rate,
        flaky: flips > 0 && rate >= threshold,
        first_flip: flip_times.iter().copied().min(),
        last_flip: flip_times.iter().copied().max(),
        buckets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(run: u64, at: i64, attempt: u32, passed: bool, head: &str) -> Observation {
        Observation {
            run,
            at,
            attempt,
            passed,
            head: head.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_test_that_always_passes_or_always_fails_is_not_flaky() {
        let pass: Vec<_> = (1..=20).map(|i| o(i, i as i64 * 100, 1, true, "h")).collect();
        let s = analyse(&pass, 0, 0.0);
        assert_eq!((s.runs, s.failures, s.flips, s.flaky), (20, 0, 0, false));
        let fail: Vec<_> = (1..=20).map(|i| o(i, i as i64 * 100, 1, false, "h")).collect();
        let s = analyse(&fail, 0, 0.0);
        assert_eq!((s.runs, s.failures, s.flips, s.flaky), (20, 20, 0, false));
        assert_eq!(s.first_flip, None);
    }

    #[test]
    fn pass_then_fail_on_one_head_is_a_flip() {
        let s = analyse(
            &[o(1, 100, 1, true, "a"), o(2, 200, 1, false, "a"), o(3, 300, 1, true, "a")],
            0,
            0.0,
        );
        assert_eq!(s.flips, 2);
        assert_eq!((s.first_flip, s.last_flip), (Some(200), Some(300)));
        assert!(s.flaky);
        assert!((s.rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_fix_on_a_new_head_is_not_a_flip() {
        // Failed on head a, passed on head b: the code changed.
        let s = analyse(&[o(1, 100, 1, false, "a"), o(2, 200, 1, true, "b")], 0, 0.0);
        assert_eq!(s.flips, 0);
        assert!(!s.flaky);
    }

    #[test]
    fn a_retry_inside_a_run_flips_even_without_a_head() {
        let s = analyse(&[o(7, 100, 1, false, ""), o(7, 130, 2, true, "")], 0, 0.0);
        assert_eq!(s.flips, 1);
        assert_eq!(s.runs, 1);
        assert!(s.flaky);
        // Results of different runs with no head are not compared.
        let s = analyse(&[o(1, 100, 1, false, ""), o(2, 200, 1, true, "")], 0, 0.0);
        assert_eq!(s.flips, 0);
    }

    #[test]
    fn the_threshold_decides() {
        // One flip in 100 runs: rate 0.01, below the default 0.02.
        let mut v: Vec<_> = (1..=100).map(|i| o(i, i as i64 * 10, 1, true, "h")).collect();
        v[50].passed = false;
        let s = analyse(&v, 100, 0.0);
        assert_eq!(s.flips, 2, "a lone failure flips out and back");
        assert!(s.flaky);
        // Two flips (one failing result) in 200 runs is 0.01.
        let mut v: Vec<_> = (1..=200).map(|i| o(i, i as i64 * 10, 1, true, "h")).collect();
        v[100].passed = false;
        let s = analyse(&v, 200, 0.0);
        assert_eq!(s.flips, 2);
        assert!(!s.flaky);
        assert!(analyse(&v, 200, 0.005).flaky);
        // A threshold that is not a usable number means the default.
        assert!(!analyse(&v, 200, f64::NAN).flaky);
        assert!(!analyse(&v, 200, -1.0).flaky);
    }

    #[test]
    fn only_the_last_runs_count() {
        // Old flips outside the window are forgotten.
        let mut v = vec![o(1, 10, 1, true, "a"), o(2, 20, 1, false, "a"), o(3, 30, 1, true, "a")];
        v.extend((4..=13).map(|i| o(i, i as i64 * 10, 1, true, "b")));
        assert!(analyse(&v, 13, 0.0).flaky);
        let s = analyse(&v, 10, 0.0);
        assert_eq!((s.runs, s.flips, s.flaky), (10, 0, false));
    }

    #[test]
    fn input_order_does_not_matter() {
        let v = vec![o(3, 300, 1, true, "a"), o(1, 100, 1, true, "a"), o(2, 200, 1, false, "a")];
        let mut r = v.clone();
        r.reverse();
        assert_eq!(analyse(&v, 0, 0.0), analyse(&r, 0, 0.0));
    }

    #[test]
    fn daily_buckets_count_runs_failures_and_flips() {
        let d0 = 1_700_000_000 - 1_700_000_000_i64.rem_euclid(DAY);
        let v = vec![
            o(1, d0 + 10, 1, true, "a"),
            o(2, d0 + 20, 1, false, "a"),
            o(3, d0 + DAY + 5, 1, true, "a"),
        ];
        let s = analyse(&v, 0, 0.0);
        assert_eq!(s.buckets.len(), 2);
        assert_eq!(
            s.buckets[0],
            Bucket { start: d0, runs: 2, failures: 1, flips: 1 }
        );
        assert_eq!(
            s.buckets[1],
            Bucket { start: d0 + DAY, runs: 1, failures: 0, flips: 1 }
        );
        assert_eq!(day_start(-1), -DAY);
    }

    #[test]
    fn no_observations_is_an_empty_verdict() {
        assert_eq!(analyse(&[], 0, 0.0), FlakyStats::default());
    }

    #[test]
    fn five_thousand_results_are_handled() {
        let v: Vec<_> = (0..5000)
            .map(|i| o(i / 2 + 1, i as i64, (i % 2) as u32 + 1, i % 7 != 0, "h"))
            .collect();
        let s = analyse(&v, 5000, 0.0);
        assert!(s.flaky);
        assert!(s.flips > 0);
    }
}
