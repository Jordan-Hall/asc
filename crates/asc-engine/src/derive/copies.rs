//! `copies.v1`: the renames and copies of a changeset against its first parent (SPEC §5, named
//! future type `copies`).
//!
//! Rename and copy detection (§4.3) reads and hashes every added and deleted file (and, with
//! `-C`, every modified one), and `log -p`, `log --stat`, `show` and `whatchanged` repeat it for
//! each changeset on every run. A `copies` value holds only the result, the matched
//! `(source, destination, score)` pairs, memoised per changeset and keyed by the two trees and the
//! detection settings, so a later run lists the tree changes (metadata only) and applies the
//! pairs without reading content. A pair that does not fit the changes (a damaged value) falls
//! back to detection.

use asc_core::id::{ChangesetId, TreeId};
use asc_core::path::RepoPath;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;

use super::keyed::DerivedKeyed;
use super::{Error, Result};
use crate::Engine;
use crate::diff::rename::{self, Pair, RenameOptions};
use crate::diff::{DiffOptions, TreeChange};
use crate::tree::Pathspec;

/// Pairs one value holds; a changeset with more is detected again each time.
pub const MAX_PAIRS: usize = 100_000;

/// What a `copies` value was derived from: the first parent's tree, the changeset's tree and the
/// detection settings.
#[derive(Clone, Copy, Debug)]
pub struct CopiesKey {
    pub base: TreeId,
    pub tree: TreeId,
    pub opts: RenameOptions,
}

/// The matched pairs, in detection order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Copies {
    pub pairs: Vec<Pair>,
}

#[async_trait]
impl DerivedKeyed for Copies {
    const NAME: &'static str = "copies";
    const VERSION: u32 = 1;
    type Key = CopiesKey;

    fn encode_key(k: &CopiesKey) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 * 2 + 1 + 8 + 8 + 1);
        out.extend(k.base.as_bytes());
        out.extend(k.tree.as_bytes());
        out.push(k.opts.threshold);
        out.extend((k.opts.limit as u64).to_le_bytes());
        out.extend(k.opts.max_file_size.to_le_bytes());
        out.push(u8::from(k.opts.copies));
        out
    }

    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        _cs: ChangesetId,
        k: &CopiesKey,
    ) -> Result<Self> {
        let changes = e.tree_changes(k.base, k.tree, None).await?;
        let pairs = rename::find_pairs(&changes, e, &k.opts).await?;
        if pairs.len() > MAX_PAIRS {
            return Err(Error::BadValue { name: "copies" });
        }
        Ok(Self { pairs })
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend((self.pairs.len() as u32).to_le_bytes());
        for p in &self.pairs {
            out.extend((p.score as u32).to_le_bytes());
            for path in [&p.src, &p.dst] {
                out.extend((path.as_bytes().len() as u32).to_le_bytes());
                out.extend(path.as_bytes());
            }
        }
        out
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let bad = || Error::BadValue { name: "copies" };
        let u32_at = |r: &mut &[u8]| -> Result<u32> {
            let (h, t) = r.split_first_chunk::<4>().ok_or_else(bad)?;
            *r = t;
            Ok(u32::from_le_bytes(*h))
        };
        let path_at = |r: &mut &[u8]| -> Result<RepoPath> {
            let n = u32_at(r)? as usize;
            if r.len() < n {
                return Err(bad());
            }
            let (h, t) = r.split_at(n);
            *r = t;
            RepoPath::new(h.to_vec()).map_err(|_| bad())
        };
        let mut rest = b;
        let n = u32_at(&mut rest)? as usize;
        if n > MAX_PAIRS {
            return Err(bad());
        }
        let mut pairs = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let score = u64::from(u32_at(&mut rest)?);
            let src = path_at(&mut rest)?;
            let dst = path_at(&mut rest)?;
            pairs.push(Pair { src, dst, score });
        }
        if !rest.is_empty() {
            return Err(bad());
        }
        Ok(Self { pairs })
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The diff of changeset `cs` (whose tree is `tree`) against `base`, its first parent's tree
    /// (the empty tree for a root), as [`diff_trees`](Self::diff_trees) returns it. Without a
    /// pathspec, rename and copy detection comes from the `copies` memo.
    pub async fn diff_changeset(
        &self,
        cs: ChangesetId,
        base: TreeId,
        tree: TreeId,
        spec: Option<&Pathspec>,
        opts: &DiffOptions,
    ) -> crate::diff::Result<Vec<TreeChange>> {
        let (Some(r), None) = (&opts.renames, spec) else {
            return self.diff_trees(base, tree, spec, opts).await;
        };
        let changes = self.tree_changes(base, tree, None).await?;
        let key = CopiesKey {
            base,
            tree,
            opts: *r,
        };
        let Ok(v) = self.get_keyed::<Copies>(cs, &key).await else {
            return rename::detect(changes, self, r).await;
        };
        match rename::apply_pairs(changes, &v.pairs) {
            Ok(out) => Ok(out),
            Err(changes) => rename::detect(changes, self, r).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::keyed::memo_key;
    use crate::derive::testutil::*;
    use crate::derive::{read_memo, write_memo};

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    const BODY: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";

    const OLD: &[u8] = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\niota\nkappa\n";

    fn with_renames(copies: bool) -> DiffOptions {
        DiffOptions {
            renames: Some(RenameOptions {
                copies,
                ..RenameOptions::default()
            }),
            ..DiffOptions::default()
        }
    }

    async fn roots(e: &TestEngine, a: ChangesetId, b: ChangesetId) -> (TreeId, TreeId) {
        (
            e.get_changeset(a).await.unwrap().root,
            e.get_changeset(b).await.unwrap().root,
        )
    }

    #[tokio::test]
    async fn memoised_detection_matches_detection_and_is_stored() {
        let e = engine();
        let base = commit_ops(
            &e,
            &[],
            &[
                ("a/old", Some(OLD)),
                ("keep", Some(b"k\n")),
                ("src", Some(BODY)),
            ],
        )
        .await;
        let mut edited = OLD.to_vec();
        edited.extend(b"eleven\n");
        let changed: &[u8] = b"changed\none\ntwo\nthree\nfour\nfive\nsix\nseven\n";
        let cs = commit_ops(
            &e,
            &[base],
            &[
                ("a/old", None),
                ("b/new", Some(&edited)),
                ("copy", Some(BODY)),
                ("src", Some(changed)),
            ],
        )
        .await;
        let (bt, ct) = roots(&e, base, cs).await;
        for copies in [false, true] {
            let opts = with_renames(copies);
            let want = e.diff_trees(bt, ct, None, &opts).await.unwrap();
            let first = e.diff_changeset(cs, bt, ct, None, &opts).await.unwrap();
            let again = e.diff_changeset(cs, bt, ct, None, &opts).await.unwrap();
            assert_eq!(first, want);
            assert_eq!(again, want);
            let key = CopiesKey {
                base: bt,
                tree: ct,
                opts: opts.renames.unwrap(),
            };
            let stored = read_memo(&*e.objects, &memo_key::<Copies>(cs, &key))
                .await
                .unwrap()
                .expect("the memo is stored");
            let v = Copies::decode(&stored).unwrap();
            assert!(
                v.pairs
                    .iter()
                    .any(|x| x.dst == p("b/new") && x.src == p("a/old"))
            );
            assert_eq!(
                v.pairs.iter().any(|x| x.dst == p("copy")),
                copies,
                "a copy pair only with copies on"
            );
        }
        // A pathspec bypasses the memo and still matches detection.
        let opts = with_renames(false);
        let spec = Pathspec::parse([b"b".as_slice()]).unwrap();
        assert_eq!(
            e.diff_changeset(cs, bt, ct, Some(&spec), &opts)
                .await
                .unwrap(),
            e.diff_trees(bt, ct, Some(&spec), &opts).await.unwrap()
        );
    }

    #[tokio::test]
    async fn pairs_that_do_not_fit_the_changes_fall_back_to_detection() {
        let e = engine();
        let base = commit_ops(&e, &[], &[("x", Some(BODY))]).await;
        let cs = commit_ops(&e, &[base], &[("x", None), ("y", Some(BODY))]).await;
        let (bt, ct) = roots(&e, base, cs).await;
        let changes = e.tree_changes(bt, ct, None).await.unwrap();
        let pair = |src: &str, dst: &str, score: u64| Pair {
            src: p(src),
            dst: p(dst),
            score,
        };
        let wrong = [pair("y", "x", 60_000)];
        assert!(rename::apply_pairs(changes.clone(), &wrong).is_err());
        let twice = [pair("x", "y", 60_000), pair("x", "y", 60_000)];
        assert!(rename::apply_pairs(changes.clone(), &twice).is_err());
        assert!(rename::apply_pairs(changes, &[pair("x", "y", 60_001)]).is_err());
        // A damaged memo is replaced by detection's answer.
        let opts = with_renames(false);
        let key = CopiesKey {
            base: bt,
            tree: ct,
            opts: RenameOptions::default(),
        };
        let damaged = Copies {
            pairs: wrong.to_vec(),
        };
        write_memo(
            &*e.objects,
            &memo_key::<Copies>(cs, &key),
            &damaged.encode(),
        )
        .await
        .unwrap();
        assert_eq!(
            e.diff_changeset(cs, bt, ct, None, &opts).await.unwrap(),
            e.diff_trees(bt, ct, None, &opts).await.unwrap()
        );
    }

    #[test]
    fn values_round_trip_and_bad_bytes_are_refused() {
        let v = Copies {
            pairs: vec![Pair {
                src: p("a/b"),
                dst: p("c"),
                score: 45_000,
            }],
        };
        assert_eq!(Copies::decode(&v.encode()).unwrap(), v);
        assert_eq!(
            Copies::decode(&Copies::default().encode()).unwrap(),
            Copies::default()
        );
        assert!(Copies::decode(&[]).is_err());
        let mut long = v.encode();
        long.push(0);
        assert!(Copies::decode(&long).is_err());
        let mut short = v.encode();
        short.pop();
        assert!(Copies::decode(&short).is_err());
        assert!(Copies::decode(&u32::MAX.to_le_bytes()).is_err());
    }
}
