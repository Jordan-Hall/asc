//! Change metas a client uploaded before the push that names their changesets (SPEC section 20.2).
//!
//! A client sends the `m/` objects of the changes it publishes with the rest of its closure, then
//! a `UpdateRefs` (or `Ingest`) that names only changesets. A meta does not name the changeset it
//! belongs to by its key, so the ingest could not find the metas of its push, and no change was
//! ever indexed for a pushed changeset: the topic `refs/for/<branch>/asc-<change>` a client
//! derived from its own change id then named no version of any change.
//!
//! `PutObjects` therefore records, for the uploading principal, which changeset each uploaded meta
//! claims ([`stage`]). The ingest of the same principal's push reads the rows of the changesets it
//! adds ([`take`]) and puts those metas in its closure, where the change index validates them as
//! for any push (the changeset is new in this push, the fields are in bounds, the first meta
//! wins) and the ingest deletes the rows in its transaction ([`consume`]). A row of another
//! principal, or of a changeset that is not part of the push, is never used, so an upload cannot
//! claim a change id for a changeset someone else pushes.

use asc_core::change::{ChangeMeta, MetaId};
use asc_core::codec::decode_envelope;
use asc_core::id::ChangesetId;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use sqlx::any::Any;
use sqlx::{AnyPool, Row, Transaction};

use super::placeholders;
use crate::registry::{Ns, RepoEntry};

/// Staged rows older than this are removed whenever a row is staged.
const KEEP_SECS: i64 = 24 * 60 * 60;
/// Most metas one call stages (a `PutObjects` batch is bounded well below this).
const MAX_STAGED: usize = 4096;
/// Ids per statement.
const BATCH: usize = 200;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Records, for `principal`, the changeset each of the just uploaded `metas` claims. A meta that
/// cannot be read back or decoded is left out: the ingest then finds no meta for its changeset and
/// the push says so. Errors are the database's.
pub async fn stage(
    pool: &AnyPool,
    entry: &RepoEntry,
    principal: &str,
    metas: &[MetaId],
) -> Result<(), sqlx::Error> {
    if metas.is_empty() {
        return Ok(());
    }
    let at = now();
    sqlx::query("DELETE FROM staged_metas WHERE staged_at < $1")
        .bind(at - KEEP_SECS)
        .execute(pool)
        .await?;
    for id in metas.iter().take(MAX_STAGED) {
        let Ok(Some(data)) = entry.objects.get(&ObjKey::ChangeMeta(*id)).await else {
            continue;
        };
        let Ok(meta) = decode_envelope::<ChangeMeta>(&data) else {
            continue;
        };
        sqlx::query(
            "INSERT INTO staged_metas (ns, principal, cs, meta, staged_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (ns, principal, cs, meta) DO NOTHING",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(principal)
        .bind(meta.cs.as_bytes().to_vec())
        .bind(id.as_bytes().to_vec())
        .bind(at)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// The metas `principal` staged for the changesets `new`.
pub async fn take(
    pool: &AnyPool,
    ns: Ns,
    principal: &str,
    new: &[ChangesetId],
) -> Result<Vec<MetaId>, sqlx::Error> {
    let mut out = Vec::new();
    for chunk in new.chunks(BATCH) {
        let sql = format!(
            "SELECT meta FROM staged_metas WHERE ns = $1 AND principal = $2 AND cs IN ({}) \
             ORDER BY staged_at, meta",
            placeholders(3, chunk.len())
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ns.as_bytes().to_vec())
            .bind(principal.to_owned());
        for c in chunk {
            q = q.bind(c.as_bytes().to_vec());
        }
        for row in q.fetch_all(pool).await? {
            let bytes: Vec<u8> = row.try_get("meta")?;
            if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                out.push(MetaId::from_bytes(arr));
            }
        }
    }
    Ok(out)
}

/// Deletes the rows [`take`] returned for `new`, in the ingest's transaction.
pub async fn consume(
    tx: &mut Transaction<'_, Any>,
    ns: Ns,
    principal: &str,
    new: &[ChangesetId],
) -> Result<(), sqlx::Error> {
    for chunk in new.chunks(BATCH) {
        let sql = format!(
            "DELETE FROM staged_metas WHERE ns = $1 AND principal = $2 AND cs IN ({})",
            placeholders(3, chunk.len())
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(ns.as_bytes().to_vec())
            .bind(principal.to_owned());
        for c in chunk {
            q = q.bind(c.as_bytes().to_vec());
        }
        q.execute(&mut **tx).await?;
    }
    Ok(())
}
