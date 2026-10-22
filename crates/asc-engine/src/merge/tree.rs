//! The tree merge algorithm (SPEC §4.4).
//!
//! A name is split into its directory view and its file view, and each is merged on its own: a
//! directory view recurses (skipping every subtree one side left unchanged), a file view merges
//! content only when both sides changed it differently. When both views survive, the path is a
//! file/directory conflict and our side is kept.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use asc_core::git::tree_oid;
use asc_core::id::TreeId;
use asc_core::object::{Entry, Kind, Tree};
use asc_core::path::{Name, PathError, RepoPath, casefold};
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt, TryStreamExt};

use super::text::{is_binary, merge_text};
use super::{
    Conflict, ConflictKind, DriverRequest, Error, Favor, MergeAttr, MergeOptions, MergeOutcome,
    Result, Stages,
};
use crate::Engine;

/// Concurrent name merges per directory.
const MERGE_FANOUT: usize = 16;

/// The side whose bytes a content merge keeps unchanged.
enum Side {
    Ours,
    Theirs,
}

/// How two changed versions of a file combine.
enum Merged {
    Pick(Side),
    Clean(Vec<u8>),
    /// Text with conflict markers.
    Conflict(Vec<u8>),
    /// A binary conflict; ours is kept.
    Binary,
}

fn child_path(prefix: Option<&RepoPath>, name: &Name) -> Result<RepoPath, PathError> {
    match prefix {
        Some(p) => Ok(p.join(name)),
        None => RepoPath::new(name.as_bytes().to_vec()),
    }
}

/// Equal kind and content (names and sizes follow from these).
fn same(a: Option<&Entry>, b: Option<&Entry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.kind == y.kind && x.id == y.id && x.git == y.git,
        _ => false,
    }
}

/// Kinds that can merge by mode: files and executables are one group.
fn kind_group(kind: Kind) -> u8 {
    match kind {
        Kind::File | Kind::Exec => 0,
        Kind::Symlink => 1,
        Kind::Gitlink => 2,
        Kind::Dir => 3,
    }
}

fn split(entry: &Option<Entry>) -> (Option<Entry>, Option<Entry>) {
    match entry {
        Some(e) if e.kind == Kind::Dir => (Some(e.clone()), None),
        Some(e) => (None, Some(e.clone())),
        None => (None, None),
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Three-way merge of whole trees.
    ///
    /// A path changed on one side takes that side; a path changed identically takes it; a path
    /// changed differently merges content (see [`Conflict`] for what can fail). The merged tree
    /// holds conflict-marked text for text conflicts, our side for binary conflicts, and the
    /// surviving side for modify/delete.
    pub async fn merge_trees(
        &self,
        base: TreeId,
        ours: TreeId,
        theirs: TreeId,
        opts: &MergeOptions,
    ) -> Result<MergeOutcome> {
        let mut conflicts = Vec::new();
        let root = if ours == theirs || base == theirs {
            ours
        } else if base == ours {
            theirs
        } else {
            let entries = self
                .merge_entries(
                    Some(base),
                    Some(ours),
                    Some(theirs),
                    None,
                    opts,
                    &mut conflicts,
                )
                .await?;
            self.put_tree(Tree::new(entries, true)?).await?
        };
        conflicts.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(MergeOutcome { root, conflicts })
    }

    async fn tree_or_empty(&self, id: Option<TreeId>) -> Result<Arc<Tree>> {
        Ok(self
            .get_tree(id.unwrap_or_else(Self::empty_tree_id))
            .await?)
    }

    /// Merged entries of one directory, from the three directory versions (absent = empty).
    fn merge_entries<'a>(
        &'a self,
        base: Option<TreeId>,
        ours: Option<TreeId>,
        theirs: Option<TreeId>,
        prefix: Option<&'a RepoPath>,
        opts: &'a MergeOptions,
        out: &'a mut Vec<Conflict>,
    ) -> BoxFuture<'a, Result<Vec<Entry>>> {
        Box::pin(async move {
            let (tb, to, tt) = futures::try_join!(
                self.tree_or_empty(base),
                self.tree_or_empty(ours),
                self.tree_or_empty(theirs)
            )?;
            let lists = [tb.entries(), to.entries(), tt.entries()];
            let mut at = [0usize; 3];
            let mut jobs: Vec<(Name, Stages)> = Vec::new();
            loop {
                let heads = [
                    lists[0].get(at[0]),
                    lists[1].get(at[1]),
                    lists[2].get(at[2]),
                ];
                let Some(name) = heads.iter().flatten().map(|e| e.name.clone()).min() else {
                    break;
                };
                let mut sides: [Option<Entry>; 3] = [None, None, None];
                for (i, head) in heads.iter().enumerate() {
                    if let Some(e) = head
                        && e.name == name
                    {
                        sides[i] = Some((*e).clone());
                        at[i] += 1;
                    }
                }
                let [base, ours, theirs] = sides;
                jobs.push((name, Stages { base, ours, theirs }));
            }
            let merged: Vec<(Stages, Option<Entry>, Vec<Conflict>)> = stream::iter(jobs)
                .map(|(name, stages)| async move {
                    let path = child_path(prefix, &name)?;
                    let mut found = Vec::new();
                    let entry = self
                        .merge_name(&name, &path, &stages, opts, &mut found)
                        .await?;
                    Ok::<_, Error>((stages, entry, found))
                })
                .buffered(MERGE_FANOUT)
                .try_collect()
                .await?;

            let mut entries = Vec::with_capacity(merged.len());
            let mut ours_new: HashMap<Vec<u8>, usize> = HashMap::new();
            let mut theirs_new: Vec<(Vec<u8>, usize)> = Vec::new();
            for (stages, entry, found) in merged {
                out.extend(found);
                let Some(entry) = entry else { continue };
                if stages.base.is_none() {
                    let fold = casefold(entry.name.as_bytes());
                    match (&stages.ours, &stages.theirs) {
                        (Some(_), None) => {
                            ours_new.insert(fold, entries.len());
                        }
                        (None, Some(_)) => theirs_new.push((fold, entries.len())),
                        _ => {}
                    }
                }
                entries.push(entry);
            }
            for (fold, ti) in theirs_new {
                if let Some(&oi) = ours_new.get(&fold) {
                    out.push(Conflict {
                        path: child_path(prefix, &entries[ti].name)?,
                        kind: ConflictKind::CaseFold,
                        stages: Stages {
                            base: None,
                            ours: Some(entries[oi].clone()),
                            theirs: Some(entries[ti].clone()),
                        },
                        marked_id: None,
                    });
                }
            }
            Ok(entries)
        })
    }

    /// The merged entry at one name, or `None` when it ends up absent.
    async fn merge_name(
        &self,
        name: &Name,
        path: &RepoPath,
        st: &Stages,
        opts: &MergeOptions,
        out: &mut Vec<Conflict>,
    ) -> Result<Option<Entry>> {
        let (db, fb) = split(&st.base);
        let (d_ours, f_ours) = split(&st.ours);
        let (d_theirs, f_theirs) = split(&st.theirs);
        let mut dir_found = Vec::new();
        let mut file_found = Vec::new();
        let dir = if d_ours.is_some() || d_theirs.is_some() {
            self.merge_dir(
                name,
                path,
                db.as_ref(),
                d_ours.as_ref(),
                d_theirs.as_ref(),
                opts,
                &mut dir_found,
            )
            .await?
        } else {
            None
        };
        let file = if f_ours.is_some() || f_theirs.is_some() {
            self.merge_leaf(
                name,
                path,
                fb.as_ref(),
                f_ours.as_ref(),
                f_theirs.as_ref(),
                opts,
                &mut file_found,
            )
            .await?
        } else {
            None
        };
        match (dir, file) {
            (Some(d), Some(f)) => {
                out.push(Conflict {
                    path: path.clone(),
                    kind: ConflictKind::FileDir,
                    stages: st.clone(),
                    marked_id: None,
                });
                if st.ours.as_ref().is_some_and(|e| e.kind != Kind::Dir) {
                    out.extend(file_found);
                    Ok(Some(f))
                } else {
                    out.extend(dir_found);
                    Ok(Some(d))
                }
            }
            (Some(d), None) => {
                out.extend(dir_found);
                Ok(Some(d))
            }
            (None, Some(f)) => {
                out.extend(file_found);
                Ok(Some(f))
            }
            (None, None) => {
                out.extend(dir_found);
                out.extend(file_found);
                Ok(None)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn merge_dir(
        &self,
        name: &Name,
        path: &RepoPath,
        base: Option<&Entry>,
        ours: Option<&Entry>,
        theirs: Option<&Entry>,
        opts: &MergeOptions,
        out: &mut Vec<Conflict>,
    ) -> Result<Option<Entry>> {
        if same(ours, theirs) || same(base, theirs) {
            return Ok(ours.cloned());
        }
        if same(base, ours) {
            return Ok(theirs.cloned());
        }
        let id = |e: Option<&Entry>| e.map(|e| TreeId::from_bytes(e.id));
        let entries = self
            .merge_entries(id(base), id(ours), id(theirs), Some(path), opts, out)
            .await?;
        if entries.is_empty() {
            return Ok(None);
        }
        let size = entries.iter().fold(0u64, |s, e| s.saturating_add(e.size));
        let tree = Tree::new(entries, false)?;
        let git = tree_oid(&tree);
        let tree_id = self.put_tree(tree).await?;
        Ok(Some(Entry {
            name: name.clone(),
            kind: Kind::Dir,
            id: *tree_id.as_bytes(),
            size,
            git,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn merge_leaf(
        &self,
        name: &Name,
        path: &RepoPath,
        base: Option<&Entry>,
        ours: Option<&Entry>,
        theirs: Option<&Entry>,
        opts: &MergeOptions,
        out: &mut Vec<Conflict>,
    ) -> Result<Option<Entry>> {
        if same(ours, theirs) || same(base, theirs) {
            return Ok(ours.cloned());
        }
        if same(base, ours) {
            return Ok(theirs.cloned());
        }
        let stages = Stages {
            base: base.cloned(),
            ours: ours.cloned(),
            theirs: theirs.cloned(),
        };
        let (o, t) = match (ours, theirs) {
            (Some(o), Some(t)) => (o, t),
            (Some(kept), None) | (None, Some(kept)) => {
                out.push(Conflict {
                    path: path.clone(),
                    kind: ConflictKind::ModifyDelete,
                    stages,
                    marked_id: None,
                });
                return Ok(Some(kept.clone()));
            }
            (None, None) => return Ok(None),
        };
        let kind = if base.is_some() {
            ConflictKind::Content
        } else {
            ConflictKind::AddAdd
        };
        let keep_ours = |out: &mut Vec<Conflict>, kind, stages| {
            out.push(Conflict {
                path: path.clone(),
                kind,
                stages,
                marked_id: None,
            });
            Ok(Some(o.clone()))
        };
        if kind_group(o.kind) != 0 || kind_group(t.kind) != 0 {
            return keep_ours(out, kind, stages);
        }
        let base = base.filter(|b| kind_group(b.kind) == 0);
        let mode = if o.kind == t.kind {
            o.kind
        } else {
            match base {
                Some(b) if b.kind == o.kind => t.kind,
                Some(b) if b.kind == t.kind => o.kind,
                _ => return keep_ours(out, kind, stages),
            }
        };
        let merged = if o.id == t.id {
            Merged::Pick(Side::Ours)
        } else if base.is_some_and(|b| b.id == o.id) {
            Merged::Pick(Side::Theirs)
        } else if base.is_some_and(|b| b.id == t.id) {
            Merged::Pick(Side::Ours)
        } else {
            self.merge_content(path, base, o, t, opts).await?
        };
        let (bytes, marked) = match merged {
            Merged::Pick(side) => {
                let src = match side {
                    Side::Ours => o,
                    Side::Theirs => t,
                };
                return Ok(Some(Entry {
                    name: name.clone(),
                    kind: mode,
                    ..src.clone()
                }));
            }
            Merged::Binary => return keep_ours(out, ConflictKind::Binary, stages),
            Merged::Clean(bytes) => (bytes, false),
            Merged::Conflict(bytes) => (bytes, true),
        };
        let blob = self.store_blob(&bytes).await?;
        if marked {
            out.push(Conflict {
                path: path.clone(),
                kind,
                stages,
                marked_id: Some(blob.file),
            });
        }
        Ok(Some(Entry {
            name: name.clone(),
            kind: mode,
            id: *blob.file.as_bytes(),
            size: blob.size,
            git: blob.git,
        }))
    }

    /// Merges two different versions of a file over `base` according to its merge attribute.
    async fn merge_content(
        &self,
        path: &RepoPath,
        base: Option<&Entry>,
        ours: &Entry,
        theirs: &Entry,
        opts: &MergeOptions,
    ) -> Result<Merged> {
        let binary = || match opts.favor {
            Favor::None => Merged::Binary,
            Favor::Ours => Merged::Pick(Side::Ours),
            Favor::Theirs => Merged::Pick(Side::Theirs),
        };
        let attr = opts.attrs.merge_attr(path);
        match attr {
            MergeAttr::Ours => return Ok(Merged::Pick(Side::Ours)),
            MergeAttr::Binary | MergeAttr::Unset => return Ok(binary()),
            _ => {}
        }
        let (b, o, t) = futures::try_join!(
            async {
                match base {
                    Some(b) => self.read_blob(b).await,
                    None => Ok(Bytes::new()),
                }
            },
            self.read_blob(ours),
            self.read_blob(theirs)
        )?;
        if let MergeAttr::Driver(driver) = &attr {
            let req = DriverRequest {
                path,
                base: &b,
                ours: &o,
                theirs: &t,
                marker_size: opts.marker_size.get(),
                labels: &opts.labels,
            };
            if let Some(r) = opts.drivers.run(driver, &req).await? {
                return Ok(if r.clean {
                    Merged::Clean(r.content)
                } else {
                    Merged::Conflict(r.content)
                });
            }
        }
        if attr != MergeAttr::Text && [&b, &o, &t].iter().any(|d| is_binary(d)) {
            return Ok(binary());
        }
        let m = merge_text(&b, &o, &t, opts, attr == MergeAttr::Union);
        Ok(if m.conflicted {
            Merged::Conflict(m.bytes)
        } else {
            Merged::Clean(m.bytes)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::path::Path;

    use async_trait::async_trait;
    use asc_core::id::FileId;
    use asc_core::object::Kind;
    use futures::TryStreamExt;

    use super::*;
    use crate::merge::testutil::*;
    use crate::merge::{
        ConflictStyle, DriverResult, Labels, MergeAttributes, MergeDrivers, Result as MResult,
    };

    fn opts() -> MergeOptions {
        MergeOptions::default()
    }

    async fn merge(
        e: &TestEngine,
        base: &[(&str, &str)],
        ours: &[(&str, &str)],
        theirs: &[(&str, &str)],
        o: &MergeOptions,
    ) -> MergeOutcome {
        let (b, a, t) = (
            files(e, base).await,
            files(e, ours).await,
            files(e, theirs).await,
        );
        e.merge_trees(b, a, t, o).await.unwrap()
    }

    #[tokio::test]
    async fn disjoint_edits_merge_cleanly() {
        let e = engine();
        let base = [("a.txt", "1\n2\n3\n"), ("d/b.txt", "x\n"), ("keep", "k\n")];
        let ours = [
            ("a.txt", "1\n2\n3\n4\n"),
            ("d/b.txt", "x\n"),
            ("keep", "k\n"),
        ];
        let theirs = [("a.txt", "1\n2\n3\n"), ("d/b.txt", "y\n"), ("keep", "k\n")];
        let got = merge(&e, &base, &ours, &theirs, &opts()).await;
        assert!(got.is_clean());
        let want = files(
            &e,
            &[
                ("a.txt", "1\n2\n3\n4\n"),
                ("d/b.txt", "y\n"),
                ("keep", "k\n"),
            ],
        )
        .await;
        assert_eq!(got.root, want);
    }

    #[tokio::test]
    async fn one_sided_changes_reuse_the_changed_tree() {
        let e = engine();
        let base = files(&e, &[("a", "1\n"), ("d/x", "x\n")]).await;
        let changed = files(&e, &[("a", "2\n"), ("d/x", "x\n"), ("d/y", "y\n")]).await;
        for (o, t) in [(base, changed), (changed, base)] {
            let got = e.merge_trees(base, o, t, &opts()).await.unwrap();
            assert_eq!((got.root, got.is_clean()), (changed, true));
        }
        let same = e
            .merge_trees(base, changed, changed, &opts())
            .await
            .unwrap();
        assert_eq!(same.root, changed);
    }

    #[tokio::test]
    async fn deletes_and_identical_changes_are_clean() {
        let e = engine();
        let base = [("gone", "g\n"), ("both", "b\n"), ("k", "k\n")];
        let ours = [("both", "B\n"), ("k", "k\n")];
        let theirs = [("gone", "g\n"), ("both", "B\n"), ("k", "k\n"), ("n", "n\n")];
        let got = merge(&e, &base, &ours, &theirs, &opts()).await;
        assert!(got.is_clean());
        let want = files(&e, &[("both", "B\n"), ("k", "k\n"), ("n", "n\n")]).await;
        assert_eq!(got.root, want);
    }

    #[tokio::test]
    async fn text_conflict_has_markers_stages_and_marked_id() {
        let e = engine();
        let got = merge(
            &e,
            &[("d/f", "a\nb\nc\n")],
            &[("d/f", "a\nB1\nc\n")],
            &[("d/f", "a\nB2\nc\n")],
            &opts(),
        )
        .await;
        assert_eq!(got.conflicts.len(), 1);
        let c = &got.conflicts[0];
        assert_eq!((c.path.clone(), c.kind), (p("d/f"), ConflictKind::Content));
        let marked = b"a\n<<<<<<< ours\nB1\n=======\nB2\n>>>>>>> theirs\nc\n".to_vec();
        assert_eq!(read(&e, got.root, "d/f").await, Some(marked.clone()));
        assert_eq!(c.marked_id, Some(FileId::of(&marked)));
        let s = &c.stages;
        assert!(s.base.is_some() && s.ours.is_some() && s.theirs.is_some());
        assert_ne!(s.ours.as_ref().unwrap().id, s.theirs.as_ref().unwrap().id);
    }

    #[tokio::test]
    async fn add_add_merges_over_an_empty_base() {
        let e = engine();
        let got = merge(&e, &[], &[("n", "x\n")], &[("n", "y\n")], &opts()).await;
        let c = &got.conflicts[0];
        assert_eq!(c.kind, ConflictKind::AddAdd);
        assert!(c.stages.base.is_none() && c.marked_id.is_some());
        assert_eq!(
            read(&e, got.root, "n").await.unwrap(),
            b"<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\n"
        );
        let same = merge(&e, &[], &[("n", "x\n")], &[("n", "x\n")], &opts()).await;
        assert!(same.is_clean());
    }

    #[tokio::test]
    async fn modify_delete_keeps_the_modified_file() {
        let e = engine();
        let base = [("m", "1\n"), ("o", "o\n")];
        let ours = [("m", "2\n"), ("o", "o\n")];
        let got = merge(&e, &base, &ours, &[("o", "o\n")], &opts()).await;
        assert_eq!(got.conflicts.len(), 1);
        assert_eq!(got.conflicts[0].kind, ConflictKind::ModifyDelete);
        assert!(got.conflicts[0].stages.theirs.is_none());
        assert_eq!(read(&e, got.root, "m").await, Some(b"2\n".to_vec()));
        let theirs = [("m", "3\n"), ("o", "o\n")];
        let got = merge(&e, &base, &[("o", "o\n")], &theirs, &opts()).await;
        assert!(got.conflicts[0].stages.ours.is_none());
        assert_eq!(read(&e, got.root, "m").await, Some(b"3\n".to_vec()));
    }

    #[tokio::test]
    async fn deleting_a_directory_conflicts_with_a_change_inside() {
        let e = engine();
        let base = [("d/a", "a\n"), ("d/b", "b\n"), ("k", "k\n")];
        let theirs = [("d/a", "a\n"), ("d/b", "B\n"), ("k", "k\n")];
        let got = merge(&e, &base, &[("k", "k\n")], &theirs, &opts()).await;
        assert_eq!(got.conflicts.len(), 1);
        assert_eq!(got.conflicts[0].path, p("d/b"));
        assert_eq!(got.conflicts[0].kind, ConflictKind::ModifyDelete);
        assert_eq!(read(&e, got.root, "d/a").await, None);
        assert_eq!(read(&e, got.root, "d/b").await, Some(b"B\n".to_vec()));
    }

    #[tokio::test]
    async fn file_against_directory_keeps_ours() {
        let e = engine();
        let got = merge(&e, &[], &[("x", "file\n")], &[("x/y", "deep\n")], &opts()).await;
        assert_eq!(got.conflicts.len(), 1);
        assert_eq!(got.conflicts[0].kind, ConflictKind::FileDir);
        assert_eq!(read(&e, got.root, "x").await, Some(b"file\n".to_vec()));
        let got = merge(&e, &[], &[("x/y", "deep\n")], &[("x", "file\n")], &opts()).await;
        assert_eq!(got.conflicts[0].kind, ConflictKind::FileDir);
        assert_eq!(read(&e, got.root, "x/y").await, Some(b"deep\n".to_vec()));
    }

    #[tokio::test]
    async fn file_replaced_by_directory_on_one_side_is_clean() {
        let e = engine();
        let base = [("x", "file\n")];
        let got = merge(&e, &base, &[("x/y", "deep\n")], &base, &opts()).await;
        assert!(got.is_clean());
        assert_eq!(read(&e, got.root, "x/y").await, Some(b"deep\n".to_vec()));
    }

    #[tokio::test]
    async fn case_only_additions_on_both_sides_conflict() {
        let e = engine();
        let got = merge(&e, &[], &[("d/A.ts", "1\n")], &[("d/a.ts", "2\n")], &opts()).await;
        assert_eq!(got.conflicts.len(), 1);
        assert_eq!(got.conflicts[0].kind, ConflictKind::CaseFold);
        assert_eq!(got.conflicts[0].path, p("d/a.ts"));
        assert!(read(&e, got.root, "d/A.ts").await.is_some());
        assert!(read(&e, got.root, "d/a.ts").await.is_some());
        let one_side = merge(&e, &[], &[("A", "1\n"), ("a", "2\n")], &[], &opts()).await;
        assert!(one_side.is_clean());
    }

    #[tokio::test]
    async fn binary_conflicts_keep_ours_unless_a_side_is_favoured() {
        let e = engine();
        let base = [("i", "a\0b")];
        let (ours, theirs) = ([("i", "o\0b")], [("i", "t\0b")]);
        let got = merge(&e, &base, &ours, &theirs, &opts()).await;
        assert_eq!(got.conflicts[0].kind, ConflictKind::Binary);
        assert_eq!(got.conflicts[0].marked_id, None);
        assert_eq!(read(&e, got.root, "i").await, Some(b"o\0b".to_vec()));
        let o = MergeOptions {
            favor: Favor::Theirs,
            ..opts()
        };
        let got = merge(&e, &base, &ours, &theirs, &o).await;
        assert!(got.is_clean());
        assert_eq!(read(&e, got.root, "i").await, Some(b"t\0b".to_vec()));
    }

    struct Table(HashMap<&'static str, MergeAttr>);

    impl MergeAttributes for Table {
        fn merge_attr(&self, path: &RepoPath) -> MergeAttr {
            let key = String::from_utf8_lossy(path.as_bytes()).into_owned();
            self.0.get(key.as_str()).cloned().unwrap_or_default()
        }
    }

    struct Upper;

    #[async_trait]
    impl MergeDrivers for Upper {
        async fn run(
            &self,
            driver: &str,
            req: &DriverRequest<'_>,
        ) -> MResult<Option<DriverResult>> {
            Ok((driver == "upper").then(|| DriverResult {
                content: [req.ours, req.theirs].concat().to_ascii_uppercase(),
                clean: true,
            }))
        }
    }

    #[tokio::test]
    async fn merge_attributes_pick_the_strategy() {
        let e = engine();
        let attrs: HashMap<&'static str, MergeAttr> = HashMap::from([
            ("union", MergeAttr::Union),
            ("ours", MergeAttr::Ours),
            ("bin", MergeAttr::Binary),
            ("unset", MergeAttr::Unset),
            ("text", MergeAttr::Text),
            ("drv", MergeAttr::Driver("upper".into())),
            ("nodrv", MergeAttr::Driver("missing".into())),
        ]);
        let o = MergeOptions {
            attrs: Arc::new(Table(attrs)),
            drivers: Arc::new(Upper),
            ..opts()
        };
        let names = ["union", "ours", "bin", "unset", "text", "drv", "nodrv"];
        let base: Vec<(&str, &str)> = names.iter().map(|n| (*n, "base\n")).collect();
        let ours: Vec<(&str, &str)> = names.iter().map(|n| (*n, "ours\n")).collect();
        let theirs: Vec<(&str, &str)> = names.iter().map(|n| (*n, "theirs\n")).collect();
        let got = merge(&e, &base, &ours, &theirs, &o).await;
        let kinds: BTreeMap<String, ConflictKind> = got
            .conflicts
            .iter()
            .map(|c| (c.path.to_string(), c.kind))
            .collect();
        assert_eq!(
            kinds,
            BTreeMap::from([
                ("bin".to_string(), ConflictKind::Binary),
                ("nodrv".to_string(), ConflictKind::Content),
                ("text".to_string(), ConflictKind::Content),
                ("unset".to_string(), ConflictKind::Binary),
            ])
        );
        let at = |n: &'static str| read(&e, got.root, n);
        assert_eq!(at("union").await, Some(b"ours\ntheirs\n".to_vec()));
        assert_eq!(at("ours").await, Some(b"ours\n".to_vec()));
        assert_eq!(at("bin").await, Some(b"ours\n".to_vec()));
        assert_eq!(at("drv").await, Some(b"OURS\nTHEIRS\n".to_vec()));
    }

    #[tokio::test]
    async fn mode_change_and_content_change_combine() {
        let e = engine();
        let base = tree_of(&e, &[("s", Kind::File, b"1\n2\n3\n")]).await;
        let ours = tree_of(&e, &[("s", Kind::Exec, b"1\n2\n3\n")]).await;
        let theirs = tree_of(&e, &[("s", Kind::File, b"1\n2\n3\n4\n")]).await;
        let got = e.merge_trees(base, ours, theirs, &opts()).await.unwrap();
        assert!(got.is_clean());
        let want = tree_of(&e, &[("s", Kind::Exec, b"1\n2\n3\n4\n")]).await;
        assert_eq!(got.root, want);
    }

    #[tokio::test]
    async fn conflicting_modes_and_symlinks_keep_ours() {
        let e = engine();
        let base = tree_of(&e, &[("s", Kind::File, b"x\n"), ("l", Kind::Symlink, b"a")]).await;
        let ours = tree_of(&e, &[("s", Kind::Exec, b"x\n"), ("l", Kind::Symlink, b"b")]).await;
        let theirs = tree_of(
            &e,
            &[("s", Kind::Symlink, b"x\n"), ("l", Kind::Symlink, b"c")],
        )
        .await;
        let got = e.merge_trees(base, ours, theirs, &opts()).await.unwrap();
        let paths: Vec<_> = got.conflicts.iter().map(|c| c.path.to_string()).collect();
        assert_eq!(paths, ["l", "s"]);
        assert_eq!(got.root, ours);
    }

    #[tokio::test]
    async fn labels_and_style_reach_the_markers() {
        let e = engine();
        let o = MergeOptions {
            style: ConflictStyle::Diff3,
            labels: Labels {
                base: "B".into(),
                ours: "HEAD".into(),
                theirs: "pick".into(),
            },
            ..opts()
        };
        let got = merge(&e, &[("f", "a\n")], &[("f", "b\n")], &[("f", "c\n")], &o).await;
        assert_eq!(
            read(&e, got.root, "f").await.unwrap(),
            b"<<<<<<< HEAD\nb\n||||||| B\na\n=======\nc\n>>>>>>> pick\n"
        );
    }

    /// Merges each `merge_golden/tree/<case>` and compares the tree and the conflicted paths
    /// with `git merge-tree --write-tree`.
    #[tokio::test]
    async fn matches_git_merge_tree() {
        if !git_available() {
            return;
        }
        let root = golden_root().join("tree");
        let cases = case_names(&root);
        assert!(!cases.is_empty());
        for case in cases {
            let dir = root.join(&case);
            let e = engine();
            let mut roots = Vec::new();
            for side in ["base", "ours", "theirs"] {
                let found = read_dir_files(&dir.join(side));
                let leaves: Vec<_> = found
                    .iter()
                    .map(|(path, data)| (path.as_str(), Kind::File, data.as_slice()))
                    .collect();
                roots.push(tree_of(&e, &leaves).await);
            }
            let got = e
                .merge_trees(roots[0], roots[1], roots[2], &opts())
                .await
                .unwrap();
            let mut ours_files = BTreeMap::new();
            let walked: Vec<_> = e.walk(got.root, None).try_collect().await.unwrap();
            for (path, entry) in walked {
                let data = e.read_blob(&entry).await.unwrap().to_vec();
                ours_files.insert(path.to_string(), data);
            }
            let (git_conflicts, git_files) = git_merge_tree(&dir);
            assert_eq!(
                ours_files.keys().collect::<Vec<_>>(),
                git_files.keys().collect::<Vec<_>>(),
                "{case}: paths"
            );
            for (path, data) in &git_files {
                assert_eq!(
                    String::from_utf8_lossy(&ours_files[path]),
                    String::from_utf8_lossy(data),
                    "{case}: {path}"
                );
            }
            let mut conflicts: Vec<String> =
                got.conflicts.iter().map(|c| c.path.to_string()).collect();
            conflicts.sort();
            conflicts.dedup();
            assert_eq!(conflicts, git_conflicts, "{case}: conflicted paths");
        }
    }

    /// `(conflicted paths, path -> content)` of `git merge-tree --write-tree` over the `base`,
    /// `ours` and `theirs` directories of a case.
    fn git_merge_tree(case: &Path) -> (Vec<String>, BTreeMap<String, Vec<u8>>) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_ok(&repo, &[], &["init", "-q"]);
        let git_dir = repo.join(".git");
        let mut base_commit = String::new();
        for side in ["base", "ours", "theirs"] {
            let index = tmp.path().join(format!("index-{side}"));
            let work = case.join(side);
            std::fs::create_dir_all(&work).unwrap();
            let envs = [
                ("GIT_INDEX_FILE", index.to_str().unwrap()),
                ("GIT_DIR", git_dir.to_str().unwrap()),
                ("GIT_WORK_TREE", work.to_str().unwrap()),
            ];
            git_ok(&work, &envs, &["-c", "core.autocrlf=false", "add", "-A"]);
            let tree = String::from_utf8(git_ok(&work, &envs, &["write-tree"])).unwrap();
            let mut args = vec!["commit-tree", tree.trim(), "-m", side];
            if side != "base" {
                args.extend(["-p", base_commit.as_str()]);
            }
            let commit = String::from_utf8(git_ok(&repo, &[], &args)).unwrap();
            let commit = commit.trim().to_string();
            let name = format!("refs/heads/{side}");
            git_ok(&repo, &[], &["update-ref", &name, &commit]);
            if side == "base" {
                base_commit = commit;
            }
        }
        let args = [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "ours",
            "theirs",
        ];
        let out = git(&repo, &[], &args);
        let text = String::from_utf8(out.stdout).unwrap();
        let mut lines = text.lines();
        let tree = lines.next().unwrap().to_string();
        let mut conflicts: Vec<String> = lines.map(str::to_string).collect();
        conflicts.sort();
        conflicts.dedup();
        let listing = git_ok(&repo, &[], &["ls-tree", "-r", "-z", &tree]);
        let mut found = BTreeMap::new();
        for rec in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let rec = String::from_utf8(rec.to_vec()).unwrap();
            let (meta, path) = rec.split_once('\t').unwrap();
            let oid = meta.split(' ').nth(2).unwrap();
            let data = git_ok(&repo, &[], &["cat-file", "blob", oid]);
            found.insert(path.to_string(), data);
        }
        (conflicts, found)
    }
}
