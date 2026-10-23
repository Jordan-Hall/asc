//! Fixture builders for CLI-unit tests (PLAN "Acceptance before U25"): a hermetic sandbox for
//! running the `asc` binary, and a [`Fixture`] that builds repositories, commits, branches and
//! staging directly through the engine and `state.db`, so no sibling command is needed.
#![allow(dead_code)] // Each test crate uses a different part.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use asc_core::content::{Body, ingest};
use asc_core::id::ChangesetId;
use asc_core::object::{Change, Kind};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::Engine;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_git::projection::{Head, Projection};
use asc_state::state::repos::{self, NewRepo, RemoteKind};
use asc_state::state::stage::{self, Leaf, StageRow, StageSet};
use asc_state::state::workspaces::{self, Mode, NewWorkspace};
use asc_state::{AscHome, RepoId, StateDb, WorkspaceId};
use asc_store::cachedir::CacheDir;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs, RepoKey};
use asc_store::sqlrefs::SqlRefs;
use asc_store::storeset::StoreSet;

/// The built `asc` binary; only set for integration tests.
pub fn asc_binary() -> Option<&'static str> {
    option_env!("CARGO_BIN_EXE_asc")
}

/// A temporary `ASC_HOME` and user home, with git's configuration isolated.
pub struct Sandbox {
    pub tmp: tempfile::TempDir,
    pub asc_home: PathBuf,
    pub user: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let asc_home = tmp.path().join("asc-home");
        let user = tmp.path().join("user");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(".gitconfig"), b"").unwrap();
        Self { tmp, asc_home, user }
    }

    /// `binary` run in `cwd` with no inherited `GIT_*`/`ASC_*` variables and an empty `HOME`.
    pub fn command(&self, binary: impl AsRef<Path>, cwd: &Path) -> Command {
        let mut cmd = Command::new(binary.as_ref());
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("GIT_") || name.starts_with("ASC_") {
                cmd.env_remove(&key);
            }
        }
        cmd.current_dir(cwd)
            .env("ASC_HOME", &self.asc_home)
            .env("HOME", &self.user)
            .env("USERPROFILE", &self.user)
            .env("XDG_CONFIG_HOME", self.user.join(".config"))
            .env("GIT_CONFIG_GLOBAL", self.user.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("NO_COLOR", "1")
            // The fixtures serve plain http on the loopback interface; asc refuses that unless
            // asked (tests that check the refusal remove this variable).
            .env("ASC_ALLOW_LOOPBACK_HTTP", "1");
        cmd
    }

    /// The `asc` binary in `cwd`.
    pub fn asc(&self, cwd: &Path) -> Command {
        self.command(asc_binary().expect("integration tests only"), cwd)
    }

    /// Runs `asc args…` in `cwd` and returns its output.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.asc(cwd).args(args).output().expect("spawn asc")
    }

    /// A directory under the sandbox.
    pub fn dir(&self, name: &str) -> PathBuf {
        let p = self.tmp.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

/// `String` of an output stream, with CRLF folded.
pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

fn sig() -> Signature {
    Signature::new(
        b"Test".to_vec(),
        b"test@example.com".to_vec(),
        1_700_000_000,
        0,
    )
    .unwrap()
}

/// A local-only repository with one plain workspace, as `asc init` would make it.
pub struct Fixture {
    pub sandbox: Sandbox,
    pub home: AscHome,
    pub db: StateDb,
    pub root: PathBuf,
    pub repo: RepoKey,
    pub ws: WorkspaceId,
    pub objects: Arc<StoreSet>,
    pub engine: Engine<StoreSet, SqlRefs>,
}

impl Fixture {
    pub async fn new() -> Self {
        let sandbox = Sandbox::new();
        let home = AscHome::open(sandbox.asc_home.clone()).unwrap();
        let db = StateDb::open(&home).await.unwrap();
        let repo_id = RepoId::new();
        let repo = repos::insert(
            &db,
            &NewRepo {
                id: repo_id,
                name: "fixture".into(),
                ns: vec![7; 16],
                remote_url: None,
                remote_kind: RemoteKind::LocalOnly,
                default_branch: "master".into(),
                windows_safe: true,
            },
        )
        .await
        .unwrap();
        let ws = WorkspaceId::new();
        let root = sandbox.dir("work");
        Projection::create(&home, repo_id, ws, &root).unwrap();
        let path = root.display().to_string();
        workspaces::insert(
            &db,
            &NewWorkspace {
                id: ws,
                repo,
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
        let objects = Arc::new(StoreSet {
            store: CacheDir::open(home.store(), home.tmp()).unwrap(),
            cache: CacheDir::open(home.cache(), home.tmp()).unwrap(),
        });
        let engine = Engine::new(objects.clone(), Arc::new(db.refs()), repo);
        Self {
            sandbox,
            home,
            db,
            root,
            repo,
            ws,
            objects,
            engine,
        }
    }

    async fn put_file(&self, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).unwrap();
        let Body::Inline(bytes) = got.body else {
            panic!("fixture files must be small");
        };
        self.objects
            .put(&ObjKey::File(got.file), encode_stored(&bytes))
            .await
            .unwrap();
        Change::write(Kind::File, got.file, got.size, got.git, None).unwrap()
    }

    /// A changeset on `parents` whose tree is the first parent's with `files` written.
    pub async fn commit(&self, parents: &[ChangesetId], files: &[(&str, &[u8])]) -> ChangesetId {
        let base = match parents.first() {
            Some(&p) => self.engine.get_changeset(p).await.unwrap().root,
            None => Engine::<StoreSet, SqlRefs>::empty_tree_id(),
        };
        let mut changes = Vec::new();
        for (path, content) in files {
            changes.push((RepoPath::new(*path).unwrap(), self.put_file(content).await));
        }
        let root = self.engine.apply(base, &changes).await.unwrap();
        self.engine
            .create_changeset(
                NewChangeset {
                    parents: parents.to_vec(),
                    author: sig(),
                    committer: sig(),
                    message: format!("commit of {} files\n", files.len()).into_bytes(),
                    extra: Vec::new(),
                    root,
                    copies: Vec::new(),
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    /// Points `refs/heads/<name>` at `cs`, bypassing the CLI's chokepoint (it is a fixture).
    pub async fn set_branch(&self, name: &str, cs: ChangesetId) {
        let refs = self.db.refs();
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(format!("refs/heads/{name}").into_bytes()).unwrap(),
                expect: Expect::Any,
                new: Some(asc_core::id::RefTarget::Changeset(cs)),
                expires_at: None,
            }],
            actor: Actor {
                id: "fixture".into(),
                via: "test".into(),
            },
            reason: "fixture".into(),
            op_id: uuid::Uuid::now_v7(),
        };
        Refs::update(&refs, self.repo, &txn).await.unwrap();
    }

    /// Checks out `branch` in the workspace: `HEAD`, the row and `base_cs` (no files).
    pub async fn checkout_branch(&self, branch: &str, base: Option<ChangesetId>) {
        let proj = Projection::open(&self.root).unwrap();
        proj.write_head(&Head::Branch(branch.to_owned())).unwrap();
        workspaces::update_head_base(&self.db, self.ws, &format!("refs/heads/{branch}"), base)
            .await
            .unwrap();
    }

    /// Adds rows to stage set 0.
    pub async fn stage(&self, rows: &[StageRow]) {
        stage::upsert(&self.db, self.ws, StageSet::Default, rows)
            .await
            .unwrap();
    }

    /// The leaf of `path` at `cs`.
    pub async fn file_leaf(&self, cs: ChangesetId, path: &str) -> Leaf {
        let root = self.engine.get_changeset(cs).await.unwrap().root;
        let entry = self
            .engine
            .lookup(root, &RepoPath::new(path).unwrap())
            .await
            .unwrap()
            .expect("path exists");
        Leaf {
            kind: entry.kind,
            file: asc_core::id::FileId::from_bytes(entry.id),
            size: entry.size,
            git: entry.git,
        }
    }

    /// The `asc` binary run in the workspace root.
    pub fn asc(&self) -> Command {
        self.sandbox.asc(&self.root)
    }
}
