//! Change identity for imported commits (SPEC §20.2 git-import row, unit C45).
//!
//! A commit's change id is, in order: the jj `change-id` header (decoded); a Gerrit
//! `Change-Id:` trailer, scoped by the first import ref that reached the commit; or none.
//! The result is a `ChangeMeta` with `op = Import` and no predecessors. It is a sidecar, so
//! the git commit and its SHA-1 are untouched. All input is untrusted: a malformed header or
//! trailer is ignored, never an import failure.

use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
use asc_core::codec::Enveloped;
use asc_core::id::ChangesetId;
use asc_engine::change::{self, ChangeRow, ChangeRows};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};

/// Longest Gerrit `Change-Id` value that is considered.
const MAX_TRAILER_VALUE: usize = 128;

/// Why the meta could not be written.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] objects::Error),
    #[error(transparent)]
    Index(#[from] change::Error),
}

/// The change id a raw commit carries, if any. `scope` is the name of the first import ref
/// (in sorted ref order) that reached the commit.
pub fn change_id_of(raw: &[u8], scope: &[u8]) -> Option<ChangeId> {
    let (headers, message) = match find_blank_line(raw) {
        Some(at) => (&raw[..at], &raw[at + 2..]),
        None => (raw, &raw[raw.len()..]),
    };
    jj_header(headers).or_else(|| gerrit_trailer(message).map(|v| ChangeId::from_gerrit(scope, v)))
}

fn find_blank_line(raw: &[u8]) -> Option<usize> {
    raw.windows(2).position(|w| w == b"\n\n")
}

/// The first well-formed `change-id` header. Continuation lines belong to another header.
fn jj_header(headers: &[u8]) -> Option<ChangeId> {
    headers
        .split(|&c| c == b'\n')
        .filter_map(|line| line.strip_prefix(b"change-id "))
        .find_map(|v| ChangeId::from_jj_header(v).ok())
}

/// The value of the last `Change-Id:` line in the message's last paragraph. The subject
/// paragraph alone is never a footer.
fn gerrit_trailer(message: &[u8]) -> Option<&[u8]> {
    let end = message
        .iter()
        .rposition(|c| !matches!(c, b'\n' | b'\r'))
        .map_or(0, |i| i + 1);
    let message = &message[..end];
    let start = message.windows(2).rposition(|w| w == b"\n\n")? + 2;
    message[start..]
        .split(|&c| c == b'\n')
        .rev()
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let (key, value) = line.split_at(line.iter().position(|&c| c == b':')?);
            if !key.eq_ignore_ascii_case(b"Change-Id") {
                return None;
            }
            let value = value[1..].trim_ascii();
            let ok = !value.is_empty()
                && value.len() <= MAX_TRAILER_VALUE
                && value.iter().all(|c| c.is_ascii_graphic());
            ok.then_some(value)
        })
}

/// Writes the `ChangeMeta` of an imported commit, if it has a change id, and indexes it in
/// `rows` when given. Idempotent: the meta is a pure function of the commit and scope.
pub async fn write_meta<O: Objects>(
    objects: &O,
    rows: Option<&dyn ChangeRows>,
    cs: ChangesetId,
    raw: &[u8],
    scope: &[u8],
) -> Result<Option<ChangeMeta>, Error> {
    let Some(change) = change_id_of(raw, scope) else {
        return Ok(None);
    };
    let meta = ChangeMeta::new(cs, change, Vec::new(), RewriteOp::Import);
    let envelope = meta.to_envelope();
    let id = meta.id();
    objects
        .put(&ObjKey::ChangeMeta(id), encode_stored(&envelope))
        .await?;
    if let Some(rows) = rows {
        let row = ChangeRow {
            cs,
            change,
            meta: id,
            op: RewriteOp::Import,
        };
        rows.insert(&row, &[]).await?;
    }
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JJ: &str = "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk";

    #[test]
    fn jj_header_wins_over_trailer() {
        let raw = format!("tree x\nchange-id {JJ}\n\nsubject\n\nbody\n\nChange-Id: I1\n");
        assert_eq!(
            change_id_of(raw.as_bytes(), b"r"),
            Some(JJ.parse().unwrap())
        );
    }

    #[test]
    fn bad_jj_header_falls_back() {
        let raw = b"tree x\nchange-id nope\n\ns\n\nChange-Id: Iab\n";
        assert_eq!(
            change_id_of(raw, b"r"),
            Some(ChangeId::from_gerrit(b"r", b"Iab"))
        );
    }

    #[test]
    fn trailer_needs_a_footer_paragraph() {
        assert_eq!(change_id_of(b"tree x\n\nChange-Id: Iab\n", b"r"), None);
        assert!(change_id_of(b"tree x\n\ns\n\nChange-Id: Iab\n\n", b"r").is_some());
    }

    #[test]
    fn last_trailer_wins_and_key_is_case_insensitive() {
        let raw = b"tree x\n\ns\n\nchange-id: Ia\nChange-Id: Ib\n";
        assert_eq!(
            change_id_of(raw, b"r"),
            Some(ChangeId::from_gerrit(b"r", b"Ib"))
        );
    }

    #[test]
    fn oversized_or_spaced_values_are_ignored() {
        let long = format!("tree x\n\ns\n\nChange-Id: {}\n", "a".repeat(200));
        assert_eq!(change_id_of(long.as_bytes(), b"r"), None);
        assert_eq!(change_id_of(b"tree x\n\ns\n\nChange-Id: a b\n", b"r"), None);
    }

    #[test]
    fn scope_changes_the_gerrit_id() {
        let raw = b"tree x\n\ns\n\nChange-Id: Iab\n";
        assert_ne!(change_id_of(raw, b"a"), change_id_of(raw, b"b"));
    }
}
