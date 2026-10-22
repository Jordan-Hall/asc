//! Values keyed by `(changeset, key)` (SPEC §5).

use async_trait::async_trait;
use asc_core::id::ChangesetId;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use super::{Result, read_memo, write_memo};
use crate::Engine;

/// A value keyed by a changeset and a key, for derivations that recurse along something
/// other than parents (for example blame, which follows the path's history).
///
/// `derive` may call [`Engine::get_keyed`] for other changesets; recursion depth is the
/// implementor's concern.
#[async_trait]
pub trait DerivedKeyed: Sized + Send + Sync {
    const NAME: &'static str;
    const VERSION: u32;
    type Key: Send + Sync;

    /// Canonical key bytes; equal keys must give equal bytes.
    fn encode_key(key: &Self::Key) -> Vec<u8>;

    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        cs: ChangesetId,
        key: &Self::Key,
    ) -> Result<Self>;

    fn encode(&self) -> Vec<u8>;
    fn decode(b: &[u8]) -> Result<Self>;
}

/// Memo key: `blake3(cs ‖ encode(key))`.
fn memo_key<D: DerivedKeyed>(cs: ChangesetId, key: &D::Key) -> ObjKey {
    let mut h = blake3::Hasher::new();
    h.update(cs.as_bytes());
    h.update(&D::encode_key(key));
    ObjKey::Derived {
        name: D::NAME,
        version: D::VERSION,
        id: *h.finalize().as_bytes(),
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The memoised keyed value, deriving and storing it when absent.
    pub async fn get_keyed<D: DerivedKeyed>(&self, cs: ChangesetId, key: &D::Key) -> Result<D> {
        let mk = memo_key::<D>(cs, key);
        if let Some(b) = read_memo(&*self.objects, &mk).await?
            && let Ok(v) = D::decode(&b)
        {
            return Ok(v);
        }
        let v = D::derive(self, cs, key).await?;
        write_memo(&*self.objects, &mk, &v.encode()).await?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::derive::Error;
    use crate::derive::testutil::*;

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    /// Test type: the key's length, plus one per call (so a rerun would differ).
    #[derive(Debug, PartialEq, Eq)]
    struct KeyLen(u32);

    #[async_trait]
    impl DerivedKeyed for KeyLen {
        const NAME: &'static str = "keylen";
        const VERSION: u32 = 1;
        type Key = String;

        fn encode_key(key: &String) -> Vec<u8> {
            key.as_bytes().to_vec()
        }

        async fn derive<O: Objects, R: Refs>(
            _: &Engine<O, R>,
            _: ChangesetId,
            key: &String,
        ) -> Result<Self> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(Self(key.len() as u32))
        }

        fn encode(&self) -> Vec<u8> {
            self.0.to_le_bytes().to_vec()
        }

        fn decode(b: &[u8]) -> Result<Self> {
            let a: [u8; 4] = b
                .try_into()
                .map_err(|_| Error::BadValue { name: "keylen" })?;
            Ok(Self(u32::from_le_bytes(a)))
        }
    }

    #[tokio::test]
    async fn keyed_values_are_memoised_per_changeset_and_key() {
        let e = engine();
        let a = commit_on(&e, &[], b"a").await;
        let b = commit_on(&e, &[a], b"b").await;
        let before = CALLS.load(Ordering::SeqCst);
        let k = "abc".to_owned();
        assert_eq!(e.get_keyed::<KeyLen>(a, &k).await.unwrap(), KeyLen(3));
        assert_eq!(e.get_keyed::<KeyLen>(a, &k).await.unwrap(), KeyLen(3));
        assert_eq!(CALLS.load(Ordering::SeqCst) - before, 1);
        // A different key or changeset is a different memo.
        e.get_keyed::<KeyLen>(a, &"abcd".to_owned()).await.unwrap();
        e.get_keyed::<KeyLen>(b, &k).await.unwrap();
        assert_eq!(CALLS.load(Ordering::SeqCst) - before, 3);
    }
}
