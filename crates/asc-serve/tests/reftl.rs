//! B08c: the landed marker, its expiry and the bounded owner-only reads (SPEC §8.8, §21.2.4,
//! §21.2.4a), over a real `Registry`, `ServerRefs` and `ImportPush`.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, unused_imports)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_git::realgit::RealGit;
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::server::{CallCtx, RefsHandler, RepoName, status_rpc_error, wire};
use asc_serve::import_push::{ImportOptions, ImportPush, PushCommand, Quarantine};
use asc_serve::registry::{NewRepo, Registry, RepoEntry};
use asc_serve::update::{HookRejected, HookRequest, PipelineHooks, PushHooks, ServerRefs};
use asc_serve::{migrate, refpolicy};
use asc_store::refs::{Refs, RepoKey};
use async_trait::async_trait;
use tonic::Status;
use uuid::Uuid;

// ------------------------------------------------------------------------------ git fixtures

struct Forge {
    _tmp: tempfile::TempDir,
    git_dir: PathBuf,
}

fn git(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
    use std::io::Write as _;
    let real = RealGit::resolve(None, None).expect("git is installed");
    let mut cmd = real.command().unwrap();
    cmd.arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@x")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@x")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

impl Forge {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("r.git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let real = RealGit::resolve(None, None).unwrap();
        assert!(
            real.command()
                .unwrap()
                .args(["init", "--bare", "--quiet", "--initial-branch=master"])
                .arg(&git_dir)
                .status()
                .unwrap()
                .success()
        );
        Self { _tmp: tmp, git_dir }
    }

    fn commit(&self, parent: Option<&str>, content: &str) -> String {
        let blob = git(
            &self.git_dir,
            &["hash-object", "-w", "--stdin"],
            content.as_bytes(),
        );
        let tree = git(
            &self.git_dir,
            &["mktree"],
            format!("100644 blob {blob}\ta.txt\n").as_bytes(),
        );
        let msg = format!("commit {content}");
        let mut args = vec!["commit-tree", tree.as_str(), "-m", msg.as_str()];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        git(&self.git_dir, &args, b"")
    }

    /// Like [`Self::commit`], with the repository policy file `policy` in the tree.
    fn commit_with_policy(&self, parent: Option<&str>, content: &str, policy: &str) -> String {
        let blob = git(
            &self.git_dir,
            &["hash-object", "-w", "--stdin"],
            content.as_bytes(),
        );
        let pol = git(
            &self.git_dir,
            &["hash-object", "-w", "--stdin"],
            policy.as_bytes(),
        );
        let dir = git(
            &self.git_dir,
            &["mktree"],
            format!(
                "100644 blob {pol}	policy.toml
"
            )
            .as_bytes(),
        );
        let tree = git(
            &self.git_dir,
            &["mktree"],
            format!(
                "100644 blob {blob}	a.txt
040000 tree {dir}	.asc
"
            )
            .as_bytes(),
        );
        let msg = format!("commit {content}");
        let mut args = vec!["commit-tree", tree.as_str(), "-m", msg.as_str()];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        git(&self.git_dir, &args, b"")
    }

    fn whole(&self) -> Quarantine {
        Quarantine {
            object_dir: self.git_dir.join("objects"),
            alternate_dirs: Vec::new(),
        }
    }
}

fn cmd(name: &str, old: Option<&str>, new: Option<&str>) -> PushCommand {
    PushCommand {
        name: name.into(),
        old: old.map(|s| s.parse().unwrap()),
        new: new.map(|s| s.parse().unwrap()),
    }
}

// ------------------------------------------------------------------------------ server fixtures

#[derive(Default)]
struct CountingHooks {
    pre_receive: AtomicUsize,
}

#[async_trait]
impl PushHooks for CountingHooks {
    async fn pre_receive(&self, _req: &HookRequest<'_>) -> Result<(), HookRejected> {
        self.pre_receive.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn outbox_effects(&self) -> &'static [&'static str] {
        &["post_receive"]
    }
}

struct Fx {
    forge: Forge,
    registry: Arc<Registry>,
    entry: Arc<RepoEntry>,
    hooks: Arc<CountingHooks>,
    push: ImportPush,
    refs: ServerRefs,
}

async fn fx(branchless: bool) -> Fx {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let entry = registry
        .create(&RepoName::parse("o/r").unwrap(), &NewRepo::default())
        .await
        .unwrap();
    sqlx::query("UPDATE repos SET branchless = $1 WHERE id = $2")
        .bind(branchless)
        .bind(entry.key.0)
        .execute(registry.pool())
        .await
        .unwrap();
    let pipeline = Arc::new(PipelineHooks::default());
    let hooks = Arc::new(CountingHooks::default());
    assert!(pipeline.set_push_hooks(hooks.clone()).is_ok());
    Fx {
        forge: Forge::new(),
        push: ImportPush::new(registry.clone(), pipeline.clone()),
        refs: ServerRefs::new(registry.clone(), pipeline),
        registry,
        entry,
        hooks,
    }
}

fn ctx(principal: &str, mode: IdMode) -> CallCtx {
    CallCtx {
        repo: RepoName::parse("o/r").unwrap(),
        id_mode: mode,
        principal: Principal {
            id: principal.into(),
        },
        op_id: Some(Uuid::new_v4()),
        client: None,
        admin: Default::default(),
    }
}

fn code(s: &Status) -> String {
    status_rpc_error(s).map_or_else(|| s.code().to_string(), |e| e.code().to_owned())
}

impl Fx {
    async fn git_push(
        &self,
        who: &str,
        commands: &[PushCommand],
    ) -> Result<wire::ImportGitPushResponse, Status> {
        self.push
            .import_git_push(
                &ctx(who, IdMode::Git),
                &self.forge.whole(),
                commands,
                &ImportOptions::default(),
            )
            .await
    }

    async fn stored(&self, name: &str) -> Option<RefTarget> {
        self.registry
            .refs()
            .get(self.entry.key, &RefName::new(name.as_bytes()).unwrap())
            .await
            .unwrap()
            .map(|e| e.target)
    }

    async fn outbox_effects(&self) -> Vec<String> {
        sqlx::query("SELECT effects FROM outbox ORDER BY id")
            .fetch_all(self.registry.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8(r.get::<Vec<u8>, _>("effects")).unwrap())
            .collect()
    }

    async fn land(&self, who: &str, to: RefTarget) -> Result<(), Status> {
        self.refs
            .update_refs(
                &ctx(who, IdMode::Asc),
                wire::UpdateRefsRequest {
                    repo: Some(wire::RepoRef {
                        name: "o/r".into(),
                        id_mode: 0,
                    }),
                    updates: vec![wire::RefUpdate {
                        name: "refs/heads/master".into(),
                        expect: Some(wire::ref_update::Expect::Any(true)),
                        new: Some(wire::RefTarget::from(&to)),
                        delete: false,
                    }],
                    mode: wire::UpdateMode::Land as i32,
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
    }

    async fn landed_at(&self, owner: i64, branch: &str) -> Option<i64> {
        sqlx::query("SELECT landed_at FROM user_ref_meta WHERE owner = $1 AND name = $2")
            .bind(owner)
            .bind(branch)
            .fetch_optional(self.registry.pool())
            .await
            .unwrap()
            .and_then(|r| r.get::<Option<i64>, _>("landed_at"))
    }

    async fn set_marker(&self, owner: i64, branch: &str, landed_at: i64, last_log_at: i64) {
        sqlx::query(
            "INSERT INTO user_ref_meta (ns, owner, name, landed_at) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (ns, owner, name) DO UPDATE SET landed_at = excluded.landed_at",
        )
        .bind(self.entry.ns.as_bytes().to_vec())
        .bind(owner)
        .bind(branch)
        .bind(landed_at)
        .execute(self.registry.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE ref_log SET at = $1 WHERE repo_id = $2 AND name = $3")
            .bind(last_log_at)
            .bind(self.entry.key.0)
            .bind(format!("refs/users/{owner}/heads/{branch}"))
            .execute(self.registry.pool())
            .await
            .unwrap();
    }

    fn reaper(&self, days: u32) -> LandedReaper {
        let expirer = Arc::new(Expirer::new(self.registry.clone(), Arc::default()));
        LandedReaper::new(self.registry.clone(), expirer, days)
    }

    async fn push_user(&self, who: &str, branch: &str, parent: &str, text: &str) -> String {
        let c = self.forge.commit(Some(parent), text);
        self.git_push(who, &[cmd(&format!("refs/heads/{branch}"), None, Some(&c))])
            .await
            .unwrap();
        c
    }
}

/// A repo with `master` pushed by git as `7:maint`; returns the commit's git id.
async fn with_master(f: &Fx) -> String {
    let c1 = f.forge.commit(None, "one");
    f.git_push("7:maint", &[cmd("refs/heads/master", None, Some(&c1))])
        .await
        .unwrap();
    c1
}

/// Like [`with_master`], with `policy` in the repository's policy file.
async fn with_policy(f: &Fx, policy: &str) -> String {
    let c1 = f.forge.commit_with_policy(None, "one", policy);
    f.git_push("7:maint", &[cmd("refs/heads/master", None, Some(&c1))])
        .await
        .unwrap();
    c1
}

async fn seed_user_refs(f: &Fx, owner: i64, n: usize) {
    let target = f.stored("refs/heads/master").await.unwrap();
    sqlx::query(
        "INSERT INTO refs (repo_id, name, target, version) \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < $3) \
         SELECT $1, 'refs/users/' || $4 || '/heads/seed' || i, $2, 1 FROM n",
    )
    .bind(f.entry.key.0)
    .bind(target.to_bytes().to_vec())
    .bind(n as i64)
    .bind(owner)
    .execute(f.registry.pool())
    .await
    .unwrap();
}

// ------------------------------------------------------------------------------ tests

use asc_core::id::ChangesetId;
use asc_serve::expire::Expirer;
use asc_serve::reftl::{self, LandedReaper, Reach, ReapReport};
use sqlx::Row;

const ALICE: &str = "42:alice";
const BOB: &str = "43:bob";
const DAY: i64 = 86_400;

fn cs_of(t: RefTarget) -> ChangesetId {
    match t {
        RefTarget::Changeset(c) => c,
        RefTarget::Tag(_) => panic!("tag"),
    }
}

#[tokio::test]
async fn a_land_marks_the_authors_landed_stack_and_only_that() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let a1 = f.push_user(ALICE, "base", &c1, "a1").await;
    let a2 = f.forge.commit(Some(&a1), "a2");
    f.git_push(ALICE, &[cmd("refs/heads/feat", None, Some(&a2))])
        .await
        .unwrap();
    f.push_user(ALICE, "other", &c1, "other").await;
    f.push_user(BOB, "bobs", &c1, "bob").await;
    let tip = f.stored("refs/users/42/heads/feat").await.unwrap();

    f.land(ALICE, tip).await.unwrap();

    assert!(f.landed_at(42, "feat").await.is_some());
    // `base` is an ancestor of the landed tip: its whole stack landed too.
    assert!(f.landed_at(42, "base").await.is_some());
    assert!(f.landed_at(42, "other").await.is_none());
    assert!(f.landed_at(43, "bobs").await.is_none());
}

#[tokio::test]
async fn a_ref_on_the_trunk_is_not_marked_and_a_marker_keeps_its_first_time() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    f.git_push(ALICE, &[cmd("refs/heads/idle", None, Some(&c1))])
        .await
        .unwrap();
    let a1 = f.push_user(ALICE, "feat", &c1, "a1").await;
    let tip = f.stored("refs/users/42/heads/feat").await.unwrap();
    f.land(ALICE, tip).await.unwrap();
    assert!(f.landed_at(42, "idle").await.is_none());
    let first = f.landed_at(42, "feat").await.unwrap();
    f.set_marker(42, "feat", first - 1_000, 0).await;
    // A later land by the same author does not move the marker.
    f.push_user(ALICE, "next", &a1, "a2").await;
    let next = f.stored("refs/users/42/heads/next").await.unwrap();
    f.land(ALICE, next).await.unwrap();
    assert_eq!(f.landed_at(42, "feat").await, Some(first - 1_000));
    assert!(f.landed_at(42, "next").await.is_some());
}

#[tokio::test]
async fn the_marker_and_the_delete_check_read_only_the_owners_rows() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let a1 = f.push_user(ALICE, "one", &c1, "a1").await;
    f.push_user(ALICE, "two", &a1, "a2").await;
    seed_user_refs(&f, 43, 300).await;
    seed_user_refs(&f, 44, 300).await;
    let tip = f.stored("refs/users/42/heads/two").await.unwrap();

    let mut tx = f.registry.pool().begin().await.unwrap();
    let scan = reftl::mark_landed(
        &mut tx,
        f.entry.ns,
        f.entry.key.0,
        "refs/heads/master",
        ALICE,
        &[cs_of(tip)],
        None,
        &[],
        1_000,
        1_000,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        scan.refs_read, 2,
        "Alice's two refs, none of the 600 others"
    );

    // `push --delete` of `one`: its change stays reachable from `two`.
    let one = cs_of(f.stored("refs/users/42/heads/one").await.unwrap());
    let (reach, stats) = reftl::delete_reachability(
        f.registry.pool(),
        f.entry.ns,
        f.entry.key.0,
        42,
        "refs/users/42/heads/one",
        &[one],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(reach, Reach::Unreachable(vec![]));
    assert_eq!(stats.refs_read, 2);

    // Deleting `two` leaves its change reachable from nothing.
    let two = cs_of(f.stored("refs/users/42/heads/two").await.unwrap());
    let (reach, _) = reftl::delete_reachability(
        f.registry.pool(),
        f.entry.ns,
        f.entry.key.0,
        42,
        "refs/users/42/heads/two",
        &[two],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(reach, Reach::Unreachable(vec![two]));
}

#[tokio::test]
async fn the_reach_check_gives_up_beyond_its_tip_cap() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    f.push_user(ALICE, "one", &c1, "a1").await;
    let one = cs_of(f.stored("refs/users/42/heads/one").await.unwrap());
    let many = vec![one; reftl::MAX_REACH_TIPS + 1];
    let (reach, _) = reftl::delete_reachability(
        f.registry.pool(),
        f.entry.ns,
        f.entry.key.0,
        42,
        "refs/users/42/heads/one",
        &[one],
        &many,
    )
    .await
    .unwrap();
    assert_eq!(reach, Reach::CapExceeded);
}

#[tokio::test]
async fn a_landed_ref_is_deleted_after_landed_ref_days_through_the_pipeline() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    f.push_user(ALICE, "feat", &c1, "a1").await;
    f.push_user(ALICE, "young", &c1, "y1").await;
    f.set_marker(42, "feat", 1_000, 100).await;
    f.set_marker(42, "young", 1_000 + 5 * DAY, 100).await;
    let at = 1_000 + 8 * DAY;
    let before = f.outbox_effects().await.len();

    let r = f.reaper(7).run_once(at).await.unwrap();

    assert_eq!(r.deleted, 1, "{r:?}");
    assert!(f.stored("refs/users/42/heads/feat").await.is_none());
    assert!(f.stored("refs/users/42/heads/young").await.is_some());
    assert!(
        f.landed_at(42, "feat").await.is_none(),
        "marker row removed"
    );
    assert_eq!(f.outbox_effects().await.len(), before + 1);
}

#[tokio::test]
async fn zero_days_disables_and_a_ref_updated_after_landing_is_kept() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    f.push_user(ALICE, "feat", &c1, "a1").await;
    f.push_user(ALICE, "again", &c1, "a2").await;
    f.set_marker(42, "feat", 1_000, 100).await;
    // Updated after it landed: the ref log is newer than the marker.
    f.set_marker(42, "again", 1_000, 5_000).await;
    let at = 1_000 + 30 * DAY;

    assert_eq!(
        f.reaper(0).run_once(at).await.unwrap(),
        ReapReport::default()
    );
    assert!(f.stored("refs/users/42/heads/feat").await.is_some());

    let r = f.reaper(7).run_once(at).await.unwrap();
    assert_eq!((r.deleted, r.cleared), (1, 1));
    assert!(f.stored("refs/users/42/heads/again").await.is_some());
    assert!(f.landed_at(42, "again").await.is_none(), "marker cleared");
    assert_eq!(
        f.reaper(7).run_once(at).await.unwrap(),
        ReapReport::default()
    );
}

#[tokio::test]
async fn a_marker_whose_ref_is_gone_is_removed() {
    let f = fx(true).await;
    with_master(&f).await;
    f.set_marker(42, "ghost", 1_000, 0).await;
    f.reaper(7).run_once(1_000 + 30 * DAY).await.unwrap();
    assert!(f.landed_at(42, "ghost").await.is_none());
}

#[tokio::test]
async fn a_policys_cleanup_days_replaces_the_servers_for_that_repository() {
    let f = fx(true).await;
    let c1 = with_policy(
        &f,
        "version = 1
[roles]
maintainer = [\"user:a\"]
[stacks]
cleanup_days = 3
",
    )
    .await;
    f.push_user(ALICE, "old", &c1, "o1").await;
    f.push_user(ALICE, "young", &c1, "y1").await;
    f.set_marker(42, "old", 1_000, 100).await;
    f.set_marker(42, "young", 1_000 + 2 * DAY, 100).await;
    let at = 1_000 + 4 * DAY;

    // The server would keep a landed ref for 30 days; the policy says 3.
    let r = f.reaper(30).run_once(at).await.unwrap();

    assert_eq!(r.deleted, 1, "{r:?}");
    assert!(f.stored("refs/users/42/heads/old").await.is_none());
    assert!(f.stored("refs/users/42/heads/young").await.is_some());
    assert!(
        f.landed_at(42, "young").await.is_some(),
        "not due: marker stays"
    );
}

#[tokio::test]
async fn a_policys_cleanup_days_can_keep_a_ref_longer_or_switch_the_server_off() {
    let longer = fx(true).await;
    let c1 = with_policy(
        &longer,
        "version = 1
[roles]
maintainer = [\"user:a\"]
[stacks]
cleanup_days = 30
",
    )
    .await;
    longer.push_user(ALICE, "feat", &c1, "a1").await;
    longer.set_marker(42, "feat", 1_000, 100).await;
    let at = 1_000 + 8 * DAY;
    assert_eq!(
        longer.reaper(7).run_once(at).await.unwrap(),
        ReapReport::default(),
        "the policy keeps it 30 days although the server says 7"
    );
    assert!(longer.stored("refs/users/42/heads/feat").await.is_some());

    // Server setting 0 (off): a repository that sets cleanup_days still cleans up.
    let own = fx(true).await;
    let c1 = with_policy(
        &own,
        "version = 1
[roles]
maintainer = [\"user:a\"]
[stacks]
cleanup_days = 3
",
    )
    .await;
    own.push_user(ALICE, "feat", &c1, "a1").await;
    own.set_marker(42, "feat", 1_000, 100).await;
    let r = own.reaper(0).run_once(1_000 + 4 * DAY).await.unwrap();
    assert_eq!(r.deleted, 1, "{r:?}");
}
