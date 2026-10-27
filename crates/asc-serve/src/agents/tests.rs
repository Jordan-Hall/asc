//! Tests of agent runs, tokens and the ref gate against a scripted model on a loopback port and a
//! real in-process server (the same services and pipeline a remote client reaches).

use std::sync::Arc;
use std::time::Duration;

use asc_core::id::RefTarget;
use asc_core::path::RefName;
use asc_rpc::auth::{Access, Authorizer, BearerToken, Credential, StaticTokens};
use asc_rpc::server::{RepoName, ServerInfo, wire};
use asc_rpc::services::agents_server::Agents;
use asc_store::refs::Refs as _;
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::AnyPool;
use tonic::Request;

use super::inproc::InProc;
use super::rpc::AgentService;
use super::runs::{EventKind, RunRow, RunState};
use super::{AgentsConfig, AgentsHub};
use crate::ai::AiHub;
use crate::ai::fake::{FakeProvider, Reply};
use crate::ai::owner::{AuthorizerOwners, Owner};
use crate::ai::prices::Prices;
use crate::ai::store::{KeyState, Provider};
use crate::ai::tests::{PLANTED, PRICES, capture_logs, pool_in};
use crate::ai::{AiKeysConfig, NewKey};
use crate::netguard::{Guard, NetGuardConfig};
use crate::registry::{Extensions, NewRepo, Registry, RepoEntry};
use crate::vault::Vault;
use crate::vault::keyring::KeyRing;
use crate::workflows::rpc::WorkflowService;
use crate::workflows::{WorkflowsConfig, WorkflowsHub};

pub(crate) const POLICY: &str = r#"version = 1

[roles]
maintainer = ["user:alice"]

[agents]
default_scope = ["src/**", "tests/**"]
denied_paths = ["src/secret/**"]

[agents.budgets]
max_changes_per_run = 5
"#;

pub(crate) const MAIN_RS: &str = "fn main() {\n    println!(\"hello\");\n}\n";

pub(crate) struct World {
    pub pool: AnyPool,
    pub registry: Arc<Registry>,
    pub entry: Arc<RepoEntry>,
    pub ai: Arc<AiHub>,
    pub hub: Arc<AgentsHub>,
    pub fake: FakeProvider,
    pub authorizer: Arc<dyn Authorizer>,
    pub inproc: InProc,
    pub svc: AgentService,
    pub base: Vec<u8>,
    pub dir: tempfile::TempDir,
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
    pub vault: Arc<Vault>,
    pub reviews: Option<Arc<crate::review::Reviews>>,
    pub wf: Option<Arc<WorkflowsHub>>,
    pub wsvc: Option<WorkflowService>,
}

impl Drop for World {
    fn drop(&mut self) {
        self.hub.shutdown();
        if let Some(w) = &self.wf {
            w.shutdown();
        }
        for t in &self.tasks {
            t.abort();
        }
    }
}

pub(crate) struct Opts {
    pub branchless: bool,
    pub policy: Option<&'static str>,
    pub cfg: Box<dyn Fn(&mut AgentsConfig)>,
    pub files: Vec<(&'static str, &'static str)>,
    /// Also build the workflow service with this configuration.
    pub workflows: Option<WorkflowsConfig>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            branchless: false,
            policy: Some(POLICY),
            cfg: Box::new(|_| {}),
            workflows: None,
            files: vec![
                ("src/main.rs", MAIN_RS),
                (
                    "src/lib.rs",
                    "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
                ),
                ("src/secret/key.rs", "// keep out\n"),
                ("docs/README.md", "# App\n"),
            ],
        }
    }
}

pub(crate) fn static_auth() -> Arc<dyn Authorizer> {
    Arc::new(
        StaticTokens::new()
            .with_token("alice-token", "7:alice", Access::Admin)
            .with_token("bob-token", "8:bob", Access::Write)
            .with_token("carol-token", "9:carol", Access::Read)
            .with_token("setup-token", "1:setup", Access::Admin)
            .with_token("host-token", "host-service", Access::Admin),
    )
}

fn file_op(path: &str, content: &str) -> wire::FileOp {
    wire::FileOp {
        path: Bytes::copy_from_slice(path.as_bytes()),
        op: Some(wire::file_op::Op::PutContent(Bytes::copy_from_slice(
            content.as_bytes(),
        ))),
        mode: 0,
    }
}

pub(crate) fn sig() -> wire::Signature {
    wire::Signature {
        name: Bytes::from_static(b"Setup"),
        email: Bytes::from_static(b"s@x"),
        time: 1_700_000_000,
        tz_min: 0,
    }
}

pub(crate) fn repo_ref() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: "acme/app".into(),
        id_mode: asc_rpc::IdMode::Asc as i32,
    })
}

impl World {
    pub(crate) async fn new() -> Self {
        Self::with(Opts::default()).await
    }

    pub(crate) async fn with(o: Opts) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool_in(&dir).await;
        let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
        let name = RepoName::parse("acme/app").unwrap();
        let entry = registry.create(&name, &NewRepo::default()).await.unwrap();
        if o.branchless {
            sqlx::query("UPDATE repos SET branchless = TRUE")
                .execute(&pool)
                .await
                .unwrap();
            registry.forget(&name);
        }
        let entry = if o.branchless {
            registry.open(&name).await.unwrap()
        } else {
            entry
        };

        let ext = Extensions::new();
        crate::update::register(&ext, &registry);
        let hooks = crate::read::register(&ext, &registry);
        let _mutations = crate::mutations::register(&ext, &registry, &hooks);
        crate::changes::register(&ext, &registry, &hooks);
        crate::review::register(&ext, &registry);

        let authorizer = static_auth();
        let vault = Vault::in_memory(KeyRing::generate(1_700_000_000).unwrap(), pool.clone());
        let vault_kept = vault.clone();
        let guard = Guard::new(NetGuardConfig {
            allow_loopback_http: true,
            ..NetGuardConfig::default()
        });
        let fake = FakeProvider::start(PLANTED).await;
        let ai = AiHub::new(
            pool.clone(),
            vault,
            guard,
            AiKeysConfig::default(),
            Prices::parse(PRICES).unwrap(),
            Arc::new(AuthorizerOwners::new(authorizer.clone())),
        );
        let mut cfg = AgentsConfig {
            enabled: true,
            work_dir: Some(dir.path().join("work")),
            ..AgentsConfig::default()
        };
        (o.cfg)(&mut cfg);
        let hub = AgentsHub::new(pool.clone(), registry.clone(), ai.clone(), cfg, None);
        let public: Arc<dyn Authorizer> = hub.authorizer(authorizer.clone());
        assert!(ext.pipeline.set_agent_gate(hub.gate()).is_ok());
        assert!(ext.ai.set(ai.clone()));
        assert!(ext.agents.set(hub.clone()));
        let reviews = ext.review.get().cloned();
        let wf = o.workflows.clone().map(|c| {
            let h = WorkflowsHub::new(
                registry.clone(),
                hub.clone(),
                vault_kept.clone(),
                reviews.clone(),
                c,
                None,
            );
            assert!(ext.workflows.set(h.clone()));
            h
        });
        let routes = crate::build_grpc(&ext, public.clone(), ServerInfo::default());
        let inproc = InProc::new(routes.clone());

        // The first commit of trunk.
        let mut ops: Vec<wire::FileOp> = o.files.iter().map(|(p, c)| file_op(p, c)).collect();
        if let Some(p) = o.policy {
            ops.push(file_op(".asc/policy.toml", p));
        }
        let seeded = inproc
            .commit(
                "setup-token",
                wire::CommitRequest {
                    repo: repo_ref(),
                    parent: String::new(),
                    ops,
                    message: Bytes::from_static(b"initial"),
                    author: Some(sig()),
                    committer: Some(sig()),
                    update_ref: "refs/heads/master".into(),
                    ..wire::CommitRequest::default()
                },
            )
            .await
            .unwrap();
        let mut tasks = hub.start(routes);
        if let Some(h) = wf.as_ref().filter(|h| h.cfg.enabled) {
            tasks.extend(h.start_workers());
        }
        let wsvc = wf
            .as_ref()
            .map(|h| WorkflowService::new(h.clone(), public.clone(), ServerInfo::default()));
        let svc = AgentService::new(hub.clone(), public.clone(), ServerInfo::default());
        Self {
            pool,
            registry,
            entry,
            ai,
            hub,
            fake,
            authorizer: public,
            inproc,
            svc,
            base: seeded.id.to_vec(),
            dir,
            tasks,
            vault: vault_kept,
            reviews,
            wf,
            wsvc,
        }
    }

    /// An active key of alice pointing at the scripted provider.
    pub(crate) async fn key(&self, owner: Owner, label: &str) -> crate::ai::store::KeyRow {
        let new = NewKey {
            label: label.into(),
            provider: Provider::OpenAiCompatible,
            api_key: zeroize::Zeroizing::new(PLANTED.into()),
            base_url: self.fake.url(),
            default_model: "model-x".into(),
            monthly_cap: 0,
        };
        let k = self.ai.create_key(&owner, new).await.unwrap();
        self.ai
            .store
            .mark(&k.id, k.key_version, KeyState::Active, "", true)
            .await
            .unwrap();
        self.ai.get_key(&k.id).await.unwrap()
    }

    /// Scripts the model: the n-th answer of a run is `plan[n]` (the last repeats).
    pub(crate) fn script(&self, plan: Vec<Reply>) {
        let plan = Arc::new(plan);
        self.fake
            .set_default(Reply::Dynamic(Arc::new(move |body: &Value| {
                let n = body["messages"]
                    .as_array()
                    .map_or(0, |m| m.iter().filter(|x| x["role"] == "assistant").count());
                plan[n.min(plan.len() - 1)].clone()
            })));
    }

    pub(crate) fn request<T>(&self, msg: T, token: &str, op: Option<&str>) -> Request<T> {
        let mut r = Request::new(msg);
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(token).unwrap()));
        if let Some(op) = op {
            r.metadata_mut().insert("x-asc-op-id", op.parse().unwrap());
        }
        r
    }

    pub(crate) async fn start_run(
        &self,
        token: &str,
        task: &str,
    ) -> Result<wire::AgentRun, tonic::Status> {
        self.start_run_with(
            token,
            wire::CreateAgentRunRequest {
                repo: repo_ref(),
                agent: "fixer".into(),
                title: "Fix it".into(),
                task: task.into(),
                ..wire::CreateAgentRunRequest::default()
            },
        )
        .await
    }

    pub(crate) async fn start_run_with(
        &self,
        token: &str,
        msg: wire::CreateAgentRunRequest,
    ) -> Result<wire::AgentRun, tonic::Status> {
        let op = uuid::Uuid::new_v4().to_string();
        let r = self
            .svc
            .create_run(self.request(msg, token, Some(&op)))
            .await?
            .into_inner();
        Ok(r.run.unwrap())
    }

    pub(crate) async fn wait(&self, id: u64) -> RunRow {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let r = self.hub.core.runs.get(id).await.unwrap().unwrap();
            if !r.state.is_active() {
                return r;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run {id} did not finish: {:?}",
                r.state
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub(crate) async fn wait_state(&self, id: u64, s: RunState) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let r = self.hub.core.runs.get(id).await.unwrap().unwrap();
            if r.state == s {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run {id} is {:?}, not {s:?}",
                r.state
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub(crate) async fn events(&self, id: u64) -> Vec<super::runs::EventRow> {
        self.hub
            .core
            .runs
            .events_after(id, 0, 10_000)
            .await
            .unwrap()
    }

    /// The target of a stored ref.
    pub(crate) async fn ref_target(&self, name: &str) -> Option<RefTarget> {
        let n = RefName::new(name.as_bytes()).unwrap();
        self.registry
            .refs()
            .get(self.entry.key, &n)
            .await
            .unwrap()
            .map(|r| r.target)
    }

    /// The tip of the run's agent branch, whatever the repository's mode.
    pub(crate) async fn agent_tip(
        &self,
        agent: &str,
        run: u64,
        owner: i64,
    ) -> Option<asc_core::id::ChangesetId> {
        let plain = format!("refs/heads/agent/{agent}/run-{run}");
        let user = format!("refs/users/{owner}/heads/agent/{agent}/run-{run}");
        for name in [plain, user] {
            if let Some(RefTarget::Changeset(c)) = self.ref_target(&name).await {
                return Some(c);
            }
        }
        None
    }

    pub(crate) async fn trunk(&self) -> Vec<u8> {
        match self.ref_target("refs/heads/master").await {
            Some(RefTarget::Changeset(c)) => c.as_bytes().to_vec(),
            other => panic!("trunk is {other:?}"),
        }
    }

    pub(crate) async fn read_at(
        &self,
        cs: asc_core::id::ChangesetId,
        path: &str,
    ) -> Option<String> {
        let root = self.hub.core.view.root_of(&self.entry, cs).await.unwrap();
        self.hub
            .core
            .view
            .read(&self.entry, root, path, 1 << 20)
            .await
            .unwrap()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

pub(crate) fn tools(calls: Vec<(&str, Value)>) -> Reply {
    Reply::Tools {
        text: String::new(),
        calls: calls.into_iter().map(|(n, a)| (n.to_owned(), a)).collect(),
        input: 100,
        output: 20,
    }
}

pub(crate) fn done(text: &str) -> Reply {
    Reply::Text {
        text: text.into(),
        input: 100,
        output: 20,
    }
}

const STANDARD_PLAN: fn() -> Vec<Reply> = || {
    vec![
        tools(vec![("read_file", json!({"path": "src/main.rs"}))]),
        tools(vec![
            (
                "edit_file",
                json!({"path": "src/main.rs", "old": "hello", "new": "hello, world"}),
            ),
            ("commit", json!({"message": "Greet the world"})),
        ]),
        tools(vec![
            (
                "write_file",
                json!({"path": "tests/greet.rs", "content": "#[test]\nfn greets() {}\n"}),
            ),
            ("commit", json!({"message": "Test the greeting"})),
        ]),
        done("All done."),
    ]
};

// ------------------------------------------------------------------------------ the happy path

async fn happy(branchless: bool) {
    let (logs, _g) = capture_logs();
    let w = World::with(Opts {
        branchless,
        ..Opts::default()
    })
    .await;
    w.key(Owner::User("alice".into()), "mine").await;
    w.script(STANDARD_PLAN());
    let trunk_before = w.trunk().await;
    let run = w
        .start_run("alice-token", "Make the greeting friendlier and test it")
        .await
        .unwrap();
    assert_eq!(run.state, wire::AgentRunState::Queued as i32);
    assert_eq!(
        (run.agent.as_str(), run.model.as_str()),
        ("fixer", "model-x")
    );
    let row = w.wait(run.id).await;
    assert_eq!(row.state, RunState::Succeeded, "{row:?}");
    assert_eq!(row.changes.len(), 2, "a stack of two changes");
    assert!(row.finished >= row.started && row.started > 0);

    // Trunk did not move; the work is on the agent's own branch, two commits deep.
    assert_eq!(w.trunk().await, trunk_before, "an agent never writes trunk");
    let tip = w
        .agent_tip("fixer", run.id, 7)
        .await
        .expect("the agent branch exists");
    assert_eq!(
        w.read_at(tip, "src/main.rs").await.unwrap(),
        MAIN_RS.replace("hello", "hello, world")
    );
    assert_eq!(
        w.read_at(tip, "tests/greet.rs").await.unwrap(),
        "#[test]\nfn greets() {}\n"
    );
    let body = w.entry.backend.engine().get_changeset(tip).await.unwrap();
    assert_eq!(body.parents.len(), 1);
    let first = w
        .entry
        .backend
        .engine()
        .get_changeset(body.parents[0])
        .await
        .unwrap();
    assert_eq!(
        first.parents[0].as_bytes().to_vec(),
        w.base,
        "the first change sits on the base"
    );
    assert_eq!(String::from_utf8_lossy(&body.message), "Test the greeting");
    assert!(
        String::from_utf8_lossy(body.author.name()).contains("fixer"),
        "the agent signs as itself"
    );

    // The change ids resolve, bottom to top.
    assert!(row.changes.iter().all(|c| c.len() == 16));

    // The events tell the story.
    let ev = w.events(run.id).await;
    let kinds: Vec<i64> = ev.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == EventKind::ToolCall as i64)
            .count(),
        5
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == EventKind::Change as i64)
            .count(),
        2
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == EventKind::Usage as i64)
            .count(),
        4
    );
    assert!(
        ev.iter()
            .any(|e| e.kind == EventKind::Message as i64 && e.text == "All done.")
    );
    assert_eq!(ev.last().unwrap().text, "succeeded");
    let seqs: Vec<u64> = ev.iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "gap free"
    );

    // The run's token carried the usage and is revoked now.
    let tok = w.hub.core.tokens.get(&row.token_id).await.unwrap().unwrap();
    assert_eq!(tok.spent.tokens, 4 * 120);
    assert_eq!(tok.spent.requests, 4);
    assert_eq!(tok.spent.changes, 2);
    assert!(tok.revoked_at > 0, "revoked when the run ended");
    assert_eq!(tok.agent, "fixer");
    // The usage of the key is reported against the repository and agent.
    let rows =
        w.ai.store
            .usage(
                &Owner::User("alice".into()),
                None,
                "2000-01-01",
                "2999-12-31",
                crate::ai::store::GroupBy::Agent,
            )
            .await
            .unwrap();
    assert_eq!((rows[0].group.as_str(), rows[0].requests), ("fixer", 4));
    // The working copy is gone.
    let left: Vec<_> = std::fs::read_dir(w.dir.path().join("work"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
    // The wire form carries the usage.
    let got = w
        .svc
        .get_run(w.request(
            wire::GetAgentRunRequest {
                repo: repo_ref(),
                id: run.id,
            },
            "carol-token",
            None,
        ))
        .await
        .unwrap()
        .into_inner()
        .run
        .unwrap();
    assert_eq!(got.usage.unwrap().tokens, 480);
    assert_eq!(got.changes.len(), 2);
    assert_eq!(got.created_by, "alice");
    assert!(!logs.text().contains(PLANTED));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_edits_commits_and_leaves_a_stack_in_branch_mode() {
    happy(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_edits_commits_and_leaves_a_stack_in_a_branchless_repository() {
    happy(true).await;
}
