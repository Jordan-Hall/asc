//! B08a: server ref policy, user-ref classification, advertisement and the `ImportGitPush`
//! mapping (SPEC §21.5.1), over a real `Registry`, `ServerRefs` and `ImportPush`, with real git
//! building the pushed objects.
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
use asc_serve::import_push::{ImportOptions, ImportPush, PushCommand, Quarantine};
use asc_serve::registry::{NewRepo, Registry, RepoEntry};
use asc_serve::update::{HookRejected, HookRequest, PipelineHooks, PushHooks, ServerRefs};
use asc_serve::{migrate, refpolicy};
use asc_store::refs::{Refs, RepoKey};
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
        self.refs
            .update_refs(
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

#[tokio::test]
async fn a_plain_git_push_to_a_non_shared_branch_lands_as_the_pushers_user_ref() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    assert_eq!(f.hooks.pre_receive.load(Ordering::SeqCst), 1);
    let master = f.stored("refs/heads/master").await.unwrap();

    // Create, then fast-forward, then a stale old value.
    let c2 = f.forge.commit(Some(&c1), "two");
    let r = f
        .git_push("42:alice", &[cmd("refs/heads/feat", None, Some(&c2))])
        .await
        .unwrap();
    // The reply names the ref as git knows it.
    assert_eq!(r.results[0].name, "refs/heads/feat");
    assert!(f.stored("refs/users/42/heads/feat").await.is_some());
    assert!(f.stored("refs/heads/feat").await.is_none());
    let c3 = f.forge.commit(Some(&c2), "three");
    f.git_push("42:alice", &[cmd("refs/heads/feat", Some(&c2), Some(&c3))])
        .await
        .unwrap();
    let stale = f
        .git_push("42:alice", &[cmd("refs/heads/feat", Some(&c2), Some(&c3))])
        .await
        .unwrap_err();
    assert_eq!(code(&stale), "REF_CONFLICT");

    // No pre-receive, no post_receive, no bridge for user refs.
    assert_eq!(f.hooks.pre_receive.load(Ordering::SeqCst), 1);
    let effects = f.outbox_effects().await;
    assert_eq!(effects.len(), 3);
    assert!(effects[0].contains("post_receive") && effects[0].contains("refs/heads/master"));
    for e in &effects[1..] {
        assert!(!e.contains("post_receive") && !e.contains("refs/"), "{e}");
    }
    // Another user pushing the same name gets a separate ref.
    f.git_push("43:bob", &[cmd("refs/heads/feat", None, Some(&c2))])
        .await
        .unwrap();
    assert!(f.stored("refs/users/43/heads/feat").await.is_some());
    assert_eq!(f.stored("refs/heads/master").await, Some(master));

    // A delete removes the user ref.
    f.git_push("42:alice", &[cmd("refs/heads/feat", Some(&c3), None)])
        .await
        .unwrap();
    assert!(f.stored("refs/users/42/heads/feat").await.is_none());
}

#[tokio::test]
async fn a_principal_without_a_user_id_has_no_user_refs() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");
    let e = f
        .git_push("alice", &[cmd("refs/heads/feat", None, Some(&c2))])
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
}

#[tokio::test]
async fn a_branch_mode_repo_keeps_branches_shared() {
    let f = fx(false).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");
    f.git_push("alice", &[cmd("refs/heads/feat", None, Some(&c2))])
        .await
        .unwrap();
    assert!(f.stored("refs/heads/feat").await.is_some());
    assert_eq!(f.hooks.pre_receive.load(Ordering::SeqCst), 2);
    // Even there a client cannot name the server's user namespace.
    let target = f.stored("refs/heads/master").await.unwrap();
    let e = f
        .update("alice", "refs/users/1/heads/x", target)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn a_tag_outside_shared_refs_is_ref_not_shared_and_a_listed_tag_is_shared() {
    let f = fx(true).await;
    with_master(&f).await;
    let target = f.stored("refs/heads/master").await.unwrap();
    // The default set includes every tag.
    f.update("42:alice", "refs/tags/v1", target).await.unwrap();
    assert!(f.stored("refs/tags/v1").await.is_some());

    sqlx::query("UPDATE repos SET shared_refs = $1 WHERE id = $2")
        .bind(r#"["master","refs/tags/v*"]"#)
        .bind(f.entry.key.0)
        .execute(f.registry.pool())
        .await
        .unwrap();
    f.update("42:alice", "refs/tags/v2", target).await.unwrap();
    let e = f
        .update("42:alice", "refs/tags/nightly", target)
        .await
        .unwrap_err();
    assert_eq!(code(&e), "REF_NOT_SHARED");
    assert!(f.stored("refs/tags/nightly").await.is_none());
    // The same through a plain git push.
    let c2 = f.forge.commit(None, "tagged");
    let e = f
        .git_push("42:alice", &[cmd("refs/tags/other", None, Some(&c2))])
        .await
        .unwrap_err();
    assert_eq!(code(&e), "REF_NOT_SHARED");
    // `refs/heads/release/*` is no longer shared once `shared_refs` is custom: it is a user ref.
    f.update("42:alice", "refs/heads/release/1", target)
        .await
        .unwrap();
    assert!(f.stored("refs/users/42/heads/release/1").await.is_some());
}

#[tokio::test]
async fn list_refs_is_shared_plus_own_and_never_another_users_refs() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");
    f.git_push("42:alice", &[cmd("refs/heads/feat", None, Some(&c2))])
        .await
        .unwrap();
    f.git_push("43:bob", &[cmd("refs/heads/secret", None, Some(&c2))])
        .await
        .unwrap();
    let target = f.stored("refs/heads/master").await.unwrap();
    f.update("42:alice", "refs/tags/v1", target).await.unwrap();

    assert_eq!(
        f.list("42:alice", "").await.unwrap(),
        ["refs/heads/feat", "refs/heads/master", "refs/tags/v1"]
    );
    assert_eq!(
        f.list("43:bob", "").await.unwrap(),
        ["refs/heads/master", "refs/heads/secret", "refs/tags/v1"]
    );
    // A caller with no user id sees shared refs only.
    assert_eq!(
        f.list("anon", "").await.unwrap(),
        ["refs/heads/master", "refs/tags/v1"]
    );
    // Prefixes narrow both halves.
    assert_eq!(
        f.list("42:alice", "refs/heads/f").await.unwrap(),
        ["refs/heads/feat"]
    );
    assert_eq!(f.list("42:alice", "refs/heads/z").await.unwrap().len(), 0);
    assert_eq!(
        f.list("42:alice", "refs/t").await.unwrap(),
        ["refs/tags/v1"]
    );
    // The explicit namespace lists the caller's own refs under stored names, never another's.
    assert_eq!(
        f.list("42:alice", "refs/users/42/").await.unwrap(),
        ["refs/users/42/heads/feat"]
    );
    assert_eq!(
        f.list("42:alice", "refs/users/43/")
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    assert_eq!(
        f.list("anon", "refs/users/").await.unwrap_err().code(),
        tonic::Code::PermissionDenied
    );
    // Paging walks the merged order.
    let p1 = f.list_page("42:alice", "", 2, "").await.unwrap();
    assert_eq!(
        p1.refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["refs/heads/feat", "refs/heads/master"]
    );
    assert_eq!(p1.next, "refs/heads/master");
    let p2 = f.list_page("42:alice", "", 2, &p1.next).await.unwrap();
    assert_eq!(p2.refs.len(), 1);
    assert!(p2.next.is_empty());
}

/// Counts the entries a `Refs` store hands back from `list`.
struct Counting {
    inner: Arc<dyn Refs>,
    rows: AtomicUsize,
}

#[async_trait]
impl Refs for Counting {
    async fn get(
        &self,
        repo: RepoKey,
        name: &RefName,
    ) -> asc_store::refs::Result<Option<asc_store::refs::RefEntry>> {
        self.inner.get(repo, name).await
    }

    async fn list(
        &self,
        repo: RepoKey,
        prefix: &str,
    ) -> asc_store::refs::Result<Vec<asc_store::refs::RefEntry>> {
        let got = self.inner.list(repo, prefix).await?;
        self.rows.fetch_add(got.len(), Ordering::SeqCst);
        Ok(got)
    }

    async fn update(
        &self,
        repo: RepoKey,
        txn: &asc_store::refs::RefTxn,
    ) -> asc_store::refs::Result<()> {
        self.inner.update(repo, txn).await
    }

    async fn log(
        &self,
        repo: RepoKey,
        name: &RefName,
        limit: u32,
    ) -> asc_store::refs::Result<Vec<asc_store::refs::RefLogEntry>> {
        self.inner.log(repo, name, limit).await
    }
}

#[tokio::test]
async fn list_refs_for_one_user_reads_only_shared_plus_own_among_100k_user_refs() {
    let f = fx(true).await;
    let c1 = with_master(&f).await;
    let c2 = f.forge.commit(Some(&c1), "two");
    f.git_push("42:alice", &[cmd("refs/heads/mine", None, Some(&c2))])
        .await
        .unwrap();
    let target = f.stored("refs/heads/master").await.unwrap();
    // 100,000 refs of other users, written straight into the table.
    sqlx::query(
        "INSERT INTO refs (repo_id, name, target, version) \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000) \
         SELECT $1, 'refs/users/' || (1000 + (i % 500)) || '/heads/b' || i, $2, 1 FROM n",
    )
    .bind(f.entry.key.0)
    .bind(target.to_bytes().to_vec())
    .execute(f.registry.pool())
    .await
    .unwrap();

    let counting = Counting {
        inner: f.registry.refs().clone(),
        rows: AtomicUsize::new(0),
    };
    let policy = refpolicy::RepoPolicy::load(f.registry.pool(), f.entry.key.0)
        .await
        .unwrap();
    let got = refpolicy::advertise(
        &counting,
        f.entry.key,
        &policy,
        Some(42),
        &wire::ListRefsRequest::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        got.refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["refs/heads/master", "refs/heads/mine"]
    );
    // One shared branch and one own ref: two rows read out of 100,002.
    assert_eq!(counting.rows.load(Ordering::SeqCst), 2);
    // The same through the handler.
    assert_eq!(
        f.list("42:alice", "").await.unwrap(),
        ["refs/heads/master", "refs/heads/mine"]
    );
}
