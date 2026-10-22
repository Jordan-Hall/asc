//! The one file that writes local refs (SPEC §9.4.1). `Ctx` keeps the writable [`LocalRefs`]
//! private and gives the engine [`ReadOnlyRefs`], so an engine-side ref move fails loudly
//! instead of skipping hooks, the reflog `op_id` and the op log.

use async_trait::async_trait;
use asc_core::path::RefName;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey, Result};
use asc_store::sqlrefs::SqlRefs;

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

    /// Applies `txn` atomically (CAS, ref log).
    pub(crate) async fn apply(&self, repo: RepoKey, txn: &RefTxn) -> Result<()> {
        self.0.update(repo, txn).await
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
