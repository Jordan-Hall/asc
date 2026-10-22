//! `skip.v1`: first-parent skip pointers (SPEC §5).
//!
//! `ptrs[k]` is the first-parent ancestor `4^(k+1)` steps back. The list stops at the first
//! level whose ancestor does not exist, so it has at most `log4(depth)` entries.

use async_trait::async_trait;
use asc_core::id::ChangesetId;
use asc_core::object::Changeset;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use super::{Derived, Error, Result};
use crate::Engine;

/// Longest pointer list a decoder accepts (`4^16` steps is beyond `u32` generations).
const MAX_PTRS: usize = 16;

/// Skip pointers of a changeset.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Skip {
    pub ptrs: Vec<ChangesetId>,
}

#[async_trait]
impl Derived for Skip {
    const NAME: &'static str = "skip";
    const VERSION: u32 = 1;
    const EAGER: bool = true;

    /// Level `k` is three more hops of `4^k` steps from level `k - 1`, so the cost is
    /// O(log n) memo reads regardless of depth.
    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        cs: &Changeset,
        _: ChangesetId,
        parents: &[Self],
    ) -> Result<Self> {
        let (Some(&first), Some(first_skip)) = (cs.parents.first(), parents.first()) else {
            return Ok(Self::default());
        };
        let mut ptrs = vec![first];
        'levels: while ptrs.len() < MAX_PTRS {
            let k = ptrs.len();
            let mut at = ptrs[k - 1];
            for _ in 0..3 {
                let hop = if at == first {
                    first_skip.ptrs.get(k - 1).copied()
                } else {
                    e.get_derived::<Self>(at).await?.ptrs.get(k - 1).copied()
                };
                match hop {
                    Some(next) => at = next,
                    None => break 'levels,
                }
            }
            ptrs.push(at);
        }
        Ok(Self { ptrs })
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 32 * self.ptrs.len());
        out.push(self.ptrs.len() as u8);
        for p in &self.ptrs {
            out.extend_from_slice(p.as_bytes());
        }
        out
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let bad = || Error::BadValue { name: Self::NAME };
        let (&n, rest) = b.split_first().ok_or_else(bad)?;
        let n = usize::from(n);
        if n > MAX_PTRS || rest.len() != 32 * n {
            return Err(bad());
        }
        let ptrs = rest
            .chunks_exact(32)
            .map(|c| ChangesetId::from_bytes(c.try_into().expect("chunk is 32 bytes")))
            .collect();
        Ok(Self { ptrs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::testutil::*;

    async fn chain(e: &TestEngine, n: usize) -> Vec<ChangesetId> {
        let mut ids = vec![commit_on(e, &[], b"0").await];
        for i in 1..n {
            let p = ids[i - 1];
            ids.push(commit_on(e, &[p], i.to_string().as_bytes()).await);
        }
        ids
    }

    #[tokio::test]
    async fn pointers_are_powers_of_four_back() {
        let e = engine();
        let ids = chain(&e, 70).await;
        let s = e.get_derived::<Skip>(ids[69]).await.unwrap();
        // 69 steps to the root: levels for 1, 4, 16 and 64 steps.
        assert_eq!(s.ptrs, vec![ids[68], ids[65], ids[53], ids[5]]);
        let r = e.get_derived::<Skip>(ids[0]).await.unwrap();
        assert!(r.ptrs.is_empty());
        let s = e.get_derived::<Skip>(ids[4]).await.unwrap();
        assert_eq!(s.ptrs, vec![ids[3], ids[0]]);
    }

    #[tokio::test]
    async fn merges_follow_the_first_parent_only() {
        let e = engine();
        let ids = chain(&e, 10).await;
        let side = commit_on(&e, &[ids[0]], b"side").await;
        let m = commit_on(&e, &[ids[9], side], b"m").await;
        let s = e.get_derived::<Skip>(m).await.unwrap();
        assert_eq!(s.ptrs, vec![ids[9], ids[6]]);
        let m2 = commit_on(&e, &[side, ids[9]], b"m2").await;
        assert_eq!(e.get_derived::<Skip>(m2).await.unwrap().ptrs, vec![side]);
    }

    #[test]
    fn codec_round_trips_and_rejects_damage() {
        let s = Skip {
            ptrs: vec![ChangesetId::of(b"a"), ChangesetId::of(b"b")],
        };
        assert_eq!(Skip::decode(&s.encode()).unwrap(), s);
        assert_eq!(
            Skip::decode(&Skip::default().encode()).unwrap(),
            Skip::default()
        );
        let mut bad = s.encode();
        bad.pop();
        assert!(Skip::decode(&bad).is_err());
        assert!(Skip::decode(&[]).is_err());
        assert!(Skip::decode(&[17]).is_err());
    }
}
