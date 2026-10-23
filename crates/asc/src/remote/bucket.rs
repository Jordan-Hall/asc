//! Bucket remotes (SPEC §3.3, §3.4, §6.2): the remote tier of the object stack, opening a
//! bucket's store, and the `refs-local` key that says which machine holds a bucket's refs.

use std::sync::Arc;

use bytes::Bytes;
use asc_state::state::repos::RemoteKind;
use asc_store::bucket::BucketObjects;
use asc_store::objects::Objects;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};

use crate::ext::{self, RemoteObjectsSelector, RemoteSpec};

/// The key under the bucket root that names the machine holding the refs (SPEC §3.4).
pub const REFS_LOCAL_KEY: &str = "refs-local";
/// The longest `refs-local` value read: a 16-byte machine id, with slack for a damaged key.
const MARKER_MAX: usize = 64;

/// Serves the lazy-fetch tier of a repository whose default remote is a bucket.
struct BucketSelector;

impl RemoteObjectsSelector for BucketSelector {
    fn objects(&self, spec: &RemoteSpec) -> Option<Arc<dyn Objects>> {
        if spec.kind != RemoteKind::Bucket {
            return None;
        }
        let objects = BucketObjects::from_url(spec.url.as_deref()?).ok()?;
        Some(Arc::new(objects))
    }
}

/// Registers the bucket object source (called once from `main`).
pub fn register() {
    ext::register_remote_objects(RemoteKind::Bucket, Arc::new(BucketSelector));
}

/// The store and key root of a normalised bucket URL.
pub fn open_store(url: &str) -> Result<(Arc<dyn ObjectStore>, Path), String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    if parsed.scheme() == "s3" {
        return asc_store::s3::open(&parsed).map_err(|e| e.to_string());
    }
    let (store, root) = object_store::parse_url(&parsed).map_err(|e| e.to_string())?;
    Ok((Arc::from(store), root))
}

/// Creates the directory behind a `file://` bucket URL; other schemes need nothing.
pub fn ensure_local_dir(url: &str) -> std::io::Result<()> {
    let Ok(parsed) = url::Url::parse(url) else {
        return Ok(());
    };
    if parsed.scheme() != "file" {
        return Ok(());
    }
    match parsed.to_file_path() {
        Ok(dir) => std::fs::create_dir_all(dir),
        Err(()) => Ok(()),
    }
}

/// Whether a `file://` bucket directory exists. Other schemes are assumed to.
pub fn exists(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "file" {
        return true;
    }
    parsed.to_file_path().is_ok_and(|p| p.is_dir())
}

/// Who holds the refs of a `--refs local` bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// No push has happened yet.
    Nobody,
    /// This machine.
    Me,
    /// Another machine (or a damaged key).
    Other,
}

fn marker_path(root: &Path) -> Path {
    root.clone().join(REFS_LOCAL_KEY)
}

async fn read_marker(store: &dyn ObjectStore, key: &Path) -> Result<Option<Bytes>, String> {
    match store.get(key).await {
        Ok(r) => {
            if r.meta.size > MARKER_MAX as u64 {
                return Ok(Some(Bytes::from_static(b"oversized")));
            }
            r.bytes().await.map(Some).map_err(|e| e.to_string())
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Reads the `refs-local` key and compares it with this machine's id.
pub async fn holder(url: &str, machine: [u8; 16]) -> Result<Holder, String> {
    let (store, root) = open_store(url)?;
    Ok(match read_marker(&*store, &marker_path(&root)).await? {
        None => Holder::Nobody,
        Some(b) if b.as_ref() == machine.as_slice() => Holder::Me,
        Some(_) => Holder::Other,
    })
}

/// Writes the create-only `refs-local` key (the first push). A concurrent claim by another
/// machine is `Holder::Other`.
pub async fn claim(url: &str, machine: [u8; 16]) -> Result<Holder, String> {
    let (store, root) = open_store(url)?;
    let key = marker_path(&root);
    let put = store
        .put_opts(
            &key,
            PutPayload::from_bytes(Bytes::copy_from_slice(&machine)),
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await;
    match put {
        Ok(_) => Ok(Holder::Me),
        Err(object_store::Error::AlreadyExists { .. }) => {
            Ok(match read_marker(&*store, &key).await? {
                Some(b) if b.as_ref() == machine.as_slice() => Holder::Me,
                _ => Holder::Other,
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_url(dir: &std::path::Path) -> String {
        url::Url::from_directory_path(dir)
            .unwrap()
            .to_string()
            .trim_end_matches('/')
            .to_owned()
    }

    #[tokio::test]
    async fn the_first_claim_wins_and_a_second_machine_is_other() {
        let t = tempfile::tempdir().unwrap();
        let url = file_url(t.path());
        let (a, b) = ([1u8; 16], [2u8; 16]);
        assert_eq!(holder(&url, a).await.unwrap(), Holder::Nobody);
        assert_eq!(claim(&url, a).await.unwrap(), Holder::Me);
        assert_eq!(claim(&url, a).await.unwrap(), Holder::Me);
        assert_eq!(claim(&url, b).await.unwrap(), Holder::Other);
        assert_eq!(holder(&url, a).await.unwrap(), Holder::Me);
        assert_eq!(holder(&url, b).await.unwrap(), Holder::Other);
    }

    #[test]
    fn a_missing_local_bucket_does_not_exist() {
        let t = tempfile::tempdir().unwrap();
        let url = file_url(&t.path().join("nope"));
        assert!(!exists(&url));
        ensure_local_dir(&url).unwrap();
        assert!(exists(&url));
    }
}
