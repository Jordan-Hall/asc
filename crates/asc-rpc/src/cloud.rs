//! Conversions for `cloud.proto` (SPEC §21.3.5).
//!
//! A draft head is named by changeset id. The change id is response only: [`CloudHead::from_request`]
//! drops whatever the client sent. Labels are untrusted display text, so [`Label::new`] enforces the
//! charset and the count and length bounds on every path in.

use asc_core::change::ChangeId;
use asc_core::id::ChangesetId;
use bytes::Bytes;

use crate::changes::{MAX_LIST, MAX_TEXT};
use crate::convert::{ConvertError, IdMode, ModeId, Result, opt_id, opt_id_to_wire};
use crate::errors::{ErrorInfo, RpcError};
use crate::pb;

/// Most labels on one head.
pub const MAX_LABELS: usize = 8;
/// Longest label, in bytes.
pub const MAX_LABEL_LEN: usize = 64;

/// A validated head label: 1 to 64 bytes of `[A-Za-z0-9._/-]`. Display text only; never a ref.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Label(String);

impl Label {
    /// Validates `s`.
    pub fn new(s: &str) -> Result<Self> {
        let ok = !s.is_empty()
            && s.len() <= MAX_LABEL_LEN
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'));
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(ConvertError::OutOfRange("label"))
        }
    }

    /// The label text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validates a label list: at most [`MAX_LABELS`], each a valid [`Label`].
pub fn labels(v: &[String]) -> Result<Vec<Label>> {
    if v.len() > MAX_LABELS {
        return Err(ConvertError::OutOfRange("labels"));
    }
    v.iter().map(|s| Label::new(s)).collect()
}

fn bounded(len: usize, what: &'static str) -> Result<()> {
    if len > MAX_LIST {
        Err(ConvertError::OutOfRange(what))
    } else {
        Ok(())
    }
}

fn changesets(v: &[Bytes], mode: IdMode, what: &'static str) -> Result<Vec<ModeId<ChangesetId>>> {
    bounded(v.len(), what)?;
    v.iter().map(|b| ModeId::from_wire_in(b, mode)).collect()
}

fn to_wire(v: &[ModeId<ChangesetId>]) -> Vec<Bytes> {
    v.iter().map(ModeId::to_wire).collect()
}

/// One draft head of a cloud workspace (`asc.v1.CloudHead`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudHead {
    pub cs: ModeId<ChangesetId>,
    /// Derived by the server from the `m/` object of `cs`; `None` in requests.
    pub change: Option<ChangeId>,
    pub labels: Vec<Label>,
    pub added: i64,
    pub hidden_at: i64,
}

impl CloudHead {
    /// Decodes a head sent by a client: the `change` field is ignored (SPEC §21.3.7).
    pub fn from_request(p: &pb::CloudHead, mode: IdMode) -> Result<Self> {
        Ok(Self {
            cs: ModeId::from_wire_in(&p.cs, mode)?,
            change: None,
            labels: labels(&p.labels)?,
            added: p.added,
            hidden_at: p.hidden_at,
        })
    }

    /// Decodes a head received from a server, keeping its `change`.
    pub fn from_response(p: &pb::CloudHead, mode: IdMode) -> Result<Self> {
        let mut h = Self::from_request(p, mode)?;
        h.change = opt_id(&p.change)?;
        Ok(h)
    }
}

impl From<&CloudHead> for pb::CloudHead {
    fn from(h: &CloudHead) -> Self {
        Self {
            cs: h.cs.to_wire(),
            change: opt_id_to_wire(h.change.as_ref()),
            labels: h.labels.iter().map(|l| l.0.clone()).collect(),
            added: h.added,
            hidden_at: h.hidden_at,
        }
    }
}

/// A decoded `UpdateWorkspaceRequest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateWorkspace {
    pub name: String,
    pub base_version: u64,
    pub add: Vec<CloudHead>,
    pub remove: Vec<ModeId<ChangesetId>>,
    pub hide: Vec<ModeId<ChangesetId>>,
    pub unhide: Vec<ModeId<ChangesetId>>,
}

impl UpdateWorkspace {
    /// Decodes and bounds a request; the id mode comes from `repo.id_mode`.
    pub fn from_request(p: &pb::UpdateWorkspaceRequest) -> Result<Self> {
        let mode = IdMode::from_wire(
            p.repo
                .as_ref()
                .ok_or(ConvertError::Missing("repo"))?
                .id_mode,
        )?;
        if p.name.len() > MAX_TEXT {
            return Err(ConvertError::OutOfRange("name"));
        }
        bounded(p.add.len(), "add")?;
        Ok(Self {
            name: p.name.clone(),
            base_version: p.base_version,
            add: p
                .add
                .iter()
                .map(|h| CloudHead::from_request(h, mode))
                .collect::<Result<_>>()?,
            remove: changesets(&p.remove, mode, "remove")?,
            hide: changesets(&p.hide, mode, "hide")?,
            unhide: changesets(&p.unhide, mode, "unhide")?,
        })
    }

    /// Encodes the request for `repo`.
    pub fn to_request(&self, repo: pb::RepoRef) -> pb::UpdateWorkspaceRequest {
        pb::UpdateWorkspaceRequest {
            repo: Some(repo),
            name: self.name.clone(),
            base_version: self.base_version,
            add: self.add.iter().map(pb::CloudHead::from).collect(),
            remove: to_wire(&self.remove),
            hide: to_wire(&self.hide),
            unhide: to_wire(&self.unhide),
        }
    }
}

/// A decoded `GetWorkspaceResponse`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub version: u64,
    pub heads: Vec<CloudHead>,
    pub removed: Vec<ModeId<ChangesetId>>,
    /// Heads older than `max_age_s` that were left out.
    pub omitted: u32,
    pub next_page_token: String,
}

impl Workspace {
    /// Decodes and bounds a response received under `mode`.
    pub fn from_response(p: &pb::GetWorkspaceResponse, mode: IdMode) -> Result<Self> {
        bounded(p.heads.len(), "heads")?;
        if p.next_page_token.len() > MAX_TEXT {
            return Err(ConvertError::OutOfRange("next_page_token"));
        }
        Ok(Self {
            version: p.version,
            heads: p
                .heads
                .iter()
                .map(|h| CloudHead::from_response(h, mode))
                .collect::<Result<_>>()?,
            removed: changesets(&p.removed, mode, "removed")?,
            omitted: p.omitted,
            next_page_token: p.next_page_token.clone(),
        })
    }
}

/// The `VERSION_CONFLICT` error carrying the workspace's current version in `actual`.
pub fn version_conflict(current: u64) -> RpcError {
    RpcError::VersionConflict(ErrorInfo {
        actual: Bytes::copy_from_slice(&current.to_be_bytes()),
        ..Default::default()
    })
}

/// The current version carried by a `VERSION_CONFLICT`; `None` for any other error or a bad payload.
pub fn conflict_version(e: &RpcError) -> Option<u64> {
    match e {
        RpcError::VersionConflict(i) => <[u8; 8]>::try_from(&i.actual[..])
            .ok()
            .map(u64::from_be_bytes),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(n: u8) -> Bytes {
        Bytes::from(vec![n; 32])
    }

    fn head(n: u8) -> pb::CloudHead {
        pb::CloudHead {
            cs: cs(n),
            change: Bytes::from(vec![n; 16]),
            labels: vec!["feat/a-b.c_d".into()],
            added: 5,
            hidden_at: 0,
        }
    }

    #[test]
    fn label_charset_and_bounds() {
        assert!(Label::new("a").is_ok());
        assert!(Label::new(&"a".repeat(64)).is_ok());
        for bad in ["", "a b", "a\n", "é", "a:b", "a\\b"] {
            assert!(Label::new(bad).is_err(), "{bad:?}");
        }
        assert!(Label::new(&"a".repeat(65)).is_err());
    }

    #[test]
    fn label_count_is_bounded() {
        let ok: Vec<String> = (0..8).map(|i| format!("l{i}")).collect();
        assert_eq!(labels(&ok).unwrap().len(), 8);
        let too_many: Vec<String> = (0..9).map(|i| format!("l{i}")).collect();
        assert!(labels(&too_many).is_err());
    }

    #[test]
    fn request_ignores_client_change_but_response_keeps_it() {
        let req = CloudHead::from_request(&head(1), IdMode::Asc).unwrap();
        assert_eq!(req.change, None);
        let resp = CloudHead::from_response(&head(1), IdMode::Asc).unwrap();
        assert_eq!(resp.change, Some(ChangeId::from_bytes([1; 16])));
        assert_eq!(pb::CloudHead::from(&resp), head(1));
    }

    #[test]
    fn bad_head_is_rejected() {
        let mut h = head(1);
        h.labels = vec!["bad label".into()];
        assert!(CloudHead::from_request(&h, IdMode::Asc).is_err());
        let mut h = head(1);
        h.cs = Bytes::from_static(&[1; 5]);
        assert!(CloudHead::from_request(&h, IdMode::Asc).is_err());
        let mut h = head(1);
        h.change = Bytes::from_static(&[1; 5]);
        assert!(CloudHead::from_response(&h, IdMode::Asc).is_err());
        // a git-length id under asc mode
        let mut h = head(1);
        h.cs = Bytes::from_static(&[1; 20]);
        assert!(CloudHead::from_request(&h, IdMode::Asc).is_err());
    }

    #[test]
    fn update_round_trip_and_repo_required() {
        let repo = pb::RepoRef {
            name: "o/r".into(),
            id_mode: IdMode::Asc.to_wire(),
        };
        let req = pb::UpdateWorkspaceRequest {
            repo: Some(repo.clone()),
            name: "laptop".into(),
            base_version: 7,
            add: vec![head(1)],
            remove: vec![cs(2)],
            hide: vec![cs(3)],
            unhide: vec![cs(4)],
        };
        let u = UpdateWorkspace::from_request(&req).unwrap();
        assert_eq!(u.add[0].change, None);
        assert_eq!(u.base_version, 7);
        let back = u.to_request(repo);
        assert_eq!(back.add[0].change, Bytes::new());
        assert_eq!(back.remove, req.remove);
        let no_repo = pb::UpdateWorkspaceRequest { repo: None, ..req };
        assert!(UpdateWorkspace::from_request(&no_repo).is_err());
    }

    #[test]
    fn workspace_response_decodes() {
        let r = pb::GetWorkspaceResponse {
            version: 3,
            heads: vec![head(1), head(2)],
            removed: vec![cs(9)],
            omitted: 4,
            next_page_token: "t".into(),
        };
        let w = Workspace::from_response(&r, IdMode::Asc).unwrap();
        assert_eq!(
            (w.version, w.heads.len(), w.removed.len(), w.omitted),
            (3, 2, 1, 4)
        );
    }

    #[test]
    fn version_conflict_carries_current_version() {
        let e = version_conflict(0x0102_0304_0506_0708);
        assert_eq!(e.code(), "VERSION_CONFLICT");
        assert_eq!(conflict_version(&e), Some(0x0102_0304_0506_0708));
        let bad = RpcError::VersionConflict(ErrorInfo::default());
        assert_eq!(conflict_version(&bad), None);
        assert_eq!(
            conflict_version(&RpcError::QuotaExceeded(ErrorInfo::default())),
            None
        );
    }

    #[test]
    fn service_is_registered() {
        let _ = crate::services::cloud_client::CloudClient::<tonic::transport::Channel>::new;
        let d = prost_reflect::DescriptorPool::decode(crate::DESCRIPTOR_SET).unwrap();
        let s = d.get_service_by_name("asc.v1.Cloud").unwrap();
        assert_eq!(s.methods().count(), 4);
    }
}
