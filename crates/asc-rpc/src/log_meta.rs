//! `LogEntry` to [`LogMeta`] conversions (PLAN C17x).
//!
//! asc-engine's sync hook takes the engine-native [`LogMeta`]; the proto `LogEntry` stays in this
//! crate. The wire ids are untrusted, so the forward direction is fallible and checks every
//! length. Empty `change` or `meta` bytes mean absent.

use bytes::Bytes;
use asc_core::change::{ChangeId, MetaId};
use asc_core::id::ChangesetId;
use asc_engine::sync::changes::LogMeta;

use crate::convert::{ConvertError, opt_id};
use crate::pb;

fn fixed<const N: usize>(what: &'static str, b: &[u8]) -> Result<Option<[u8; N]>, ConvertError> {
    if b.is_empty() {
        return Ok(None);
    }
    b.try_into().map(Some).map_err(|_| ConvertError::BadLength {
        what,
        expected: N,
        got: b.len(),
    })
}

impl TryFrom<&pb::LogEntry> for LogMeta {
    type Error = ConvertError;

    fn try_from(e: &pb::LogEntry) -> Result<Self, Self::Error> {
        let id = opt_id::<ChangesetId>(&e.id)?.ok_or(ConvertError::Missing("LogEntry.id"))?;
        Ok(Self {
            id,
            change: fixed::<16>("ChangeId", &e.change)?.map(ChangeId::from_bytes),
            meta: fixed::<32>("MetaId", &e.meta)?.map(MetaId::from_bytes),
        })
    }
}

/// Sets the `change` and `meta` fields of `entry` from `m`; absent ids become empty bytes.
/// Every other field, `id` included, is left as it was.
pub fn set_change_meta(entry: &mut pb::LogEntry, m: &LogMeta) {
    entry.change = m
        .change
        .map_or_else(Bytes::new, |c| Bytes::copy_from_slice(c.as_bytes()));
    entry.meta = m
        .meta
        .map_or_else(Bytes::new, |c| Bytes::copy_from_slice(c.as_bytes()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(change: &[u8], meta: &[u8]) -> pb::LogEntry {
        pb::LogEntry {
            id: Bytes::copy_from_slice(&[7; 32]),
            change: Bytes::copy_from_slice(change),
            meta: Bytes::copy_from_slice(meta),
            ..pb::LogEntry::default()
        }
    }

    fn roundtrip(e: &pb::LogEntry) -> pb::LogEntry {
        let m = LogMeta::try_from(e).unwrap();
        let mut out = pb::LogEntry {
            id: e.id.clone(),
            message: Bytes::from_static(b"keep"),
            ..pb::LogEntry::default()
        };
        set_change_meta(&mut out, &m);
        out
    }

    #[test]
    fn log_meta_roundtrips_with_and_without_fields() {
        for (c, m) in [
            (&[1u8; 16][..], &[2u8; 32][..]),
            (&[], &[]),
            (&[1; 16], &[]),
            (&[], &[2; 32]),
        ] {
            let e = entry(c, m);
            let out = roundtrip(&e);
            assert_eq!(out.change, e.change);
            assert_eq!(out.meta, e.meta);
            assert_eq!(out.message, Bytes::from_static(b"keep"));
        }
        let m = LogMeta::try_from(&entry(&[], &[])).unwrap();
        assert_eq!((m.change, m.meta), (None, None));
    }

    #[test]
    fn log_meta_rejects_bad_lengths() {
        assert!(matches!(
            LogMeta::try_from(&entry(&[1; 15], &[])),
            Err(ConvertError::BadLength {
                expected: 16,
                got: 15,
                ..
            })
        ));
        assert!(matches!(
            LogMeta::try_from(&entry(&[], &[2; 33])),
            Err(ConvertError::BadLength {
                expected: 32,
                got: 33,
                ..
            })
        ));
        let mut e = entry(&[], &[]);
        e.id = Bytes::new();
        assert!(matches!(
            LogMeta::try_from(&e),
            Err(ConvertError::Missing(_))
        ));
        e.id = Bytes::from_static(&[1; 20]);
        assert!(LogMeta::try_from(&e).is_err());
    }
}
