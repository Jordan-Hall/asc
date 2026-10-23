//! `asc absorb` (SPEC §20.7): assigns each changed hunk of a work tree to the mutable change it
//! belongs to, then rewrites those changes and restacks what sits above them.
//!
//! **Where a hunk goes.** The line origins come from `blame.v1` of the file at HEAD. A hunk that
//! replaces or deletes lines belongs to the change that introduced every one of those lines; a
//! pure insertion belongs to the change that introduced the lines on both sides of it (or the
//! one side that exists). A hunk whose lines come from several changes is ambiguous, and one
//! whose origin is not a candidate (immutable, off the stack, excluded by `--into`) cannot move.
//! Both stay where they are. Changes between the origin and HEAD that did not touch those
//! lines commute with the hunk by construction; one that touches the lines next to them shows
//! up as a conflict when the hunk is merged or the changes above are restacked, and then the
//! hunk stays too.
//!
//! **Applying.** The hunks of one target are applied to HEAD's version of the file, and that
//! result is three-way merged into the target's own version (base: HEAD's version). The new
//! target replaces the old one, keeping author, committer, message and parents, and the
//! changes above are restacked onto it by [`Engine::execute_restack`]. A group of hunks whose
//! merge or restack conflicts is left out whole; nothing is half applied. The caller moves the
//! refs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use bytes::Bytes;
use asc_core::git::EXTRA_GIT_GPGSIG;
use asc_core::id::ChangesetId;
use asc_core::object::{Change, Kind};
use asc_core::path::RepoPath;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use super::restack::{self, CommitSigner, Outcome, Pause, TxnMeta};
use crate::Engine;
use crate::blame::{self, BlameApi};
use crate::commit::{self, NewChangeset, PathPolicy};
use crate::diff::line::{self, Algorithm};
use crate::merge::{self, MergeOptions, is_binary};

/// Most files one absorb looks at.
pub const MAX_FILES: usize = 4096;
/// Largest file side that is diffed.
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Most hunks one absorb handles.
pub const MAX_HUNKS: usize = 20_000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Restack(#[from] restack::Error),
    #[error(transparent)]
    Blame(#[from] blame::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Merge(#[from] merge::Error),
    #[error(transparent)]
    Tree(#[from] crate::tree::Error),
    #[error(transparent)]
    Object(#[from] asc_core::object::ObjectError),
    #[error("signing failed: {0}")]
    Sign(String),
    #[error("{0} exceeds the limit of {1}")]
    TooMany(&'static str, usize),
    #[error("hunk does not fit {0}")]
    BadHunk(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// One file whose content differs from HEAD's: HEAD's side and the new side.
#[derive(Clone, Debug)]
pub struct FileEdit {
    pub path: RepoPath,
    pub old: Bytes,
    pub new: Bytes,
}

/// One changed range of a file, in HEAD's line numbering (zero-based, half-open).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub path: RepoPath,
    pub old_start: u32,
    pub old_len: u32,
    pub new_len: u32,
    /// The lines that replace the old range, as they are in the file (newlines included).
    pub replacement: Vec<u8>,
}

/// Why a hunk (or file) stays where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stay {
    Binary,
    TooLarge,
    /// The lines come from several changes, or the two neighbours of an insertion do.
    Ambiguous,
    /// The change that owns the lines is immutable, off this stack or outside `--into`.
    NotCandidate,
    /// There are no lines to anchor the hunk to.
    NoContext,
    /// The merge into the target, or the restack above it, conflicts.
    Conflict,
    /// The file is not at HEAD or its history cannot be followed.
    NoHistory,
}

impl Stay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::TooLarge => "too-large",
            Self::Ambiguous => "ambiguous",
            Self::NotCandidate => "not-a-candidate",
            Self::NoContext => "no-context",
            Self::Conflict => "conflict",
            Self::NoHistory => "no-history",
        }
    }
}

/// A hunk and where it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub hunk: Hunk,
    pub target: std::result::Result<ChangesetId, Stay>,
}

/// The assignment of every hunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AbsorbPlan {
    pub decisions: Vec<Decision>,
    /// Files left out whole.
    pub skipped: Vec<(RepoPath, Stay)>,
}

/// One rewritten change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewritten {
    pub old: ChangesetId,
    pub new: ChangesetId,
    /// The change took hunks (rather than only being restacked).
    pub target: bool,
}

/// The result of [`Engine::execute_absorb`]. Stored objects only; no ref has moved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Executed {
    /// Oldest first.
    pub rewrites: Vec<Rewritten>,
}

/// The hunks that turn `old` into `new`, in `old`'s numbering.
pub fn hunks_of(path: &RepoPath, old: &[u8], new: &[u8]) -> Vec<Hunk> {
    let d = line::diff(old, new, Algorithm::Myers, true);
    let span = |r: &Range<u32>| r.start as usize..r.end as usize;
    d.hunks
        .iter()
        .map(|h| Hunk {
            path: path.clone(),
            old_start: h.before.start,
            old_len: h.before.end - h.before.start,
            new_len: h.after.end - h.after.start,
            replacement: d.new[span(&h.after)].concat(),
        })
        .collect()
}

/// `old` with `hunks` (of the same file, non-overlapping) applied.
pub fn apply_hunks(old: &[u8], hunks: &[&Hunk]) -> Result<Vec<u8>> {
    let lines = line::split_lines(old);
    let mut order: Vec<&&Hunk> = hunks.iter().collect();
    order.sort_by_key(|h| h.old_start);
    let mut out = Vec::with_capacity(old.len());
    let mut at = 0usize;
    for h in order {
        let (start, end) = (h.old_start as usize, (h.old_start + h.old_len) as usize);
        if start < at || end > lines.len() {
            return Err(Error::BadHunk(
                String::from_utf8_lossy(h.path.as_bytes()).into_owned(),
            ));
        }
        out.extend(lines[at..start].iter().flat_map(|l| l.iter().copied()));
        out.extend_from_slice(&h.replacement);
        at = end;
    }
    out.extend(lines[at..].iter().flat_map(|l| l.iter().copied()));
    Ok(out)
}

/// The lines of the blame whose origins decide where `h` goes.
fn anchor(h: &Hunk, lines: u32) -> Range<u32> {
    if h.old_len > 0 {
        return h.old_start..(h.old_start + h.old_len).min(lines);
    }
    // An insertion: the line before and the line after.
    h.old_start.saturating_sub(1).min(lines)..(h.old_start + 1).min(lines)
}

fn decide(
    h: &Hunk,
    blame: &blame::Blame,
    candidates: &BTreeSet<ChangesetId>,
) -> std::result::Result<ChangesetId, Stay> {
    let range = anchor(h, blame.lines());
    if range.is_empty() {
        return Err(Stay::NoContext);
    }
    let origins: BTreeSet<ChangesetId> = blame
        .runs()
        .iter()
        .filter(|r| r.start < range.end && r.start + r.len > range.start)
        .map(|r| r.origin)
        .collect();
    let mut it = origins.into_iter();
    match (it.next(), it.next()) {
        (Some(one), None) if candidates.contains(&one) => Ok(one),
        (Some(_), None) => Err(Stay::NotCandidate),
        (Some(_), Some(_)) => Err(Stay::Ambiguous),
        (None, _) => Err(Stay::NoContext),
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Assigns every hunk of `files` (HEAD `head` against the new side) to one of
    /// `candidates`, or leaves it. Reads history only; stores nothing.
    pub async fn plan_absorb(
        &self,
        head: ChangesetId,
        candidates: &BTreeSet<ChangesetId>,
        files: &[FileEdit],
    ) -> Result<AbsorbPlan> {
        if files.len() > MAX_FILES {
            return Err(Error::TooMany("files", MAX_FILES));
        }
        let mut plan = AbsorbPlan::default();
        let mut total = 0usize;
        for f in files {
            if f.old.len() > MAX_FILE_BYTES || f.new.len() > MAX_FILE_BYTES {
                plan.skipped.push((f.path.clone(), Stay::TooLarge));
                continue;
            }
            if is_binary(&f.old) || is_binary(&f.new) {
                plan.skipped.push((f.path.clone(), Stay::Binary));
                continue;
            }
            let hunks = hunks_of(&f.path, &f.old, &f.new);
            if hunks.is_empty() {
                continue;
            }
            total += hunks.len();
            if total > MAX_HUNKS {
                return Err(Error::TooMany("hunks", MAX_HUNKS));
            }
            let blame = match self.blame(head, &f.path, None).await {
                Ok(b) if b.lines() as usize == line::split_lines(&f.old).len() => b,
                Ok(_) | Err(blame::Error::NotFound(_)) => {
                    plan.skipped.push((f.path.clone(), Stay::NoHistory));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            for hunk in hunks {
                let target = decide(&hunk, &blame, candidates);
                plan.decisions.push(Decision { hunk, target });
            }
        }
        Ok(plan)
    }

    /// Applies `plan`: rewrites each target of `chain` (oldest first, ending at HEAD) with its
    /// hunks and restacks the changes above. A group that conflicts is set to
    /// [`Stay::Conflict`] in `plan` and left out. Stores objects; moves no ref.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_absorb(
        &self,
        plan: &mut AbsorbPlan,
        chain: &[ChangesetId],
        files: &[FileEdit],
        signer: Option<&dyn CommitSigner>,
        opts: &MergeOptions,
        policy: PathPolicy,
        meta: &TxnMeta,
    ) -> Result<Executed> {
        let by_path: HashMap<&RepoPath, &FileEdit> = files.iter().map(|f| (&f.path, f)).collect();
        let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, d) in plan.decisions.iter_mut().enumerate() {
            let Ok(t) = d.target else { continue };
            match chain.iter().position(|&c| c == t) {
                Some(pos) => groups.entry(pos).or_default().push(i),
                None => d.target = Err(Stay::NotCandidate),
            }
        }
        let mut cur: Vec<ChangesetId> = chain.to_vec();
        let mut targets: BTreeSet<usize> = BTreeSet::new();
        for (pos, idx) in groups {
            let hunks: Vec<&Hunk> = idx.iter().map(|&i| &plan.decisions[i].hunk).collect();
            let done = self
                .absorb_group(
                    cur[pos],
                    &cur[pos + 1..],
                    &hunks,
                    &by_path,
                    signer,
                    opts,
                    policy,
                    meta,
                )
                .await?;
            match done {
                Some(rewrites) => {
                    // `rewrites[0]` is the new target; the rest are the changes above it.
                    for (k, new) in rewrites.into_iter().enumerate() {
                        cur[pos + k] = new;
                    }
                    targets.insert(pos);
                }
                None => {
                    for i in idx {
                        plan.decisions[i].target = Err(Stay::Conflict);
                    }
                }
            }
        }
        let rewrites = chain
            .iter()
            .zip(&cur)
            .enumerate()
            .filter(|(_, (o, n))| o != n)
            .map(|(i, (&old, &new))| Rewritten {
                old,
                new,
                target: targets.contains(&i),
            })
            .collect();
        Ok(Executed { rewrites })
    }

    /// Folds `hunks` into `target` and restacks `above` onto it. `None` when the merge or the
    /// restack conflicts; otherwise the new target followed by the new `above`.
    #[allow(clippy::too_many_arguments)]
    async fn absorb_group(
        &self,
        target: ChangesetId,
        above: &[ChangesetId],
        hunks: &[&Hunk],
        files: &HashMap<&RepoPath, &FileEdit>,
        signer: Option<&dyn CommitSigner>,
        opts: &MergeOptions,
        policy: PathPolicy,
        meta: &TxnMeta,
    ) -> Result<Option<Vec<ChangesetId>>> {
        let cs = self.get_changeset(target).await?;
        let mut per_file: BTreeMap<&RepoPath, Vec<&Hunk>> = BTreeMap::new();
        for h in hunks {
            per_file.entry(&h.path).or_default().push(h);
        }
        let mut changes = Vec::with_capacity(per_file.len());
        for (path, hs) in per_file {
            let Some(file) = files.get(path) else {
                return Ok(None);
            };
            let theirs = apply_hunks(&file.old, &hs)?;
            let Some(entry) = self.lookup(cs.root, path).await? else {
                return Ok(None);
            };
            if !matches!(entry.kind, Kind::File | Kind::Exec) || entry.size > MAX_FILE_BYTES as u64
            {
                return Ok(None);
            }
            let ours = self.read_blob(&entry).await?;
            if is_binary(&ours) {
                return Ok(None);
            }
            let merged = merge::merge_text(&file.old, &ours, &theirs, opts, false);
            if merged.conflicted {
                return Ok(None);
            }
            let blob = self.store_blob(&merged.bytes).await?;
            changes.push((
                path.clone(),
                Change::write(entry.kind, blob.file, blob.size, blob.git, None)?,
            ));
        }
        let root = self.apply(cs.root, &changes).await?;
        let copies: Vec<(RepoPath, RepoPath)> = cs
            .changes
            .as_slice()
            .iter()
            .filter_map(|(dest, c)| match c {
                Change::Write {
                    copy_from: Some(src),
                    ..
                } => Some((dest.clone(), src.clone())),
                _ => None,
            })
            .collect();
        let extra: Vec<(Vec<u8>, Vec<u8>)> = cs
            .extra
            .as_slice()
            .iter()
            .filter(|(k, _)| k.as_slice() != EXTRA_GIT_GPGSIG)
            .cloned()
            .collect();
        let new = NewChangeset {
            parents: cs.parents.clone(),
            author: cs.author.clone(),
            committer: cs.committer.clone(),
            message: cs.message.clone(),
            extra,
            root,
            copies,
        };
        let mut id = match self.create_changeset(new.clone(), policy).await {
            Ok(id) => id,
            // The edit undid the copy's write; the copy record goes with it.
            Err(commit::Error::CopyNotWrite(_)) => {
                self.create_changeset(
                    NewChangeset {
                        copies: Vec::new(),
                        ..new
                    },
                    policy,
                )
                .await?
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(signer) = signer {
            let unsigned = self.get_changeset(id).await?;
            match self.sign_changeset(&unsigned, signer).await? {
                Ok(signed) => id = self.store_rewrite(signed).await?,
                Err(e) => return Err(Error::Sign(e.0)),
            }
        }
        let mut out = vec![id];
        if above.is_empty() {
            return Ok(Some(out));
        }
        let plan = self.plan_restack(above, id).await?;
        match self.execute_restack(&plan, &[], signer, opts, meta).await? {
            Outcome::Complete { rewrites, .. } => {
                out.extend(rewrites.iter().map(|r| r.new));
                Ok(Some(out))
            }
            Outcome::Paused {
                cause: Pause::Sign(m),
                ..
            } => Err(Error::Sign(m)),
            Outcome::Paused { .. } => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use asc_store::refs::Actor;

    use super::*;
    use crate::merge::testutil::{TestEngine, child, commit, engine, files, p, read};

    fn meta() -> TxnMeta {
        TxnMeta {
            actor: Actor {
                id: "u".into(),
                via: "test".into(),
            },
            reason: "absorb".into(),
            op_id: uuid::Uuid::nil(),
        }
    }

    fn policy() -> PathPolicy {
        PathPolicy::Native {
            windows_safe: false,
        }
    }

    const BASE: &str = "a1\na2\na3\na4\na5\na6\n";

    /// base (a file of 6 lines) <- s1 (edits line 2) <- s2 (appends b1) <- s3 (edits line 5).
    async fn stack() -> (TestEngine, ChangesetId, Vec<ChangesetId>) {
        let e = engine();
        let root = files(&e, &[("f", BASE)]).await;
        let base = commit(&e, &[], root, "base").await;
        let s1 = child(&e, base, &[("f", "a1\nS1\na3\na4\na5\na6\n")], Vec::new()).await;
        let s2 = child(&e, s1, &[("f", "a1\nS1\na3\na4\na5\na6\nS2\n")], Vec::new()).await;
        let s3 = child(&e, s2, &[("f", "a1\nS1\na3\na4\nS3\na6\nS2\n")], Vec::new()).await;
        (e, base, vec![s1, s2, s3])
    }

    async fn file_of(e: &TestEngine, head: ChangesetId, new: &str) -> FileEdit {
        let root = e.get_changeset(head).await.unwrap().root;
        FileEdit {
            path: p("f"),
            old: read(e, root, "f").await.unwrap().into(),
            new: Bytes::from(new.to_owned()),
        }
    }

    fn set(v: &[ChangesetId]) -> BTreeSet<ChangesetId> {
        v.iter().copied().collect()
    }

    #[tokio::test]
    async fn each_hunk_goes_to_the_change_that_owns_its_lines() {
        let (e, _, st) = stack().await;
        // Edits line 2 (S1) and line 5 (S3) and line 7 (S2).
        let f = file_of(&e, st[2], "a1\nS1x\na3\na4\nS3x\na6\nS2x\n").await;
        let plan = e.plan_absorb(st[2], &set(&st), &[f]).await.unwrap();
        let got: Vec<_> = plan
            .decisions
            .iter()
            .map(|d| (d.hunk.old_start, d.target))
            .collect();
        assert_eq!(got, vec![(1, Ok(st[0])), (4, Ok(st[2])), (6, Ok(st[1]))]);
    }

    #[tokio::test]
    async fn an_immutable_or_excluded_owner_cannot_take_the_hunk() {
        let (e, _, st) = stack().await;
        // Line 1 belongs to the base, which is not a candidate.
        let f = file_of(&e, st[2], "a1x\nS1\na3\na4\nS3\na6\nS2\n").await;
        let plan = e
            .plan_absorb(st[2], &set(&st), std::slice::from_ref(&f))
            .await
            .unwrap();
        assert_eq!(plan.decisions[0].target, Err(Stay::NotCandidate));
        let plan = e
            .plan_absorb(
                st[2],
                &set(&st[1..]),
                &[file_of(&e, st[2], "a1\nS1x\na3\na4\nS3\na6\nS2\n").await],
            )
            .await
            .unwrap();
        assert_eq!(plan.decisions[0].target, Err(Stay::NotCandidate));
    }

    #[tokio::test]
    async fn a_hunk_over_lines_of_two_changes_is_ambiguous() {
        let (e, _, st) = stack().await;
        let f = file_of(&e, st[2], "a1\nS1\na3\na4\nS3\nZ\nZ2\n").await;
        let plan = e.plan_absorb(st[2], &set(&st), &[f]).await.unwrap();
        assert_eq!(plan.decisions.len(), 1);
        assert_eq!(plan.decisions[0].target, Err(Stay::Ambiguous));
    }

    #[tokio::test]
    async fn an_insertion_follows_both_neighbours() {
        let (e, _, st) = stack().await;
        // Between S3 (line 5) and a6 (base): the neighbours differ.
        let f = file_of(&e, st[2], "a1\nS1\na3\na4\nS3\nNEW\na6\nS2\n").await;
        let plan = e.plan_absorb(st[2], &set(&st), &[f]).await.unwrap();
        assert_eq!(plan.decisions[0].target, Err(Stay::Ambiguous));
        // After the last line (S2): one neighbour.
        let f = file_of(&e, st[2], "a1\nS1\na3\na4\nS3\na6\nS2\nEND\n").await;
        let plan = e.plan_absorb(st[2], &set(&st), &[f]).await.unwrap();
        assert_eq!(plan.decisions[0].target, Ok(st[1]));
    }

    #[tokio::test]
    async fn binary_and_oversized_files_are_skipped_whole() {
        let (e, _, st) = stack().await;
        let bin = FileEdit {
            path: p("f"),
            old: Bytes::from_static(b"a\0b"),
            new: Bytes::from_static(b"a\0c"),
        };
        let plan = e.plan_absorb(st[2], &set(&st), &[bin]).await.unwrap();
        assert_eq!(plan.skipped, vec![(p("f"), Stay::Binary)]);
        assert!(plan.decisions.is_empty());
    }

    #[tokio::test]
    async fn applying_hunks_reproduces_the_new_side() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\ne\n";
        let hs = hunks_of(&p("f"), old.as_bytes(), new.as_bytes());
        assert_eq!(hs.len(), 2);
        let refs: Vec<&Hunk> = hs.iter().collect();
        assert_eq!(apply_hunks(old.as_bytes(), &refs).unwrap(), new.as_bytes());
        assert_eq!(
            apply_hunks(old.as_bytes(), &refs[..1]).unwrap(),
            b"a\nB\nc\nd\n"
        );
        assert!(apply_hunks(b"a\n", &refs).is_err());
    }

    #[tokio::test]
    async fn executing_folds_each_hunk_into_its_owner_and_restacks() {
        let (e, base, st) = stack().await;
        let new = "a1\nS1x\na3\na4\nS3x\na6\nS2x\n";
        let f = file_of(&e, st[2], new).await;
        let mut plan = e
            .plan_absorb(st[2], &set(&st), std::slice::from_ref(&f))
            .await
            .unwrap();
        let done = e
            .execute_absorb(
                &mut plan,
                &st,
                &[f],
                None,
                &MergeOptions::default(),
                policy(),
                &meta(),
            )
            .await
            .unwrap();
        assert_eq!(done.rewrites.len(), 3);
        assert!(done.rewrites.iter().all(|r| r.target));
        // The tip holds the whole edit; each change holds only its own.
        let tip = e.get_changeset(done.rewrites[2].new).await.unwrap();
        assert_eq!(read(&e, tip.root, "f").await.unwrap(), new.as_bytes());
        let first = e.get_changeset(done.rewrites[0].new).await.unwrap();
        assert_eq!(first.parents, vec![base]);
        assert_eq!(
            read(&e, first.root, "f").await.unwrap(),
            b"a1\nS1x\na3\na4\na5\na6\n"
        );
        let second = e.get_changeset(done.rewrites[1].new).await.unwrap();
        assert_eq!(second.parents, vec![done.rewrites[0].new]);
        assert_eq!(
            read(&e, second.root, "f").await.unwrap(),
            b"a1\nS1x\na3\na4\na5\na6\nS2x\n"
        );
        // Author and message survive.
        assert_eq!(first.message, e.get_changeset(st[0]).await.unwrap().message);
    }

    #[tokio::test]
    async fn only_the_changes_from_the_lowest_target_up_are_rewritten() {
        let (e, _, st) = stack().await;
        let f = file_of(&e, st[2], "a1\nS1\na3\na4\nS3x\na6\nS2\n").await;
        let mut plan = e
            .plan_absorb(st[2], &set(&st), std::slice::from_ref(&f))
            .await
            .unwrap();
        let done = e
            .execute_absorb(
                &mut plan,
                &st,
                &[f],
                None,
                &MergeOptions::default(),
                policy(),
                &meta(),
            )
            .await
            .unwrap();
        assert_eq!(
            done.rewrites,
            vec![Rewritten {
                old: st[2],
                new: done.rewrites[0].new,
                target: true
            }]
        );
    }

    #[tokio::test]
    async fn a_conflicting_group_stays_and_nothing_is_rewritten() {
        let e = engine();
        let root = files(&e, &[("f", "1\n2\n3\n4\n5\n6\n7\n8\n")]).await;
        let base = commit(&e, &[], root, "base").await;
        let s1 = child(&e, base, &[("f", "1\nS1\n3\n4\n5\n6\n7\n8\n")], Vec::new()).await;
        // s2 deletes line 3, which sits next to s1's line.
        let s2 = child(&e, s1, &[("f", "1\nS1\n4\n5\n6\n7\n8\n")], Vec::new()).await;
        let st = vec![s1, s2];
        let f = file_of(&e, s2, "1\nS1x\n4\n5\n6\n7\n8x\n").await;
        let mut plan = e
            .plan_absorb(s2, &set(&st), std::slice::from_ref(&f))
            .await
            .unwrap();
        assert_eq!(plan.decisions[0].target, Ok(s1));
        let done = e
            .execute_absorb(
                &mut plan,
                &st,
                &[f],
                None,
                &MergeOptions::default(),
                policy(),
                &meta(),
            )
            .await
            .unwrap();
        // The S1 line hunk collides with s2's deletion next to it; the last line is the base's.
        assert_eq!(plan.decisions[0].target, Err(Stay::Conflict));
        assert_eq!(plan.decisions[1].target, Err(Stay::NotCandidate));
        assert!(done.rewrites.is_empty());
    }
}
