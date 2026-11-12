//! Delta storage through the server: the pass, the GC roots, purges and the admin command.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use asc_core::id::{ChangesetId, FileId};
use asc_rpc::server::RepoName;
use asc_store::gc::{GcConfig, GcError, GcRootSource};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use async_trait::async_trait;

use crate::admin_cli::{AdminCli, DeltaCmd};
use crate::delta_service::{self, DeltaSettings};
use crate::gc::GcService;
use crate::ingest::testutil::{commit, registry, repo};
use crate::registry::{Registry, RepoEntry};
use crate::update::PipelineHooks;

const D15: Duration = Duration::from_secs(15 * 24 * 3600);
const H25: Duration = Duration::from_secs(25 * 3600);

fn name() -> RepoName {
    RepoName::parse("o/r").unwrap()
}

/// A source file of about 90 KiB; `edit` changes one line.
fn version(edit: usize) -> Vec<u8> {
    (0..3000)
        .map(|i| {
            if i == edit {
                format!("fn edited_{i}() -> u32 {{ {i} }}\n")
            } else {
                format!("fn item_{i}() -> u32 {{ {} }}\n", i * 3)
            }
        })
        .collect::<String>()
        .into_bytes()
}

/// A child of `parent` that writes `bytes` at `to`, recorded as a copy of `from`.
async fn copy_commit(
    e: &RepoEntry,
    parent: ChangesetId,
    to: &str,
    from: &str,
    bytes: &[u8],
) -> ChangesetId {
    use asc_core::path::RepoPath;
    use asc_engine::commit::{NewChangeset, PathPolicy};
    let engine = e.backend.engine();
    let base = engine.get_changeset(parent).await.unwrap().root;
    let root = crate::ingest::testutil::tree(e, base, &[(to, bytes)]).await;
    engine
        .create_changeset(
            NewChangeset {
                parents: vec![parent],
                author: crate::ingest::testutil::sig(),
                committer: crate::ingest::testutil::sig(),
                message: b"copy".to_vec(),
                extra: Vec::new(),
                root,
                copies: vec![(RepoPath::new(to).unwrap(), RepoPath::new(from).unwrap())],
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap()
}

struct Fx {
    reg: Arc<Registry>,
    entry: Arc<RepoEntry>,
    hooks: Arc<PipelineHooks>,
    gc: Arc<GcService>,
    /// `master`: `a` writes `f` (version 0) and `small`, `b` edits `f` (version 1) and
    /// `small`, `c` copies `f` to `g` with another edit (version 2).
    css: [ChangesetId; 3],
}

impl Fx {
    async fn new() -> Self {
        let reg = registry().await;
        let entry = repo(&reg).await;
        let (v0, v1, v2) = (version(usize::MAX), version(10), version(20));
        let a = commit(&entry, &[], &[("f", &v0), ("small", b"one")]).await;
        let b = commit(&entry, &[a], &[("f", &v1), ("small", b"two")]).await;
        let c = copy_commit(&entry, b, "g", "f", &v2).await;
        let ns = entry.ns.as_bytes().to_vec();
        for (pos, cs) in [a, b, c].into_iter().enumerate() {
            sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, $3, $4)")
                .bind(ns.clone())
                .bind("refs/heads/master")
                .bind(pos as i64)
                .bind(cs.as_bytes().to_vec())
                .execute(reg.pool())
                .await
                .unwrap();
        }
        let hooks = Arc::new(PipelineHooks::default());
        let gc = Arc::new(GcService::new(
            reg.clone(),
            hooks.clone(),
            GcConfig::default(),
        ));
        Self {
            reg,
            entry,
            hooks,
            gc,
            css: [a, b, c],
        }
    }

    async fn rows(&self) -> Vec<(FileId, FileId)> {
        let rows: Vec<(Vec<u8>, Vec<u8>)> =
            sqlx::query_as("SELECT file, base FROM delta_bases ORDER BY file")
                .fetch_all(self.reg.pool())
                .await
                .unwrap();
        rows.into_iter()
            .map(|(f, b)| {
                (
                    FileId::from_bytes(f.try_into().unwrap()),
                    FileId::from_bytes(b.try_into().unwrap()),
                )
            })
            .collect()
    }

    async fn has(&self, f: FileId) -> bool {
        self.entry
            .objects
            .has_many(&[ObjKey::File(f)])
            .await
            .unwrap()[0]
    }

    async fn read(&self, f: FileId) -> Vec<u8> {
        self.entry
            .objects
            .get(&ObjKey::File(f))
            .await
            .unwrap()
            .unwrap()
            .to_vec()
    }

    async fn set_master(&self, to: Option<ChangesetId>) {
        use asc_core::id::RefTarget;
        use asc_core::path::RefName;
        use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs as _};
        let rn = RefName::new(b"refs/heads/master".to_vec()).unwrap();
        let cur = self.reg.refs().get(self.entry.key, &rn).await.unwrap();
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: rn,
                expect: cur.map_or(Expect::Absent, |c| Expect::Value(c.target)),
                new: to.map(RefTarget::Changeset),
                expires_at: None,
            }],
            actor: Actor {
                id: "t".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: uuid::Uuid::new_v4(),
        };
        self.reg.refs().update(self.entry.key, &txn).await.unwrap();
    }
}

fn on() -> DeltaSettings {
    DeltaSettings {
        enabled: true,
        ..DeltaSettings::default()
    }
}

#[tokio::test]
async fn a_pass_stores_edited_versions_as_deltas_that_read_back_in_full() {
    let f = Fx::new().await;
    let (v0, v1, v2) = (version(usize::MAX), version(10), version(20));
    let (i0, i1, i2) = (FileId::of(&v0), FileId::of(&v1), FileId::of(&v2));

    let rep = f.gc.run_delta_repo(&name(), &on()).await.unwrap();
    assert_eq!(rep.changesets, 3, "{rep:?}");
    // `f` in `b` against `a`, `g` in `c` against `f` in `b` (the copy source); `small` is
    // under the minimum size.
    assert_eq!(rep.written, 2, "{rep:?}");
    assert!(rep.bytes_after * 10 < rep.bytes_before, "{rep:?}");
    let mut want = vec![(i1, i0), (i2, i1)];
    want.sort();
    assert_eq!(f.rows().await, want);
    assert_eq!(f.entry.objects.delta_base_of(i1).await.unwrap(), Some(i0));
    assert_eq!(f.entry.objects.delta_base_of(i2).await.unwrap(), Some(i1));
    assert_eq!(f.read(i1).await, v1);
    assert_eq!(f.read(i2).await, v2);
    // What leaves the bucket is a wire value.
    let stored = f
        .entry
        .objects
        .get_stored(&ObjKey::File(i2))
        .await
        .unwrap()
        .unwrap();
    asc_store::codec::check_codec(&stored).unwrap();

    let s = f.gc.delta_status(&name()).await.unwrap();
    assert_eq!((s.deltas, s.bases, s.backlog), (2, 2, 0));
    // The cursor moved: a second pass reads nothing.
    let again = f.gc.run_delta_repo(&name(), &on()).await.unwrap();
    assert_eq!((again.changesets, again.written), (0, 0), "{again:?}");
    // A new landed changeset is read on the next pass.
    let d = commit(&f.entry, &[f.css[2]], &[("f", &version(30))]).await;
    sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, 3, $3)")
        .bind(f.entry.ns.as_bytes().to_vec())
        .bind("refs/heads/master")
        .bind(d.as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(f.gc.delta_status(&name()).await.unwrap().backlog, 1);
    let next = f.gc.run_delta_repo(&name(), &on()).await.unwrap();
    assert_eq!((next.changesets, next.written), (1, 1), "{next:?}");
    assert_eq!(f.read(FileId::of(&version(30))).await, version(30));
}

#[tokio::test]
async fn a_merge_brings_its_side_branch_into_the_pass() {
    let f = Fx::new().await;
    f.gc.run_delta_repo(&name(), &on()).await.unwrap();
    // A side branch off `c` adds `side` and then edits it; the merge lands. Against `c` the
    // merge only adds `side`, so its edited version has a base only on the side branch.
    let (w0, w1) = (version(40), version(50));
    let (j0, j1) = (FileId::of(&w0), FileId::of(&w1));
    let s1 = commit(&f.entry, &[f.css[2]], &[("side", &w0)]).await;
    let s2 = commit(&f.entry, &[s1], &[("side", &w1)]).await;
    let m = commit(&f.entry, &[f.css[2], s2], &[("side", &w1)]).await;
    sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, 3, $3)")
        .bind(f.entry.ns.as_bytes().to_vec())
        .bind("refs/heads/master")
        .bind(m.as_bytes().to_vec())
        .execute(f.reg.pool())
        .await
        .unwrap();
    let rep = f.gc.run_delta_repo(&name(), &on()).await.unwrap();
    // The merge, then `s2` and `s1`; the walk stops at `c`, which is on the mainline.
    assert_eq!(
        (rep.changesets, rep.side, rep.written),
        (1, 2, 1),
        "{rep:?}"
    );
    assert_eq!(f.entry.objects.delta_base_of(j1).await.unwrap(), Some(j0));
    assert_eq!(f.read(j1).await, w1);
    // A side changeset is read once: the next pass has nothing to do.
    let again = f.gc.run_delta_repo(&name(), &on()).await.unwrap();
    assert_eq!((again.changesets, again.side), (0, 0), "{again:?}");
    // A walk is bounded by `max_changesets`.
    let tight = DeltaSettings {
        max_changesets: 1,
        ..on()
    };
    let n = commit(&f.entry, &[m], &[("small", b"x")]).await;
    let t1 = commit(&f.entry, &[m], &[("side", &version(60))]).await;
    let t2 = commit(&f.entry, &[t1], &[("side", &version(70))]).await;
    let merge2 = commit(&f.entry, &[n, t2], &[("side", &version(70))]).await;
    for (pos, cs) in [(4i64, n), (5, merge2)] {
        sqlx::query("INSERT INTO mainline (ns, ref, pos, cs) VALUES ($1, $2, $3, $4)")
            .bind(f.entry.ns.as_bytes().to_vec())
            .bind("refs/heads/master")
            .bind(pos)
            .bind(cs.as_bytes().to_vec())
            .execute(f.reg.pool())
            .await
            .unwrap();
    }
    let r1 = f.gc.run_delta_repo(&name(), &tight).await.unwrap();
    assert_eq!((r1.changesets, r1.side), (1, 0), "{r1:?}");
    let r2 = f.gc.run_delta_repo(&name(), &tight).await.unwrap();
    assert_eq!((r2.changesets, r2.side), (1, 1), "{r2:?}");
}

#[tokio::test]
async fn a_pass_respects_the_size_bounds_and_its_batch_limit() {
    let f = Fx::new().await;
    let big = DeltaSettings {
        min_size: crate::config::ByteSize(1 << 20),
        ..on()
    };
    let rep = f.gc.run_delta_repo(&name(), &big).await.unwrap();
    assert_eq!((rep.changesets, rep.written), (3, 0), "{rep:?}");
    let f = Fx::new().await;
    let one = DeltaSettings {
        max_changesets: 2,
        ..on()
    };
    let rep = f.gc.run_delta_repo(&name(), &one).await.unwrap();
    assert_eq!((rep.changesets, rep.written), (2, 1), "{rep:?}");
    assert_eq!(f.gc.delta_status(&name()).await.unwrap().backlog, 1);
}

#[tokio::test]
async fn the_collector_keeps_a_base_until_its_last_delta_is_swept() {
    let f = Fx::new().await;
    let (v0, v1, v2) = (version(usize::MAX), version(10), version(20));
    let (i0, i1, i2) = (FileId::of(&v0), FileId::of(&v1), FileId::of(&v2));
    f.gc.run_delta_repo(&name(), &on()).await.unwrap();
    // Only `b` is live: `g` (a delta against `f` in `b`) is garbage, `f` in `a` is live
    // through `b`'s parent.
    f.set_master(Some(f.css[1])).await;
    let t1 = SystemTime::now() + D15;
    f.gc.run_repo(&name(), t1, false).await.unwrap();
    f.gc.run_repo(&name(), t1 + H25, false).await.unwrap();
    assert!(!f.has(i2).await, "the dead delta is swept");
    assert_eq!(f.rows().await, vec![(i1, i0)], "and its row with it");
    assert_eq!(f.read(i1).await, v1);

    // Nothing is live: `f` in `b` goes, and `f` in `a` stays while it is still a base.
    f.set_master(None).await;
    let t2 = t1 + 3 * H25;
    f.gc.run_repo(&name(), t2, false).await.unwrap();
    f.gc.run_repo(&name(), t2 + H25, false).await.unwrap();
    assert!(!f.has(i1).await);
    assert!(f.rows().await.is_empty());
    // The base was a root during that sweep; the next two marks find it unreachable.
    let t3 = t2 + 3 * H25;
    f.gc.run_repo(&name(), t3, false).await.unwrap();
    f.gc.run_repo(&name(), t3 + H25, false).await.unwrap();
    assert!(!f.has(i0).await);
}

#[tokio::test]
async fn a_purged_base_expands_its_dependents_and_takes_no_new_ones() {
    /// Keeps `keep` alive as an extra root and purges `purge`.
    struct Src {
        keep: ObjKey,
        purge: ObjKey,
    }
    #[async_trait]
    impl GcRootSource for Src {
        async fn roots(&self, _: &[u8]) -> Result<Vec<ObjKey>, GcError> {
            Ok(vec![self.keep])
        }
        async fn purged(&self, _: &[u8]) -> Result<Vec<ObjKey>, GcError> {
            Ok(vec![self.purge])
        }
    }
    let f = Fx::new().await;
    let (v0, v1) = (version(usize::MAX), version(10));
    let (i0, i1, i2) = (FileId::of(&v0), FileId::of(&v1), FileId::of(&version(20)));
    let two = DeltaSettings {
        max_changesets: 2,
        ..on()
    };
    f.gc.run_delta_repo(&name(), &two).await.unwrap();
    assert_eq!(f.rows().await, vec![(i1, i0)]);
    // No ref reaches the base (a purge never deletes what a ref reaches); the delta is kept
    // by another root.
    let purge_row = |k: ObjKey| {
        sqlx::query("INSERT INTO gc_purge (ns, key, purged_at) VALUES ($1, $2, 1)")
            .bind(f.entry.ns.as_bytes().to_vec())
            .bind(k.to_string())
            .execute(f.reg.pool())
    };
    purge_row(ObjKey::File(i0)).await.unwrap();
    f.hooks.gc_roots().register(Arc::new(Src {
        keep: ObjKey::File(i1),
        purge: ObjKey::File(i0),
    }));

    // A dry run changes nothing.
    let t1 = SystemTime::now() + D15;
    f.gc.run_repo(&name(), t1, true).await.unwrap();
    assert_eq!(f.rows().await.len(), 1);
    assert!(f.entry.objects.delta_base_of(i1).await.unwrap().is_some());

    let r =
        f.gc.run_repo(&name(), t1 + Duration::from_secs(60), false)
            .await
            .unwrap();
    assert_eq!(r.sweep.purged, 1, "{r}");
    assert!(!f.has(i0).await, "the purged base is gone");
    assert!(f.rows().await.is_empty());
    assert_eq!(f.entry.objects.delta_base_of(i1).await.unwrap(), None);
    assert_eq!(f.read(i1).await, v1, "its dependent was expanded first");

    // A pass makes no delta against a purged base.
    purge_row(ObjKey::File(i1)).await.unwrap();
    let next = f.gc.run_delta_repo(&name(), &on()).await.unwrap();
    assert_eq!((next.changesets, next.written), (1, 0), "{next:?}");
    assert_eq!(f.entry.objects.delta_base_of(i2).await.unwrap(), None);
}

#[tokio::test]
async fn release_purged_and_expand_copied_skip_full_values() {
    let f = Fx::new().await;
    assert_eq!(
        delta_service::release_purged(f.reg.pool(), &f.entry)
            .await
            .unwrap(),
        0
    );
    let other = f
        .reg
        .create(&RepoName::parse("o/f").unwrap(), &Default::default())
        .await
        .unwrap();
    let keys = [ObjKey::File(FileId::of(&version(10)))];
    assert_eq!(
        delta_service::expand_copied(&f.entry, &other, &keys)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn the_admin_command_runs_and_reports() {
    let f = Fx::new().await;
    let cli = AdminCli::new(f.reg.clone(), None, None, None).with_gc(GcConfig::default(), on());
    let bad = cli.delta(&DeltaCmd::Status, &["o/r".into()], true).await;
    assert!(bad.is_err());
    let s = cli.delta(&DeltaCmd::Status, &[], true).await.unwrap();
    assert!(s.ok);
    assert!(
        s.text
            .contains("o/r: 0 delta(s) over 0 base(s); 3 mainline changeset(s) not yet read"),
        "{}",
        s.text
    );
    let r = cli
        .delta(&DeltaCmd::Run, &["o/r".into()], false)
        .await
        .unwrap();
    assert!(r.text.contains("wrote 2 delta(s)"), "{}", r.text);
    let s = cli.delta(&DeltaCmd::Status, &[], true).await.unwrap();
    assert!(
        s.text.contains("2 delta(s) over 2 base(s); 0 mainline"),
        "{}",
        s.text
    );
    // The rollback: every delta in full, the cursor kept.
    let x = cli
        .delta(&DeltaCmd::Expand, &["o/r".into()], false)
        .await
        .unwrap();
    assert!(
        x.text.contains("o/r: rewrote 2 delta(s) in full"),
        "{}",
        x.text
    );
    assert!(f.rows().await.is_empty());
    let i2 = FileId::of(&version(20));
    assert_eq!(f.read(i2).await, version(20));
    assert_eq!(f.entry.objects.delta_base_of(i2).await.unwrap(), None);
    let again = cli.delta(&DeltaCmd::Run, &[], true).await.unwrap();
    assert!(again.text.contains("read 0 changeset(s)"), "{}", again.text);
}
