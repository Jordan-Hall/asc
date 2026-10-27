//! C52: the `no_content_merge` land flag (SPEC §20.5). A land that would need a content merge
//! fails with `LAND_NEEDS_CONTENT_MERGE` and changes no ref; a path-disjoint land succeeds.
//! Real git builds the pushed objects; the policy is the production `ContentMerge`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_git::realgit::RealGit;
use asc_rpc::IdMode;
use asc_rpc::auth::Principal;
use asc_rpc::server::{CallCtx, RefsHandler, RepoName, status_rpc_error, wire};
use asc_serve::import_push::{ImportOptions, ImportPush, PushCommand, Quarantine};
use asc_serve::migrate;
use asc_serve::mutations::land_merge::ContentMerge;
use asc_serve::read::ReadHooks;
use asc_serve::registry::{NewRepo, Registry, RepoEntry};
use asc_serve::update::{PipelineHooks, ServerRefs};
use asc_store::refs::Refs;
use tonic::Status;
use uuid::Uuid;

const LINES: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
const ONE: &str = "ONE\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
const TEN: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\nTEN\n";

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

    /// A commit whose tree holds `files`.
    fn commit(&self, parent: Option<&str>, files: &[(&str, &str)], msg: &str) -> String {
        let mut tree = String::new();
        for (name, content) in files {
            let blob = git(
                &self.git_dir,
                &["hash-object", "-w", "--stdin"],
                content.as_bytes(),
            );
            tree.push_str(&format!("100644 blob {blob}\t{name}\n"));
        }
        let tree = git(&self.git_dir, &["mktree"], tree.as_bytes());
        let mut args = vec!["commit-tree", tree.as_str(), "-m", msg];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        git(&self.git_dir, &args, b"")
    }
}

struct Fx {
    forge: Forge,
    registry: Arc<Registry>,
    entry: Arc<RepoEntry>,
    push: ImportPush,
    refs: ServerRefs,
}

async fn fx() -> Fx {
    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let entry = registry
        .create(&RepoName::parse("o/r").unwrap(), &NewRepo::default())
        .await
        .unwrap();
    let pipeline = Arc::new(PipelineHooks::default());
    let policy = ContentMerge::with_defaults(Arc::new(ReadHooks::default()));
    assert!(pipeline.set_overlap_policy(Arc::new(policy)).is_ok());
    Fx {
        forge: Forge::new(),
        push: ImportPush::new(registry.clone(), pipeline.clone()),
        refs: ServerRefs::new(registry.clone(), pipeline),
        registry,
        entry,
    }
}

fn ctx(mode: IdMode) -> CallCtx {
    CallCtx {
        repo: RepoName::parse("o/r").unwrap(),
        id_mode: mode,
        principal: Principal {
            id: "7:maint".into(),
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
    async fn publish(&self, name: &str, git_id: &str) {
        let cmd = PushCommand {
            name: name.into(),
            old: None,
            new: Some(git_id.parse().unwrap()),
        };
        let whole = Quarantine {
            object_dir: self.forge.git_dir.join("objects"),
            alternate_dirs: Vec::new(),
        };
        self.push
            .import_git_push(&ctx(IdMode::Git), &whole, &[cmd], &ImportOptions::default())
            .await
            .unwrap();
    }

    async fn stored(&self, name: &str) -> RefTarget {
        self.registry
            .refs()
            .get(self.entry.key, &RefName::new(name.as_bytes()).unwrap())
            .await
            .unwrap()
            .unwrap()
            .target
    }

    async fn land(
        &self,
        from: &str,
        no_content_merge: bool,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        let to = self.stored(from).await;
        self.refs
            .update_refs(
                &ctx(IdMode::Asc),
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
                    no_content_merge,
                    ..Default::default()
                },
            )
            .await
    }
}

/// `master` at base, plus `mine` and `theirs` branches forked from it.
async fn forked(f: &Fx, base: &[(&str, &str)], mine: &[(&str, &str)], theirs: &[(&str, &str)]) {
    let b = f.forge.commit(None, base, "base");
    let m = f.forge.commit(Some(&b), mine, "mine");
    let t = f.forge.commit(Some(&b), theirs, "theirs");
    f.publish("refs/heads/master", &b).await;
    f.publish("refs/heads/mine", &m).await;
    f.publish("refs/heads/theirs", &t).await;
}

#[tokio::test]
async fn a_land_that_needs_a_content_merge_fails_and_changes_no_ref() {
    let f = fx().await;
    forked(
        &f,
        &[("f.txt", LINES)],
        &[("f.txt", ONE)],
        &[("f.txt", TEN)],
    )
    .await;
    f.land("refs/heads/theirs", true).await.unwrap();
    let before = f.stored("refs/heads/master").await;
    let err = f.land("refs/heads/mine", true).await.unwrap_err();
    assert_eq!(code(&err), "LAND_NEEDS_CONTENT_MERGE");
    assert_eq!(f.stored("refs/heads/master").await, before);
}

#[tokio::test]
async fn the_same_land_merges_without_the_flag() {
    let f = fx().await;
    forked(
        &f,
        &[("f.txt", LINES)],
        &[("f.txt", ONE)],
        &[("f.txt", TEN)],
    )
    .await;
    f.land("refs/heads/theirs", false).await.unwrap();
    let before = f.stored("refs/heads/master").await;
    f.land("refs/heads/mine", false).await.unwrap();
    assert_ne!(f.stored("refs/heads/master").await, before);
}

#[tokio::test]
async fn a_path_disjoint_land_succeeds_with_the_flag() {
    let f = fx().await;
    forked(
        &f,
        &[("a.txt", LINES), ("b.txt", LINES)],
        &[("a.txt", ONE), ("b.txt", LINES)],
        &[("a.txt", LINES), ("b.txt", TEN)],
    )
    .await;
    f.land("refs/heads/theirs", true).await.unwrap();
    let before = f.stored("refs/heads/master").await;
    f.land("refs/heads/mine", true).await.unwrap();
    assert_ne!(f.stored("refs/heads/master").await, before);
}
