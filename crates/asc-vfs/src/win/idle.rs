//! The idle dehydration policy both Windows backends apply (SPEC §10.2 item 9): a clean hydrated
//! file goes back to empty when no process has read it for [`IDLE`], and then the least recently
//! read files go until the hydrated bytes are within the budget. The hydration log lives in
//! memory, so a file the log has never seen counts as read when the provider started.

use std::time::{Duration, SystemTime};

use asc_core::path::RepoPath;

/// No process has read the file for this long: it is dropped.
pub const IDLE: Duration = Duration::from_secs(3 * 24 * 3600);
/// `asc.vfs.hydrationBudget` default: 500 MiB.
pub const DEFAULT_BUDGET: u64 = 500 * 1024 * 1024;

/// When the idle pass drops files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub idle: Duration,
    pub budget: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            idle: IDLE,
            budget: DEFAULT_BUDGET,
        }
    }
}

/// What an idle pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub files: u64,
    /// Bytes of the files that went back to empty.
    pub bytes: u64,
    /// Candidates that could not be dehydrated (open, changed meanwhile).
    pub skipped: Vec<RepoPath>,
}

/// A hydrated clean file a backend's scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path: RepoPath,
    pub size: u64,
    pub last_read: SystemTime,
}

/// The candidates the idle pass at `now` drops: every idle one, then the least recently read until
/// the rest fits the budget.
pub fn select(mut all: Vec<Candidate>, policy: &Policy, now: SystemTime) -> Vec<Candidate> {
    let total: u64 = all.iter().map(|c| c.size).sum();
    let idle_for =
        |c: &Candidate| now.duration_since(c.last_read).unwrap_or_default() >= policy.idle;
    let (mut chosen, mut rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|c| idle_for(c));
    let mut left = total.saturating_sub(chosen.iter().map(|c| c.size).sum());
    if left > policy.budget {
        rest.sort_by(|a, b| a.last_read.cmp(&b.last_read).then(a.path.cmp(&b.path)));
        for c in rest {
            if left <= policy.budget {
                break;
            }
            left = left.saturating_sub(c.size);
            chosen.push(c);
        }
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(path: &str, size: u64, at: u64) -> Candidate {
        Candidate {
            path: RepoPath::new(path).unwrap(),
            size,
            last_read: SystemTime::UNIX_EPOCH + Duration::from_secs(at),
        }
    }

    fn names(v: &[Candidate]) -> Vec<&[u8]> {
        v.iter().map(|c| c.path.as_bytes()).collect()
    }

    #[test]
    fn idle_files_go_and_the_budget_takes_the_oldest_next() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let all = vec![c("new", 10, 990), c("old", 10, 1), c("mid", 10, 500)];
        let p = Policy {
            idle: Duration::from_secs(900),
            budget: u64::MAX,
        };
        assert_eq!(names(&select(all.clone(), &p, now)), [b"old".as_slice()]);
        let p = Policy {
            idle: Duration::from_secs(900),
            budget: 10,
        };
        assert_eq!(
            names(&select(all, &p, now)),
            [b"old".as_slice(), b"mid".as_slice()]
        );
    }

    #[test]
    fn a_read_in_the_future_is_not_idle() {
        let now = SystemTime::UNIX_EPOCH;
        let p = Policy {
            idle: Duration::ZERO,
            budget: u64::MAX,
        };
        // A zero idle period takes everything, even a clock that went backwards.
        assert_eq!(select(vec![c("a", 1, 50)], &p, now).len(), 1);
        let p = Policy {
            idle: Duration::from_secs(1),
            budget: u64::MAX,
        };
        assert!(select(vec![c("a", 1, 50)], &p, now).is_empty());
    }
}
