//! `gen.v1`: generation number, `1 + max(parent gens)` (roots are 1).

use async_trait::async_trait;
use asc_core::id::ChangesetId;
use asc_core::object::Changeset;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use super::{Derived, Error, Result};
use crate::Engine;

/// Generation number of a changeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gen {
    pub generation: u32,
}

#[async_trait]
impl Derived for Gen {
    const NAME: &'static str = "gen";
    const VERSION: u32 = 1;
    const EAGER: bool = true;

    async fn derive<O: Objects, R: Refs>(
        _: &Engine<O, R>,
        _: &Changeset,
        _: ChangesetId,
        parents: &[Self],
    ) -> Result<Self> {
        let max = parents.iter().map(|p| p.generation).max().unwrap_or(0);
        Ok(Self {
            generation: max + 1,
        })
    }

    fn encode(&self) -> Vec<u8> {
        self.generation.to_le_bytes().to_vec()
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let a: [u8; 4] = b
            .try_into()
            .map_err(|_| Error::BadValue { name: Self::NAME })?;
        Ok(Self {
            generation: u32::from_le_bytes(a),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::testutil::*;

    #[tokio::test]
    async fn gen_counts_longest_path() {
        let e = engine();
        let a = commit_on(&e, &[], b"a").await;
        let b = commit_on(&e, &[a], b"b").await;
        let c = commit_on(&e, &[a], b"c").await;
        let d = commit_on(&e, &[b, c], b"d").await;
        assert_eq!(e.get_derived::<Gen>(a).await.unwrap().generation, 1);
        assert_eq!(e.get_derived::<Gen>(d).await.unwrap().generation, 3);
    }

    #[tokio::test]
    async fn long_chain_does_not_overflow_the_stack() {
        let e = engine();
        let mut tip = commit_on(&e, &[], b"0").await;
        for i in 1..10_000u32 {
            tip = commit_on(&e, &[tip], i.to_string().as_bytes()).await;
        }
        assert_eq!(e.get_derived::<Gen>(tip).await.unwrap().generation, 10_000);
        // The memo now answers directly.
        assert_eq!(e.get_derived::<Gen>(tip).await.unwrap().generation, 10_000);
    }

    #[tokio::test]
    async fn warm_derives_eager_types() {
        let e = engine();
        let a = commit_on(&e, &[], b"a").await;
        e.warm(a).await.unwrap();
        let key = asc_store::key::ObjKey::Derived {
            name: "gen",
            version: 1,
            id: *a.as_bytes(),
        };
        assert!(e.objects.get(&key).await.unwrap().is_some());
    }
}
