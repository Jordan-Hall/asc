//! The turbo diff base of a stacked PR (SPEC §21.4.4).
//!
//! For a stacked PR `GITHUB_BASE_REF` is the trunk (D8), so turbo would diff against the trunk and
//! include the parent changes. `asc ci checkout` exports `TURBO_SCM_BASE` = the bridge SHA of the
//! PR's `merge_base` (the parent change) and `TURBO_SCM_HEAD=HEAD`. The PR record comes from the
//! server, so the SHA is checked before it reaches `$GITHUB_ENV`.
//!
//! Spike S13 (does Forgejo Actions fire `pull_request` for draft PRs) is a fork experiment; its
//! result is recorded by the parent, not by this module.

/// The PR facts `asc ci checkout` needs from the server (C34, C40).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrBase {
    /// The PR's `merge_base` as a bridge (git) commit id: the parent change's commit for a
    /// stacked PR, the trunk commit it was cut from for a root change. `None` when unknown.
    pub merge_base: Option<String>,
    /// The repository's default branch; always fetched and written as `origin/<default_branch>`.
    pub default_branch: String,
}

/// What `asc ci checkout` exports for the diff base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackBase {
    /// Value of `TURBO_SCM_BASE`; `None` leaves turbo on its `origin/<default_branch>` fallback,
    /// a safe superset.
    pub turbo_base: Option<String>,
    /// The remote-tracking ref to register: `origin/<default_branch>`, even when a base is set.
    pub origin_ref: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StackBaseError {
    #[error("merge_base is not a full hex commit id")]
    MergeBase,
    #[error("default branch name is not usable")]
    DefaultBranch,
}

fn sha_ok(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn branch_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with(['-', '/'])
        && !s.ends_with(['/', '.'])
        && !s.contains("..")
        && !s.contains("//")
        && s.bytes().all(|b| {
            b.is_ascii_graphic() && !matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

impl StackBase {
    pub fn resolve(pr: &PrBase) -> Result<Self, StackBaseError> {
        if !branch_ok(&pr.default_branch) {
            return Err(StackBaseError::DefaultBranch);
        }
        let turbo_base = match &pr.merge_base {
            Some(mb) if sha_ok(mb) => Some(mb.clone()),
            Some(_) => return Err(StackBaseError::MergeBase),
            None => None,
        };
        Ok(Self {
            turbo_base,
            origin_ref: format!("origin/{}", pr.default_branch),
        })
    }
}
