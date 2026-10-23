//! Reading refs for `log`: tips by prefix, with annotated tags peeled to their commit.

use asc_core::codec::decode_envelope;
use asc_core::id::{ChangesetId, RefTarget, TagId};
use asc_core::object::TagRecord;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use crate::cli::common::{CliError, Result, WsCtx};

/// One ref and the commit it leads to.
#[derive(Clone, Debug)]
pub struct RefTip {
    /// The full name, `refs/tags/v1`.
    pub name: String,
    pub commit: ChangesetId,
    /// The tag object, for an annotated tag.
    pub tag: Option<TagId>,
}

/// The tag record `id` names.
pub async fn tag_record(ws: &WsCtx, id: TagId) -> Result<TagRecord> {
    let bytes = ws
        .objects
        .get(&ObjKey::Tag(id))
        .await?
        .ok_or_else(|| CliError::Fatal(format!("tag object {id} is missing")))?;
    decode_envelope(&bytes).map_err(|e| CliError::Fatal(format!("tag object {id} is corrupt: {e}")))
}

/// The commit `target` leads to, and the tag object on the way.
pub async fn peel(ws: &WsCtx, target: RefTarget) -> Result<(ChangesetId, Option<TagId>)> {
    match target {
        RefTarget::Changeset(c) => Ok((c, None)),
        RefTarget::Tag(t) => Ok((tag_record(ws, t).await?.target, Some(t))),
    }
}

/// Every ref under `prefix` (`refs/` for all), by name.
pub async fn tips(ws: &WsCtx, prefix: &str) -> Result<Vec<RefTip>> {
    let entries = ws.db.refs().list(ws.repo.key, prefix).await?;
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
        let (commit, tag) = peel(ws, e.target).await?;
        out.push(RefTip { name, commit, tag });
    }
    Ok(out)
}
