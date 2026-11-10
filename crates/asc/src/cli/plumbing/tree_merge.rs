//! The index merges of `read-tree` with several trees, as git's `unpack-trees.c` does them:
//! the union of trees without `-m`, `twoway_merge` for `-m <old> <new>`, and `threeway_merge`
//! for `-m <base>... <ours> <theirs>`. Pure functions over whole path maps; the caller reads
//! the trees and the index and writes the result.
//!
//! A tree that has a file where another has a directory takes part as git's "D/F conflict
//! entry" at the paths that clash: at the file's own path in a tree where that path is a
//! directory, and at every path under it in a tree where it is a file. Merges treat that entry
//! as absent, except that it never matches an ancestor.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use asc_core::id::{FileId, GitOid};
use asc_core::object::Kind;

/// One index or tree entry at a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ent {
    pub kind: Kind,
    pub git: GitOid,
    pub file: FileId,
    pub size: u64,
    /// An intent-to-add index entry.
    pub ita: bool,
}

/// What one tree holds at a path.
#[derive(Clone, Copy, Debug)]
enum Slot<'a> {
    Missing,
    /// A directory where the path is a file elsewhere, or a file at an ancestor.
    Df,
    Leaf(&'a Ent),
}

impl<'a> Slot<'a> {
    /// The entry, with a D/F conflict entry read as absent (as git's merges do for heads).
    fn entry(self) -> Option<&'a Ent> {
        match self {
            Slot::Leaf(e) => Some(e),
            Slot::Missing | Slot::Df => None,
        }
    }
}

/// git's `same()`: both absent, or the same mode and object.
fn same(a: Option<&Ent>, b: Option<&Ent>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.kind == b.kind && a.git == b.git,
        _ => false,
    }
}

/// Why a merge stopped; git prints the message and `read-tree` exits 128.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `Entry '<path>' would be overwritten by merge. Cannot merge.`
    WouldOverwrite(Vec<u8>),
    /// `Entry '<path>' not uptodate. Cannot merge.`
    NotUptodate(Vec<u8>),
    /// `Merge requires file-level merging` (`--trivial`).
    NeedsFileMerge,
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Self::WouldOverwrite(p) => format!(
                "error: Entry '{}' would be overwritten by merge. Cannot merge.",
                String::from_utf8_lossy(p)
            ),
            Self::NotUptodate(p) => format!(
                "error: Entry '{}' not uptodate. Cannot merge.",
                String::from_utf8_lossy(p)
            ),
            Self::NeedsFileMerge => "error: Merge requires file-level merging".to_owned(),
        }
    }
}

/// The merged index: every path with its entries by stage (0 alone, or some of 1, 2 and 3).
pub type Merged = BTreeMap<Vec<u8>, Vec<(u8, Ent)>>;

/// A path's entries in each tree, by path.
pub type Tree = BTreeMap<Vec<u8>, Ent>;

/// Whether the work-tree file at a path still holds an index entry.
pub type Check<'a, E> = dyn FnMut(&[u8], &Ent) -> Result<bool, E> + 'a;

/// Options of a merge.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    /// `--aggressive`: also resolve deletions on one or both sides and identical additions.
    pub aggressive: bool,
    /// `--trivial`: refuse when some path needs a file-level merge.
    pub trivial: bool,
    /// The index does not exist yet (`twoway_merge`'s initial checkout).
    pub initial: bool,
}

/// `read-tree <tree>...` without `-m`: the union of the trees, a later tree's entry replacing an
/// earlier one at the same path and a deeper path replacing a file at its ancestor.
pub fn union(trees: &[Tree]) -> Merged {
    let mut out: Merged = BTreeMap::new();
    let paths: BTreeSet<&Vec<u8>> = trees.iter().flat_map(|t| t.keys()).collect();
    for path in paths {
        for (at, b) in path.iter().enumerate() {
            if *b == b'/' {
                out.remove(&path[..at]);
            }
        }
        if let Some(e) = trees.iter().rev().find_map(|t| t.get(path)) {
            out.insert(path.clone(), vec![(0, e.clone())]);
        }
    }
    out
}

/// The directories of a tree: every proper ancestor of its paths.
fn dirs(tree: &Tree) -> HashSet<&[u8]> {
    let mut out = HashSet::new();
    for path in tree.keys() {
        for (at, b) in path.iter().enumerate() {
            if *b == b'/' {
                out.insert(&path[..at]);
            }
        }
    }
    out
}

fn slot<'a>(tree: &'a Tree, dirs: &HashSet<&[u8]>, path: &[u8]) -> Slot<'a> {
    if let Some(e) = tree.get(path) {
        return Slot::Leaf(e);
    }
    let file_above = path
        .iter()
        .enumerate()
        .any(|(at, b)| *b == b'/' && tree.contains_key(&path[..at]));
    if file_above || dirs.contains(path) {
        Slot::Df
    } else {
        Slot::Missing
    }
}

/// What a merge function decided for one path.
struct Step {
    entries: Vec<(u8, Ent)>,
}

impl Step {
    fn none() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn at(stage: u8, e: &Ent) -> Self {
        Self {
            entries: vec![(stage, e.clone())],
        }
    }
}

/// `read-tree -m` with two or more trees over the stage 0 `index`. `uptodate(path, entry)` says
/// whether the work-tree file still holds the index entry (always true under `-i`).
pub fn merge<E>(
    index: &Tree,
    trees: &[Tree],
    opts: Opts,
    uptodate: &mut Check<'_, E>,
) -> Result<Result<Merged, Refusal>, E> {
    let dir_sets: Vec<HashSet<&[u8]>> = trees.iter().map(dirs).collect();
    let paths: BTreeSet<&Vec<u8>> = index
        .keys()
        .chain(trees.iter().flat_map(|t| t.keys()))
        .collect();
    let mut out: Merged = BTreeMap::new();
    let mut nontrivial = false;
    for path in paths {
        let slots: Vec<Slot> = trees
            .iter()
            .zip(&dir_sets)
            .map(|(t, d)| slot(t, d, path))
            .collect();
        let current = index.get(path);
        let step = if trees.len() == 2 {
            twoway(path, current, slots[0], slots[1], opts, uptodate)?
        } else {
            threeway(path, current, &slots, opts, &mut nontrivial, uptodate)?
        };
        match step {
            Ok(step) if step.entries.is_empty() => {}
            Ok(step) => {
                out.insert(path.clone(), step.entries);
            }
            Err(refusal) => return Ok(Err(refusal)),
        }
    }
    if opts.trivial && nontrivial {
        return Ok(Err(Refusal::NeedsFileMerge));
    }
    Ok(Ok(out))
}

type StepResult<E> = Result<Result<Step, Refusal>, E>;

/// git's `merged_entry`: `merge` at stage 0, refusing when it replaces an index entry whose
/// file is not up to date.
fn merged_entry<E>(
    path: &[u8],
    merge: &Ent,
    old: Option<&Ent>,
    uptodate: &mut Check<'_, E>,
) -> StepResult<E> {
    match old {
        Some(old) if same(Some(old), Some(merge)) => Ok(Ok(Step::at(0, old))),
        Some(old) if !uptodate(path, old)? => Ok(Err(Refusal::NotUptodate(path.to_vec()))),
        _ => Ok(Ok(Step::at(0, merge))),
    }
}

/// git's `deleted_entry`: the path leaves the index, refusing when its file is not up to date.
fn deleted_entry<E>(path: &[u8], old: Option<&Ent>, uptodate: &mut Check<'_, E>) -> StepResult<E> {
    match old {
        Some(old) if !uptodate(path, old)? => Ok(Err(Refusal::NotUptodate(path.to_vec()))),
        _ => Ok(Ok(Step::none())),
    }
}

fn reject(path: &[u8]) -> Result<Step, Refusal> {
    Err(Refusal::WouldOverwrite(path.to_vec()))
}

/// git's `twoway_merge` (the table in `git-read-tree(1)`, "Two Tree Merge").
fn twoway<E>(
    path: &[u8],
    current: Option<&Ent>,
    old: Slot,
    new: Slot,
    opts: Opts,
    uptodate: &mut Check<'_, E>,
) -> StepResult<E> {
    let (old, new) = (old.entry(), new.entry());
    let Some(cur) = current else {
        return match (old, new) {
            // The deletion of the path was staged.
            (Some(o), Some(n)) if !opts.initial => Ok(if same(Some(o), Some(n)) {
                Ok(Step::none())
            } else {
                reject(path)
            }),
            (_, Some(n)) => merged_entry(path, n, None, uptodate),
            _ => deleted_entry(path, None, uptodate),
        };
    };
    let c = Some(cur);
    let keep = (old.is_none() && new.is_none())
        || (old.is_none() && new.is_some() && same(c, new))
        || (old.is_some() && new.is_some() && same(old, new))
        || (old.is_some() && new.is_some() && !same(old, new) && same(c, new));
    if keep {
        return Ok(Ok(Step::at(0, cur)));
    }
    match (old, new) {
        (Some(_), None) if same(c, old) => deleted_entry(path, c, uptodate),
        (Some(_), Some(n)) if same(c, old) && !same(c, new) => merged_entry(path, n, c, uptodate),
        _ => Ok(reject(path)),
    }
}

/// git's `threeway_merge`: `slots` are the bases, then ours (stage 2), then theirs (stage 3).
fn threeway<E>(
    path: &[u8],
    index: Option<&Ent>,
    slots: &[Slot],
    opts: Opts,
    nontrivial: &mut bool,
    uptodate: &mut Check<'_, E>,
) -> StepResult<E> {
    let (bases, sides) = slots.split_at(slots.len() - 2);
    let any_anc_missing = bases.iter().any(|b| b.entry().is_none());
    let no_anc_exists = bases.iter().all(|b| b.entry().is_none());
    let df_head = matches!(sides[0], Slot::Df);
    let df_remote = matches!(sides[1], Slot::Df);
    let (head, remote) = (sides[0].entry(), sides[1].entry());

    // A base that is a D/F conflict entry matches nothing; a missing one matches a missing side.
    let base_same = |b: &Slot, x: Option<&Ent>| match b {
        Slot::Df => false,
        _ => same(b.entry(), x),
    };
    let (mut head_match, mut remote_match) = (false, false);
    if !same(remote, head) {
        for b in bases {
            head_match |= base_same(b, head);
            remote_match |= base_same(b, remote);
        }
    }

    // #14, #14ALT, #2ALT: the index may match the result instead of the head.
    if let Some(r) = remote
        && !df_head
        && head_match
        && !remote_match
    {
        if index.is_some() && !same(index, remote) && !same(index, head) {
            return Ok(reject(path));
        }
        return merged_entry(path, r, index, uptodate);
    }
    // Otherwise an index entry must match the head.
    if index.is_some() && !same(index, head) {
        return Ok(reject(path));
    }
    if let Some(h) = head {
        // #5ALT, #15
        if same(head, remote) {
            return merged_entry(path, h, index, uptodate);
        }
        // #13, #3ALT
        if !df_remote && remote_match && !head_match {
            return merged_entry(path, h, index, uptodate);
        }
    }
    // #1
    if head.is_none() && remote.is_none() && any_anc_missing {
        return Ok(Ok(Step::none()));
    }
    if opts.aggressive {
        let deleted_both = head.is_none() && remote.is_none();
        let ours_only = head.is_none() && remote.is_some() && remote_match;
        let theirs_only = remote.is_none() && head.is_some() && head_match;
        if deleted_both || ours_only || theirs_only {
            return match index {
                Some(_) => deleted_entry(path, index, uptodate),
                None => Ok(Ok(Step::none())),
            };
        }
        if no_anc_exists
            && let Some(h) = head
            && remote.is_some()
            && same(head, remote)
        {
            return merged_entry(path, h, index, uptodate);
        }
    }
    // The no-merge cases leave stages; the index must be up to date with the work tree.
    if let Some(i) = index
        && !uptodate(path, i)?
    {
        return Ok(Err(Refusal::NotUptodate(path.to_vec())));
    }
    *nontrivial = true;
    let mut entries = Vec::new();
    if (!head_match || !remote_match)
        && let Some(b) = bases.iter().find_map(|b| b.entry())
    {
        entries.push((1, b.clone()));
    }
    if let Some(h) = head {
        entries.push((2, h.clone()));
    }
    if let Some(r) = remote {
        entries.push((3, r.clone()));
    }
    Ok(Ok(Step { entries }))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    fn ent(n: u8) -> Ent {
        Ent {
            kind: Kind::File,
            git: GitOid::from_bytes([n; 20]),
            file: FileId::from_bytes([n; 32]),
            size: 1,
            ita: false,
        }
    }

    fn tree(items: &[(&str, u8)]) -> Tree {
        items
            .iter()
            .map(|(p, n)| (p.as_bytes().to_vec(), ent(*n)))
            .collect()
    }

    fn run(index: &Tree, trees: &[Tree], opts: Opts) -> Result<Merged, Refusal> {
        let mut clean = |_: &[u8], _: &Ent| Ok::<_, Infallible>(true);
        merge(index, trees, opts, &mut clean).unwrap()
    }

    fn stages(m: &Merged, path: &str) -> Vec<(u8, u8)> {
        m.get(path.as_bytes())
            .map(|v| v.iter().map(|(s, e)| (*s, e.git.as_bytes()[0])).collect())
            .unwrap_or_default()
    }

    #[test]
    fn the_union_lets_later_trees_win_and_deeper_paths_replace_files() {
        let m = union(&[tree(&[("a", 1), ("x", 1)]), tree(&[("a", 2), ("x/y", 2)])]);
        assert_eq!(stages(&m, "a"), [(0, 2)]);
        assert_eq!(stages(&m, "x"), []);
        assert_eq!(stages(&m, "x/y"), [(0, 2)]);
    }

    #[test]
    fn three_way_trivial_cases_collapse_and_the_rest_keep_stages() {
        let o = tree(&[
            ("same", 1),
            ("ours", 1),
            ("theirs", 1),
            ("both", 1),
            ("gone", 1),
        ]);
        let a = tree(&[
            ("same", 1),
            ("ours", 2),
            ("theirs", 1),
            ("both", 2),
            ("new", 5),
        ]);
        let b = tree(&[
            ("same", 1),
            ("ours", 1),
            ("theirs", 3),
            ("both", 3),
            ("new", 6),
        ]);
        let m = run(&a, &[o, a.clone(), b], Opts::default()).unwrap();
        assert_eq!(stages(&m, "same"), [(0, 1)]);
        assert_eq!(stages(&m, "ours"), [(0, 2)]);
        assert_eq!(stages(&m, "theirs"), [(0, 3)]);
        assert_eq!(stages(&m, "both"), [(1, 1), (2, 2), (3, 3)]);
        assert_eq!(stages(&m, "new"), [(2, 5), (3, 6)]);
        // Deleted on both sides stays at stage 1 without --aggressive.
        assert_eq!(stages(&m, "gone"), [(1, 1)]);
    }

    #[test]
    fn aggressive_resolves_deletions_and_identical_additions() {
        let o = tree(&[("gone", 1), ("ours-del", 1), ("theirs-del", 1)]);
        let a = tree(&[("theirs-del", 1), ("added", 4)]);
        let b = tree(&[("ours-del", 1), ("added", 4)]);
        let opts = Opts {
            aggressive: true,
            ..Opts::default()
        };
        let m = run(&a, &[o.clone(), a.clone(), b.clone()], opts).unwrap();
        assert_eq!(m.keys().collect::<Vec<_>>(), [b"added"]);
        assert_eq!(stages(&m, "added"), [(0, 4)]);
        let plain = run(&a, &[o, a.clone(), b], Opts::default()).unwrap();
        assert_eq!(stages(&plain, "ours-del"), [(1, 1), (3, 1)]);
        assert_eq!(stages(&plain, "added"), [(0, 4)]);
    }

    #[test]
    fn three_way_refuses_an_index_that_differs_from_ours() {
        let o = tree(&[("f", 1)]);
        let a = tree(&[("f", 1)]);
        let b = tree(&[("f", 1)]);
        let index = tree(&[("f", 9)]);
        assert_eq!(
            run(&index, &[o, a, b], Opts::default()),
            Err(Refusal::WouldOverwrite(b"f".to_vec()))
        );
    }

    #[test]
    fn three_way_lets_the_index_hold_the_result_already() {
        // #14ALT: ours unchanged, theirs changed, and the index already holds theirs.
        let o = tree(&[("f", 1)]);
        let a = tree(&[("f", 1)]);
        let b = tree(&[("f", 3)]);
        let index = tree(&[("f", 3)]);
        let m = run(&index, &[o, a, b], Opts::default()).unwrap();
        assert_eq!(stages(&m, "f"), [(0, 3)]);
    }

    #[test]
    fn trivial_refuses_a_file_level_merge() {
        let o = tree(&[("f", 1)]);
        let a = tree(&[("f", 2)]);
        let b = tree(&[("f", 3)]);
        let opts = Opts {
            trivial: true,
            ..Opts::default()
        };
        assert_eq!(
            run(&a, &[o, a.clone(), b], opts),
            Err(Refusal::NeedsFileMerge)
        );
    }

    #[test]
    fn a_file_against_a_directory_leaves_both_unmerged() {
        let o = Tree::new();
        let a = tree(&[("x", 2)]);
        let b = tree(&[("x/y", 3)]);
        let m = run(&a, &[o, a.clone(), b], Opts::default()).unwrap();
        assert_eq!(stages(&m, "x"), [(2, 2)]);
        assert_eq!(stages(&m, "x/y"), [(3, 3)]);
    }

    #[test]
    fn a_stale_work_tree_file_stops_a_non_trivial_merge() {
        let o = tree(&[("f", 1)]);
        let a = tree(&[("f", 2)]);
        let b = tree(&[("f", 3)]);
        let mut dirty = |_: &[u8], _: &Ent| Ok::<_, Infallible>(false);
        let got = merge(&a, &[o, a.clone(), b], Opts::default(), &mut dirty).unwrap();
        assert_eq!(got, Err(Refusal::NotUptodate(b"f".to_vec())));
    }

    #[test]
    fn two_way_follows_the_table() {
        let h = tree(&[
            ("keep", 1),
            ("upd", 1),
            ("del", 1),
            ("staged", 1),
            ("clash", 1),
        ]);
        let m = tree(&[
            ("keep", 1),
            ("upd", 2),
            ("staged", 1),
            ("clash", 3),
            ("add", 4),
        ]);
        // The index staged a change at `staged` and at `clash`, and deleted nothing.
        let index = tree(&[
            ("keep", 1),
            ("upd", 1),
            ("del", 1),
            ("staged", 7),
            ("clash", 1),
        ]);
        let got = run(&index, &[h.clone(), m.clone()], Opts::default()).unwrap();
        assert_eq!(stages(&got, "keep"), [(0, 1)]);
        assert_eq!(stages(&got, "upd"), [(0, 2)]);
        assert_eq!(stages(&got, "del"), []);
        // 14/15: H == M keeps the index.
        assert_eq!(stages(&got, "staged"), [(0, 7)]);
        assert_eq!(stages(&got, "clash"), [(0, 3)]);
        assert_eq!(stages(&got, "add"), [(0, 4)]);
        // 16/17: the index differs from both.
        let index = tree(&[("clash", 9)]);
        assert_eq!(
            run(&index, &[h.clone(), m.clone()], Opts::default()),
            Err(Refusal::WouldOverwrite(b"clash".to_vec()))
        );
        // A staged deletion of a path that H and M both have stays deleted, and one that M
        // changes is refused.
        let index = tree(&[("upd", 1), ("staged", 1), ("del", 1), ("clash", 1)]);
        let got = run(&index, &[h.clone(), m.clone()], Opts::default()).unwrap();
        assert_eq!(stages(&got, "keep"), []);
        let index = tree(&[("keep", 1)]);
        assert_eq!(
            run(&index, &[h, m], Opts::default()),
            Err(Refusal::WouldOverwrite(b"clash".to_vec()))
        );
    }

    #[test]
    fn two_way_initial_checkout_takes_the_new_tree() {
        let h = tree(&[("f", 1)]);
        let m = tree(&[("f", 2)]);
        let opts = Opts {
            initial: true,
            ..Opts::default()
        };
        let got = run(&Tree::new(), &[h, m], opts).unwrap();
        assert_eq!(stages(&got, "f"), [(0, 2)]);
    }
}
