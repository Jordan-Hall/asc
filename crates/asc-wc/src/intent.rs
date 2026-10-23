//! The switch intent (SPEC §10.2.8): the record that lets a switch, `reset --hard` or `restore`
//! resume after a crash or a held-open file. `begin` persists it before any path is touched,
//! [`replay`] re-runs the pending paths idempotently, and `clear` removes it once nothing is left.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

use asc_core::id::TreeId;
use asc_core::path::RepoPath;
use asc_state::state::tables::switch_intent;
use asc_state::{StateDb, WorkspaceId};

pub use switch_intent::SwitchIntent;

use crate::plain::{Error, Result};

/// Most paths one intent may list; a larger switch fails before it touches the work tree.
pub const MAX_PENDING: usize = 4_000_000;

/// Persists the intent `from_root -> to_root` with the paths still to write, replacing any earlier
/// intent (SPEC §10.2.8 step 1).
pub async fn begin(
    db: &StateDb,
    ws: WorkspaceId,
    from_root: TreeId,
    to_root: TreeId,
    to_head: &str,
    pending: &BTreeSet<RepoPath>,
) -> Result<()> {
    if pending.len() > MAX_PENDING {
        return Err(invalid(format!(
            "switch touches {} paths, more than the limit of {MAX_PENDING}",
            pending.len()
        )));
    }
    let intent = SwitchIntent {
        from_root,
        to_root,
        to_head: to_head.to_owned(),
        pending: pending.iter().map(|p| p.as_bytes().to_vec()).collect(),
        started: 0,
    };
    Ok(switch_intent::set(db, ws, &intent).await?)
}

/// The stored intent, if a switch was interrupted or left paths pending.
pub async fn load(db: &StateDb, ws: WorkspaceId) -> Result<Option<SwitchIntent>> {
    Ok(switch_intent::get(db, ws).await?)
}

/// Removes the intent.
pub async fn clear(db: &StateDb, ws: WorkspaceId) -> Result<()> {
    Ok(switch_intent::clear(db, ws).await?)
}

/// How one pending path ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The path is at the target version (also when it already was).
    Done,
    /// The file is held open; leave it pending (SPEC §10.2.8 step 5).
    InUse,
}

/// Brings one path to the target version. Must be idempotent: replay calls it again for paths a
/// crashed run may already have written.
pub trait Apply {
    fn apply(&mut self, intent: &SwitchIntent, path: &RepoPath) -> io::Result<Applied>;
}

/// The result of a [`replay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replayed {
    pub applied: usize,
    pub pending: usize,
}

impl Replayed {
    /// Nothing is pending: the caller now rewrites the index and `HEAD`.
    pub fn complete(&self) -> bool {
        self.pending == 0
    }
}

/// Re-runs the stored intent. Applied paths leave the pending list; `InUse` ones stay. The intent is
/// deleted when nothing remains. An `apply` error saves the progress made so far and is returned.
/// `None` means there was no intent.
pub async fn replay(
    db: &StateDb,
    ws: WorkspaceId,
    apply: &mut dyn Apply,
) -> Result<Option<Replayed>> {
    let Some(intent) = load(db, ws).await? else {
        return Ok(None);
    };
    let mut keep: Vec<Vec<u8>> = Vec::new();
    let mut applied = 0usize;
    let mut failure = None;
    for (i, raw) in intent.pending.iter().enumerate() {
        let outcome = RepoPath::new(raw.as_slice())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
            .and_then(|p| apply.apply(&intent, &p));
        match outcome {
            Ok(Applied::Done) => applied += 1,
            Ok(Applied::InUse) => keep.push(raw.clone()),
            Err(e) => {
                keep.extend(intent.pending[i..].iter().cloned());
                failure = Some(e);
                break;
            }
        }
    }
    let pending = keep.len();
    if keep.is_empty() {
        clear(db, ws).await?;
    } else if applied > 0 {
        let rest = SwitchIntent {
            pending: keep,
            ..intent
        };
        switch_intent::set(db, ws, &rest).await?;
    }
    match failure {
        Some(source) => Err(Error::Io {
            path: PathBuf::from("switch_intent"),
            source,
        }),
        None => Ok(Some(Replayed { applied, pending })),
    }
}

/// The FORCE work set `W = leafdiff ∪ dirty(pathspec)` (SPEC §10.2.8 step 2). `leafdiff` is the
/// path list of the outgoing-to-incoming tree diff. In `force` mode (`reset --hard`,
/// `restore --worktree`, `checkout -- <paths>`, `switch -f`) the dirty paths that match `pathspec`
/// are added, so the work happens even when both trees are equal. Otherwise dirty paths are the
/// switch rules' business (refuse or merge) and `W` is the diff alone.
pub fn work_set<'a>(
    leafdiff: impl IntoIterator<Item = &'a RepoPath>,
    dirty: &BTreeSet<RepoPath>,
    force: bool,
    pathspec: &dyn Fn(&RepoPath) -> bool,
) -> BTreeSet<RepoPath> {
    let mut w: BTreeSet<RepoPath> = leafdiff.into_iter().cloned().collect();
    if force {
        w.extend(dirty.iter().filter(|p| pathspec(p)).cloned());
    }
    w
}

fn invalid(msg: String) -> Error {
    Error::Io {
        path: PathBuf::from("switch_intent"),
        source: io::Error::new(io::ErrorKind::InvalidInput, msg),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::journal::fixtures;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn set(paths: &[&str]) -> BTreeSet<RepoPath> {
        paths.iter().map(|s| p(s)).collect()
    }

    /// Records every call; `busy` paths stay in use, `boom` fails once.
    #[derive(Default)]
    struct Fake {
        calls: BTreeMap<RepoPath, u32>,
        busy: BTreeSet<RepoPath>,
        boom: Option<RepoPath>,
    }

    impl Apply for Fake {
        fn apply(&mut self, _: &SwitchIntent, path: &RepoPath) -> io::Result<Applied> {
            *self.calls.entry(path.clone()).or_default() += 1;
            if self.boom.as_ref() == Some(path) {
                self.boom = None;
                return Err(io::Error::other("crash"));
            }
            Ok(if self.busy.contains(path) {
                Applied::InUse
            } else {
                Applied::Done
            })
        }
    }

    fn roots() -> (TreeId, TreeId) {
        (TreeId::from_bytes([1; 32]), TreeId::from_bytes([2; 32]))
    }

    #[tokio::test]
    async fn persists_and_clears() {
        let (_d, db, ws) = fixtures::db().await;
        assert!(load(&db, ws).await.unwrap().is_none());
        let (a, b) = roots();
        begin(&db, ws, a, b, "refs/heads/master", &set(&["x", "d/y"]))
            .await
            .unwrap();
        let got = load(&db, ws).await.unwrap().unwrap();
        assert_eq!((got.from_root, got.to_root), (a, b));
        assert_eq!(got.to_head, "refs/heads/master");
        assert_eq!(got.pending, [b"d/y".to_vec(), b"x".to_vec()]);
        clear(&db, ws).await.unwrap();
        assert!(load(&db, ws).await.unwrap().is_none());
        assert_eq!(replay(&db, ws, &mut Fake::default()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn replay_is_idempotent_and_keeps_busy_paths() {
        let (_d, db, ws) = fixtures::db().await;
        let (a, b) = roots();
        begin(&db, ws, a, b, "", &set(&["a", "b", "c"]))
            .await
            .unwrap();
        let mut fake = Fake {
            busy: set(&["b"]),
            ..Fake::default()
        };
        let r = replay(&db, ws, &mut fake).await.unwrap().unwrap();
        assert_eq!(
            r,
            Replayed {
                applied: 2,
                pending: 1
            }
        );
        assert!(!r.complete());
        // Running it again touches only what is still pending and ends in the same state.
        let r2 = replay(&db, ws, &mut fake).await.unwrap().unwrap();
        assert_eq!(
            r2,
            Replayed {
                applied: 0,
                pending: 1
            }
        );
        assert_eq!(fake.calls[&p("a")], 1);
        assert_eq!(fake.calls[&p("b")], 2);
        assert_eq!(
            load(&db, ws).await.unwrap().unwrap().pending,
            [b"b".to_vec()]
        );

        fake.busy.clear();
        let r3 = replay(&db, ws, &mut fake).await.unwrap().unwrap();
        assert!(r3.complete());
        assert!(load(&db, ws).await.unwrap().is_none());
        assert_eq!(replay(&db, ws, &mut fake).await.unwrap(), None);
    }

    #[tokio::test]
    async fn crash_mid_replay_saves_progress_and_resumes() {
        let (_d, db, ws) = fixtures::db().await;
        let (a, b) = roots();
        begin(&db, ws, a, b, "", &set(&["a", "b", "c"]))
            .await
            .unwrap();
        let mut fake = Fake {
            boom: Some(p("b")),
            ..Fake::default()
        };
        assert!(replay(&db, ws, &mut fake).await.is_err());
        assert_eq!(
            load(&db, ws).await.unwrap().unwrap().pending,
            [b"b".to_vec(), b"c".to_vec()]
        );
        let r = replay(&db, ws, &mut fake).await.unwrap().unwrap();
        assert!(r.complete() && r.applied == 2);
        assert_eq!(fake.calls[&p("a")], 1);
    }

    #[tokio::test]
    async fn rejects_oversized_and_corrupt_intents() {
        let (_d, db, ws) = fixtures::db().await;
        let (a, b) = roots();
        // A hand-edited intent with an escaping path is refused, not applied.
        switch_intent::set(
            &db,
            ws,
            &SwitchIntent {
                from_root: a,
                to_root: b,
                to_head: String::new(),
                pending: vec![b"../evil".to_vec()],
                started: 0,
            },
        )
        .await
        .unwrap();
        let mut fake = Fake::default();
        assert!(replay(&db, ws, &mut fake).await.is_err());
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn work_set_for_reset_hard_on_an_unchanged_tree_is_the_dirty_paths() {
        let dirty = set(&["a", "src/b", "docs/c"]);
        let none: Vec<RepoPath> = Vec::new();
        let all = |_: &RepoPath| true;
        assert_eq!(work_set(&none, &dirty, true, &all), dirty);
        // Limited to the pathspec.
        let src = |q: &RepoPath| q.as_bytes().starts_with(b"src/");
        assert_eq!(work_set(&none, &dirty, true, &src), set(&["src/b"]));
        // Not forced: only the diff.
        assert!(work_set(&none, &dirty, false, &all).is_empty());
        // The diff is always included.
        let diff = [p("z"), p("a")];
        assert_eq!(
            work_set(&diff, &dirty, true, &all),
            set(&["a", "src/b", "docs/c", "z"])
        );
        assert_eq!(work_set(&diff, &dirty, false, &all), set(&["a", "z"]));
    }
}
