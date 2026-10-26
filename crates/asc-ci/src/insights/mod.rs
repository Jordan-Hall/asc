//! CI failure grouping, flaky-test detection, quarantine and stale results (docs/BUILDOUT.md
//! section 8). The server (`asc-serve`, `CiInsights`) stores the data and calls these pure
//! functions; nothing here reads a database or the network.
//!
//! * [`normalize`]: a failure message becomes a stable text, and the text a [`signature`] that
//!   names its group.
//! * [`flaky`]: a test's recent results become a flip count and a rate.
//! * [`is_stale`]: a result for a change version that a newer version replaced.
//! * [`Quarantine`]: which failing tests a landing check ignores, and who may add one.

pub mod flaky;
pub mod normalize;

use std::collections::BTreeSet;

pub use flaky::{Bucket, FlakyStats, Observation, analyse};
pub use normalize::{Normalised, mask, normalise, signature, signature_hex, truncate_message};

/// Whether a result made for `result_version` of a change is stale when the change is now at
/// `latest_version`. Version 0 means the result belongs to no version (a run on the trunk) and is
/// never stale. A result for a version the change has not reached (a wrong report) is not stale
/// either: it is rejected where it is stored.
pub fn is_stale(result_version: u32, latest_version: u32) -> bool {
    result_version != 0 && result_version < latest_version
}

/// Whether a failure group is new compared with the trunk: none of its occurrences came from a
/// run on the trunk. `on_trunk` lists, for each occurrence, whether its run was on the trunk.
pub fn is_new_vs_main(on_trunk: impl IntoIterator<Item = bool>) -> bool {
    !on_trunk.into_iter().any(|t| t)
}

/// Why a quarantine request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QuarantineDenied {
    #[error("the repository policy does not allow quarantine ([ci] allow_quarantine)")]
    PolicyOff,
    #[error("quarantine needs a maintainer")]
    NotMaintainer,
}

/// The decision behind `SetFailureGroupState(QUARANTINED)`: the policy has to allow quarantine
/// and the caller has to be a maintainer. The policy is checked first so that the answer does not
/// depend on who asks.
pub fn may_quarantine(
    allow_quarantine: bool,
    caller_is_maintainer: bool,
) -> Result<(), QuarantineDenied> {
    if !allow_quarantine {
        return Err(QuarantineDenied::PolicyOff);
    }
    if !caller_is_maintainer {
        return Err(QuarantineDenied::NotMaintainer);
    }
    Ok(())
}

/// A test (in a job) that a landing check ignores while it fails.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Quarantine {
    entries: BTreeSet<(String, String)>,
}

impl Quarantine {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `test` of `job`. An empty `test` quarantines the job-level failure of `job`.
    pub fn add(&mut self, job: &str, test: &str) {
        self.entries.insert((job.to_owned(), test.to_owned()));
    }

    pub fn contains(&self, job: &str, test: &str) -> bool {
        self.entries.contains(&(job.to_owned(), test.to_owned()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The failures of `failing` (job, test) that are not quarantined: the ones that still block.
    pub fn blocking<'a>(
        &self,
        failing: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<(&'a str, &'a str)> {
        failing
            .into_iter()
            .filter(|(job, test)| !self.contains(job, test))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_results() {
        assert!(is_stale(1, 2));
        assert!(!is_stale(2, 2));
        assert!(!is_stale(3, 2));
        assert!(!is_stale(0, 5));
    }

    #[test]
    fn new_versus_main() {
        assert!(is_new_vs_main([false, false]));
        assert!(is_new_vs_main([]));
        assert!(!is_new_vs_main([false, true]));
    }

    #[test]
    fn quarantine_needs_policy_and_a_maintainer() {
        assert_eq!(
            may_quarantine(false, true),
            Err(QuarantineDenied::PolicyOff)
        );
        assert_eq!(
            may_quarantine(false, false),
            Err(QuarantineDenied::PolicyOff)
        );
        assert_eq!(
            may_quarantine(true, false),
            Err(QuarantineDenied::NotMaintainer)
        );
        assert_eq!(may_quarantine(true, true), Ok(()));
    }

    #[test]
    fn a_quarantined_failure_does_not_block() {
        let mut q = Quarantine::new();
        assert!(q.is_empty());
        q.add("unit", "suite::flaky_one");
        q.add("unit", "suite::flaky_one");
        q.add("e2e", "");
        assert_eq!(q.len(), 2);
        let failing = [
            ("unit", "suite::flaky_one"),
            ("unit", "suite::other"),
            ("e2e", ""),
            ("e2e", "login"),
        ];
        assert_eq!(
            q.blocking(failing),
            vec![("unit", "suite::other"), ("e2e", "login")]
        );
    }
}
