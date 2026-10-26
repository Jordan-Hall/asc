//! The commit-cloud upload pipeline (SPEC §21.3.1, §21.3.2).
//!
//! One queue entry at a time: the guard runs over the new changesets first, then the closure is
//! sent (`HasObjects`, then `PutObjects` of the missing ids only, files and chunks first, trees
//! bottom-up, changesets last), the changesets are ingested without a ref, the head set is synced
//! through [`HeadSync`], the queue row is deleted and the sent objects are demoted.
//!
//! The upload itself is [`Engine::push`] in `NoRef` mode, so every step is idempotent: a killed
//! upload resumes with `HasObjects` and sends only what is still missing, and a race with
//! `push` costs one repeated `HasObjects`. The queue rows are reached only through
//! [`UploadQueue`], which the daemon implements over asc-state, so asc-engine takes no dependency
//! on asc-state. Everything is bounded: new changesets, scanned files, files read per chunk of
//! the guard walk and the memory of the batch window.

use std::collections::HashSet;

use asc_core::id::{ChangesetId, FileId, RefTarget};
use asc_core::object::{Change, Changeset};
use asc_core::path::{RefName, RepoPath};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Expect, Refs};
use async_trait::async_trait;

use super::guard::{self, AddedBlob, Guard, GuardVerdict, Hold, MAX_HOLDS, MAX_SCAN_BLOB};
use crate::Engine;
use crate::sync::{self, HAS_BATCH, PushMode, PushReport, PushUpdate, Remote, SyncOptions};

/// New changesets one upload may cover (`asc.cloud.maxChangesetSize` bounds the bytes).
pub const MAX_NEW_CHANGESETS: usize = 100_000;
/// `asc.cloud.maxChangesetSize` default: 1 GiB of new content.
pub const DEFAULT_MAX_SIZE: u64 = 1024 * 1024 * 1024;
/// Changeset ids per closure-walk request (SPEC §21.3.1 step 2).
pub const WALK_BATCH: usize = 256;
/// Blobs the guard reads at a time; with [`MAX_SCAN_BLOB`] this bounds its memory.
const GUARD_CHUNK: usize = 16;

/// Failures from the upload pipeline.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sync(#[from] sync::Error),
    #[error(transparent)]
    Guard(#[from] guard::Error),
    #[error(transparent)]
    Branchless(#[from] super::Error),
    #[error("reading {0}: {1}")]
    Read(FileId, String),
    #[error("{0} is missing locally")]
    Missing(ChangesetId),
    #[error("{0} exceeds the limit of {1}")]
    TooLarge(&'static str, usize),
    /// The queue or the head sync failed.
    #[error("upload queue: {0}")]
    Queue(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where a queue entry stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueState {
    Pending,
    /// The guard held it; it waits for `asc cloud release`.
    Held,
    /// Over `asc.cloud.maxChangesetSize`; `push` still uploads it.
    TooLarge,
}

/// One `cloud_queue` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEntry {
    pub cs: ChangesetId,
    pub state: QueueState,
    pub attempts: u32,
}

/// The `cloud_queue` rows, implemented by the daemon over asc-state.
#[async_trait]
pub trait UploadQueue: Send + Sync {
    /// The oldest `pending` row whose `next_at <= now`.
    async fn next_pending(&self, now: i64) -> Result<Option<QueueEntry>>;
    /// Records a guard or size outcome for `cs`.
    async fn set_state(&self, cs: ChangesetId, state: QueueState) -> Result<()>;
    /// Removes the row of an uploaded changeset.
    async fn delete(&self, cs: ChangesetId) -> Result<()>;
}

/// Step 5 of the pipeline: syncs the head set with one idempotent `Cloud.UpdateWorkspace`.
#[async_trait]
pub trait HeadSync: Send + Sync {
    async fn sync_heads(&self, uploaded: ChangesetId) -> Result<()>;
}

/// What an upload needs besides the engine and the remote.
pub struct UploadOptions<'a> {
    pub guard: &'a Guard,
    /// Answers the `.gitignore`/`.ascignore` rules.
    pub ignored: &'a dyn Fn(&RepoPath) -> bool,
    /// `asc.cloud.maxChangesetSize`, in bytes of new content.
    pub max_size: u64,
    /// Skip the guard: `asc cloud release`, or `push --no-secret-check`.
    pub skip_guard: bool,
    /// Upload past `max_size`: `push` still uploads a `too_large` entry.
    pub ignore_size: bool,
    /// Pushed objects are demoted through `sync.demote`; `sync.ext` adds `m/` objects.
    pub sync: SyncOptions<'a>,
    pub heads: &'a dyn HeadSync,
}

/// What one upload did.
#[derive(Debug)]
pub enum Outcome {
    /// Uploaded, ingested and head-synced.
    Uploaded(PushReport),
    /// The guard held the changeset; nothing was sent.
    Held(GuardVerdict),
    /// Over the size bound; nothing was sent.
    TooLarge { bytes: u64, max: u64 },
}

/// The new changesets under `head`, and the guard and size result over them.
struct Scan {
    new: usize,
    bytes: u64,
    verdict: GuardVerdict,
}

fn push_update(cs: ChangesetId) -> PushUpdate {
    PushUpdate {
        // `NoRef` mode never sends this name.
        dst: RefName::new("refs/heads/cloud-upload").expect("a constant ref name"),
        new: Some(RefTarget::Changeset(cs)),
        expect: Expect::Any,
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// Uploads `head` and its closure. Held and oversized changesets are reported, not sent.
    pub async fn upload_draft(
        &self,
        remote: &dyn Remote,
        head: ChangesetId,
        opts: &UploadOptions<'_>,
    ) -> Result<Outcome> {
        let scan = self.scan_new(remote, head, opts).await?;
        if !scan.verdict.is_clear() && !opts.skip_guard {
            return Ok(Outcome::Held(scan.verdict));
        }
        if scan.bytes > opts.max_size && !opts.ignore_size {
            return Ok(Outcome::TooLarge {
                bytes: scan.bytes,
                max: opts.max_size,
            });
        }
        let report = self
            .push(remote, &[push_update(head)], PushMode::NoRef, &opts.sync)
            .await?;
        debug_assert!(report.changesets <= scan.new);
        opts.heads.sync_heads(head).await?;
        Ok(Outcome::Uploaded(report))
    }

    /// Takes the next due queue entry and uploads it, then updates the queue. `None` when no
    /// entry is due. A failure leaves the row for the caller's backoff.
    pub async fn drain_one(
        &self,
        remote: &dyn Remote,
        queue: &dyn UploadQueue,
        now: i64,
        opts: &UploadOptions<'_>,
    ) -> Result<Option<(ChangesetId, Outcome)>> {
        let Some(entry) = queue.next_pending(now).await? else {
            return Ok(None);
        };
        let outcome = self.upload_draft(remote, entry.cs, opts).await?;
        match &outcome {
            Outcome::Uploaded(_) => queue.delete(entry.cs).await?,
            Outcome::Held(_) => queue.set_state(entry.cs, QueueState::Held).await?,
            Outcome::TooLarge { .. } => queue.set_state(entry.cs, QueueState::TooLarge).await?,
        }
        Ok(Some((entry.cs, outcome)))
    }

    /// Walks from `head` back to the changesets the remote has, level by level in batches,
    /// running the guard and counting new bytes as it goes. Nothing but one level of ids and the
    /// set of seen ids is kept.
    async fn scan_new(
        &self,
        remote: &dyn Remote,
        head: ChangesetId,
        opts: &UploadOptions<'_>,
    ) -> Result<Scan> {
        let mut seen: HashSet<ChangesetId> = HashSet::from([head]);
        let mut files: HashSet<FileId> = HashSet::new();
        let mut frontier = vec![head];
        let mut scan = Scan {
            new: 0,
            bytes: 0,
            verdict: GuardVerdict::Clear,
        };
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for batch in frontier.chunks(WALK_BATCH.min(HAS_BATCH)) {
                let keys: Vec<ObjKey> = batch.iter().map(|&c| ObjKey::Changeset(c)).collect();
                let has = remote.has_many(&keys).await?;
                if has.len() != keys.len() {
                    return Err(
                        sync::Error::Remote("has_many answered the wrong count".into()).into(),
                    );
                }
                for (&cs, _) in batch.iter().zip(&has).filter(|(_, h)| !**h) {
                    scan.new += 1;
                    if scan.new > MAX_NEW_CHANGESETS {
                        return Err(Error::TooLarge("new changesets", MAX_NEW_CHANGESETS));
                    }
                    let body = self
                        .get_changeset(cs)
                        .await
                        .map_err(|_| Error::Missing(cs))?;
                    self.check_body(&body, &mut files, &mut scan, opts).await?;
                    for &p in &body.parents {
                        if seen.insert(p) {
                            next.push(p);
                        }
                    }
                }
            }
            frontier = next;
        }
        Ok(scan)
    }

    async fn check_body(
        &self,
        body: &Changeset,
        files: &mut HashSet<FileId>,
        scan: &mut Scan,
        opts: &UploadOptions<'_>,
    ) -> Result<()> {
        let mut added: Vec<(&RepoPath, FileId, u64)> = Vec::new();
        for (path, change) in body.changes.as_slice() {
            if let Change::Write { file, size, .. } = change {
                if files.insert(*file) {
                    scan.bytes = scan.bytes.saturating_add(*size);
                }
                added.push((path, *file, *size));
                if files.len() > guard::MAX_FILES {
                    return Err(Error::TooLarge("added files", guard::MAX_FILES));
                }
            }
        }
        if opts.skip_guard {
            return Ok(());
        }
        for group in added.chunks(GUARD_CHUNK) {
            let mut loaded = Vec::with_capacity(group.len());
            for &(path, file, size) in group {
                // Blobs over the scan limit are not read; their paths are still checked.
                let content = if size as usize <= MAX_SCAN_BLOB && size <= MAX_SCAN_BLOB as u64 {
                    self.read_file(file, size)
                        .await
                        .map_err(|e| Error::Read(file, e.to_string()))?
                } else {
                    bytes::Bytes::new()
                };
                loaded.push((path, content));
            }
            let verdict = opts.guard.check(
                loaded.iter().map(|(path, content)| AddedBlob {
                    path,
                    content: content.as_ref(),
                }),
                opts.ignored,
            )?;
            merge(&mut scan.verdict, verdict);
        }
        Ok(())
    }
}

/// Folds `more` into `into`, keeping at most [`MAX_HOLDS`] holds.
fn merge(into: &mut GuardVerdict, more: GuardVerdict) {
    let GuardVerdict::Held {
        holds: more_holds,
        truncated: more_trunc,
    } = more
    else {
        return;
    };
    match into {
        GuardVerdict::Clear => {
            *into = GuardVerdict::Held {
                holds: more_holds,
                truncated: more_trunc,
            }
        }
        GuardVerdict::Held { holds, truncated } => {
            *truncated |= more_trunc;
            for h in more_holds {
                if holds.len() < MAX_HOLDS {
                    holds.push(Hold { ..h });
                } else {
                    *truncated = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use asc_store::bucket::BucketObjects;

    use super::*;
    use crate::commit::PathPolicy;
    use crate::history::MemBoundaries;
    use crate::history::testkit::*;

    struct NoHeads(Mutex<Vec<ChangesetId>>);

    #[async_trait]
    impl HeadSync for NoHeads {
        async fn sync_heads(&self, uploaded: ChangesetId) -> Result<()> {
            self.0.lock().unwrap().push(uploaded);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemQueue(Mutex<Vec<QueueEntry>>);

    #[async_trait]
    impl UploadQueue for MemQueue {
        async fn next_pending(&self, _now: i64) -> Result<Option<QueueEntry>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.state == QueueState::Pending)
                .cloned())
        }
        async fn set_state(&self, cs: ChangesetId, state: QueueState) -> Result<()> {
            for e in self.0.lock().unwrap().iter_mut().filter(|e| e.cs == cs) {
                e.state = state;
            }
            Ok(())
        }
        async fn delete(&self, cs: ChangesetId) -> Result<()> {
            self.0.lock().unwrap().retain(|e| e.cs != cs);
            Ok(())
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        eng: std::sync::Arc<Eng>,
        remote: MockRemote,
        bounds: MemBoundaries,
        guard: Guard,
        heads: NoHeads,
    }

    async fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let refs = sqlite(dir.path(), "m.db").await;
        let eng = engine(&refs, 1);
        let remote = MockRemote::bucket(std::sync::Arc::new(BucketObjects::memory()), refs, 99);
        Rig {
            _dir: dir,
            eng,
            remote,
            bounds: MemBoundaries::default(),
            guard: Guard::with_defaults().unwrap(),
            heads: NoHeads(Mutex::new(Vec::new())),
        }
    }

    fn opts<'a>(r: &'a Rig, ignored: &'a dyn Fn(&RepoPath) -> bool) -> UploadOptions<'a> {
        UploadOptions {
            guard: &r.guard,
            ignored,
            max_size: DEFAULT_MAX_SIZE,
            skip_guard: false,
            ignore_size: false,
            sync: SyncOptions::new(&r.bounds),
            heads: &r.heads,
        }
    }

    fn none(_: &RepoPath) -> bool {
        false
    }

    async fn files_commit(
        r: &Rig,
        parents: &[ChangesetId],
        files: &[(&str, &[u8])],
    ) -> ChangesetId {
        commit_files(
            &r.eng,
            parents,
            files,
            PathPolicy::Native { windows_safe: true },
            Vec::new(),
        )
        .await
    }

    #[tokio::test]
    async fn a_clear_draft_uploads_ingests_syncs_heads_and_leaves_the_queue() {
        let r = rig().await;
        let c1 = commit(&r.eng, &[], b"one").await;
        let c2 = commit(&r.eng, &[c1], b"two").await;
        let queue = MemQueue(Mutex::new(vec![QueueEntry {
            cs: c2,
            state: QueueState::Pending,
            attempts: 0,
        }]));
        let (cs, out) = r
            .eng
            .drain_one(&r.remote, &queue, 0, &opts(&r, &none))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cs, c2);
        let Outcome::Uploaded(report) = out else {
            panic!("{out:?}")
        };
        assert_eq!(report.changesets, 2);
        assert!(queue.0.lock().unwrap().is_empty());
        assert_eq!(*r.heads.0.lock().unwrap(), vec![c2]);
        let has = r
            .remote
            .has_many(&[ObjKey::Changeset(c1), ObjKey::Changeset(c2)])
            .await
            .unwrap();
        assert_eq!(has, vec![true, true]);
        assert!(r.remote.list_refs().await.unwrap().is_empty(), "no ref");
    }

    #[tokio::test]
    async fn a_denied_path_or_planted_token_is_held_and_nothing_is_sent() {
        let r = rig().await;
        let env = files_commit(&r, &[], &[(".env", b"X=1")]).await;
        let token = files_commit(
            &r,
            &[],
            &[(
                "notes.txt",
                b"key = ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            )],
        )
        .await;
        let queue = MemQueue(Mutex::new(vec![QueueEntry {
            cs: env,
            state: QueueState::Pending,
            attempts: 0,
        }]));
        let (_, out) = r
            .eng
            .drain_one(&r.remote, &queue, 0, &opts(&r, &none))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(out, Outcome::Held(_)), "{out:?}");
        assert_eq!(queue.0.lock().unwrap()[0].state, QueueState::Held);
        let out = r
            .eng
            .upload_draft(&r.remote, token, &opts(&r, &none))
            .await
            .unwrap();
        assert!(matches!(out, Outcome::Held(_)), "{out:?}");
        assert_eq!(r.remote.trips.puts.load(Ordering::SeqCst), 0);
        // Release uploads the held changeset.
        let mut o = opts(&r, &none);
        o.skip_guard = true;
        let out = r.eng.upload_draft(&r.remote, env, &o).await.unwrap();
        assert!(matches!(out, Outcome::Uploaded(_)), "{out:?}");
    }

    #[tokio::test]
    async fn an_ignored_path_is_held() {
        let r = rig().await;
        let c = files_commit(&r, &[], &[("build/out.bin", b"x")]).await;
        let ignored = |p: &RepoPath| p.as_bytes().starts_with(b"build/");
        let out = r
            .eng
            .upload_draft(&r.remote, c, &opts(&r, &ignored))
            .await
            .unwrap();
        assert!(matches!(out, Outcome::Held(_)), "{out:?}");
    }

    #[tokio::test]
    async fn an_oversized_changeset_is_marked_and_push_still_uploads_it() {
        let r = rig().await;
        let c = files_commit(&r, &[], &[("a.txt", &[7u8; 4096])]).await;
        let mut o = opts(&r, &none);
        o.max_size = 1024;
        let out = r.eng.upload_draft(&r.remote, c, &o).await.unwrap();
        assert!(
            matches!(
                out,
                Outcome::TooLarge {
                    bytes: 4096,
                    max: 1024
                }
            ),
            "{out:?}"
        );
        assert_eq!(r.remote.trips.puts.load(Ordering::SeqCst), 0);
        o.ignore_size = true;
        let out = r.eng.upload_draft(&r.remote, c, &o).await.unwrap();
        assert!(matches!(out, Outcome::Uploaded(_)), "{out:?}");
    }

    #[tokio::test]
    async fn a_killed_upload_resumes_with_has_objects_and_sends_only_the_rest() {
        let r = rig().await;
        let mut tip = commit(&r.eng, &[], b"c0").await;
        for i in 1..5 {
            tip = commit(&r.eng, &[tip], format!("c{i}").as_bytes()).await;
        }
        const TOTAL: u64 = 6 + 6 + 5;
        r.remote.budget.store(4, Ordering::SeqCst);
        let err = r
            .eng
            .upload_draft(&r.remote, tip, &opts(&r, &none))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Sync(sync::Error::Remote(_))), "{err}");
        assert!(
            r.heads.0.lock().unwrap().is_empty(),
            "heads sync only after ingest"
        );
        r.remote.budget.store(-1, Ordering::SeqCst);
        let out = r
            .eng
            .upload_draft(&r.remote, tip, &opts(&r, &none))
            .await
            .unwrap();
        let Outcome::Uploaded(report) = out else {
            panic!("{out:?}")
        };
        assert_eq!(report.objects as u64, TOTAL - 4);
        assert_eq!(r.remote.trips.put_objects.load(Ordering::SeqCst), TOTAL);
        // A third run finds everything there.
        let Outcome::Uploaded(again) = r
            .eng
            .upload_draft(&r.remote, tip, &opts(&r, &none))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(again.objects, 0);
    }

    #[tokio::test]
    async fn a_stack_of_large_files_is_sent_in_bounded_batches() {
        let r = rig().await;
        let mut tip = None;
        let mut total = 0u64;
        for i in 0..24u8 {
            let mut x = u64::from(i) + 0x9E37_79B9_7F4A_7C15;
            let blob: Vec<u8> = (0..1024 * 1024)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 24) as u8
                })
                .collect();
            total += blob.len() as u64;
            let name = format!("f{i}.dat");
            let parents: Vec<_> = tip.into_iter().collect();
            tip = Some(files_commit(&r, &parents, &[(&name, &blob)]).await);
        }
        let out = r
            .eng
            .upload_draft(&r.remote, tip.unwrap(), &opts(&r, &none))
            .await
            .unwrap();
        let Outcome::Uploaded(report) = out else {
            panic!("{out:?}")
        };
        assert!(report.bytes >= total);
        // 24 MiB of new content crosses the 16 MiB batch bound at least twice over the run.
        assert!(r.remote.trips.puts.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn merging_verdicts_caps_the_holds() {
        let hold = |i: usize| Hold {
            path: RepoPath::new(format!("p{i}")).unwrap(),
            reason: guard::HoldReason::Denied,
        };
        let mut v = GuardVerdict::Clear;
        for chunk in 0..3 {
            merge(
                &mut v,
                GuardVerdict::Held {
                    holds: (0..40).map(|i| hold(chunk * 40 + i)).collect(),
                    truncated: false,
                },
            );
        }
        let GuardVerdict::Held { holds, truncated } = v else {
            panic!()
        };
        assert_eq!(holds.len(), MAX_HOLDS);
        assert!(truncated);
    }
}
