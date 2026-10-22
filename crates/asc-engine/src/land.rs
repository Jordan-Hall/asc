//! Path-set disjointness and the land content merge (SPEC §4.5, §8.5 step 3).

use std::collections::HashMap;

use asc_core::id::ChangesetId;
use asc_core::object::{Change, SortedMap};
use asc_core::path::{RepoPath, casefold};
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::Engine;
use crate::merge::{Conflict, Error, MergeOptions, Result};
use crate::rebase::Rebased;

/// Longest first-parent chain [`Engine::paths_since`] walks before it falls back to a tree diff.
const FIRST_PARENT_LIMIT: usize = 4096;

/// A set of changed paths, sorted and unique.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathSet {
    paths: Vec<RepoPath>,
}

#[derive(Default)]
struct TrieNode {
    children: HashMap<Vec<u8>, usize>,
    /// The path that ends exactly here.
    ends: Option<RepoPath>,
    /// The first path through (or ending at) this node.
    witness: Option<RepoPath>,
}

/// Casefolded component trie holding every path and all its ancestor directories.
struct Trie {
    nodes: Vec<TrieNode>,
}

impl Trie {
    fn new(set: &PathSet) -> Self {
        let mut trie = Trie {
            nodes: vec![TrieNode::default()],
        };
        for path in &set.paths {
            let mut at = 0;
            for comp in path.components() {
                let fold = casefold(comp);
                at = match trie.nodes[at].children.get(&fold) {
                    Some(&next) => next,
                    None => {
                        trie.nodes.push(TrieNode::default());
                        let next = trie.nodes.len() - 1;
                        trie.nodes[at].children.insert(fold, next);
                        next
                    }
                };
                trie.nodes[at].witness.get_or_insert_with(|| path.clone());
            }
            trie.nodes[at].ends = Some(path.clone());
        }
        trie
    }

    /// A path of the trie that equals, contains or lies below `path` (under casefold).
    fn find(&self, path: &RepoPath) -> Option<&RepoPath> {
        let comps: Vec<&[u8]> = path.components().collect();
        let mut at = 0;
        for (i, comp) in comps.iter().enumerate() {
            at = *self.nodes[at].children.get(&casefold(comp))?;
            let node = &self.nodes[at];
            if i + 1 == comps.len() {
                return node.ends.as_ref().or(node.witness.as_ref());
            }
            if let Some(b) = &node.ends {
                return Some(b);
            }
        }
        None
    }
}

impl PathSet {
    pub fn new(mut paths: Vec<RepoPath>) -> Self {
        paths.sort();
        paths.dedup();
        Self { paths }
    }

    /// The paths of a changeset's `changes`.
    pub fn from_changes(changes: &SortedMap<RepoPath, Change>) -> Self {
        Self {
            paths: changes.as_slice().iter().map(|(p, _)| p.clone()).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RepoPath> {
        self.paths.iter()
    }

    /// Pairs `(a, b)` with `a` in `self` and `b` in `other` that conflict: equal, equal under
    /// casefold, or one a directory prefix of the other (SPEC §4.5). Each `a` is reported once,
    /// with the first `b` found.
    pub fn conflicts(&self, other: &PathSet) -> Vec<(RepoPath, RepoPath)> {
        if self.is_empty() || other.is_empty() {
            return Vec::new();
        }
        let trie = Trie::new(other);
        self.paths
            .iter()
            .filter_map(|a| trie.find(a).map(|b| (a.clone(), b.clone())))
            .collect()
    }

    /// True when no pair of paths conflicts.
    pub fn is_disjoint(&self, other: &PathSet) -> bool {
        self.conflicts(other).is_empty()
    }
}

/// `land.noMergeGlobs`: a pattern without `/` matches a file name at any depth, one with `/`
/// matches the whole path.
pub struct NoMergeGlobs(GlobSet);

impl NoMergeGlobs {
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<Self> {
        let mut set = GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            let full = if pattern.contains('/') {
                pattern.to_string()
            } else {
                format!("**/{pattern}")
            };
            let glob = Glob::new(&full).map_err(|e| Error::Glob {
                glob: pattern.to_string(),
                message: e.to_string(),
            })?;
            set.add(glob);
        }
        let set = set.build().map_err(|e| Error::Glob {
            glob: String::new(),
            message: e.to_string(),
        })?;
        Ok(Self(set))
    }

    pub fn is_match(&self, path: &RepoPath) -> bool {
        std::str::from_utf8(path.as_bytes()).is_ok_and(|p| self.0.is_match(p))
    }
}

/// Result of [`Engine::content_merge_land`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandMerge {
    /// `(old, new)` for each stack changeset rewritten onto the tip, in stack order. When
    /// `conflicts` is not empty this stops before the conflicting changeset.
    pub rewritten: Vec<(ChangesetId, ChangesetId)>,
    /// Conflicts of the first changeset that could not land; text conflicts carry `marked_id`.
    pub conflicts: Vec<Conflict>,
    /// Rewritten changesets that lost a `git.gpgsig`.
    pub signatures_dropped: usize,
    /// Rewritten changesets that needed a content merge (the rest were metadata-only).
    pub content_merged: usize,
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The union of `changes` along the first-parent chain from `tip` back to (excluding)
    /// `base`, when `base` is on that chain within [`FIRST_PARENT_LIMIT`] steps; otherwise the
    /// keys of the tree diff `base.root..tip.root`. Either way a superset of the real
    /// difference (SPEC §4.5).
    pub async fn paths_since(&self, base: ChangesetId, tip: ChangesetId) -> Result<PathSet> {
        let mut paths = Vec::new();
        let mut at = tip;
        for _ in 0..FIRST_PARENT_LIMIT {
            if at == base {
                return Ok(PathSet::new(paths));
            }
            let cs = self.get_changeset(at).await?;
            paths.extend(cs.changes.as_slice().iter().map(|(p, _)| p.clone()));
            match cs.parents.first() {
                Some(&parent) => at = parent,
                None => break,
            }
        }
        let (base_root, tip_root) = (
            self.get_changeset(base).await?.root,
            self.get_changeset(tip).await?.root,
        );
        let diff = self.leafdiff(base_root, tip_root, None).await?;
        Ok(PathSet::new(diff.into_iter().map(|(p, _)| p).collect()))
    }

    /// Rewrites `stack` (oldest first) onto `tip` for a land (SPEC §8.5 steps 3 and 5): a
    /// changeset whose paths are disjoint from what `tip` changed is metadata-rebased, otherwise
    /// its intersecting files are merged. Stops at the first changeset that conflicts: a text
    /// conflict, or an intersecting path matching `no_merge_globs`. Nothing but the rewritten
    /// changesets and conflict-marked files is stored.
    pub async fn content_merge_land(
        &self,
        stack: &[ChangesetId],
        tip: ChangesetId,
        no_merge_globs: &NoMergeGlobs,
        opts: &MergeOptions,
    ) -> Result<LandMerge> {
        let mut out = LandMerge::default();
        let mut onto = tip;
        for &old in stack {
            let cs = self.get_changeset(old).await?;
            match self
                .rebase_one(&cs, onto, opts, Some(no_merge_globs))
                .await?
            {
                Rebased::Done { id, merged } => {
                    out.signatures_dropped +=
                        usize::from(cs.extra.get(&b"git.gpgsig".to_vec()).is_some());
                    out.content_merged += usize::from(merged);
                    out.rewritten.push((old, id));
                    onto = id;
                }
                Rebased::Conflicts(conflicts) => {
                    out.conflicts = conflicts;
                    break;
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::testutil::{child, commit, files, fixture, p, read};

    fn set(paths: &[&str]) -> PathSet {
        PathSet::new(paths.iter().map(|s| p(s)).collect())
    }

    fn conflicts(a: &[&str], b: &[&str]) -> Vec<(String, String)> {
        set(a)
            .conflicts(&set(b))
            .into_iter()
            .map(|(x, y)| (x.to_string(), y.to_string()))
            .collect()
    }

    #[test]
    fn disjointness_truth_table() {
        // A file against a directory of the same name, in both directions.
        assert_eq!(conflicts(&["a/b"], &["a"]), [("a/b".into(), "a".into())]);
        assert_eq!(conflicts(&["a"], &["a/b"]), [("a".into(), "a/b".into())]);
        // Case-only differences, also inside directories.
        assert_eq!(conflicts(&["A.ts"], &["a.ts"]).len(), 1);
        assert_eq!(conflicts(&["Src/A.ts"], &["src/a.ts"]).len(), 1);
        assert_eq!(conflicts(&["Src/a"], &["src"]).len(), 1);
        // A directory prefix against a deeper path.
        assert_eq!(conflicts(&["x"], &["x/y/z"]).len(), 1);
        assert_eq!(conflicts(&["x/y/z"], &["x"]).len(), 1);
        assert_eq!(conflicts(&["x/y"], &["x/y/z"]).len(), 1);
        // Equal paths.
        assert_eq!(conflicts(&["p/q"], &["p/q"]).len(), 1);
        // Unrelated paths, including shared prefixes that are not directories.
        assert!(conflicts(&["a/b", "c"], &["a/c", "d/e"]).is_empty());
        assert!(conflicts(&["ab"], &["a"]).is_empty());
        assert!(conflicts(&["x/y"], &["x/yz"]).is_empty());
        assert!(conflicts(&[], &["a"]).is_empty());
        assert!(set(&["a"]).is_disjoint(&set(&["b"])));
    }

    #[test]
    fn each_path_reports_once_with_a_witness() {
        let got = conflicts(&["d/a", "d/b", "e"], &["d", "d/a/x"]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "d/a");
        assert_eq!(got[1].0, "d/b");
    }

    #[test]
    fn no_merge_globs_match_names_at_any_depth() {
        let g = NoMergeGlobs::new(&["Cargo.lock", "**/packages.lock.json", "a/b.txt"]).unwrap();
        for hit in [
            "Cargo.lock",
            "crates/x/Cargo.lock",
            "packages.lock.json",
            "p/q/packages.lock.json",
            "a/b.txt",
        ] {
            assert!(g.is_match(&p(hit)), "{hit}");
        }
        for miss in ["Cargo.lock.bak", "x/a/b.txt", "src/main.rs"] {
            assert!(!g.is_match(&p(miss)), "{miss}");
        }
        assert!(NoMergeGlobs::new(&["["]).is_err());
    }

    #[tokio::test]
    async fn paths_since_walks_the_first_parent_chain() {
        let (e, base) = fixture().await;
        let c1 = child(&e, base, &[("a", "x\n")], Vec::new()).await;
        let c2 = child(&e, c1, &[("b", "y\n"), ("d/n", "n\n")], Vec::new()).await;
        let got = e.paths_since(base, c2).await.unwrap();
        assert_eq!(got, set(&["a", "b", "d/n"]));
        assert!(e.paths_since(c2, c2).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn paths_since_falls_back_to_a_tree_diff_off_the_chain() {
        let (e, base) = fixture().await;
        let left = child(&e, base, &[("a", "L\n")], Vec::new()).await;
        let right = child(&e, base, &[("b", "R\n")], Vec::new()).await;
        // `left` is not an ancestor of `right`: the diff of their roots.
        let got = e.paths_since(left, right).await.unwrap();
        assert_eq!(got, set(&["a", "b"]));
    }

    fn globs() -> NoMergeGlobs {
        NoMergeGlobs::new(&["Cargo.lock"]).unwrap()
    }

    #[tokio::test]
    async fn land_rebases_disjoint_stacks_on_metadata_and_counts_signatures() {
        let (e, base) = fixture().await;
        let sig = vec![(b"git.gpgsig".to_vec(), b"sig".to_vec())];
        let s1 = child(&e, base, &[("a", "A\n")], sig).await;
        let s2 = child(&e, s1, &[("c", "C\n")], Vec::new()).await;
        let tip = child(&e, base, &[("b", "B\n")], Vec::new()).await;
        let got = e
            .content_merge_land(&[s1, s2], tip, &globs(), &MergeOptions::default())
            .await
            .unwrap();
        assert!(got.conflicts.is_empty());
        assert_eq!((got.signatures_dropped, got.content_merged), (1, 0));
        assert_eq!(got.rewritten.len(), 2);
        let new_tip = e.get_changeset(got.rewritten[1].1).await.unwrap();
        for (path, want) in [("a", "A\n"), ("b", "B\n"), ("c", "C\n")] {
            assert_eq!(
                read(&e, new_tip.root, path).await,
                Some(want.as_bytes().to_vec())
            );
        }
        let first = e.get_changeset(got.rewritten[0].1).await.unwrap();
        assert!(first.extra.is_empty());
    }

    #[tokio::test]
    async fn land_merges_intersecting_files() {
        let (e, base) = fixture().await;
        let s1 = child(&e, base, &[("a", "ONE\n2\n3\n4\n5\n")], Vec::new()).await;
        let tip = child(&e, base, &[("a", "1\n2\n3\n4\nFIVE\n")], Vec::new()).await;
        let got = e
            .content_merge_land(&[s1], tip, &globs(), &MergeOptions::default())
            .await
            .unwrap();
        assert!(got.conflicts.is_empty());
        assert_eq!(got.content_merged, 1);
        let cs = e.get_changeset(got.rewritten[0].1).await.unwrap();
        assert_eq!(cs.parents, [tip]);
        assert_eq!(
            read(&e, cs.root, "a").await,
            Some(b"ONE\n2\n3\n4\nFIVE\n".to_vec())
        );
    }

    #[tokio::test]
    async fn land_reports_text_conflicts_with_marked_content() {
        let (e, base) = fixture().await;
        let s1 = child(&e, base, &[("b", "mine\n")], Vec::new()).await;
        let tip = child(&e, base, &[("b", "theirs\n")], Vec::new()).await;
        let got = e
            .content_merge_land(&[s1], tip, &globs(), &MergeOptions::default())
            .await
            .unwrap();
        assert!(got.rewritten.is_empty());
        assert_eq!(got.conflicts.len(), 1);
        let c = &got.conflicts[0];
        assert_eq!(c.path, p("b"));
        let marked = b"<<<<<<< ours\ntheirs\n=======\nmine\n>>>>>>> theirs\n";
        assert_eq!(c.marked_id, Some(asc_core::id::FileId::of(marked)));
    }

    #[tokio::test]
    async fn land_refuses_to_merge_no_merge_globs() {
        let (e, _) = fixture().await;
        let root = files(&e, &[("Cargo.lock", "1\n2\n3\n"), ("x", "x\n")]).await;
        let base = commit(&e, &[], root, "base").await;
        // The edits would merge cleanly, but a lockfile must be regenerated, not merged.
        let s1 = child(&e, base, &[("Cargo.lock", "one\n2\n3\n")], Vec::new()).await;
        let tip = child(&e, base, &[("Cargo.lock", "1\n2\nthree\n")], Vec::new()).await;
        let got = e
            .content_merge_land(&[s1], tip, &globs(), &MergeOptions::default())
            .await
            .unwrap();
        assert!(got.rewritten.is_empty());
        assert_eq!(got.conflicts.len(), 1);
        assert_eq!(got.conflicts[0].path, p("Cargo.lock"));
        assert!(got.conflicts[0].marked_id.is_none());
        assert!(got.conflicts[0].stages.base.is_some());
        assert!(got.conflicts[0].stages.theirs.is_some());
    }
}
