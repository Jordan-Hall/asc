//! The one file that writes local refs (SPEC §9.4.1). `Ctx` keeps the writable [`LocalRefs`]
//! private and gives the engine [`ReadOnlyRefs`], so an engine-side ref move fails loudly
//! instead of skipping hooks, the reflog `op_id` and the op log.

use asc_core::path::RefName;
use asc_state::state::Tx;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey, Result};
use asc_store::sqlrefs::SqlRefs;
use async_trait::async_trait;

/// The error a [`ReadOnlyRefs`] update returns, wrapped in [`RefError::Backend`].
#[derive(Debug, thiserror::Error)]
#[error("local refs are read-only here; move them through Ctx::update_local_refs")]
pub struct ReadOnly;

impl ReadOnly {
    /// Whether `err` is a refused write.
    pub fn is(err: &RefError) -> bool {
        matches!(err, RefError::Backend(inner) if inner.is::<ReadOnly>())
    }
}

/// Reads delegate to `R`; `update` always fails with [`ReadOnly`].
#[derive(Clone, Debug)]
pub struct ReadOnlyRefs<R>(pub R);

#[async_trait]
impl<R: Refs> Refs for ReadOnlyRefs<R> {
    async fn get(&self, repo: RepoKey, name: &RefName) -> Result<Option<RefEntry>> {
        self.0.get(repo, name).await
    }

    async fn list(&self, repo: RepoKey, prefix: &str) -> Result<Vec<RefEntry>> {
        self.0.list(repo, prefix).await
    }

    async fn update(&self, _repo: RepoKey, _txn: &RefTxn) -> Result<()> {
        Err(RefError::Backend(Box::new(ReadOnly)))
    }

    async fn log(&self, repo: RepoKey, name: &RefName, limit: u32) -> Result<Vec<RefLogEntry>> {
        self.0.log(repo, name, limit).await
    }
}

/// The writable local ref store. Only `Ctx::update_local_refs` calls [`LocalRefs::apply`].
pub(crate) struct LocalRefs(SqlRefs);

impl LocalRefs {
    pub(crate) fn new(refs: SqlRefs) -> Self {
        Self(refs)
    }

    /// Applies `txn` (CAS, ref log) inside `tx`, so the caller commits the ref move together with
    /// the stage rebase and the new head base, or rolls all of them back. There is no
    /// autocommitting variant: a ref that moved before its dependent state would leave that state
    /// on the old head when a later step failed.
    pub(crate) async fn apply_in_tx(&self, tx: &mut Tx, repo: RepoKey, txn: &RefTxn) -> Result<()> {
        self.0.update_in_tx(tx, repo, txn).await
    }
}

#[cfg(test)]
mod tests {
    use asc_store::refs::Actor;
    use uuid::Uuid;

    use super::*;

    struct Empty;

    #[async_trait]
    impl Refs for Empty {
        async fn get(&self, _: RepoKey, _: &RefName) -> Result<Option<RefEntry>> {
            Ok(None)
        }
        async fn list(&self, _: RepoKey, _: &str) -> Result<Vec<RefEntry>> {
            Ok(Vec::new())
        }
        async fn update(&self, _: RepoKey, _: &RefTxn) -> Result<()> {
            Ok(())
        }
        async fn log(&self, _: RepoKey, _: &RefName, _: u32) -> Result<Vec<RefLogEntry>> {
            Ok(Vec::new())
        }
    }

    fn txn() -> RefTxn {
        RefTxn {
            updates: Vec::new(),
            actor: Actor {
                id: "t".into(),
                via: "cli".into(),
            },
            reason: "test".into(),
            op_id: Uuid::nil(),
        }
    }

    #[tokio::test]
    async fn update_is_refused_and_reads_pass_through() {
        let refs = ReadOnlyRefs(Empty);
        let err = refs.update(RepoKey(1), &txn()).await.unwrap_err();
        assert!(ReadOnly::is(&err), "{err}");
        let name = RefName::new(b"refs/heads/x".to_vec()).unwrap();
        assert!(refs.get(RepoKey(1), &name).await.unwrap().is_none());
        assert!(refs.list(RepoKey(1), "refs/").await.unwrap().is_empty());
    }
}
