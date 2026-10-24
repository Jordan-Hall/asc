//! B08b: user-ref authorisation, the per-user ref cap and the quota on user-ref pushes (SPEC
//! §21.5.1, §21.3.2, §21.3.7), over a real `Registry`, `ServerRefs` and `ImportPush`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_git::realgit::RealGit;
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::server::{CallCtx, RefsHandler, RepoName, status_rpc_error, wire};
use asc_serve::cloud::Limits;
use asc_serve::cloud::quota::{CHANGESET_BASE_COST, Quota, UserQuota};
use asc_serve::import_push::{ImportOptions, ImportPush, PushCommand, Quarantine};
use asc_serve::migrate;
use asc_serve::registry::{NewRepo, Registry, RepoEntry};
use asc_serve::update::{HookRejected, HookRequest, PipelineHooks, PushHooks, ServerRefs};
use asc_store::refs::Refs;
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
    #[allow(dead_code)]
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

    async fn list(&self, who: &str, prefix: &str) -> Result<Vec<String>, Status> {
        self.list_page(who, prefix, 0, "")
            .await
            .map(|r| r.refs.into_iter().map(|x| x.name).collect())
    }

    async fn list_page(
        &self,
        who: &str,
        prefix: &str,
        limit: u32,
        after: &str,
    ) -> Result<wire::ListRefsResponse, Status> {
        self.refs
            .list_refs(
                &ctx(who, IdMode::Asc),
                wire::ListRefsRequest {
                    repo: Some(wire::RepoRef {
                        name: "o/r".into(),
                        id_mode: 0,
                    }),
                    prefix: prefix.into(),
                    limit,
                    after: after.into(),
                    ..Default::default()
                },
            )
            .await
    }

    async fn update(&self, who: &str, name: &str, to: RefTarget) -> Result<(), Status> {
        self.update_with(&self.refs, who, name, to).await
    }

    async fn update_with(
        &self,
        refs: &ServerRefs,
        who: &str,
        name: &str,
        to: RefTarget,
    ) -> Result<(), Status> {
        refs.update_refs(
            &ctx(who, IdMode::Asc),
            wire::UpdateRefsRequest {
                repo: Some(wire::RepoRef {
                    name: "o/r".into(),
                    id_mode: 0,
                }),
                updates: vec![wire::RefUpdate {
                    name: name.into(),
                    expect: Some(wire::ref_update::Expect::Absent(true)),
                    new: Some(wire::RefTarget::from(&to)),
                    delete: false,
                }],
                mode: wire::UpdateMode::FastForward as i32,
                ..Default::default()
            },
        )
        .await
        .map(|_| ())
    }

    async fn outbox_effects(&self) -> Vec<String> {
        use sqlx::Row;
        sqlx::query("SELECT effects FROM outbox ORDER BY id")
            .fetch_all(self.registry.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8(r.get::<Vec<u8>, _>("effects")).unwrap())
            .collect()
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

// ------------------------------------------------------------------------------ tests

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

#[tokio::test]
async fn a_non_owner_cannot_push_or_list_another_users_ref() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");
    f.git_push("42:alice", &[cmd("refs/heads/feat", None, Some(&c2))])
        .await
        .unwrap();
    let target = f.stored("refs/users/42/heads/feat").await.unwrap();

    // Naming the user namespace in an update is refused, whoever asks.
    for who in ["43:bob", "42:alice"] {
        let e = f
            .update(who, "refs/users/42/heads/feat2", target)
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "{who}");
    }
    // Bob's push of the same branch name never touches Alice's ref.
    let c3 = f.forge.commit(Some(&c2), "three");
    f.git_push("43:bob", &[cmd("refs/heads/feat", None, Some(&c3))])
        .await
        .unwrap();
    assert_eq!(f.stored("refs/users/42/heads/feat").await, Some(target));
    // Bob cannot delete Alice's ref: a delete in his namespace is against his own ref.
    let e = f
        .git_push("43:bob", &[cmd("refs/heads/feat", Some(&c2), None)])
        .await
        .unwrap_err();
    assert_eq!(code(&e), "REF_CONFLICT");
    assert_eq!(f.stored("refs/users/42/heads/feat").await, Some(target));

    // Listing: Bob cannot list Alice's refs by prefix.
    for prefix in ["refs/users/42/", "refs/users/42/heads/"] {
        let e = f.list("43:bob", prefix).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "{prefix}");
    }
    assert_eq!(
        f.list("43:bob", "refs/users/43/").await.unwrap(),
        ["refs/users/43/heads/feat"]
    );
    // The bare namespace lists only the caller's own.
    assert_eq!(
        f.list("43:bob", "refs/users/").await.unwrap(),
        ["refs/users/43/heads/feat"]
    );
    // A principal with no user id has no user refs at all.
    let e = f.list("carol", "refs/users/42/").await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
}

#[tokio::test]
async fn user_ref_updates_write_no_outbox_post_receive() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let before = f.outbox_effects().await.len();
    let c2 = f.forge.commit(Some(&c1), "two");
    f.git_push("42:alice", &[cmd("refs/heads/feat", None, Some(&c2))])
        .await
        .unwrap();
    let c3 = f.forge.commit(Some(&c2), "three");
    f.git_push("42:alice", &[cmd("refs/heads/feat", Some(&c2), Some(&c3))])
        .await
        .unwrap();
    f.git_push("42:alice", &[cmd("refs/heads/feat", Some(&c3), None)])
        .await
        .unwrap();
    let all = f.outbox_effects().await;
    for e in &all[before..] {
        assert!(!e.contains("post_receive"), "{e}");
    }
}

#[tokio::test]
async fn the_1001st_user_ref_gets_user_refs_limit_and_other_users_are_unaffected() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");
    seed_user_refs(&f, 42, 1000).await;

    let e = f
        .git_push(
            "42:alice",
            &[cmd("refs/heads/one-too-many", None, Some(&c2))],
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), "USER_REFS_LIMIT");
    assert!(f.stored("refs/users/42/heads/one-too-many").await.is_none());
    // The same through UpdateRefs.
    let target = f.stored("refs/heads/master").await.unwrap();
    let e = f
        .update("42:alice", "refs/heads/also", target)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "USER_REFS_LIMIT");
    // Another user has their own budget; deleting one of Alice's frees a slot.
    f.git_push("43:bob", &[cmd("refs/heads/x", None, Some(&c2))])
        .await
        .unwrap();
    sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = 'refs/users/42/heads/seed1'")
        .bind(f.entry.key.0)
        .execute(f.registry.pool())
        .await
        .unwrap();
    f.git_push(
        "42:alice",
        &[cmd("refs/heads/one-too-many", None, Some(&c2))],
    )
    .await
    .unwrap();
}

/// A pipeline whose user quota allows `bytes` per user per repository and `max` user refs.
fn pipeline_with(bytes: u64, max: usize) -> Arc<PipelineHooks> {
    let pipeline = Arc::new(PipelineHooks::default());
    let limits = Limits {
        max_user_bytes: bytes,
        ..Limits::default()
    };
    assert!(
        pipeline
            .set_user_quota(UserQuota::new(Quota::local(limits), max))
            .is_ok()
    );
    pipeline
}

fn pusher(f: &Fx, bytes: u64, max: usize) -> ImportPush {
    ImportPush::new(f.registry.clone(), pipeline_with(bytes, max))
}

/// Bytes charged to `owner` in the fixture repository.
async fn used(f: &Fx, owner: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT bytes FROM cloud_usage WHERE ns = $1 AND owner = $2")
        .bind(f.entry.ns.as_bytes().to_vec())
        .bind(owner)
        .fetch_optional(f.registry.pool())
        .await
        .unwrap()
        .unwrap_or(0)
}

/// Marks `owner` as having used `bytes` already.
async fn set_used(f: &Fx, owner: i64, bytes: i64) {
    sqlx::query("INSERT INTO cloud_usage (ns, owner, bytes, workspaces) VALUES ($1, $2, $3, 0)")
        .bind(f.entry.ns.as_bytes().to_vec())
        .bind(owner)
        .bind(bytes)
        .execute(f.registry.pool())
        .await
        .unwrap();
}

async fn push_as_alice(
    f: &Fx,
    push: &ImportPush,
    name: &str,
    new: &str,
) -> Result<wire::ImportGitPushResponse, Status> {
    push.import_git_push(
        &ctx("42:alice", IdMode::Git),
        &f.forge.whole(),
        &[cmd(name, None, Some(new))],
        &ImportOptions::default(),
    )
    .await
}

#[tokio::test]
async fn a_user_ref_push_is_charged_to_the_quota_and_a_refusal_moves_nothing() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");

    // A byte cap too small for even one changeset: the push is refused and nothing is charged.
    let push = pusher(&f, 100, 2);
    let e = push_as_alice(&f, &push, "refs/heads/feat", &c2)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "QUOTA_EXCEEDED");
    assert!(f.stored("refs/users/42/heads/feat").await.is_none());
    assert_eq!(used(&f, 42).await, 0);

    // With the quota open, this server's cap (2) applies.
    let push = pusher(&f, 1 << 30, 2);
    push_as_alice(&f, &push, "refs/heads/a", &c2).await.unwrap();
    push_as_alice(&f, &push, "refs/heads/b", &c2).await.unwrap();
    let e = push_as_alice(&f, &push, "refs/heads/c", &c2)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "USER_REFS_LIMIT");
}

fn server(f: &Fx, bytes: u64, max: usize) -> ServerRefs {
    ServerRefs::new(f.registry.clone(), pipeline_with(bytes, max))
}

async fn count(f: &Fx, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(f.registry.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn concurrent_creates_near_the_configured_cap_end_at_or_below_it() {
    let f = fx(true).await;
    with_master(&f).await;
    let target = f.stored("refs/heads/master").await.unwrap();
    let refs = server(&f, 1 << 30, 3);
    seed_user_refs(&f, 42, 1).await;

    let names: Vec<String> = (0..8).map(|i| format!("refs/heads/c{i}")).collect();
    let results = futures::future::join_all(
        names
            .iter()
            .map(|n| f.update_with(&refs, "42:alice", n, target)),
    )
    .await;
    let held = count(
        &f,
        "SELECT COUNT(*) FROM refs WHERE name LIKE 'refs/users/42/heads/%'",
    )
    .await;
    assert!(held <= 3, "{held} user refs with a cap of 3");
    assert_eq!(held, 3);
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 2);
    for e in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(code(e), "USER_REFS_LIMIT");
    }
}

#[tokio::test]
async fn an_over_quota_owner_is_refused_before_anything_is_ingested() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");
    let graph = count(&f, "SELECT COUNT(*) FROM cs_graph").await;
    let mapped = count(&f, "SELECT COUNT(*) FROM git_map").await;

    // Alice has used her whole byte cap.
    let cap = 10_000;
    set_used(&f, 42, cap as i64).await;
    // The git push: the import writes nothing.
    let push = pusher(&f, cap, 10);
    let e = push_as_alice(&f, &push, "refs/heads/feat", &c2)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "QUOTA_EXCEEDED");
    assert_eq!(count(&f, "SELECT COUNT(*) FROM cs_graph").await, graph);
    assert_eq!(count(&f, "SELECT COUNT(*) FROM git_map").await, mapped);
    assert_eq!(used(&f, 42).await, cap as i64);

    // UpdateRefs: the quota is asked before the closure is read, so a target that does not exist
    // is still QUOTA_EXCEEDED (an ingest would have said it is missing).
    let ghost = RefTarget::Changeset(asc_core::id::ChangesetId::from_bytes([9; 32]));
    let refs = server(&f, cap, 10);
    let e = f
        .update_with(&refs, "42:alice", "refs/heads/ghost", ghost)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "QUOTA_EXCEEDED");
    assert!(f.stored("refs/users/42/heads/ghost").await.is_none());
    assert_eq!(used(&f, 42).await, cap as i64);

    // A delete is never refused for quota, so an over-quota owner can shrink.
    seed_user_refs(&f, 42, 1).await;
    let seeded = f.stored("refs/heads/master").await.unwrap();
    let del = refs
        .update_refs(
            &ctx("42:alice", IdMode::Asc),
            wire::UpdateRefsRequest {
                repo: Some(wire::RepoRef {
                    name: "o/r".into(),
                    id_mode: 0,
                }),
                updates: vec![wire::RefUpdate {
                    name: "refs/heads/seed1".into(),
                    expect: Some(wire::ref_update::Expect::Value(wire::RefTarget::from(
                        &seeded,
                    ))),
                    new: None,
                    delete: true,
                }],
                mode: wire::UpdateMode::FastForward as i32,
                ..Default::default()
            },
        )
        .await;
    assert!(del.is_ok(), "{del:?}");
    assert!(f.stored("refs/users/42/heads/seed1").await.is_none());
}

#[tokio::test]
async fn the_charge_is_the_changesets_actually_added_not_the_target_count() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");
    let c3 = f.forge.commit(Some(&c2), "three");
    let c4 = f.forge.commit(Some(&c3), "four");

    let push = pusher(&f, 1 << 30, 10);
    // One ref, three new changesets: each costs at least the fixed charge.
    push_as_alice(&f, &push, "refs/heads/stack", &c4)
        .await
        .unwrap();
    let charged = used(&f, 42).await;
    assert!(
        charged >= 3 * CHANGESET_BASE_COST as i64
            && charged < 4 * CHANGESET_BASE_COST as i64 + 1024,
        "charged {charged}"
    );

    // The same history under a second name adds nothing, so nothing is charged.
    push_as_alice(&f, &push, "refs/heads/again", &c4)
        .await
        .unwrap();
    assert_eq!(used(&f, 42).await, charged);

    // UpdateRefs to a changeset the repository already holds adds nothing either.
    let refs = server(&f, 1 << 30, 10);
    let target = f.stored("refs/heads/master").await.unwrap();
    f.update_with(&refs, "42:alice", "refs/heads/third", target)
        .await
        .unwrap();
    assert_eq!(used(&f, 42).await, charged);
}

fn update_req(name: &str, to: &RefTarget) -> wire::UpdateRefsRequest {
    wire::UpdateRefsRequest {
        repo: Some(wire::RepoRef {
            name: "o/r".into(),
            id_mode: 0,
        }),
        updates: vec![wire::RefUpdate {
            name: name.into(),
            expect: Some(wire::ref_update::Expect::Absent(true)),
            new: Some(wire::RefTarget::from(to)),
            delete: false,
        }],
        mode: wire::UpdateMode::FastForward as i32,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_retried_push_cannot_bypass_or_double_the_quota() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");

    // A refused push, retried with the same op id, is refused again and charges nothing.
    let tight = pusher(&f, 100, 10);
    let one = ctx("42:alice", IdMode::Git);
    for _ in 0..2 {
        let e = tight
            .import_git_push(
                &one,
                &f.forge.whole(),
                &[cmd("refs/heads/feat", None, Some(&c2))],
                &ImportOptions::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(code(&e), "QUOTA_EXCEEDED");
    }
    assert_eq!(used(&f, 42).await, 0);

    // A completed push, replayed with the same op id, returns the stored result: no second charge.
    let refs = server(&f, 1 << 30, 10);
    let push = pusher(&f, 1 << 30, 10);
    push_as_alice(&f, &push, "refs/heads/stack", &c2)
        .await
        .unwrap();
    let charged = used(&f, 42).await;
    assert!(charged > 0);
    let target = f.stored("refs/users/42/heads/stack").await.unwrap();
    let again = ctx("42:alice", IdMode::Asc);
    for _ in 0..3 {
        refs.update_refs(&again, update_req("refs/heads/copy", &target))
            .await
            .unwrap();
    }
    assert_eq!(used(&f, 42).await, charged);
}

#[test]
fn the_cloud_config_caps_become_the_pipeline_quota() {
    let cfg: asc_serve::config::CloudConfig =
        toml::from_str("max_user_refs = 7\nmax_user_bytes = \"2 MiB\"").unwrap();
    let q = cfg.user_quota();
    assert_eq!(q.max_user_refs(), 7);
    assert_eq!(q.quota().limits().max_user_bytes, 2 << 20);
    let pipeline = PipelineHooks::default();
    assert!(pipeline.set_user_quota(q).is_ok());
    assert_eq!(pipeline.user_quota().max_user_refs(), 7);
}
