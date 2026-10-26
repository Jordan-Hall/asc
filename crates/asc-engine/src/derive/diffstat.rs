//! `diffstat.v1`: per-file added and removed line counts against `parents[0]` (SPEC §5).
//!
//! Feeds `CommitGraph` stats, visual file history and `Log.with_stats`. It is not eager: it is
//! derived on demand, one changeset at a time. It is keyed by the changeset alone
//! ([`DerivedKeyed`]) so deriving one never walks the history below it.
//!
//! Every input is bounded: at most [`MAX_FILES`] files are listed (the tree walk itself stops
//! after the `MAX_FILES + 1`th change, so a huge commit costs no more than a big one), a file larger than
//! [`MAX_FILE_BYTES`] on either side counts as binary without being read, and one changeset
//! reads at most [`MAX_READ_BYTES`] of content in total (the rest is listed as binary and the
//! stat is marked `truncated`).

use asc_core::id::{ChangesetId, FileId};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;

use super::{DerivedKeyed, Error, Result, read_memo};
use crate::Engine;
use crate::diff::TreeChange;
use crate::diff::line::{self, Algorithm};
use crate::tree::MAX_DIFF_VISITS;

/// Files one stat lists.
pub const MAX_FILES: usize = 10_000;
/// Largest side of a file the stat reads; a larger one counts as binary.
pub const MAX_FILE_BYTES: u64 = 1 << 20;
/// Content one changeset's stat reads in total.
pub const MAX_READ_BYTES: u64 = 32 << 20;

/// Line counts of one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub path: RepoPath,
    pub added: u32,
    pub removed: u32,
    /// Binary, too large to count, or past the read budget; the counts are zero.
    pub binary: bool,
}

/// `diffstat.v1`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffStatV1 {
    /// Sorted by path.
    pub files: Vec<FileStat>,
    /// Files were left out or left uncounted because a bound was reached.
    pub truncated: bool,
}

fn sat(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `blake3(cs ‖ b"")`, the memo key [`Engine::get_keyed`] uses for the unit key.
fn memo_key(cs: ChangesetId) -> ObjKey {
    let mut h = blake3::Hasher::new();
    h.update(cs.as_bytes());
    ObjKey::Derived {
        name: DiffStatV1::NAME,
        version: DiffStatV1::VERSION,
        id: *h.finalize().as_bytes(),
    }
}

struct Count {
    added: u32,
    removed: u32,
    binary: bool,
}

async fn count<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    c: &TreeChange,
    read: &mut u64,
    truncated: &mut bool,
) -> Result<Count> {
    let uncounted = Count {
        added: 0,
        removed: 0,
        binary: true,
    };
    let gitlink =
        |x: &Option<asc_core::object::Entry>| x.as_ref().is_some_and(|x| x.kind == Kind::Gitlink);
    if gitlink(&c.old) || gitlink(&c.new) {
        // One line per side, as the "Subproject commit" line of a patch.
        let (old, new) = (c.old.as_ref(), c.new.as_ref());
        let same = matches!((old, new), (Some(o), Some(n)) if o.git == n.git && o.kind == n.kind);
        return Ok(Count {
            added: u32::from(new.is_some() && !same),
            removed: u32::from(old.is_some() && !same),
            binary: false,
        });
    }
    if let (Some(o), Some(n)) = (&c.old, &c.new)
        && o.git == n.git
    {
        // A mode change only.
        return Ok(Count {
            added: 0,
            removed: 0,
            binary: false,
        });
    }
    let size = |x: &Option<asc_core::object::Entry>| x.as_ref().map_or(0, |x| x.size);
    let (old_size, new_size) = (size(&c.old), size(&c.new));
    if old_size > MAX_FILE_BYTES || new_size > MAX_FILE_BYTES {
        return Ok(uncounted);
    }
    if *read + old_size + new_size > MAX_READ_BYTES {
        *truncated = true;
        return Ok(uncounted);
    }
    *read += old_size + new_size;
    let mut sides = [bytes::Bytes::new(), bytes::Bytes::new()];
    for (slot, entry) in sides.iter_mut().zip([&c.old, &c.new]) {
        if let Some(x) = entry {
            *slot = e.read_file(FileId::from_bytes(x.id), x.size).await?;
        }
    }
    let [old, new] = sides;
    if line::is_binary(&old) || line::is_binary(&new) {
        return Ok(uncounted);
    }
    let (added, removed) = line::diff(&old, &new, Algorithm::Myers, false).counts();
    Ok(Count {
        added: sat(added),
        removed: sat(removed),
        binary: false,
    })
}

/// The stat of `id` listing at most `max_files` files, and the tree entries visited. The tree
/// walk stops after the `max_files + 1`th change (that extra one only marks the list as cut),
/// so a commit touching a million files costs no more than one touching `max_files`.
async fn stat_of<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    id: ChangesetId,
    max_files: usize,
) -> Result<(DiffStatV1, usize)> {
    let cs = e.get_changeset(id).await?;
    let base = match cs.parents.first() {
        Some(&p) => e.get_changeset(p).await?.root,
        None => Engine::<O, R>::empty_tree_id(),
    };
    let (changes, visits) = e
        .tree_changes_limited(base, cs.root, None, MAX_DIFF_VISITS, max_files)
        .await?;
    let mut truncated = changes.len() > max_files;
    let mut read = 0;
    let mut files = Vec::with_capacity(changes.len().min(max_files));
    for c in changes.iter().take(max_files) {
        let n = count(e, c, &mut read, &mut truncated).await?;
        files.push(FileStat {
            path: c.path.clone(),
            added: n.added,
            removed: n.removed,
            binary: n.binary,
        });
    }
    Ok((DiffStatV1 { files, truncated }, visits))
}

#[async_trait]
impl DerivedKeyed for DiffStatV1 {
    const NAME: &'static str = "diffstat";
    const VERSION: u32 = 1;
    type Key = ();

    fn encode_key(_: &()) -> Vec<u8> {
        Vec::new()
    }

    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        id: ChangesetId,
        _: &(),
    ) -> Result<Self> {
        Ok(stat_of(e, id, MAX_FILES).await?.0)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + self.files.len() * 24);
        out.extend(sat(self.files.len()).to_le_bytes());
        out.push(u8::from(self.truncated));
        for f in &self.files {
            out.extend(sat(f.path.as_bytes().len()).to_le_bytes());
            out.extend(f.path.as_bytes());
            out.extend(f.added.to_le_bytes());
            out.extend(f.removed.to_le_bytes());
            out.push(u8::from(f.binary));
        }
        out
    }

    fn decode(mut b: &[u8]) -> Result<Self> {
        let bad = || Error::BadValue { name: "diffstat" };
        fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            let (head, tail) = b.split_at_checked(n)?;
            *b = tail;
            Some(head)
        }
        let u32_of = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap_or([0; 4]));
        let n = u32_of(take(&mut b, 4).ok_or_else(bad)?) as usize;
        let truncated = take(&mut b, 1).ok_or_else(bad)?[0] != 0;
        if n > MAX_FILES {
            return Err(bad());
        }
        let mut files = Vec::with_capacity(n.min(b.len() / 13));
        for _ in 0..n {
            let len = u32_of(take(&mut b, 4).ok_or_else(bad)?) as usize;
            let path = RepoPath::new(take(&mut b, len).ok_or_else(bad)?).map_err(|_| bad())?;
            let added = u32_of(take(&mut b, 4).ok_or_else(bad)?);
            let removed = u32_of(take(&mut b, 4).ok_or_else(bad)?);
            let binary = take(&mut b, 1).ok_or_else(bad)?[0] != 0;
            files.push(FileStat {
                path,
                added,
                removed,
                binary,
            });
        }
        if !b.is_empty() {
            return Err(bad());
        }
        Ok(Self { files, truncated })
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The stat of `id` against its first parent, deriving and memoising it when absent.
    pub async fn diffstat(&self, id: ChangesetId) -> Result<DiffStatV1> {
        self.get_keyed::<DiffStatV1>(id, &()).await
    }

    /// The memoised stats of `ids`, in order; `None` where none is stored. Never derives, so it
    /// costs one presence query plus a read per stored memo.
    pub async fn diffstat_memos(&self, ids: &[ChangesetId]) -> Result<Vec<Option<DiffStatV1>>> {
        let keys: Vec<ObjKey> = ids.iter().map(|&id| memo_key(id)).collect();
        let present = self.objects.has_many(&keys).await?;
        let mut out = Vec::with_capacity(ids.len());
        for (key, here) in keys.iter().zip(present) {
            let got = if here {
                read_memo(&*self.objects, key)
                    .await?
                    .and_then(|b| DiffStatV1::decode(&b).ok())
            } else {
                None
            };
            out.push(got);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use asc_core::content::{Body, ingest};
    use asc_core::object::Change;
    use asc_core::path::RepoPath;
    use asc_store::codec::encode_stored;

    use super::*;
    use crate::commit::{NewChangeset, PathPolicy};
    use crate::derive::testutil::*;

    async fn put(e: &TestEngine, kind: Kind, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).unwrap();
        match got.body {
            Body::Inline(b) => {
                e.objects
                    .put(&ObjKey::File(got.file), encode_stored(&b))
                    .await
                    .unwrap();
            }
            Body::Chunked { list, chunks } => {
                use asc_core::codec::Enveloped;
                for (id, data) in chunks {
                    e.objects
                        .put(&ObjKey::Chunk(id), encode_stored(&data))
                        .await
                        .unwrap();
                }
                e.objects
                    .put(
                        &ObjKey::ChunkList(got.file),
                        encode_stored(&list.to_envelope()),
                    )
                    .await
                    .unwrap();
            }
        }
        Change::write(kind, got.file, got.size, got.git, None).unwrap()
    }

    /// A changeset on `parent` applying `ops` to its tree (`None` deletes).
    async fn mk(
        e: &TestEngine,
        parent: Option<ChangesetId>,
        ops: &[(&str, Option<&[u8]>)],
    ) -> ChangesetId {
        let base = match parent {
            Some(p) => e.get_changeset(p).await.unwrap().root,
            None => TestEngine::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (path, content) in ops {
            let c = match content {
                Some(b) => put(e, Kind::File, b).await,
                None => Change::Delete,
            };
            changes.push((RepoPath::new(*path).unwrap(), c));
        }
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        let root = e.apply(base, &changes).await.unwrap();
        let sig = asc_core::sig::Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap();
        e.create_changeset(
            NewChangeset {
                parents: parent.into_iter().collect(),
                author: sig.clone(),
                committer: sig,
                message: b"m\n".to_vec(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap()
    }

    fn row(s: &DiffStatV1, path: &str) -> (u32, u32, bool) {
        let f = s
            .files
            .iter()
            .find(|f| f.path.as_bytes() == path.as_bytes())
            .unwrap_or_else(|| panic!("{path} is not listed"));
        (f.added, f.removed, f.binary)
    }

    #[tokio::test]
    async fn counts_lines_against_the_first_parent() {
        let e = engine();
        let a = mk(
            &e,
            None,
            &[
                ("a.txt", Some(b"1\n2\n3\n")),
                ("gone.txt", Some(b"x\ny\n")),
                ("bin", Some(b"\0\x01")),
            ],
        )
        .await;
        // The root changeset is compared with the empty tree.
        let s = e.diffstat(a).await.unwrap();
        assert_eq!(row(&s, "a.txt"), (3, 0, false));
        assert_eq!(row(&s, "gone.txt"), (2, 0, false));
        assert_eq!(row(&s, "bin"), (0, 0, true));
        assert!(!s.truncated);
        let b = mk(
            &e,
            Some(a),
            &[
                ("a.txt", Some(b"1\nTWO\n3\n4\n")),
                ("gone.txt", None),
                ("new/n.txt", Some(b"n\n")),
            ],
        )
        .await;
        let s = e.diffstat(b).await.unwrap();
        assert_eq!(row(&s, "a.txt"), (2, 1, false));
        assert_eq!(row(&s, "gone.txt"), (0, 2, false));
        assert_eq!(row(&s, "new/n.txt"), (1, 0, false));
        assert_eq!(s.files.len(), 3, "untouched files are not listed");
        let paths: Vec<_> = s.files.iter().map(|f| f.path.clone()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[tokio::test]
    async fn only_the_first_parent_counts_on_a_merge() {
        let e = engine();
        let a = mk(&e, None, &[("f", Some(b"a\n"))]).await;
        let b = mk(&e, Some(a), &[("f", Some(b"a\nb\n"))]).await;
        let c = mk(&e, Some(a), &[("g", Some(b"g\n"))]).await;
        // Merge: b's tree plus c's `g`.
        let root = {
            let g = put(&e, Kind::File, b"g\n").await;
            let base = e.get_changeset(b).await.unwrap().root;
            e.apply(base, &[(RepoPath::new("g").unwrap(), g)])
                .await
                .unwrap()
        };
        let sig = asc_core::sig::Signature::new(b"A".to_vec(), b"a@x".to_vec(), 2, 0).unwrap();
        let m = e
            .create_changeset(
                NewChangeset {
                    parents: vec![b, c],
                    author: sig.clone(),
                    committer: sig,
                    message: b"merge\n".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap();
        let s = e.diffstat(m).await.unwrap();
        assert_eq!(s.files.len(), 1);
        assert_eq!(row(&s, "g"), (1, 0, false));
    }

    #[tokio::test]
    async fn a_mode_only_change_lists_zero_counts() {
        let e = engine();
        let a = mk(&e, None, &[("f", Some(b"a\n"))]).await;
        let f = put(&e, Kind::Exec, b"a\n").await;
        let base = e.get_changeset(a).await.unwrap().root;
        let root = e
            .apply(base, &[(RepoPath::new("f").unwrap(), f)])
            .await
            .unwrap();
        let sig = asc_core::sig::Signature::new(b"A".to_vec(), b"a@x".to_vec(), 2, 0).unwrap();
        let b = e
            .create_changeset(
                NewChangeset {
                    parents: vec![a],
                    author: sig.clone(),
                    committer: sig,
                    message: b"chmod\n".to_vec(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap();
        assert_eq!(row(&e.diffstat(b).await.unwrap(), "f"), (0, 0, false));
    }

    #[tokio::test]
    async fn a_file_over_the_size_bound_counts_as_binary() {
        let e = engine();
        let text: Vec<u8> = (0..(MAX_FILE_BYTES as usize / 8) + 1000)
            .flat_map(|i| {
                format!(
                    "{i:07}
"
                )
                .into_bytes()
            })
            .collect();
        assert!(text.len() as u64 > MAX_FILE_BYTES);
        let a = mk(
            &e,
            None,
            &[
                ("big", Some(&text)),
                (
                    "small",
                    Some(
                        b"x
",
                    ),
                ),
            ],
        )
        .await;
        let s = e.diffstat(a).await.unwrap();
        assert_eq!(row(&s, "big"), (0, 0, true));
        assert_eq!(row(&s, "small"), (1, 0, false));
    }

    #[tokio::test]
    async fn memo_round_trips_and_is_read_without_deriving() {
        let e = engine();
        let a = commit_on(&e, &[], b"one\n").await;
        let b = commit_on(&e, &[a], b"one\ntwo\n").await;
        assert_eq!(e.diffstat_memos(&[a, b]).await.unwrap(), vec![None, None]);
        let s = e.diffstat(b).await.unwrap();
        assert_eq!(row(&s, "f.txt"), (1, 0, false));
        let got = e.diffstat_memos(&[a, b]).await.unwrap();
        assert_eq!(got, vec![None, Some(s.clone())]);
        assert_eq!(DiffStatV1::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn decode_refuses_damaged_values() {
        let s = DiffStatV1 {
            files: vec![FileStat {
                path: RepoPath::new("a/b").unwrap(),
                added: 1,
                removed: 2,
                binary: false,
            }],
            truncated: true,
        };
        let b = s.encode();
        assert_eq!(DiffStatV1::decode(&b).unwrap(), s);
        for cut in 0..b.len() {
            assert!(DiffStatV1::decode(&b[..cut]).is_err(), "cut at {cut}");
        }
        let mut long = b.clone();
        long.push(0);
        assert!(DiffStatV1::decode(&long).is_err());
        // A count past the bound is refused before anything is allocated for it.
        let mut huge = b;
        huge[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(DiffStatV1::decode(&huge).is_err());
        // A path that is not a valid repository path.
        let mut bad = s.encode();
        bad[9] = b'/';
        assert!(DiffStatV1::decode(&bad).is_err());
    }

    #[tokio::test]
    async fn a_huge_change_stops_the_walk_after_the_cap() {
        let e = engine();
        let names: Vec<String> = (0..8)
            .flat_map(|d| (0..25).map(move |f| format!("d{d}/f{f:02}")))
            .collect();
        let ops: Vec<(&str, Option<&[u8]>)> = names
            .iter()
            .map(|n| {
                (
                    n.as_str(),
                    Some(
                        &b"x
"[..],
                    ),
                )
            })
            .collect();
        let a = mk(&e, None, &ops).await;
        let (all, all_visits) = stat_of(&e, a, 1000).await.unwrap();
        assert_eq!((all.files.len(), all.truncated), (200, false));
        assert!(
            all_visits >= 200,
            "a full walk visits every file: {all_visits}"
        );
        let (cut, visits) = stat_of(&e, a, 10).await.unwrap();
        assert_eq!((cut.files.len(), cut.truncated), (10, true));
        // The walk ends at the 11th change inside the first directory; the other seven
        // directories are never opened.
        assert!(
            visits < 30,
            "the walk went on past the cap: {visits} entries"
        );
    }

    #[tokio::test]
    async fn more_than_max_files_lists_max_files_and_marks_the_cut() {
        let e = engine();
        let names: Vec<String> = (0..MAX_FILES + 5).map(|i| format!("f{i:05}")).collect();
        let ops: Vec<(&str, Option<&[u8]>)> = names
            .iter()
            .map(|n| {
                (
                    n.as_str(),
                    Some(
                        &b"x
"[..],
                    ),
                )
            })
            .collect();
        let a = mk(&e, None, &ops).await;
        let (s, visits) = stat_of(&e, a, MAX_FILES).await.unwrap();
        assert_eq!((s.files.len(), s.truncated), (MAX_FILES, true));
        assert!(visits <= MAX_FILES + 3, "{visits}");
    }
}
