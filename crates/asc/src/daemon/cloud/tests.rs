use std::collections::VecDeque;
use std::sync::atomic::AtomicI64;

use asc_core::id::ChangesetId;
use asc_state::home::RepoId;
use asc_state::state::repos::{NewRepo, RemoteKind};

use super::*;
use crate::branchless::mode::{CloudSetting, Workspace};

fn cs(n: u8) -> ChangesetId {
    ChangesetId::of(&[n])
}

fn ch(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

// ------------------------------------------------------------------------------ policy

#[test]
fn classification_follows_the_retry_table() {
    assert_eq!(classify("INVALID_OBJECT: x"), Class::Permanent);
    assert_eq!(classify("PermissionDenied: no"), Class::Permanent);
    assert_eq!(classify("PATH_POLICY: .env"), Class::Permanent);
    assert_eq!(classify("QUOTA_EXCEEDED"), Class::Permanent);
    // A refusal wins over a word that looks like an outage.
    assert_eq!(classify("Unavailable: PATH_POLICY"), Class::Permanent);
    assert_eq!(classify("Unavailable: transport error"), Class::Offline);
    assert_eq!(
        classify("offline (origin unreachable): fetch needs GetObjects"),
        Class::Offline
    );
    assert_eq!(classify("DeadlineExceeded: slow"), Class::Transient(None));
    assert_eq!(classify("something odd"), Class::Transient(None));
    assert_eq!(
        classify("ResourceExhausted: over limit; retry after 2500 ms"),
        Class::Transient(Some(Duration::from_millis(2500)))
    );
}

#[test]
fn the_delay_is_full_jitter_between_one_second_and_five_minutes() {
    let low = RetryPolicy {
        jitter: || 0.0,
        ..policy()
    };
    let high = RetryPolicy {
        jitter: || 0.999_999,
        ..policy()
    };
    assert_eq!(delay_secs(&low, 1, None), 1);
    assert!(delay_secs(&high, 1, None) <= 2);
    assert!(delay_secs(&high, 5, None) <= 16);
    // Unbounded attempts still never wait more than five minutes.
    assert!(delay_secs(&high, 500, None) <= 300);
    assert_eq!(delay_secs(&low, 500, None), 1);
    // The server's hint raises the wait, and a hostile one is capped.
    assert_eq!(delay_secs(&low, 1, Some(Duration::from_secs(7))), 7);
    assert_eq!(delay_secs(&low, 1, Some(Duration::from_secs(86_400))), 300);
}

#[test]
fn stored_and_logged_text_is_bounded_and_printable() {
    let long = format!("a\u{7}b\n{}", "x".repeat(10_000));
    let n = note(&long);
    assert!(n.chars().count() <= MAX_NOTE);
    assert!(!n.chars().any(char::is_control));
}

#[test]
fn workspace_names_match_the_server_rule() {
    assert_eq!(workspace_name(&Workspace::Default), "default");
    assert_eq!(
        workspace_name(&Workspace::Named("my.box_1".into())),
        "my.box_1"
    );
    let odd = workspace_name(&Workspace::Named("a b/c\u{0}".repeat(40)));
    assert!(odd.len() <= 64);
    assert!(
        odd.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    );
}

// ------------------------------------------------------------------------------ gate

fn facts() -> Facts {
    Facts {
        settings: ModeSettings::default(),
        policy: RepoPolicy {
            branchless: true,
            cloud: true,
            public: false,
        },
        cloud_v1: true,
    }
}

#[test]
fn cloud_is_on_only_for_a_private_branchless_cloud_repo() {
    assert_eq!(gate(&facts()), Gate::On);

    let mut f = facts();
    f.policy.public = true;
    assert_eq!(gate(&f), Gate::Off, "a public repo never auto-uploads");

    let mut f = facts();
    f.policy.public = true;
    f.settings.cloud = CloudSetting::On;
    assert!(
        matches!(gate(&f), Gate::Refused(_)),
        "asc.cloud=on is refused"
    );

    let mut f = facts();
    f.policy.cloud = false;
    assert_eq!(gate(&f), Gate::Off, "repos.cloud off");

    let mut f = facts();
    f.policy.branchless = false;
    assert_eq!(gate(&f), Gate::Off, "repo branchless off");

    let mut f = facts();
    f.cloud_v1 = false;
    assert_eq!(gate(&f), Gate::Off, "no cloud.v1");

    let mut f = facts();
    f.settings.cloud = CloudSetting::Off;
    assert_eq!(gate(&f), Gate::Off, "asc.cloud=off");

    let mut f = facts();
    f.settings.branchless = false;
    f.settings.cloud = CloudSetting::On;
    assert_eq!(
        gate(&f),
        Gate::Off,
        "asc.branchless=false beats asc.cloud=on"
    );
}

#[test]
fn no_daemon_registers_no_background_task() {
    let dir = tempfile::tempdir().unwrap();
    let home = AscHome::open(dir.path().join("home")).unwrap();
    let off = |k: &str| (k == "ASC_NO_DAEMON").then(|| OsString::from("1"));
    assert!(extra_jobs(home.clone(), &off).is_empty());
    let on = |_: &str| None;
    let names: Vec<_> = extra_jobs(home, &on).iter().map(|(s, _)| s.name).collect();
    assert_eq!(names, ["cloud_upload", "cloud_sync"]);
}

// ------------------------------------------------------------------------------ state rows

async fn open_db(dir: &tempfile::TempDir) -> (StateDb, RepoKey) {
    let home = AscHome::open(dir.path().join("home")).unwrap();
    let db = StateDb::open(&home).await.unwrap();
    let repo = asc_state::state::repos::insert(
        &db,
        &NewRepo {
            id: RepoId::new(),
            name: "r".into(),
            ns: vec![1],
            remote_url: None,
            remote_kind: RemoteKind::Server,
            default_branch: "master".into(),
            windows_safe: true,
        },
    )
    .await
    .unwrap();
    (db, repo)
}

async fn enqueue(db: &StateDb, repo: RepoKey, c: ChangesetId, change: ChangeId, now: i64) {
    let d = Drafts::new(repo);
    let mut tx = db.begin().await.unwrap();
    d.new_changeset(&mut tx, c, change, &[], now, drafts::QUEUE_MAX)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn a_held_entry_leaves_the_pending_queue_and_a_done_one_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo) = open_db(&dir).await;
    enqueue(&db, repo, cs(1), ch(1), 10).await;
    enqueue(&db, repo, cs(2), ch(2), 11).await;
    let rows = StateRows::new(db.clone(), repo);

    let first = rows.next_pending(100).await.unwrap().unwrap();
    assert_eq!(first.cs, cs(1), "oldest first");
    rows.set_state(cs(1), EngineQueueState::Held).await.unwrap();
    assert_eq!(rows.next_pending(100).await.unwrap().unwrap().cs, cs(2));
    rows.set_state(cs(2), EngineQueueState::TooLarge)
        .await
        .unwrap();
    assert!(rows.next_pending(100).await.unwrap().is_none());
    assert_eq!(rows.drafts().queue_len(&db).await.unwrap(), 2, "rows stay");
    rows.delete(cs(1)).await.unwrap();
    assert_eq!(rows.drafts().queue_len(&db).await.unwrap(), 1);
    assert_eq!(rows.visible_heads(10).await.unwrap().len(), 2);
}

// ------------------------------------------------------------------------------ the head sync

#[derive(Default)]
struct FakeStore {
    doc: Mutex<CloudDoc>,
    updates: Mutex<Vec<CloudUpdate>>,
}

#[async_trait]
impl CloudStore for FakeStore {
    async fn get(&self) -> Result<CloudDoc, StoreError> {
        Ok(self.doc.lock().unwrap().clone())
    }

    async fn update(&self, _op: &str, u: &CloudUpdate) -> Result<u64, StoreError> {
        let mut doc = self.doc.lock().unwrap();
        if u.base_version != doc.version {
            return Err(StoreError::VersionConflict {
                current: doc.version,
            });
        }
        doc.version += 1;
        for h in &u.add {
            doc.heads.push(h.clone());
        }
        doc.heads.retain(|h| !u.remove.contains(&h.cs));
        self.updates.lock().unwrap().push(u.clone());
        Ok(doc.version)
    }
}

struct Fixed(i64);

impl Clock for Fixed {
    fn now(&self) -> i64 {
        self.0
    }
}

fn remote_head(n: u8, hidden_at: Option<i64>) -> CloudHead {
    CloudHead {
        cs: cs(n),
        change: ch(n),
        labels: vec![],
        added: 1,
        hidden_at,
    }
}

#[tokio::test]
async fn a_remote_head_not_yet_pulled_is_not_read_back_as_a_local_removal() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo) = open_db(&dir).await;
    enqueue(&db, repo, cs(1), ch(1), 10).await;
    let rows = StateRows::new(db.clone(), repo);
    let store = FakeStore::default();
    store.doc.lock().unwrap().heads.push(remote_head(9, None));
    store.doc.lock().unwrap().version = 4;
    let sync = CloudSync::new(rows.clone(), store, "default".into(), Arc::new(Fixed(500)));

    assert_eq!(sync.run().await.unwrap(), 5);
    {
        let updates = sync.store.updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].add.iter().map(|h| h.cs).collect::<Vec<_>>(),
            [cs(1)]
        );
        assert!(updates[0].remove.is_empty());
    }
    let base = rows.drafts().base(&db).await.unwrap();
    assert_eq!(base, [cs(1)], "the unpulled head cs(9) is not in the base");
    let state = rows.drafts().cloud_state(&db).await.unwrap().unwrap();
    assert_eq!(
        (state.ws_name.as_str(), state.version, state.last_sync),
        ("default", 5, 500)
    );

    // The second sync must not delete cs(9) from the server.
    sync.run().await.unwrap();
    let doc = sync.store.doc.lock().unwrap();
    assert!(doc.heads.iter().any(|h| h.cs == cs(9)), "cs(9) survives");
    assert!(
        sync.store
            .updates
            .lock()
            .unwrap()
            .iter()
            .all(|u| u.remove.is_empty())
    );
}

#[tokio::test]
async fn a_head_hidden_on_another_machine_is_hidden_here_and_stays_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let (db, repo) = open_db(&dir).await;
    enqueue(&db, repo, cs(1), ch(1), 10).await;
    let rows = StateRows::new(db.clone(), repo);
    let store = FakeStore::default();
    let sync = CloudSync::new(rows.clone(), store, "default".into(), Arc::new(Fixed(500)));
    sync.run().await.unwrap();
    // Machine B hides cs(1).
    {
        let mut doc = sync.store.doc.lock().unwrap();
        doc.version += 1;
        for h in &mut doc.heads {
            h.hidden_at = Some(400);
        }
    }
    sync.run().await.unwrap();
    assert!(rows.visible_heads(10).await.unwrap().is_empty());
    let hidden = rows.hidden_heads(10).await.unwrap();
    assert_eq!(hidden.iter().map(|h| h.cs).collect::<Vec<_>>(), [cs(1)]);
}

#[test]
fn a_resent_update_carries_the_same_request_id() {
    let u = CloudUpdate {
        base_version: 3,
        add: vec![remote_head(1, None)],
        ..CloudUpdate::default()
    };
    assert_eq!(op_uuid(&u.op_id()), op_uuid(&u.op_id()));
    let v = CloudUpdate {
        base_version: 4,
        ..u.clone()
    };
    assert_ne!(op_uuid(&u.op_id()), op_uuid(&v.op_id()));
}

#[test]
fn heads_survive_the_wire_conversion_and_a_git_id_is_refused() {
    let mut h = remote_head(3, Some(77));
    h.labels = vec!["wip/a-b".into()];
    let w = to_wire_head(&h).unwrap();
    assert_eq!(w.change, None, "the change id is never sent");
    let back = from_wire_head(&wire_cloud::CloudHead {
        change: Some(ch(3)),
        ..w
    })
    .unwrap();
    assert_eq!(back, h);
    h.labels = vec!["bad label".into()];
    assert!(
        to_wire_head(&h).is_err(),
        "labels are validated on the way out"
    );
    let git = wire_cloud::CloudHead {
        cs: asc_rpc::ModeId::Git(asc_core::id::GitOid::from_bytes([1; 20])),
        change: None,
        labels: vec![],
        added: 0,
        hidden_at: 0,
    };
    assert!(from_wire_head(&git).is_err());
}

// ------------------------------------------------------------------------------ the job

struct TickClock(AtomicI64);

impl TickClock {
    fn new(at: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(at)))
    }

    fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for TickClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct Script {
    facts: Option<Result<Facts, String>>,
    /// What `settings()` answers; the settings of `facts` when unset.
    settings: Option<ModeSettings>,
    ping: VecDeque<Result<(), String>>,
    due: VecDeque<Due>,
    uploads: VecDeque<Result<Option<Step>, String>>,
    sync: VecDeque<Result<(), String>>,
    calls: Vec<String>,
    failures: Vec<(ChangesetId, bool, i64, String)>,
}

#[derive(Default)]
struct FakeLink(Mutex<Script>);

impl FakeLink {
    fn with(f: impl FnOnce(&mut Script)) -> Arc<Self> {
        let link = Self::default();
        f(&mut link.0.lock().unwrap());
        Arc::new(link)
    }

    fn calls(&self, name: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| *c == name)
            .count()
    }
}

#[async_trait]
impl Uplink for FakeLink {
    async fn facts(&self) -> Result<Facts, String> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("facts".into());
        s.facts.clone().unwrap_or_else(|| Ok(facts()))
    }

    async fn settings(&self) -> Result<ModeSettings, String> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("settings".into());
        if let Some(settings) = &s.settings {
            return Ok(settings.clone());
        }
        match &s.facts {
            Some(Ok(f)) => Ok(f.settings.clone()),
            Some(Err(_)) | None => Ok(facts().settings),
        }
    }

    async fn ping(&self) -> Result<(), String> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("ping".into());
        s.ping.pop_front().unwrap_or(Ok(()))
    }

    async fn next_due(&self, _now: i64) -> Result<Option<Due>, String> {
        Ok(self.0.lock().unwrap().due.front().copied())
    }

    async fn upload_next(&self, _now: i64) -> Result<Option<Step>, String> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("upload".into());
        let r = s.uploads.pop_front().unwrap_or(Ok(None));
        if matches!(r, Ok(Some(_))) {
            s.due.pop_front();
        }
        r
    }

    async fn record_failure(
        &self,
        cs: ChangesetId,
        permanent: bool,
        next_at: i64,
        error: &str,
    ) -> Result<(), String> {
        let mut s = self.0.lock().unwrap();
        s.failures.push((cs, permanent, next_at, error.to_owned()));
        if permanent {
            s.due.pop_front();
        }
        Ok(())
    }

    async fn sync(&self) -> Result<(), String> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("sync".into());
        s.sync.pop_front().unwrap_or(Ok(()))
    }
}

struct FakeLinks(Option<Arc<FakeLink>>);

#[async_trait]
impl Uplinks for FakeLinks {
    async fn repos(&self, _: Phase, _: i64) -> Result<Vec<RepoKey>, String> {
        Ok(vec![RepoKey(1)])
    }

    async fn open(&self, _: RepoKey) -> Result<Option<Arc<dyn Uplink>>, String> {
        Ok(self.0.clone().map(|l| l as Arc<dyn Uplink>))
    }
}

fn job(link: &Arc<FakeLink>, clock: &Arc<TickClock>) -> CloudJob {
    CloudJob::with_policy(
        Arc::new(FakeLinks(Some(link.clone()))),
        clock.clone(),
        RetryPolicy {
            jitter: || 0.0,
            ..policy()
        },
    )
}

fn due(n: u8, attempts: u32) -> Due {
    Due {
        cs: cs(n),
        attempts,
    }
}

#[tokio::test]
async fn nothing_uploads_when_the_gate_is_closed() {
    for edit in [
        (|f: &mut Facts| f.policy.public = true) as fn(&mut Facts),
        |f| f.policy.cloud = false,
        |f| f.policy.branchless = false,
        |f| f.cloud_v1 = false,
        |f| f.settings.cloud = CloudSetting::Off,
        |f| f.settings.branchless = false,
    ] {
        let mut f = facts();
        edit(&mut f);
        let link = FakeLink::with(|s| {
            s.facts = Some(Ok(f));
            s.due.push_back(due(1, 0));
            s.uploads.push_back(Ok(Some(Step::Uploaded)));
        });
        let clock = TickClock::new(1_000);
        let job = job(&link, &clock);
        let t = job.run_phase(Phase::Upload).await.unwrap();
        assert_eq!(t, Tally::default());
        assert_eq!(link.calls("upload"), 0, "a closed gate sends nothing");
        job.run_phase(Phase::Sync).await.unwrap();
        assert_eq!(link.calls("sync"), 0, "and syncs nothing");
    }
}

#[tokio::test]
async fn a_repo_without_a_asc_server_is_skipped() {
    let job = CloudJob::new(Arc::new(FakeLinks(None)), TickClock::new(1));
    assert_eq!(
        job.run_phase(Phase::Upload).await.unwrap(),
        Tally::default()
    );
}

#[tokio::test]
async fn an_open_gate_drains_the_queue_up_to_the_per_run_bound() {
    let link = FakeLink::with(|s| {
        for n in 0..40 {
            s.due.push_back(due(n, 0));
            s.uploads.push_back(Ok(Some(Step::Uploaded)));
        }
    });
    let clock = TickClock::new(1_000);
    let job = job(&link, &clock);
    let t = job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(t.uploaded, MAX_UPLOADS_PER_RUN);
    assert_eq!(link.calls("upload"), MAX_UPLOADS_PER_RUN);
}

#[tokio::test]
async fn a_guard_hold_is_counted_and_the_run_goes_on() {
    let link = FakeLink::with(|s| {
        s.due.extend([due(1, 0), due(2, 0)]);
        s.uploads.push_back(Ok(Some(Step::Held)));
        s.uploads.push_back(Ok(Some(Step::Uploaded)));
    });
    let clock = TickClock::new(1_000);
    let t = job(&link, &clock).run_phase(Phase::Upload).await.unwrap();
    assert_eq!((t.held, t.uploaded), (1, 1));
}

#[tokio::test]
async fn a_transient_error_backs_off_and_honours_the_server_hint() {
    let link = FakeLink::with(|s| {
        s.due.push_back(due(1, 0));
        s.uploads.push_back(Err(
            "ResourceExhausted: over limit; retry after 7000 ms".into()
        ));
    });
    let clock = TickClock::new(1_000);
    let t = job(&link, &clock).run_phase(Phase::Upload).await.unwrap();
    assert_eq!(t.failed, 1);
    let s = link.0.lock().unwrap();
    let (c, permanent, next_at, _) = &s.failures[0];
    assert_eq!((*c, *permanent), (cs(1), false));
    assert_eq!(*next_at, 1_007, "retried after the hint");
}

#[tokio::test]
async fn a_transient_error_stops_the_run_and_attempts_widen_the_wait() {
    let link = FakeLink::with(|s| {
        s.due.extend([due(1, 3), due(2, 0)]);
        s.uploads.push_back(Err("DeadlineExceeded: slow".into()));
    });
    let clock = TickClock::new(1_000);
    job(&link, &clock).run_phase(Phase::Upload).await.unwrap();
    assert_eq!(
        link.calls("upload"),
        1,
        "the second entry waits for the next tick"
    );
    // Jitter 0 gives the floor; the ceiling grows with the attempts.
    let s = link.0.lock().unwrap();
    assert_eq!(s.failures[0].2, 1_001);
}

#[tokio::test]
async fn a_permanent_error_parks_only_that_entry() {
    let link = FakeLink::with(|s| {
        s.due.extend([due(1, 0), due(2, 0)]);
        s.uploads.push_back(Err("PATH_POLICY: .env".into()));
        s.uploads.push_back(Ok(Some(Step::Uploaded)));
    });
    let clock = TickClock::new(1_000);
    let t = job(&link, &clock).run_phase(Phase::Upload).await.unwrap();
    assert_eq!((t.failed, t.uploaded), (1, 1));
    let s = link.0.lock().unwrap();
    assert_eq!(s.failures.len(), 1);
    assert!(s.failures[0].1, "state = failed");
}

#[tokio::test]
async fn offline_pauses_the_queue_without_touching_a_row_and_probes_with_backoff() {
    let link = FakeLink::with(|s| {
        s.due.push_back(due(1, 0));
        s.uploads
            .push_back(Err("Unavailable: transport error".into()));
        s.ping.push_back(Err("Unavailable: transport error".into()));
        s.ping.push_back(Ok(()));
        s.uploads.push_back(Ok(Some(Step::Uploaded)));
    });
    let clock = TickClock::new(1_000);
    // The ceiling of each wait (no jitter below it), so the second pause is visibly longer.
    let job = CloudJob::with_policy(
        Arc::new(FakeLinks(Some(link.clone()))),
        clock.clone(),
        RetryPolicy {
            jitter: || 1.0,
            ..policy()
        },
    );

    let t = job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!((t.paused, t.failed), (1, 0));
    assert!(
        link.0.lock().unwrap().failures.is_empty(),
        "no attempt is counted offline"
    );

    // Still inside the pause: nothing is called.
    let calls = link.0.lock().unwrap().calls.len();
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.0.lock().unwrap().calls.len(), calls);

    // Time is up: a probe, which fails and doubles the pause.
    clock.advance(2);
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("ping"), 1);
    assert_eq!(link.calls("upload"), 1, "a failed probe sends nothing");

    clock.advance(1);
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("ping"), 1, "paused again for longer");

    clock.advance(10);
    let t = job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("ping"), 2);
    assert_eq!(t.uploaded, 1, "the probe succeeded and the queue drains");
}

#[tokio::test]
async fn the_head_sync_runs_every_five_minutes_and_on_request() {
    let link = FakeLink::default();
    let link = Arc::new(link);
    let clock = TickClock::new(1_000);
    let job = job(&link, &clock);

    job.run_phase(Phase::Sync).await.unwrap();
    assert_eq!(link.calls("sync"), 1);
    clock.advance(60);
    job.run_phase(Phase::Sync).await.unwrap();
    assert_eq!(link.calls("sync"), 1, "not before the period");
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(
        link.calls("sync"),
        1,
        "the upload tick does not sync by itself"
    );

    job.sync_request().store(true, Ordering::SeqCst);
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("sync"), 2, "a fetch or sl asked for one");
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("sync"), 2, "one request, one sync");

    clock.advance(SYNC_EVERY.as_secs() as i64);
    job.run_phase(Phase::Sync).await.unwrap();
    assert_eq!(link.calls("sync"), 3);
}

#[tokio::test]
async fn the_fetch_marker_forces_one_sync_and_is_consumed() {
    let link = Arc::new(FakeLink::default());
    let clock = TickClock::new(1_000);
    let run = tempfile::tempdir().unwrap();
    let marker = run.path().join(crate::branchless::pull::MARKER);
    let job = job(&link, &clock).with_marker(run.path());

    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("sync"), 0, "no marker, no sync");
    std::fs::write(&marker, b"").unwrap();
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("sync"), 1, "fetch asked");
    assert!(!marker.exists(), "the ask is consumed");
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("sync"), 1, "one ask, one sync");

    // Something that is not a plain file is never an ask.
    std::fs::create_dir(&marker).unwrap();
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("sync"), 1);
    assert!(marker.is_dir());
}

#[tokio::test]
async fn a_failed_sync_is_retried_with_backoff_not_in_a_tight_loop() {
    let link = FakeLink::with(|s| {
        s.sync.push_back(Err(
            "cloud workspace changed under 3 attempts (VERSION_CONFLICT)".into(),
        ));
    });
    let clock = TickClock::new(1_000);
    let job = job(&link, &clock);
    job.run_phase(Phase::Sync).await.unwrap();
    job.run_phase(Phase::Sync).await.unwrap();
    assert_eq!(link.calls("sync"), 1, "the retry waits for its backoff");
    clock.advance(2);
    job.run_phase(Phase::Sync).await.unwrap();
    assert_eq!(link.calls("sync"), 2);
}

#[tokio::test]
async fn facts_are_cached_so_an_idle_tick_asks_the_server_nothing() {
    let link = Arc::new(FakeLink::default());
    let clock = TickClock::new(1_000);
    let job = job(&link, &clock);
    for _ in 0..5 {
        job.run_phase(Phase::Upload).await.unwrap();
        clock.advance(5);
    }
    assert_eq!(link.calls("facts"), 1);
    clock.advance(FACTS_TTL_SECS);
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("facts"), 2);
}

// ------------------------------------------------------------------------------ B14 review fixes

#[tokio::test]
async fn cloud_off_stops_uploads_at_the_next_tick_not_when_the_cached_facts_expire() {
    let link = FakeLink::with(|s| {
        s.due.push_back(due(1, 0));
        s.uploads.push_back(Ok(Some(Step::Uploaded)));
    });
    let clock = TickClock::new(1_000);
    let job = job(&link, &clock);
    job.run_phase(Phase::Upload).await.unwrap();
    assert!(link.calls("upload") >= 1, "on: the queue drains");

    for edit in [
        (|s: &mut ModeSettings| s.cloud = CloudSetting::Off) as fn(&mut ModeSettings),
        |s| s.branchless = false,
    ] {
        let mut off = facts().settings;
        edit(&mut off);
        {
            let mut s = link.0.lock().unwrap();
            s.settings = Some(off);
            s.due.push_back(due(3, 0));
            s.uploads.push_back(Ok(Some(Step::Uploaded)));
        }
        let before = link.calls("upload");
        clock.advance(5);
        job.run_phase(Phase::Upload).await.unwrap();
        job.sync_request().store(true, Ordering::SeqCst);
        job.run_phase(Phase::Upload).await.unwrap();
        job.run_phase(Phase::Sync).await.unwrap();
        assert_eq!(
            link.calls("upload"),
            before,
            "nothing is sent once the user switched it off"
        );
        assert_eq!(link.calls("sync"), 0, "and nothing is synced");
        assert_eq!(
            link.calls("facts"),
            1,
            "the cached answer of the server was not what stopped it"
        );
        let mut s = link.0.lock().unwrap();
        s.settings = None;
        s.due.clear();
        s.uploads.clear();
    }
}

#[tokio::test]
async fn a_failing_facts_call_is_not_repeated_every_tick() {
    let link = FakeLink::with(|s| {
        s.facts = Some(Err("DeadlineExceeded: slow".into()));
    });
    let clock = TickClock::new(1_000);
    let job = job(&link, &clock);
    job.run_phase(Phase::Upload).await.unwrap();
    job.run_phase(Phase::Upload).await.unwrap();
    job.run_phase(Phase::Sync).await.unwrap();
    assert_eq!(link.calls("facts"), 1, "the repo waits for its backoff");
    clock.advance(1);
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(link.calls("facts"), 2);
}

struct FailingLinks(std::sync::atomic::AtomicUsize);

#[async_trait]
impl Uplinks for FailingLinks {
    async fn repos(&self, _: Phase, _: i64) -> Result<Vec<RepoKey>, String> {
        Ok(vec![RepoKey(1)])
    }

    async fn open(&self, _: RepoKey) -> Result<Option<Arc<dyn Uplink>>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("no usable workspace".into())
    }
}

#[tokio::test]
async fn a_repo_that_cannot_be_opened_is_not_reopened_every_tick() {
    let links = Arc::new(FailingLinks(Default::default()));
    let clock = TickClock::new(1_000);
    let job = CloudJob::with_policy(
        links.clone(),
        clock.clone(),
        RetryPolicy {
            jitter: || 0.0,
            ..policy()
        },
    );
    let opens = || links.0.load(Ordering::SeqCst);
    job.run_phase(Phase::Upload).await.unwrap();
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(opens(), 1);
    clock.advance(1);
    job.run_phase(Phase::Upload).await.unwrap();
    assert_eq!(opens(), 2);
}

#[test]
fn only_a_private_or_internal_repo_is_not_public() {
    assert!(!prod::is_public("private"));
    assert!(!prod::is_public("internal"));
    // Fail closed: what the server leaves empty, or writes in a way we do not know, is public.
    for v in [
        "", "public", "Public", "PRIVATE", "Private", " private", "private ", "secret", "\0",
    ] {
        assert!(prod::is_public(v), "{v:?} counts as public");
    }
}

#[test]
fn a_directory_binds_only_to_the_repo_and_remote_it_holds() {
    let url = "asc+https://h/acme/a";
    let k = RepoKey;
    assert!(prod::binds(k(1), Some(url), RemoteKind::Server, k(1), url));
    assert!(
        !prod::binds(k(2), Some(url), RemoteKind::Server, k(1), url),
        "another repo"
    );
    assert!(
        !prod::binds(
            k(1),
            Some("asc+https://h/acme/b"),
            RemoteKind::Server,
            k(1),
            url
        ),
        "another remote"
    );
    assert!(!prod::binds(k(1), None, RemoteKind::Server, k(1), url));
    assert!(!prod::binds(
        k(1),
        Some(url),
        RemoteKind::GitRemote,
        k(1),
        url
    ));
}

mod binding {
    use asc_state::state::workspaces::{self, Mode, NewWorkspace};
    use asc_state::{RepoId, WorkspaceId};

    use super::*;
    use crate::cli::common::support::Fixture;

    const URL_A: &str = "asc+http://127.0.0.1:1/acme/a";

    /// Repo A (an asc server repo) whose workspace row names the directory of the fixture, which
    /// holds the fixture repo B.
    async fn a_row_pointing_at_b() -> (Fixture, RepoKey) {
        let f = Fixture::new().await;
        let a = asc_state::state::repos::insert(
            &f.db,
            &NewRepo {
                id: RepoId::new(),
                name: "a".into(),
                ns: vec![1],
                remote_url: Some(URL_A.into()),
                remote_kind: RemoteKind::Server,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        assert_ne!(a, f.repo);
        // The same directory spelled another way: the old row of A outlived A.
        let path = f.root.join(".").display().to_string();
        workspaces::insert(
            &f.db,
            &NewWorkspace {
                id: WorkspaceId::new(),
                repo: a,
                path: path.clone(),
                mount: path,
                mode: Mode::Plain,
                head: "refs/heads/master".into(),
                base_cs: None,
                sparse: None,
                ephemeral_until: None,
            },
        )
        .await
        .unwrap();
        (f, a)
    }

    #[tokio::test]
    async fn a_directory_of_another_repo_is_never_connected_to_the_server_of_this_one() {
        let (f, a) = a_row_pointing_at_b().await;
        let opened = prod::open_inner(f.home.clone(), f.root.clone(), a, URL_A.into())
            .await
            .map(|o| o.is_some());
        assert_eq!(opened, Ok(false), "the directory holds repo B, not A");

        let uplinks = prod::ProdUplinks::new(f.home.clone());
        let link = uplinks.open(a).await.map(|l| l.is_some());
        assert_eq!(link, Ok(false), "A has no usable workspace");
    }
}
