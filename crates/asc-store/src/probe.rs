//! The create-only probe that gates `BucketRefs` (SPEC §3.4).

use bytes::Bytes;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use uuid::Uuid;

use crate::s3::is_create_unsupported;

/// Outcome of [`probe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    /// True only when a second `Create` of the same key failed with `AlreadyExists`.
    pub create_exclusive: bool,
    /// Why the store passed or failed; names the store.
    pub detail: String,
}

impl ProbeResult {
    fn fail(store: &dyn ObjectStore, why: impl std::fmt::Display) -> Self {
        Self {
            create_exclusive: false,
            detail: format!("{store}: {why}"),
        }
    }
}

/// Puts `probe/<uuid>` under `root` with `Create` twice; the second put must report
/// `AlreadyExists`. Any other outcome refuses `BucketRefs` for this store. The probe key is
/// removed afterwards (best effort).
pub async fn probe(store: &dyn ObjectStore, root: &Path) -> ProbeResult {
    let key = root.clone().join("probe").join(Uuid::now_v7().to_string());
    let create = || async {
        store
            .put_opts(
                &key,
                PutPayload::from_bytes(Bytes::from_static(b"asc probe")),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
    };
    let result = match create().await {
        Err(e) if is_create_unsupported(&e) => {
            ProbeResult::fail(store, format!("create-only puts are not supported ({e})"))
        }
        Err(e) => ProbeResult::fail(store, format!("first create failed ({e})")),
        Ok(_) => match create().await {
            Ok(_) => ProbeResult::fail(store, "ignores create-only puts (second create succeeded)"),
            Err(object_store::Error::AlreadyExists { .. }) => ProbeResult {
                create_exclusive: true,
                detail: format!("{store}: create-only puts are exclusive"),
            },
            Err(e) if is_create_unsupported(&e) => {
                ProbeResult::fail(store, format!("create-only puts are not supported ({e})"))
            }
            Err(e) => ProbeResult::fail(store, format!("second create failed unexpectedly ({e})")),
        },
    };
    let _ = store.delete(&key).await;
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;
    use crate::bucketrefs::tests::Wrap;

    #[tokio::test]
    async fn passes_on_exclusive_store() {
        let r = probe(&InMemory::new(), &Path::default()).await;
        assert!(r.create_exclusive, "{}", r.detail);
    }

    #[tokio::test]
    async fn fails_on_store_that_ignores_create() {
        let store = Wrap::new();
        store.ignore_create();
        let r = probe(&*Arc::new(store), &Path::from("root")).await;
        assert!(!r.create_exclusive);
        assert!(r.detail.contains("ignores create-only"), "{}", r.detail);
    }

    #[tokio::test]
    async fn probe_key_is_removed() {
        let store = InMemory::new();
        probe(&store, &Path::from("r")).await;
        let listed = store
            .list_with_delimiter(Some(&Path::from("r/probe")))
            .await;
        assert!(listed.unwrap().objects.is_empty());
    }
}
