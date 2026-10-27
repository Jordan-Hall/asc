//! B18: `asc serve admin branchless enable` over an ASC-shaped fixture (SPEC §21.6.2),
//! over a real `Registry`, `ServerRefs` and `ImportPush`.
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
}

// ------------------------------------------------------------------------------ the fixture

use asc_core::change::ChangeId;
use asc_core::change_migrate::import_id;
use asc_core::id::ChangesetId;
use asc_serve::admin::branchless::{self, Action, ForgeFacts, NoForge, Options};
use sqlx::Row;
use std::collections::HashSet;

fn cs_of(t: RefTarget) -> ChangesetId {
    match t {
        RefTarget::Changeset(c) => c,
        RefTarget::Tag(_) => panic!("tag"),
    }
}

impl Forge {
    fn commit_msg(&self, parents: &[&str], content: &str, msg: &str) -> String {
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
        let mut args = vec!["commit-tree", tree.as_str(), "-m", msg];
        for p in parents {
            args.extend(["-p", p]);
        }
        git(&self.git_dir, &args, b"")
    }
}

struct Facts;

#[async_trait]
impl ForgeFacts for Facts {
    async fn open_pr_heads(&self) -> Result<HashSet<String>, Status> {
        Ok(["feat/pr".to_owned()].into())
    }

    async fn pusher(&self, branch: &str) -> Result<Option<i64>, Status> {
        Ok(match branch {
            "feat/a" => Some(42),
            b if branchless::is_agent_branch(b) => Some(43),
            _ => None,
        })
    }
}

const NOW: i64 = 1_800_000_000;

fn repo_name() -> RepoName {
    RepoName::parse("o/r").unwrap()
}

/// 117 branches: 3 shared, 61 merged (60 fast-forwarded, 1 by a merge commit), 1 open-PR head,
/// 2 feature branches and 50 agent branches.
async fn asc_shaped() -> Fx {
    let f = fx(false).await;
    let c1 = f.forge.commit(None, "one");
    let c2 = f.forge.commit(Some(&c1), "two");
    let c3 = f.forge.commit(Some(&c2), "three");
    let side = f.forge.commit(Some(&c1), "side");
    let c4 = f.forge.commit_msg(&[&c3, &side], "four", "merge side");
    let u1 = f.forge.commit(Some(&c2), "u1");
    let u2 = f
        .forge
        .commit_msg(&[&u1], "u2", "u2\n\nbody\n\nChange-Id: Iabc123\n");
    let pr = f.forge.commit(Some(&c2), "pr");
    let agents: Vec<String> = (0..5)
        .map(|i| f.forge.commit(Some(&c3), &format!("agent{i}")))
        .collect();
    let mut cmds = vec![
        cmd("refs/heads/master", None, Some(&c4)),
        cmd("refs/heads/release/1.0", None, Some(&c3)),
        cmd("refs/heads/green/x", None, Some(&c3)),
        cmd("refs/heads/feat/merged-via-merge", None, Some(&side)),
        cmd("refs/heads/feat/a", None, Some(&u2)),
        cmd("refs/heads/feat/b", None, Some(&u1)),
        cmd("refs/heads/feat/pr", None, Some(&pr)),
    ];
    for i in 0..60 {
        let at = [&c1, &c2, &c3][i % 3];
        cmds.push(cmd(&format!("refs/heads/old/m{i}"), None, Some(at)));
    }
    for i in 0..45 {
        cmds.push(cmd(
            &format!("refs/heads/worktree-agent-{i}"),
            None,
            Some(&agents[i % 5]),
        ));
    }
    for (i, a) in agents.iter().enumerate() {
        cmds.push(cmd(&format!("refs/heads/agent/t{i}"), None, Some(a)));
    }
    assert_eq!(cmds.len(), 117);
    f.git_push("7:maint", &cmds).await.unwrap();
    f
}

impl Fx {
    async fn names(&self, prefix: &str) -> Vec<String> {
        self.registry
            .refs()
            .list(self.entry.key, prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|e| String::from_utf8(e.name.as_bytes().to_vec()).unwrap())
            .collect()
    }

    async fn branchless(&self) -> (bool, Option<String>) {
        let r = sqlx::query(
            "SELECT CAST(CASE WHEN branchless THEN 1 ELSE 0 END AS BIGINT) AS b, legacy_shared \
             FROM repos WHERE id = $1",
        )
        .bind(self.entry.key.0)
        .fetch_one(self.registry.pool())
        .await
        .unwrap();
        (r.get::<i64, _>("b") != 0, r.get("legacy_shared"))
    }

    async fn change_of(&self, cs: ChangesetId) -> Option<ChangeId> {
        sqlx::query("SELECT change FROM changes WHERE cs = $1")
            .bind(cs.as_bytes().to_vec())
            .fetch_optional(self.registry.pool())
            .await
            .unwrap()
            .map(|r| ChangeId::from_bytes(r.get::<Vec<u8>, _>("change").try_into().unwrap()))
    }

    async fn run(&self, dry: bool) -> branchless::Report {
        let mut o = Options::new(7, NOW);
        o.dry_run = dry;
        branchless::enable(&self.registry, &repo_name(), &Facts, &o)
            .await
            .unwrap()
    }
}

// ------------------------------------------------------------------------------ tests

#[tokio::test]
async fn a_dry_run_reports_the_classification_and_changes_nothing() {
    let f = asc_shaped().await;
    let before = f.names("refs/").await;
    let r = f.run(true).await;
    assert!(r.dry_run);
    assert_eq!(r.branches.len(), 117);
    assert_eq!(r.shared, 3);
    assert_eq!(r.legacy_shared, vec!["feat/pr".to_owned()]);
    assert_eq!(r.deleted, 61);
    assert_eq!(r.converted, 52);
    assert_eq!(r.batches, 0);
    // u1, u2 and the five agent commits.
    assert_eq!(r.change_metas, 7);
    let action = |b: &str| {
        r.branches
            .iter()
            .find(|x| x.branch == b)
            .unwrap()
            .action
            .clone()
    };
    assert_eq!(action("master"), Action::Shared);
    assert_eq!(action("feat/pr"), Action::LegacyShared);
    assert_eq!(action("feat/merged-via-merge"), Action::DeleteMerged);
    assert_eq!(action("old/m5"), Action::DeleteMerged);
    assert_eq!(
        action("feat/a"),
        Action::UserRef {
            owner: 42,
            ttl: false
        }
    );
    assert_eq!(
        action("feat/b"),
        Action::UserRef {
            owner: 7,
            ttl: false
        }
    );
    assert_eq!(
        action("agent/t3"),
        Action::UserRef {
            owner: 43,
            ttl: true
        }
    );
    assert!(r.to_json().contains("\"action\": \"delete_merged\""));
    assert_eq!(f.names("refs/").await, before);
    assert_eq!(f.branchless().await, (false, None));
}

#[tokio::test]
async fn applying_converts_deletes_and_is_a_no_op_the_second_time() {
    let f = asc_shaped().await;
    let outbox_before = f.outbox_effects().await.len();
    let r = f.run(false).await;
    assert_eq!((r.deleted, r.converted, r.change_metas), (61, 52, 7));
    assert_eq!(r.batches, 2, "113 branch moves in batches of 100");

    let mut heads = f.names("refs/heads/").await;
    heads.sort();
    assert_eq!(
        heads,
        [
            "refs/heads/feat/pr",
            "refs/heads/green/x",
            "refs/heads/master",
            "refs/heads/release/1.0"
        ]
    );
    assert_eq!(
        f.names("refs/users/42/").await,
        ["refs/users/42/heads/feat/a"]
    );
    assert_eq!(
        f.names("refs/users/7/").await,
        ["refs/users/7/heads/feat/b"]
    );
    assert_eq!(f.names("refs/users/43/").await.len(), 50);
    assert_eq!(
        f.branchless().await,
        (true, Some("[\"feat/pr\"]".to_owned()))
    );

    // Agent refs carry the 7-day TTL, the others none.
    let name = |s: &str| RefName::new(s.as_bytes()).unwrap();
    let agent = f
        .registry
        .refs()
        .get(f.entry.key, &name("refs/users/43/heads/agent/t0"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(agent.expires_at, Some(NOW + 7 * 86_400));
    let feat = f
        .registry
        .refs()
        .get(f.entry.key, &name("refs/users/42/heads/feat/a"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(feat.expires_at, None);

    // The log says how; the deletions reach Forgejo through the outbox.
    let via: Vec<String> = sqlx::query("SELECT DISTINCT via FROM ref_log WHERE reason = $1")
        .bind(branchless::VIA)
        .fetch_all(f.registry.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| r.get("via"))
        .collect();
    assert_eq!(via, [branchless::VIA]);
    let effects = f.outbox_effects().await;
    assert_eq!(effects.len(), outbox_before + 2);
    let mut deleted = 0;
    for e in &effects[outbox_before..] {
        let d: serde_json::Value = serde_json::from_str(e).unwrap();
        assert_eq!(d["effects"], serde_json::json!(["post_receive"]));
        deleted += d["refs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["new"].is_null())
            .count();
    }
    assert_eq!(
        deleted, 113,
        "every deleted or moved branch is reported once"
    );

    // Change ids by the B18a rule.
    let u2 = cs_of(f.stored("refs/users/42/heads/feat/a").await.unwrap());
    let u1 = cs_of(f.stored("refs/users/7/heads/feat/b").await.unwrap());
    assert_eq!(
        f.change_of(u2).await,
        Some(ChangeId::from_gerrit(b"refs/heads/feat/a", b"Iabc123"))
    );
    assert_eq!(f.change_of(u1).await, Some(import_id(&u1)));

    // The second apply finds nothing to do.
    let after = f.names("refs/").await;
    let log: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ref_log")
        .fetch_one(f.registry.pool())
        .await
        .unwrap();
    let again = f.run(false).await;
    assert!(again.is_noop(), "{again:?}");
    assert_eq!(again.legacy_shared, vec!["feat/pr".to_owned()]);
    assert_eq!(f.names("refs/").await, after);
    let log2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ref_log")
        .fetch_one(f.registry.pool())
        .await
        .unwrap();
    assert_eq!(log, log2);
    assert_eq!(f.outbox_effects().await.len(), outbox_before + 2);
}

#[tokio::test]
async fn an_existing_user_ref_is_never_overwritten_and_the_branch_stays_shared() {
    let f = asc_shaped().await;
    // Owner 42 already holds `feat/a` at another value.
    let other = cs_of(f.stored("refs/heads/old/m0").await.unwrap());
    sqlx::query("INSERT INTO refs (repo_id, name, target, version) VALUES ($1, $2, $3, 1)")
        .bind(f.entry.key.0)
        .bind("refs/users/42/heads/feat/a")
        .bind(RefTarget::Changeset(other).to_bytes().to_vec())
        .execute(f.registry.pool())
        .await
        .unwrap();
    let r = f.run(false).await;
    assert!(matches!(
        r.branches
            .iter()
            .find(|b| b.branch == "feat/a")
            .unwrap()
            .action,
        Action::Kept { .. }
    ));
    assert!(r.legacy_shared.contains(&"feat/a".to_owned()));
    assert!(f.stored("refs/heads/feat/a").await.is_some());
    assert_eq!(
        f.stored("refs/users/42/heads/feat/a").await,
        Some(RefTarget::Changeset(other))
    );
}

#[tokio::test]
async fn bad_options_and_unknown_repositories_are_refused_before_anything_moves() {
    let f = asc_shaped().await;
    let run = |o: Options, name: RepoName| {
        let reg = f.registry.clone();
        async move { branchless::enable(&reg, &name, &NoForge, &o).await }
    };
    assert!(run(Options::new(0, NOW), repo_name()).await.is_err());
    let mut o = Options::new(7, NOW);
    o.batch = 0;
    assert!(run(o.clone(), repo_name()).await.is_err());
    o.batch = branchless::MAX_BATCH + 1;
    assert!(run(o, repo_name()).await.is_err());
    let e = run(Options::new(7, NOW), RepoName::parse("o/none").unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::NotFound);
    assert_eq!(f.branchless().await, (false, None));
}
