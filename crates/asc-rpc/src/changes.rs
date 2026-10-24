//! Conversions for `changes.proto` and the stacked-change additions to `repo.proto` (SPEC §20.5).
//!
//! Change ids are 16 bytes ([`ChangeId`]); changeset ids follow the request's id mode and are
//! decoded by length ([`ModeId`]). Every list and string is bounded, because the peer is untrusted.

use bytes::Bytes;
use asc_core::change::ChangeId;
use asc_core::id::{ChangesetId, GitOid, RefTarget};
use asc_core::path::RepoPath;

use crate::convert::{Conflict, ConvertError, ModeId, Result, WireId, opt_id, opt_id_to_wire};
use crate::pb;

/// Longest accepted repeated field in a change message.
pub const MAX_LIST: usize = 100_000;
/// Longest accepted free-text field (host name, URL, state, checks, actor, reason).
pub const MAX_TEXT: usize = 4096;

impl WireId for ChangeId {
    const LEN: usize = 16;
    const NAME: &'static str = "ChangeId";

    fn from_slice(b: &[u8]) -> Result<Self> {
        let a: [u8; 16] = b.try_into().map_err(|_| ConvertError::BadLength {
            what: Self::NAME,
            expected: Self::LEN,
            got: b.len(),
        })?;
        Ok(Self::from_bytes(a))
    }

    fn as_slice(&self) -> &[u8] {
        self.as_bytes()
    }
}

fn bounded(len: usize, what: &'static str) -> Result<()> {
    if len > MAX_LIST {
        Err(ConvertError::OutOfRange(what))
    } else {
        Ok(())
    }
}

fn text(s: &str, what: &'static str) -> Result<String> {
    if s.len() > MAX_TEXT {
        Err(ConvertError::OutOfRange(what))
    } else {
        Ok(s.to_owned())
    }
}

fn change_id(b: &[u8]) -> Result<ChangeId> {
    ChangeId::from_slice(b)
}

fn change_ids(v: &[Bytes], what: &'static str) -> Result<Vec<ChangeId>> {
    bounded(v.len(), what)?;
    v.iter().map(|b| change_id(b)).collect()
}

fn change_ids_to_wire(v: &[ChangeId]) -> Vec<Bytes> {
    v.iter()
        .map(|c| Bytes::copy_from_slice(c.as_slice()))
        .collect()
}

fn mode_ids(v: &[Bytes], what: &'static str) -> Result<Vec<ModeId<ChangesetId>>> {
    bounded(v.len(), what)?;
    v.iter().map(|b| ModeId::from_wire(b)).collect()
}

fn opt_mode_id(b: &[u8]) -> Result<Option<ModeId<ChangesetId>>> {
    if b.is_empty() {
        Ok(None)
    } else {
        ModeId::from_wire(b).map(Some)
    }
}

fn opt_mode_to_wire(id: Option<&ModeId<ChangesetId>>) -> Bytes {
    id.map_or_else(Bytes::new, ModeId::to_wire)
}

fn opt_change(b: &[u8]) -> Result<Option<ChangeId>> {
    opt_id(b)
}

fn paths(v: &[Bytes], what: &'static str) -> Result<Vec<RepoPath>> {
    bounded(v.len(), what)?;
    v.iter()
        .map(|b| RepoPath::new(&b[..]).map_err(ConvertError::from))
        .collect()
}

// ----------------------------------------------------------- ReviewState

/// The review host's summary for a change (`asc.v1.ReviewState`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewState {
    pub host: String,
    pub number: u64,
    pub url: String,
    pub state: String,
    pub draft: bool,
    pub approvals: u32,
    pub checks: String,
    pub queue: String,
    pub reviewed_version: u32,
    pub updated: i64,
}

impl TryFrom<&pb::ReviewState> for ReviewState {
    type Error = ConvertError;

    fn try_from(r: &pb::ReviewState) -> Result<Self> {
        Ok(Self {
            host: text(&r.host, "ReviewState.host")?,
            number: r.number,
            url: text(&r.url, "ReviewState.url")?,
            state: text(&r.state, "ReviewState.state")?,
            draft: r.draft,
            approvals: r.approvals,
            checks: text(&r.checks, "ReviewState.checks")?,
            queue: text(&r.queue, "ReviewState.queue")?,
            reviewed_version: r.reviewed_version,
            updated: r.updated,
        })
    }
}

impl From<&ReviewState> for pb::ReviewState {
    fn from(r: &ReviewState) -> Self {
        Self {
            host: r.host.clone(),
            number: r.number,
            url: r.url.clone(),
            state: r.state.clone(),
            draft: r.draft,
            approvals: r.approvals,
            checks: r.checks.clone(),
            queue: r.queue.clone(),
            reviewed_version: r.reviewed_version,
            updated: r.updated,
        }
    }
}

fn opt_review(r: Option<&pb::ReviewState>) -> Result<Option<ReviewState>> {
    r.map(ReviewState::try_from).transpose()
}

// -------------------------------------------------------------- DiffStat

/// Added and removed line counts of one path (`asc.v1.FileStat`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub path: RepoPath,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}

impl TryFrom<&pb::FileStat> for FileStat {
    type Error = ConvertError;

    fn try_from(s: &pb::FileStat) -> Result<Self> {
        Ok(Self {
            path: RepoPath::new(&s.path[..])?,
            added: s.added,
            removed: s.removed,
            binary: s.binary,
        })
    }
}

impl From<&FileStat> for pb::FileStat {
    fn from(s: &FileStat) -> Self {
        Self {
            path: Bytes::copy_from_slice(s.path.as_bytes()),
            added: s.added,
            removed: s.removed,
            binary: s.binary,
        }
    }
}

/// Per-file line counts of a changeset (`asc.v1.DiffStat`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub files: Vec<FileStat>,
}

impl TryFrom<&pb::DiffStat> for DiffStat {
    type Error = ConvertError;

    fn try_from(d: &pb::DiffStat) -> Result<Self> {
        bounded(d.files.len(), "DiffStat.files")?;
        Ok(Self {
            files: d
                .files
                .iter()
                .map(FileStat::try_from)
                .collect::<Result<_>>()?,
        })
    }
}

impl From<&DiffStat> for pb::DiffStat {
    fn from(d: &DiffStat) -> Self {
        Self {
            files: d.files.iter().map(pb::FileStat::from).collect(),
        }
    }
}

// ---------------------------------------------------------------- ChangeInfo

/// One change as the server sees it (`asc.v1.ChangeInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeInfo {
    pub change: ChangeId,
    pub head: ModeId<ChangesetId>,
    pub git: Option<GitOid>,
    pub version: u32,
    pub parent_change: Option<ChangeId>,
    pub parent_current: bool,
    /// The changeset that landed this change on a land branch, if any.
    pub landed_as: Option<ModeId<ChangesetId>>,
    pub review: Option<ReviewState>,
    /// Visible heads; more than one means the change is divergent.
    pub visible: Vec<ModeId<ChangesetId>>,
}

impl TryFrom<&pb::ChangeInfo> for ChangeInfo {
    type Error = ConvertError;

    fn try_from(c: &pb::ChangeInfo) -> Result<Self> {
        Ok(Self {
            change: change_id(&c.change)?,
            head: ModeId::from_wire(&c.head)?,
            git: opt_id(&c.git)?,
            version: c.version,
            parent_change: opt_change(&c.parent_change)?,
            parent_current: c.parent_current,
            landed_as: opt_mode_id(&c.landed_as)?,
            review: opt_review(c.review.as_ref())?,
            visible: mode_ids(&c.visible, "ChangeInfo.visible")?,
        })
    }
}

impl From<&ChangeInfo> for pb::ChangeInfo {
    fn from(c: &ChangeInfo) -> Self {
        Self {
            change: Bytes::copy_from_slice(c.change.as_slice()),
            head: c.head.to_wire(),
            git: opt_id_to_wire(c.git.as_ref()),
            version: c.version,
            parent_change: opt_id_to_wire(c.parent_change.as_ref()),
            parent_current: c.parent_current,
            landed_as: opt_mode_to_wire(c.landed_as.as_ref()),
            review: c.review.as_ref().map(pb::ReviewState::from),
            visible: c.visible.iter().map(ModeId::to_wire).collect(),
        }
    }
}

/// Decodes the changes of a `StackResponse` or `ResolveChangesResponse`.
pub fn change_infos(v: &[pb::ChangeInfo]) -> Result<Vec<ChangeInfo>> {
    bounded(v.len(), "ChangeInfo list")?;
    v.iter().map(ChangeInfo::try_from).collect()
}

// --------------------------------------------------------------- Version

/// One recorded version of a change (`asc.v1.Version`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub v: u32,
    pub cs: ModeId<ChangesetId>,
    pub git: Option<GitOid>,
    pub parent_change: Option<ChangeId>,
    pub at: i64,
    pub actor: String,
}

impl TryFrom<&pb::Version> for Version {
    type Error = ConvertError;

    fn try_from(v: &pb::Version) -> Result<Self> {
        Ok(Self {
            v: v.v,
            cs: ModeId::from_wire(&v.cs)?,
            git: opt_id(&v.git)?,
            parent_change: opt_change(&v.parent_change)?,
            at: v.at,
            actor: text(&v.actor, "Version.actor")?,
        })
    }
}

impl From<&Version> for pb::Version {
    fn from(v: &Version) -> Self {
        Self {
            v: v.v,
            cs: v.cs.to_wire(),
            git: opt_id_to_wire(v.git.as_ref()),
            parent_change: opt_id_to_wire(v.parent_change.as_ref()),
            at: v.at,
            actor: v.actor.clone(),
        }
    }
}

// ------------------------------------------------------------ Mergeable

/// How a change would merge into its target (`asc.v1.Mergeable.State`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MergeState {
    Disjoint,
    CleanMerge,
    Conflicts,
    NeedsRestack,
}

impl MergeState {
    fn from_wire(v: i32) -> Result<Self> {
        match pb::mergeable::State::try_from(v) {
            Ok(pb::mergeable::State::Disjoint) => Ok(Self::Disjoint),
            Ok(pb::mergeable::State::CleanMerge) => Ok(Self::CleanMerge),
            Ok(pb::mergeable::State::Conflicts) => Ok(Self::Conflicts),
            Ok(pb::mergeable::State::NeedsRestack) => Ok(Self::NeedsRestack),
            Err(_) => Err(ConvertError::BadEnum {
                what: "Mergeable.State",
                value: v,
            }),
        }
    }

    fn to_wire(self) -> i32 {
        (match self {
            Self::Disjoint => pb::mergeable::State::Disjoint,
            Self::CleanMerge => pb::mergeable::State::CleanMerge,
            Self::Conflicts => pb::mergeable::State::Conflicts,
            Self::NeedsRestack => pb::mergeable::State::NeedsRestack,
        }) as i32
    }
}

/// Mergeability of one change (`asc.v1.Mergeable`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mergeable {
    pub change: ChangeId,
    pub state: MergeState,
    pub conflicts: Vec<Conflict>,
}

impl TryFrom<&pb::Mergeable> for Mergeable {
    type Error = ConvertError;

    fn try_from(m: &pb::Mergeable) -> Result<Self> {
        bounded(m.conflicts.len(), "Mergeable.conflicts")?;
        Ok(Self {
            change: change_id(&m.change)?,
            state: MergeState::from_wire(m.state)?,
            conflicts: m
                .conflicts
                .iter()
                .map(Conflict::try_from)
                .collect::<Result<_>>()?,
        })
    }
}

impl From<&Mergeable> for pb::Mergeable {
    fn from(m: &Mergeable) -> Self {
        Self {
            change: Bytes::copy_from_slice(m.change.as_slice()),
            state: m.state.to_wire(),
            conflicts: m.conflicts.iter().map(pb::Conflict::from).collect(),
        }
    }
}

// --------------------------------------------------------------- Overlap

/// Another open change that touches the same paths (`asc.v1.Overlap`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    pub change: ChangeId,
    pub review: Option<ReviewState>,
    pub paths: Vec<RepoPath>,
}

impl TryFrom<&pb::Overlap> for Overlap {
    type Error = ConvertError;

    fn try_from(o: &pb::Overlap) -> Result<Self> {
        Ok(Self {
            change: change_id(&o.change)?,
            review: opt_review(o.review.as_ref())?,
            paths: paths(&o.paths, "Overlap.paths")?,
        })
    }
}

impl From<&Overlap> for pb::Overlap {
    fn from(o: &Overlap) -> Self {
        Self {
            change: Bytes::copy_from_slice(o.change.as_slice()),
            review: o.review.as_ref().map(pb::ReviewState::from),
            paths: o
                .paths
                .iter()
                .map(|p| Bytes::copy_from_slice(p.as_bytes()))
                .collect(),
        }
    }
}

// ---------------------------------------------------------- RestackItem

/// One change to restack, with the ref it moves (`asc.v1.RestackItem`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestackItem {
    pub change: ChangeId,
    pub update_ref: String,
    /// The ref's expected current target; `None` skips the compare-and-swap.
    pub expect: Option<RefTarget>,
}

impl TryFrom<&pb::RestackItem> for RestackItem {
    type Error = ConvertError;

    fn try_from(i: &pb::RestackItem) -> Result<Self> {
        Ok(Self {
            change: change_id(&i.change)?,
            update_ref: text(&i.update_ref, "RestackItem.update_ref")?,
            expect: i.expect.as_ref().map(RefTarget::try_from).transpose()?,
        })
    }
}

impl From<&RestackItem> for pb::RestackItem {
    fn from(i: &RestackItem) -> Self {
        Self {
            change: Bytes::copy_from_slice(i.change.as_slice()),
            update_ref: i.update_ref.clone(),
            expect: i.expect.as_ref().map(pb::RefTarget::from),
        }
    }
}

/// Decodes `RestackChangesRequest.items`.
pub fn restack_items(v: &[pb::RestackItem]) -> Result<Vec<RestackItem>> {
    bounded(v.len(), "RestackChangesRequest.items")?;
    v.iter().map(RestackItem::try_from).collect()
}

// ------------------------------------------------------ SetReviewState

/// Decoded `SetReviewStateRequest`: `changes[i]` has review state `states[i]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewUpdates {
    pub entries: Vec<(ChangeId, ReviewState)>,
}

impl TryFrom<&pb::SetReviewStateRequest> for ReviewUpdates {
    type Error = ConvertError;

    /// The two lists are parallel; a length mismatch is rejected.
    fn try_from(r: &pb::SetReviewStateRequest) -> Result<Self> {
        if r.changes.len() != r.states.len() {
            return Err(ConvertError::OutOfRange("SetReviewStateRequest.states"));
        }
        let changes = change_ids(&r.changes, "SetReviewStateRequest.changes")?;
        let entries = changes
            .into_iter()
            .zip(&r.states)
            .map(|(c, s)| Ok((c, ReviewState::try_from(s)?)))
            .collect::<Result<_>>()?;
        Ok(Self { entries })
    }
}

impl ReviewUpdates {
    /// The wire request body (without the repo).
    pub fn into_parts(self) -> (Vec<Bytes>, Vec<pb::ReviewState>) {
        let changes: Vec<ChangeId> = self.entries.iter().map(|(c, _)| *c).collect();
        let states = self.entries.iter().map(|(_, s)| pb::ReviewState::from(s));
        (change_ids_to_wire(&changes), states.collect())
    }
}

// --------------------------------------------------------- RequestMerge

/// The outcome of `RequestMerge` (`asc.v1.RequestMergeResponse.State`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MergeRequestState {
    Queued,
    Landed,
    Refused,
}

/// Decoded `RequestMergeResponse`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeOutcome {
    pub state: MergeRequestState,
    /// Changes still waiting below the requested one.
    pub pending: Vec<ChangeId>,
    pub reason: String,
}

impl TryFrom<&pb::RequestMergeResponse> for MergeOutcome {
    type Error = ConvertError;

    fn try_from(r: &pb::RequestMergeResponse) -> Result<Self> {
        use pb::request_merge_response::State;
        let state = match State::try_from(r.state) {
            Ok(State::Queued) => MergeRequestState::Queued,
            Ok(State::Landed) => MergeRequestState::Landed,
            Ok(State::Refused) => MergeRequestState::Refused,
            Err(_) => {
                return Err(ConvertError::BadEnum {
                    what: "RequestMergeResponse.State",
                    value: r.state,
                });
            }
        };
        Ok(Self {
            state,
            pending: change_ids(&r.pending, "RequestMergeResponse.pending")?,
            reason: text(&r.reason, "RequestMergeResponse.reason")?,
        })
    }
}

impl From<&MergeOutcome> for pb::RequestMergeResponse {
    fn from(o: &MergeOutcome) -> Self {
        use pb::request_merge_response::State;
        Self {
            state: (match o.state {
                MergeRequestState::Queued => State::Queued,
                MergeRequestState::Landed => State::Landed,
                MergeRequestState::Refused => State::Refused,
            }) as i32,
            pending: change_ids_to_wire(&o.pending),
            reason: o.reason.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::id::TagId;

    fn ch(n: u8) -> ChangeId {
        ChangeId::from_bytes([n; 16])
    }
    fn cs(n: u8) -> ModeId<ChangesetId> {
        ModeId::Asc(ChangesetId::from_bytes([n; 32]))
    }
    fn git(n: u8) -> GitOid {
        GitOid::from_bytes([n; 20])
    }
    fn review() -> ReviewState {
        ReviewState {
            host: "forgejo".into(),
            number: 7,
            url: "https://example.test/pulls/7".into(),
            state: "open".into(),
            draft: true,
            approvals: 2,
            checks: "passing".into(),
            queue: String::new(),
            reviewed_version: 3,
            updated: 1_700_000_000,
        }
    }

    #[test]
    fn change_id_wire_lengths() {
        assert_eq!(ChangeId::from_slice(ch(1).as_slice()), Ok(ch(1)));
        for len in [0usize, 15, 17, 32] {
            assert!(ChangeId::from_slice(&vec![0; len]).is_err(), "len {len}");
        }
    }

    #[test]
    fn review_state_round_trip_and_bound() {
        let r = review();
        let w = pb::ReviewState::from(&r);
        assert_eq!(ReviewState::try_from(&w), Ok(r));
        let long = pb::ReviewState {
            url: "x".repeat(MAX_TEXT + 1),
            ..w
        };
        assert_eq!(
            ReviewState::try_from(&long),
            Err(ConvertError::OutOfRange("ReviewState.url"))
        );
    }

    #[test]
    fn diff_stat_round_trip() {
        let d = DiffStat {
            files: vec![
                FileStat {
                    path: RepoPath::new("a/b.rs").unwrap(),
                    added: 3,
                    removed: 1,
                    binary: false,
                },
                FileStat {
                    path: RepoPath::new("img.png").unwrap(),
                    added: 0,
                    removed: 0,
                    binary: true,
                },
            ],
        };
        assert_eq!(DiffStat::try_from(&pb::DiffStat::from(&d)), Ok(d));
        let bad = pb::DiffStat {
            files: vec![pb::FileStat {
                path: Bytes::from_static(b"../x"),
                ..Default::default()
            }],
        };
        assert!(DiffStat::try_from(&bad).is_err());
    }

    fn info() -> ChangeInfo {
        ChangeInfo {
            change: ch(1),
            head: cs(2),
            git: Some(git(3)),
            version: 4,
            parent_change: Some(ch(5)),
            parent_current: true,
            landed_as: Some(cs(6)),
            review: Some(review()),
            visible: vec![cs(2), ModeId::Git(git(9))],
        }
    }

    #[test]
    fn change_info_round_trip() {
        let full = info();
        let minimal = ChangeInfo {
            git: None,
            parent_change: None,
            landed_as: None,
            review: None,
            visible: vec![],
            ..info()
        };
        for c in [full, minimal] {
            assert_eq!(ChangeInfo::try_from(&pb::ChangeInfo::from(&c)), Ok(c));
        }
    }

    #[test]
    fn change_info_rejects_bad_ids() {
        let ok = pb::ChangeInfo::from(&info());
        for bad in [
            pb::ChangeInfo {
                change: Bytes::from_static(&[1; 32]),
                ..ok.clone()
            },
            pb::ChangeInfo {
                head: Bytes::new(),
                ..ok.clone()
            },
            pb::ChangeInfo {
                git: Bytes::from_static(&[1; 32]),
                ..ok.clone()
            },
            pb::ChangeInfo {
                parent_change: Bytes::from_static(&[1; 3]),
                ..ok.clone()
            },
            pb::ChangeInfo {
                visible: vec![Bytes::from_static(&[1; 7])],
                ..ok.clone()
            },
        ] {
            assert!(ChangeInfo::try_from(&bad).is_err());
        }
        let many = pb::ChangeInfo {
            visible: vec![ok.head.clone(); MAX_LIST + 1],
            ..ok
        };
        assert_eq!(
            ChangeInfo::try_from(&many),
            Err(ConvertError::OutOfRange("ChangeInfo.visible"))
        );
    }

    #[test]
    fn version_round_trip() {
        let v = Version {
            v: 2,
            cs: cs(1),
            git: Some(git(2)),
            parent_change: None,
            at: 5,
            actor: "ada".into(),
        };
        assert_eq!(Version::try_from(&pb::Version::from(&v)), Ok(v));
        let bad = pb::Version {
            cs: Bytes::from_static(&[1; 5]),
            ..Default::default()
        };
        assert!(Version::try_from(&bad).is_err());
    }

    #[test]
    fn mergeable_round_trip_and_bad_state() {
        let conflict = Conflict {
            changeset: cs(1),
            path: RepoPath::new("a").unwrap(),
            kind: crate::convert::ConflictKind::Content,
            base: None,
            ours: None,
            theirs: None,
            marked: None,
        };
        for state in [
            MergeState::Disjoint,
            MergeState::CleanMerge,
            MergeState::Conflicts,
            MergeState::NeedsRestack,
        ] {
            let m = Mergeable {
                change: ch(1),
                state,
                conflicts: vec![conflict.clone()],
            };
            assert_eq!(Mergeable::try_from(&pb::Mergeable::from(&m)), Ok(m));
        }
        let bad = pb::Mergeable {
            change: Bytes::from_static(&[1; 16]),
            state: 9,
            conflicts: vec![],
        };
        assert!(matches!(
            Mergeable::try_from(&bad),
            Err(ConvertError::BadEnum { value: 9, .. })
        ));
    }

    #[test]
    fn overlap_round_trip_and_bad_path() {
        let o = Overlap {
            change: ch(1),
            review: Some(review()),
            paths: vec![RepoPath::new("x/y").unwrap()],
        };
        assert_eq!(Overlap::try_from(&pb::Overlap::from(&o)), Ok(o));
        let bad = pb::Overlap {
            change: Bytes::from_static(&[1; 16]),
            review: None,
            paths: vec![Bytes::from_static(b"a//b")],
        };
        assert!(Overlap::try_from(&bad).is_err());
    }

    #[test]
    fn restack_item_round_trip() {
        for expect in [
            None,
            Some(RefTarget::Changeset(ChangesetId::from_bytes([1; 32]))),
            Some(RefTarget::Tag(TagId::from_bytes([2; 32]))),
        ] {
            let i = RestackItem {
                change: ch(3),
                update_ref: "refs/heads/feature".into(),
                expect,
            };
            assert_eq!(RestackItem::try_from(&pb::RestackItem::from(&i)), Ok(i));
        }
        let no_arm = pb::RestackItem {
            change: Bytes::from_static(&[1; 16]),
            update_ref: String::new(),
            expect: Some(pb::RefTarget { t: None }),
        };
        assert!(RestackItem::try_from(&no_arm).is_err());
    }

    #[test]
    fn review_updates_are_parallel() {
        let req = pb::SetReviewStateRequest {
            repo: None,
            changes: vec![Bytes::from_static(&[1; 16]), Bytes::from_static(&[2; 16])],
            states: vec![pb::ReviewState::from(&review()); 2],
        };
        let u = ReviewUpdates::try_from(&req).unwrap();
        assert_eq!(u.entries.len(), 2);
        assert_eq!(u.entries[1].0, ch(2));
        let (changes, states) = u.into_parts();
        assert_eq!((changes, states), (req.changes.clone(), req.states.clone()));
        let short = pb::SetReviewStateRequest {
            states: vec![],
            ..req
        };
        assert!(ReviewUpdates::try_from(&short).is_err());
    }

    #[test]
    fn merge_outcome_round_trip() {
        for state in [
            MergeRequestState::Queued,
            MergeRequestState::Landed,
            MergeRequestState::Refused,
        ] {
            let o = MergeOutcome {
                state,
                pending: vec![ch(1), ch(2)],
                reason: "parent unmerged".into(),
            };
            assert_eq!(
                MergeOutcome::try_from(&pb::RequestMergeResponse::from(&o)),
                Ok(o)
            );
        }
        let bad = pb::RequestMergeResponse {
            state: 7,
            ..Default::default()
        };
        assert!(MergeOutcome::try_from(&bad).is_err());
    }

    #[test]
    fn repo_proto_amendment_is_wired() {
        // Fields added to repo.proto messages by C17 exist and default to empty.
        let e = pb::LogEntry::default();
        assert!(e.change.is_empty() && e.meta.is_empty() && e.stats.is_none());
        let r = pb::LogResponse::default();
        assert!(!r.truncated && r.resume_token.is_empty());
        assert!(!pb::UpdateRefsRequest::default().no_content_merge);
        assert!(pb::BlameResponse::default().reviews.is_empty());
        assert_eq!(pb::ObjKind::ChangeMeta as i32, 9);
    }
}
