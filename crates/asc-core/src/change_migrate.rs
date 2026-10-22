//! Migration-mode change-id tier (SPEC §21.6.2, PLAN B18a).
//!
//! Order: a jj `change-id` header, then a Gerrit `Change-Id` trailer (both through the built
//! `ChangeId` constructors), then an id derived from the changeset id alone. Pure and
//! deterministic, so client and server agree with no coordination.

use crate::change::ChangeId;
use crate::id::{ChangesetId, derive};

/// Derive-key context of the import tier; its own domain, apart from the Gerrit one.
pub const CONTEXT_IMPORT: &str = "asc v1 change import";

/// Id derived from the changeset id: `blake3_derive("asc v1 change import", cs)[..16]`.
pub fn import_id(commit: &ChangesetId) -> ChangeId {
    let h = derive(CONTEXT_IMPORT, commit.as_bytes());
    ChangeId::from_bytes(h[..16].try_into().expect("16 of 32 bytes"))
}

/// Picks the change id of a commit without a `ChangeMeta`.
///
/// A malformed jj header or an empty Gerrit value is untrusted input and is skipped, so the
/// next tier applies. The last tier always yields an id, so this returns `Some` today; the
/// `Option` leaves room for a stricter policy without changing callers.
pub fn migration_id(
    commit: &ChangesetId,
    jj_header: Option<&[u8]>,
    gerrit: Option<&[u8]>,
    scope: &[u8],
) -> Option<ChangeId> {
    if let Some(id) = jj_header.and_then(|v| ChangeId::from_jj_header(v).ok()) {
        return Some(id);
    }
    if let Some(v) = gerrit.filter(|v| !v.is_empty()) {
        return Some(ChangeId::from_gerrit(scope, v));
    }
    Some(import_id(commit))
}
