//! Tests of the gate, the policy store and the `Policy` service. The fixture here is shared with
//! the secret scanning and signing tests.

use std::sync::Arc;

use asc_core::change::{ChangeId, ChangeMeta, RewriteOp};
use asc_core::id::{ChangesetId, RefTarget};
use asc_core::path::RefName;
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, BearerToken, Credential, Principal, StaticTokens};
use asc_rpc::server::{CallCtx, RefsHandler, RepoName, ServerInfo, status_rpc_error, wire};
use asc_store::refs::Refs;
use prost::Message;
use tonic::{Request, Status};
use uuid::Uuid;

use super::gate::{GateMode, GateRequest, GateUpdate, RefGate, ServerGate};
use super::*;
use crate::changes::index::ChangeIndex;
use crate::ingest::testutil::{commit, registry, repo};
use crate::mutations::ServerMutations;
use crate::read::ReadHooks;
use crate::secretscan::Scanner;
use crate::signing::rpc::Signing;
use crate::update::{PipelineHooks, ServerRefs};

pub(crate) const ALICE: &str = "42:alice";
pub(crate) const BOB: &str = "43:bob";
pub(crate) const CAROL: &str = "pat:carol";

/// A maintainer-only policy; `extra` is appended.
pub(crate) fn policy_text(extra: &str) -> String {
    format!(
        "version = 1\n[roles]\nmaintainer = [\"user:alice\"]\nreviewer = [\"user:bob\"]\n{extra}"
    )
}

/// A github-shaped token that is built here, so no source file holds a whole one.
pub(crate) fn token() -> String {
    ["ghp_", "q7Zk3MxR9vTb2LpWd8NcYf4HgJa6SeUo5Ei1"].concat()
}

pub(crate) fn cid(n: u8) -> ChangeId {
    ChangeId::from_bytes([n; 16])
}

pub(crate) fn code(s: &Status) -> String {
    status_rpc_error(s).map_or_else(|| s.code().to_string(), |e| e.code().to_owned())
}

pub(crate) fn paths(s: &Status) -> Vec<String> {
    status_rpc_error(s).map_or_else(Vec::new, |e| e.info().paths.clone())
}

pub(crate) fn ctx(principal: &str) -> CallCtx {
    CallCtx {
        repo: RepoName::parse("o/r").unwrap(),
        id_mode: IdMode::Asc,
        principal: Principal {
            id: principal.into(),
        },
        op_id: Some(Uuid::new_v4()),
        client: None,
        admin: Default::default(),
    }
}

fn master() -> RefName {
    RefName::new("refs/heads/master").unwrap()
}

/// A repository with a gated server on top of it.
pub(crate) struct Fx {
    pub reg: Arc<Registry>,
    pub e: Arc<RepoEntry>,
    /// Pushes through the gate.
    pub gated: ServerRefs,
    /// Pushes with no gate, to set the stage.
    pub plain: ServerRefs,
    pub gate: Arc<ServerGate>,
    pub policies: Arc<PolicyStore>,
    #[allow(dead_code)] // held so the services outlive the gate
    pub scanner: Arc<Scanner>,
    #[allow(dead_code)] // held so the services outlive the gate
    pub signing: Arc<Signing>,
}

pub(crate) async fn fx_with(server: Option<Arc<crate::signing::ServerSigner>>) -> Fx {
    let reg = registry().await;
    let e = repo(&reg).await;
    let policies = Arc::new(PolicyStore::new(reg.clone()));
    let scanner = Arc::new(Scanner::new(
        reg.clone(),
        crate::secretscan::Config::default(),
    ));
    let signing = Arc::new(Signing::new(
        reg.clone(),
        crate::signing::Config::default(),
        server,
    ));
    let gate = Arc::new(ServerGate::new(
        reg.clone(),
        policies.clone(),
        scanner.clone(),
        signing.clone(),
    ));
    let hooks = Arc::new(PipelineHooks::default());
    hooks.set_gate(gate.clone()).ok().unwrap();
    Fx {
        gated: ServerRefs::new(reg.clone(), hooks),
        plain: ServerRefs::new(reg.clone(), Arc::default()),
        reg,
        e,
        gate,
        policies,
        scanner,
        signing,
    }
}

pub(crate) async fn fx() -> Fx {
    fx_with(None).await
}

pub(crate) fn update_req(
    name: &str,
    expect: wire::ref_update::Expect,
    to: ChangesetId,
    mode: wire::UpdateMode,
) -> wire::UpdateRefsRequest {
    wire::UpdateRefsRequest {
        repo: Some(wire::RepoRef {
            name: "o/r".into(),
            id_mode: 0,
        }),
        updates: vec![wire::RefUpdate {
            name: name.into(),
            expect: Some(expect),
            new: Some(wire::RefTarget::from(&RefTarget::Changeset(to))),
            delete: false,
        }],
        mode: mode as i32,
        ..wire::UpdateRefsRequest::default()
    }
}

impl Fx {
    /// Creates `master` at `cs` without the gate.
    pub async fn set_master(&self, cs: ChangesetId) {
        let req = update_req(
            "refs/heads/master",
            wire::ref_update::Expect::Any(true),
            cs,
            wire::UpdateMode::Force,
        );
        self.plain.update_refs(&ctx(ALICE), req).await.unwrap();
    }

    pub async fn tip(&self) -> ChangesetId {
        match self
            .reg
            .refs()
            .get(self.e.key, &master())
            .await
            .unwrap()
            .unwrap()
            .target
        {
            RefTarget::Changeset(c) => c,
            RefTarget::Tag(_) => panic!("tag"),
        }
    }

    pub async fn land(
        &self,
        who: &str,
        to: ChangesetId,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        let req = update_req(
            "refs/heads/master",
            wire::ref_update::Expect::Any(true),
            to,
            wire::UpdateMode::Land,
        );
        self.gated.update_refs(&ctx(who), req).await
    }

    pub async fn land_with(
        &self,
        ctx: &CallCtx,
        to: ChangesetId,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        let req = update_req(
            "refs/heads/master",
            wire::ref_update::Expect::Any(true),
            to,
            wire::UpdateMode::Land,
        );
        self.gated.update_refs(ctx, req).await
    }

    pub async fn push(
        &self,
        who: &str,
        name: &str,
        expect: wire::ref_update::Expect,
        to: ChangesetId,
        mode: wire::UpdateMode,
    ) -> Result<wire::UpdateRefsResponse, Status> {
        self.gated
            .update_refs(&ctx(who), update_req(name, expect, to, mode))
            .await
    }

    /// Makes the server know `cs` and its ancestors, as a push does, without moving a shared ref.
    pub async fn ingest(&self, cs: ChangesetId) {
        let name = format!("refs/heads/ingest-{cs}");
        let req = update_req(
            &name,
            wire::ref_update::Expect::Any(true),
            cs,
            wire::UpdateMode::Force,
        );
        self.plain.update_refs(&ctx(ALICE), req).await.unwrap();
    }

    /// Registers `cs` as version `v` of change `n`, pushed by `actor`.
    pub async fn register(&self, cs: ChangesetId, n: u8, v: i64, actor: &str) {
        let idx = ChangeIndex::new(self.reg.clone());
        let mut tx = self.reg.refs().begin().await.unwrap();
        idx.write_meta(
            &mut tx,
            self.e.key,
            &ChangeMeta::new(cs, cid(n), vec![], RewriteOp::New),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        sqlx::query(
            "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) \
             VALUES ($1, $2, $3, $4, NULL, 1, $5, $6)",
        )
        .bind(self.e.ns.as_bytes().to_vec())
        .bind(cid(n).as_bytes().to_vec())
        .bind(v)
        .bind(cs.as_bytes().to_vec())
        .bind(actor)
        .bind(Uuid::new_v4().as_bytes().to_vec())
        .execute(self.reg.pool())
        .await
        .unwrap();
    }

    /// What the host reports about change `n`.
    pub async fn review(&self, n: u8, approvals: u32, reviewed_version: u32) {
        let state = wire::ReviewState {
            approvals,
            reviewed_version,
            ..wire::ReviewState::default()
        };
        sqlx::query(
            "INSERT INTO review_state (ns, change, state, updated) VALUES ($1, $2, $3, 1) \
             ON CONFLICT (ns, change) DO UPDATE SET state = excluded.state",
        )
        .bind(self.e.ns.as_bytes().to_vec())
        .bind(cid(n).as_bytes().to_vec())
        .bind(state.encode_to_vec())
        .execute(self.reg.pool())
        .await
        .unwrap();
    }

    /// A check run named `name` for version `v` of change `n` with `status` (3 = success).
    pub async fn check_run(&self, n: u8, v: i64, name: &str, status: i64) {
        let id: i64 =
            sqlx::query_scalar("SELECT CAST(COALESCE(MAX(id), 0) AS BIGINT) + 1 FROM check_runs")
                .fetch_one(self.reg.pool())
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO check_runs (ns, id, source, external_id, name, change, version, status, created) \
             VALUES ($1, $2, 'test', $3, $4, $5, $6, $7, 1)",
        )
        .bind(self.e.ns.as_bytes().to_vec())
        .bind(id)
        .bind(format!("run-{id}"))
        .bind(name)
        .bind(cid(n).as_bytes().to_vec())
        .bind(v)
        .bind(status)
        .execute(self.reg.pool())
        .await
        .unwrap();
    }

    pub async fn rows(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(self.reg.pool())
            .await
            .unwrap()
    }
}

// ------------------------------------------------------------------------------ who

#[test]
fn principals_map_to_policy_subjects() {
    let kind = |id: &str| who_of(id).kind;
    assert_eq!(who_of("42:alice"), Who::new(WhoKind::User, "alice"));
    assert_eq!(who_of("pat:carol"), Who::new(WhoKind::User, "carol"));
    assert_eq!(
        who_of("agent:reviewer-1"),
        Who::new(WhoKind::Agent, "reviewer-1")
    );
    assert_eq!(who_of("42:alice:9"), Who::new(WhoKind::Deploy, "alice"));
    assert_eq!(who_of("deploy:o/r"), Who::new(WhoKind::Deploy, "o/r"));
    assert_eq!(who_of("-2:task"), Who::new(WhoKind::Ci, "task"));
    assert_eq!(who_of("ci:abc"), Who::new(WhoKind::Ci, "abc"));
    assert_eq!(kind("host-service"), Some(WhoKind::Host));
    assert_eq!(kind("forgejo-internal"), Some(WhoKind::Host));
    // Anyone the server cannot classify is bound by `*` only.
    for id in [
        "",
        "0:anonymous",
        "alice",
        "agent:",
        "pat:",
        "weird thing",
        "1:a:b:c",
    ] {
        assert_eq!(who_of(id), Who::unknown(), "{id:?}");
    }
    assert_eq!(
        who_from_ref("user", "alice"),
        Who::new(WhoKind::User, "alice")
    );
    assert_eq!(who_from_ref("system", "x"), Who::unknown());
    assert_eq!(who_from_ref("", ""), Who::unknown());
}

// ------------------------------------------------------------------------------ the store

#[tokio::test]
async fn the_store_reads_the_file_at_a_changeset_and_caches_by_id() {
    let f = fx().await;
    let none = commit(&f.e, &[], &[("a", b"1")]).await;
    let loaded = f.policies.at(&f.e, none).await.unwrap();
    assert!(matches!(loaded.state, State::Absent));
    assert!(!loaded.present());
    assert_eq!(loaded.source, Some(none));

    let text = policy_text("");
    let with = commit(&f.e, &[none], &[(POLICY_PATH, text.as_bytes())]).await;
    let loaded = f.policies.at(&f.e, with).await.unwrap();
    assert!(loaded.policy().is_some());
    assert_eq!(&loaded.text[..], text.as_bytes());
    assert!(
        Arc::ptr_eq(&loaded, &f.policies.at(&f.e, with).await.unwrap()),
        "cached"
    );

    let broken = commit(&f.e, &[with], &[(POLICY_PATH, b"version = 1\nbogus = 2\n")]).await;
    let loaded = f.policies.at(&f.e, broken).await.unwrap();
    let d = loaded.error().expect("unusable");
    assert_eq!(d.code, "unknown_key");
    assert!(loaded.policy().is_none() && loaded.present());

    // A directory or an oversized file at the path is unusable, not absent.
    let dir = commit(&f.e, &[none], &[(".asc/policy.toml/inner", b"x")]).await;
    assert_eq!(
        f.policies
            .at(&f.e, dir)
            .await
            .unwrap()
            .error()
            .unwrap()
            .code,
        "not_a_file"
    );
    let big = vec![b'#'; asc_core::policy::MAX_POLICY_BYTES + 1];
    let large = commit(&f.e, &[none], &[(POLICY_PATH, &big)]).await;
    assert_eq!(
        f.policies
            .at(&f.e, large)
            .await
            .unwrap()
            .error()
            .unwrap()
            .code,
        "too_large"
    );

    // The tip of trunk is the policy in force, and an empty repository has none.
    assert!(matches!(
        f.policies.in_force(&f.e).await.unwrap().state,
        State::Absent
    ));
    assert_eq!(f.policies.trunk_tip(&f.e).await.unwrap(), None);
    f.set_master(with).await;
    assert_eq!(f.policies.trunk_tip(&f.e).await.unwrap(), Some(with));
    assert!(f.policies.in_force(&f.e).await.unwrap().policy().is_some());
}

// ------------------------------------------------------------------------------ the gate

#[tokio::test]
async fn a_repository_without_a_policy_accepts_what_it_always_did() {
    let f = fx().await;
    let a = commit(&f.e, &[], &[("base", b"0")]).await;
    f.push(
        ALICE,
        "refs/heads/master",
        wire::ref_update::Expect::Absent(true),
        a,
        wire::UpdateMode::FastForward,
    )
    .await
    .unwrap();
    let b = commit(
        &f.e,
        &[a],
        &[
            ("src/x.rs", b"fn main() {}\n"),
            (".github/ci.yml", b"on: push\n"),
        ],
    )
    .await;
    f.land(CAROL, b).await.unwrap();
    assert_eq!(f.tip().await, b);
    // Anyone can write anywhere, and unknown principals too.
    let c = commit(&f.e, &[b], &[(".asc/other.toml", b"x = 1\n")]).await;
    f.land("who knows", c).await.unwrap();
}

#[tokio::test]
async fn a_secret_added_to_trunk_is_refused_by_place_and_rule_and_never_by_value() {
    let f = fx().await;
    let a = commit(&f.e, &[], &[("base", b"0")]).await;
    f.set_master(a).await;
    let t = token();
    let body = format!("name = \"svc\"\ngithub = \"{t}\"\n");
    let bad = commit(&f.e, &[a], &[("conf/app.toml", body.as_bytes())]).await;
    let err = f.land(ALICE, bad).await.unwrap_err();
    assert_eq!(code(&err), "SECRET_DETECTED");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert_eq!(paths(&err), ["conf/app.toml:2 github-token"]);
    // Nothing of the value is anywhere in what the caller sees.
    assert!(!format!("{err:?}").contains(&t[4..]), "{err:?}");
    assert!(!err.message().contains(&t[4..]));
    // The ref did not move and nothing was recorded as a finding (the secret never entered).
    assert_eq!(f.tip().await, a);
    assert_eq!(f.rows("SELECT COUNT(*) FROM secret_findings").await, 0);
    // The refusal is audited, without the value.
    let details: String = sqlx::query_scalar(
        "SELECT details_json FROM audit_events WHERE action = 'secret.push.blocked' AND scope_kind = 'repo'",
    )
    .fetch_one(f.reg.pool())
    .await
    .unwrap();
    assert!(
        details.contains("github-token") && details.contains("refs/heads/master"),
        "{details}"
    );
    assert!(!details.contains(&t[4..]), "{details}");
    // A direct push is checked the same way.
    let err = f
        .push(
            ALICE,
            "refs/heads/master",
            wire::ref_update::Expect::Value(wire::RefTarget::from(&RefTarget::Changeset(a))),
            bad,
            wire::UpdateMode::FastForward,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "SECRET_DETECTED");
    // The same change without the secret lands.
    let good = commit(&f.e, &[a], &[("conf/app.toml", b"name = \"svc\"\n")]).await;
    f.land(ALICE, good).await.unwrap();
}

#[tokio::test]
async fn private_keys_and_each_file_of_a_stack_are_found() {
    let f = fx().await;
    let a = commit(&f.e, &[], &[("base", b"0")]).await;
    f.set_master(a).await;
    let key = format!(
        "{}{}\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7\n",
        "-----BEGIN ", "PRIVATE KEY-----"
    );
    let one = commit(&f.e, &[a], &[("ok.txt", b"fine")]).await;
    let two = commit(&f.e, &[one], &[("deploy/id", key.as_bytes())]).await;
    let three = commit(&f.e, &[two], &[("more.txt", b"fine too")]).await;
    let err = f.land(ALICE, three).await.unwrap_err();
    assert_eq!(code(&err), "SECRET_DETECTED");
    assert_eq!(paths(&err), ["deploy/id:1 private-key-block"]);
    assert_eq!(f.tip().await, a);
}

#[tokio::test]
async fn only_protected_refs_are_scanned() {
    let f = fx().await;
    let a = commit(&f.e, &[], &[("base", b"0")]).await;
    f.set_master(a).await;
    let body = format!("t = \"{}\"\n", token());
    let leaky = commit(&f.e, &[a], &[("x.toml", body.as_bytes())]).await;
    // A feature branch and a tag are not protected: the author is still working.
    f.push(
        ALICE,
        "refs/heads/feature",
        wire::ref_update::Expect::Absent(true),
        leaky,
        wire::UpdateMode::FastForward,
    )
    .await
    .unwrap();
    f.push(
        ALICE,
        "refs/tags/v1",
        wire::ref_update::Expect::Absent(true),
        leaky,
        wire::UpdateMode::FastForward,
    )
    .await
    .unwrap();
    // Landing the branch's content on trunk is where the scan stops it.
    let err = f.land(ALICE, leaky).await.unwrap_err();
    assert_eq!(code(&err), "SECRET_DETECTED");
    // A release branch is protected.
    let err = f
        .push(
            ALICE,
            "refs/heads/release/1.0",
            wire::ref_update::Expect::Absent(true),
            leaky,
            wire::UpdateMode::FastForward,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "SECRET_DETECTED");
    // So is a land branch the repository names.
    sqlx::query("UPDATE repos SET land_branches = '[\"stable\"]' WHERE id = $1")
        .bind(f.e.key.0)
        .execute(f.reg.pool())
        .await
        .unwrap();
    let err = f
        .push(
            ALICE,
            "refs/heads/stable",
            wire::ref_update::Expect::Absent(true),
            leaky,
            wire::UpdateMode::FastForward,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "SECRET_DETECTED");
}

#[tokio::test]
async fn block_on_push_off_records_findings_and_lets_the_update_through() {
    let f = fx().await;
    let p = policy_text("[secrets]\nblock_on_push = false\n");
    let a = commit(&f.e, &[], &[(POLICY_PATH, p.as_bytes())]).await;
    f.set_master(a).await;
    let body = format!("t = \"{}\"\n", token());
    let leaky = commit(&f.e, &[a], &[("x.toml", body.as_bytes())]).await;
    f.land(ALICE, leaky).await.unwrap();
    assert_eq!(f.tip().await, leaky);
    let (rule, path, state, preview, fp): (String, Vec<u8>, i64, String, String) = {
        use sqlx::Row;
        let r =
            sqlx::query("SELECT rule_id, path, state, preview, fingerprint FROM secret_findings")
                .fetch_one(f.reg.pool())
                .await
                .unwrap();
        (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4))
    };
    assert_eq!(
        (rule.as_str(), &path[..], state),
        ("github-token", &b"x.toml"[..], 1)
    );
    assert_eq!(preview, "gh***");
    assert_eq!(fp.len(), 64);
    assert!(!fp.contains(&token()[4..]));
    // The same secret in the same file is one finding, however often it is pushed.
    let again = commit(
        &f.e,
        &[leaky],
        &[("x.toml", body.as_bytes()), ("y.txt", b"z")],
    )
    .await;
    f.land(ALICE, again).await.unwrap();
    assert_eq!(f.rows("SELECT COUNT(*) FROM secret_findings").await, 1);
}

#[tokio::test]
async fn scanning_can_be_switched_off_and_extended_by_the_trunk_policy() {
    let f = fx().await;
    let off = policy_text("[secrets]\nenabled = false\n");
    let a = commit(&f.e, &[], &[(POLICY_PATH, off.as_bytes())]).await;
    f.set_master(a).await;
    let body = format!("t = \"{}\"\n", token());
    let leaky = commit(&f.e, &[a], &[("x.toml", body.as_bytes())]).await;
    f.land(ALICE, leaky).await.unwrap();

    let g = fx().await;
    let custom = policy_text(
        "[secrets.rules.acme]\npattern = \"ACME-[0-9A-F]{16}\"\nseverity = \"critical\"\n",
    );
    let a = commit(&g.e, &[], &[(POLICY_PATH, custom.as_bytes())]).await;
    g.set_master(a).await;
    let hit = commit(&g.e, &[a], &[("k.txt", b"id: ACME-0123456789ABCDEF\n")]).await;
    let err = g.land(ALICE, hit).await.unwrap_err();
    assert_eq!(paths(&err), ["k.txt:1 custom.acme"]);
}

#[tokio::test]
async fn allow_paths_are_read_from_the_policy_in_force_and_never_from_the_update() {
    let f = fx().await;
    let a = commit(&f.e, &[], &[("base", b"0")]).await;
    f.set_master(a).await;
    let body = format!("t = \"{}\"\n", token());
    // One update that brings an allow list and the secret it would allow: refused, because the
    // list is not in force yet.
    let allow = policy_text("[secrets]\nallow_paths = [\"fixtures/**\"]\n");
    let both = commit(
        &f.e,
        &[a],
        &[
            (POLICY_PATH, allow.as_bytes()),
            ("fixtures/k.txt", body.as_bytes()),
        ],
    )
    .await;
    let err = f.land(ALICE, both).await.unwrap_err();
    assert_eq!(code(&err), "SECRET_DETECTED");
    // The list lands first, as a reviewed change of its own; then the fixture is fine.
    let list = commit(&f.e, &[a], &[(POLICY_PATH, allow.as_bytes())]).await;
    f.land(ALICE, list).await.unwrap();
    let fixture = commit(&f.e, &[list], &[("fixtures/k.txt", body.as_bytes())]).await;
    f.land(ALICE, fixture).await.unwrap();
    // The allowance is for that path only.
    let elsewhere = commit(&f.e, &[fixture], &[("src/k.txt", body.as_bytes())]).await;
    assert_eq!(
        code(&f.land(ALICE, elsewhere).await.unwrap_err()),
        "SECRET_DETECTED"
    );
}

#[tokio::test]
async fn path_rules_bind_the_pusher_of_a_protected_ref() {
    let f = fx().await;
    let rules = policy_text(
        "[paths.\"src/**\"]\nwrite = [\"role:maintainer\"]\n[paths.\"docs/**\"]\nwrite = [\"*\"]\n[paths.\"locked/**\"]\nwrite = [\"role:maintainer\"]\n",
    );
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, rules.as_bytes()), ("src/lib.rs", b"//\n")],
    )
    .await;
    f.set_master(a).await;
    let edit_src = commit(
        &f.e,
        &[a],
        &[("src/lib.rs", b"// changed\n"), ("docs/a.md", b"hi\n")],
    )
    .await;
    // Bob is no maintainer: the whole update is refused and names the path and the rule.
    let err = f.land(BOB, edit_src).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    let p = paths(&err);
    assert!(p[0].contains("role:maintainer"), "{p:?}");
    assert_eq!(p[1], "src/lib.rs");
    assert_eq!(f.tip().await, a);
    assert!(
        f.rows("SELECT COUNT(*) FROM audit_events WHERE action = 'policy.denied'")
            .await
            > 0
    );
    // Alice is.
    f.land(ALICE, edit_src).await.unwrap();
    // Anyone may write the open path; a path no rule names is open too.
    let docs = commit(
        &f.e,
        &[edit_src],
        &[("docs/b.md", b"x\n"), ("README", b"y\n")],
    )
    .await;
    f.land(BOB, docs).await.unwrap();
    // Unknown principals match `*` only.
    let more = commit(&f.e, &[docs], &[("docs/c.md", b"x\n")]).await;
    f.land("somebody", more).await.unwrap();
    let src = commit(&f.e, &[more], &[("src/new.rs", b"x\n")]).await;
    assert_eq!(
        code(&f.land("somebody", src).await.unwrap_err()),
        "POLICY_DENIED"
    );
    // Paths are matched whatever their case.
    let upper = commit(&f.e, &[more], &[("LOCKED/Other.rs", b"x\n")]).await;
    assert_eq!(
        code(&f.land(BOB, upper).await.unwrap_err()),
        "POLICY_DENIED"
    );
}

#[tokio::test]
async fn deleting_a_protected_path_is_a_write_too() {
    let f = fx().await;
    let rules = policy_text("[paths.\"src/**\"]\nwrite = [\"role:maintainer\"]\n");
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, rules.as_bytes()), ("src/lib.rs", b"//\n")],
    )
    .await;
    f.set_master(a).await;
    let engine = f.e.backend.engine();
    let base = engine.get_changeset(a).await.unwrap().root;
    let root = engine
        .apply(
            base,
            &[(
                asc_core::path::RepoPath::new("src/lib.rs").unwrap(),
                asc_core::object::Change::Delete,
            )],
        )
        .await
        .unwrap();
    let del = engine
        .create_changeset(
            asc_engine::commit::NewChangeset {
                parents: vec![a],
                author: crate::ingest::testutil::sig(),
                committer: crate::ingest::testutil::sig(),
                message: b"rm".to_vec(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            asc_engine::commit::PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap();
    assert_eq!(code(&f.land(BOB, del).await.unwrap_err()), "POLICY_DENIED");
    f.land(ALICE, del).await.unwrap();
}

#[tokio::test]
async fn only_a_maintainer_changes_the_policy_and_the_new_file_must_parse() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, policy_text("").as_bytes()), ("x", b"1")],
    )
    .await;
    f.set_master(a).await;
    let relaxed = policy_text("[trunk]\nallow_force = true\n");
    let by_bob = commit(&f.e, &[a], &[(POLICY_PATH, relaxed.as_bytes())]).await;
    let err = f.land(BOB, by_bob).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    assert!(paths(&err)[0].contains("maintainer"));
    // A policy that does not parse is refused whoever sends it: it would lock the repository.
    let broken = commit(
        &f.e,
        &[a],
        &[(POLICY_PATH, b"version = 1\n[trunk]\nbogus = 1\n")],
    )
    .await;
    let err = f.land(ALICE, broken).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_INVALID");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(paths(&err)[0].contains("line 3"), "{:?}", paths(&err));
    // So is one that leaves no maintainer.
    let nobody = commit(
        &f.e,
        &[a],
        &[(POLICY_PATH, b"version = 1\n[roles]\nmaintainer = []\n")],
    )
    .await;
    assert_eq!(
        code(&f.land(ALICE, nobody).await.unwrap_err()),
        "POLICY_INVALID"
    );
    // A maintainer lands a good change of it.
    f.land(ALICE, by_bob).await.unwrap();
    assert_eq!(f.tip().await, by_bob);
    // The rules that bound that update were the old ones; now the new file is in force.
    let after = f.policies.in_force(&f.e).await.unwrap();
    assert!(after.policy().unwrap().trunk.allow_force);
}

#[tokio::test]
async fn an_update_cannot_lift_the_rules_that_bind_it() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[
            (
                POLICY_PATH,
                policy_text("[paths.\"locked/**\"]\nwrite = [\"role:maintainer\"]\n").as_bytes(),
            ),
            ("x", b"1"),
        ],
    )
    .await;
    f.set_master(a).await;
    // Bob drops the rule and writes the path it locked, in one update.
    let open = policy_text("");
    let both = commit(
        &f.e,
        &[a],
        &[(POLICY_PATH, open.as_bytes()), ("locked/f", b"1")],
    )
    .await;
    let err = f.land(BOB, both).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    // A maintainer may do exactly that.
    f.land(ALICE, both).await.unwrap();
}

#[tokio::test]
async fn an_unusable_policy_refuses_everything_but_its_repair() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, b"this is not toml ["), ("x", b"1")],
    )
    .await;
    f.set_master(a).await;
    assert!(f.policies.in_force(&f.e).await.unwrap().error().is_some());
    let other = commit(&f.e, &[a], &[("y", b"2")]).await;
    let err = f.land(ALICE, other).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    assert!(
        paths(&err)[0].contains("cannot be used"),
        "{:?}",
        paths(&err)
    );
    // An update that still leaves it broken is no repair.
    let still = commit(&f.e, &[a], &[(POLICY_PATH, b"still [broken")]).await;
    assert_eq!(
        code(&f.land(ALICE, still).await.unwrap_err()),
        "POLICY_INVALID"
    );
    // A repair of the policy file alone goes through once it has the approval a reviewed change
    // needs, and the repository works again.
    let fixed = commit(&f.e, &[a], &[(POLICY_PATH, policy_text("").as_bytes())]).await;
    assert_eq!(
        code(&f.land(ALICE, fixed).await.unwrap_err()),
        "POLICY_DENIED",
        "an unreviewed repair by a non-administrator is refused"
    );
    f.register(fixed, 1, 1, ALICE).await;
    f.review(1, 1, 0).await;
    f.land(ALICE, fixed).await.unwrap();
    let next = commit(&f.e, &[fixed], &[("y", b"2")]).await;
    f.land(ALICE, next).await.unwrap();
    // A release branch cannot be moved while trunk's policy is unusable either.
    let g = fx().await;
    let a = commit(&g.e, &[], &[(POLICY_PATH, b"x ="), ("x", b"1")]).await;
    g.set_master(a).await;
    let r = commit(&g.e, &[a], &[("r", b"1")]).await;
    let err = g
        .push(
            ALICE,
            "refs/heads/release/1",
            wire::ref_update::Expect::Absent(true),
            r,
            wire::UpdateMode::FastForward,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
}

#[tokio::test]
async fn trunk_cannot_be_rewritten_or_deleted_unless_the_policy_allows_it() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, policy_text("").as_bytes()), ("x", b"1")],
    )
    .await;
    f.set_master(a).await;
    let b = commit(&f.e, &[a], &[("y", b"2")]).await;
    f.land(ALICE, b).await.unwrap();
    // A history rewrite: `c` forks from `a`.
    let c = commit(&f.e, &[a], &[("z", b"3")]).await;
    let expect_b = wire::ref_update::Expect::Value(wire::RefTarget::from(&RefTarget::Changeset(b)));
    let err = f
        .push(
            ALICE,
            "refs/heads/master",
            expect_b.clone(),
            c,
            wire::UpdateMode::Force,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    assert!(paths(&err)[0].contains("allow_force"));
    assert_eq!(f.tip().await, b);
    // Deleting it is refused the same way.
    let mut del = update_req(
        "refs/heads/master",
        expect_b.clone(),
        b,
        wire::UpdateMode::Force,
    );
    del.updates[0].new = None;
    del.updates[0].delete = true;
    let err = f
        .gated
        .update_refs(&ctx(ALICE), del.clone())
        .await
        .unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    // A fast-forward force push is not a rewrite.
    let d = commit(&f.e, &[b], &[("w", b"4")]).await;
    f.push(
        ALICE,
        "refs/heads/master",
        expect_b,
        d,
        wire::UpdateMode::Force,
    )
    .await
    .unwrap();
    // With allow_force on, both go through.
    let g = fx().await;
    let open = policy_text("[trunk]\nallow_force = true\n");
    let a = commit(&g.e, &[], &[(POLICY_PATH, open.as_bytes()), ("x", b"1")]).await;
    g.set_master(a).await;
    let b = commit(&g.e, &[a], &[("y", b"2")]).await;
    g.land(ALICE, b).await.unwrap();
    let c = commit(&g.e, &[a], &[("z", b"3")]).await;
    let expect_b = wire::ref_update::Expect::Value(wire::RefTarget::from(&RefTarget::Changeset(b)));
    g.push(
        ALICE,
        "refs/heads/master",
        expect_b,
        c,
        wire::UpdateMode::Force,
    )
    .await
    .unwrap();
    assert_eq!(g.tip().await, c);
    let mut del = update_req(
        "refs/heads/master",
        wire::ref_update::Expect::Value(wire::RefTarget::from(&RefTarget::Changeset(c))),
        c,
        wire::UpdateMode::Force,
    );
    del.updates[0].new = None;
    del.updates[0].delete = true;
    g.gated.update_refs(&ctx(ALICE), del).await.unwrap();
}

#[tokio::test]
async fn landing_needs_the_approvals_the_policy_requires_for_the_version_that_lands() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[
            (
                POLICY_PATH,
                policy_text("[trunk]\nrequire_approvals = 2\n").as_bytes(),
            ),
            ("x", b"1"),
        ],
    )
    .await;
    f.set_master(a).await;
    let b = commit(&f.e, &[a], &[("y", b"2")]).await;
    // Not a version of any change: nothing can show an approval.
    let err = f.land(ALICE, b).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    assert!(
        paths(&err)[1].contains("not a version of a reviewed change"),
        "{:?}",
        paths(&err)
    );
    f.register(b, 1, 1, ALICE).await;
    // No review state, then one approval of two.
    assert_eq!(code(&f.land(ALICE, b).await.unwrap_err()), "POLICY_DENIED");
    f.review(1, 1, 0).await;
    let err = f.land(ALICE, b).await.unwrap_err();
    assert!(
        paths(&err)[1].contains("1 of 2 approvals"),
        "{:?}",
        paths(&err)
    );
    // Approved at an older version than the one that lands.
    f.review(1, 2, 7).await;
    let err = f.land(ALICE, b).await.unwrap_err();
    assert!(
        paths(&err)[1].contains("approved at version 7"),
        "{:?}",
        paths(&err)
    );
    assert_eq!(f.tip().await, a);
    // Two approvals of this version (0 means the host keeps them current).
    f.review(1, 2, 1).await;
    f.land(ALICE, b).await.unwrap();
    assert_eq!(f.tip().await, b);
}

#[tokio::test]
async fn every_change_of_a_stack_needs_its_own_approvals() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[
            (
                POLICY_PATH,
                policy_text("[trunk]\nrequire_approvals = 1\n").as_bytes(),
            ),
            ("x", b"1"),
        ],
    )
    .await;
    f.set_master(a).await;
    let one = commit(&f.e, &[a], &[("one", b"1")]).await;
    let two = commit(&f.e, &[one], &[("two", b"2")]).await;
    f.register(one, 1, 1, ALICE).await;
    f.register(two, 2, 1, ALICE).await;
    f.review(1, 1, 0).await;
    let err = f.land(ALICE, two).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    assert_eq!(
        paths(&err).len(),
        2,
        "only the unapproved change is named: {:?}",
        paths(&err)
    );
    f.review(2, 1, 0).await;
    f.land(ALICE, two).await.unwrap();
}

#[tokio::test]
async fn landing_needs_the_required_checks_to_have_passed_on_that_version() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[
            (
                POLICY_PATH,
                policy_text("[trunk]\nrequire_checks = [\"build\", \"test\"]\n").as_bytes(),
            ),
            ("x", b"1"),
        ],
    )
    .await;
    f.set_master(a).await;
    let b = commit(&f.e, &[a], &[("y", b"2")]).await;
    f.register(b, 1, 1, ALICE).await;
    let err = f.land(ALICE, b).await.unwrap_err();
    assert_eq!(paths(&err).len(), 3, "{:?}", paths(&err));
    f.check_run(1, 1, "build", 3).await;
    f.check_run(1, 1, "test", 4).await; // failure
    let err = f.land(ALICE, b).await.unwrap_err();
    assert!(paths(&err)[1].contains("\"test\""), "{:?}", paths(&err));
    // A run of another version does not count, and the latest run of a name decides.
    f.check_run(1, 2, "test", 3).await;
    assert_eq!(code(&f.land(ALICE, b).await.unwrap_err()), "POLICY_DENIED");
    f.check_run(1, 1, "test", 3).await;
    f.land(ALICE, b).await.unwrap();
}

#[tokio::test]
async fn a_check_can_be_named_by_its_job() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[
            (
                POLICY_PATH,
                policy_text("[trunk]\nrequire_checks = [\"lint\"]\n").as_bytes(),
            ),
            ("x", b"1"),
        ],
    )
    .await;
    f.set_master(a).await;
    let b = commit(&f.e, &[a], &[("y", b"2")]).await;
    f.register(b, 1, 1, ALICE).await;
    f.check_run(1, 1, "ci", 3).await;
    assert_eq!(code(&f.land(ALICE, b).await.unwrap_err()), "POLICY_DENIED");
    let run: i64 = f.rows("SELECT MAX(id) FROM check_runs").await;
    sqlx::query("INSERT INTO check_jobs (ns, id, run, name, status) VALUES ($1, 1, $2, 'lint', 3)")
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(run)
        .execute(f.reg.pool())
        .await
        .unwrap();
    f.land(ALICE, b).await.unwrap();
}

impl Fx {
    /// A failed test case in group `group` (state 3 is quarantined) of job `job` of `run`.
    async fn failing_case(&self, run: i64, job: i64, seq: i64, group: i64, quarantined: bool) {
        let ns = self.e.ns.as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO failure_groups (ns, id, signature, title, test_name, job_name, state, first_seen, last_seen) \
             VALUES ($1, $2, $3, 't', $4, 'unit', $5, 1, 1) ON CONFLICT (ns, id) DO NOTHING",
        )
        .bind(ns.clone())
        .bind(group)
        .bind(format!("sig-{group}"))
        .bind(format!("t{group}"))
        .bind(if quarantined { 3_i64 } else { 1_i64 })
        .execute(self.reg.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO test_cases (ns, run_id, job_id, seq, job_name, name, outcome, attempt, group_id, change, version, head, at) \
             VALUES ($1, $2, $3, $4, 'unit', $5, 2, 1, $6, $7, 1, $8, 1)",
        )
        .bind(ns)
        .bind(run)
        .bind(job)
        .bind(seq)
        .bind(format!("t{group}"))
        .bind(group)
        .bind(cid(1).as_bytes().to_vec())
        .bind(vec![0u8; 32])
        .execute(self.reg.pool())
        .await
        .unwrap();
    }
}

async fn quarantine_fixture(policy: &str) -> (Fx, ChangesetId, i64) {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, policy_text(policy).as_bytes()), ("x", b"1")],
    )
    .await;
    f.set_master(a).await;
    let b = commit(&f.e, &[a], &[("y", b"2")]).await;
    f.register(b, 1, 1, ALICE).await;
    f.check_run(1, 1, "test", 4).await; // failure
    let run: i64 = f.rows("SELECT MAX(id) FROM check_runs").await;
    sqlx::query("INSERT INTO check_jobs (ns, id, run, name, status) VALUES ($1, 1, $2, 'unit', 4)")
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(run)
        .execute(f.reg.pool())
        .await
        .unwrap();
    (f, b, run)
}

const QUARANTINE_ON: &str = "[trunk]\nrequire_checks = [\"test\"]\n[ci]\nallow_quarantine = true\n";

#[tokio::test]
async fn a_check_that_failed_only_on_quarantined_failures_lets_the_change_land() {
    let (f, b, run) = quarantine_fixture(QUARANTINE_ON).await;
    // A failed job that reported no failing test is a failure nobody quarantined.
    assert_eq!(code(&f.land(ALICE, b).await.unwrap_err()), "POLICY_DENIED");
    // Its one failing test is in a quarantined group: the check no longer blocks.
    f.failing_case(run, 1, 1, 1, true).await;
    f.land(ALICE, b).await.unwrap();
}

#[tokio::test]
async fn a_failure_that_is_not_quarantined_still_refuses_the_landing() {
    let (f, b, run) = quarantine_fixture(QUARANTINE_ON).await;
    f.failing_case(run, 1, 1, 1, true).await;
    f.failing_case(run, 1, 2, 2, false).await;
    let err = f.land(ALICE, b).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    assert!(
        paths(&err).iter().any(|p| p.contains("\"test\"")),
        "{:?}",
        paths(&err)
    );
}

#[tokio::test]
async fn quarantine_counts_only_while_the_policy_allows_it() {
    let (f, b, run) = quarantine_fixture(
        "[trunk]\nrequire_checks = [\"test\"]\n[ci]\nallow_quarantine = false\n",
    )
    .await;
    f.failing_case(run, 1, 1, 1, true).await;
    assert_eq!(code(&f.land(ALICE, b).await.unwrap_err()), "POLICY_DENIED");
}

// ------------------------------------------------------------------------------ agents

fn gate_req<'a>(
    c: &'a CallCtx,
    e: &'a RepoEntry,
    updates: &'a [GateUpdate],
    mode: GateMode,
) -> GateRequest<'a> {
    GateRequest {
        ctx: c,
        entry: e,
        updates,
        mode,
    }
}

fn draft(new: ChangesetId) -> GateUpdate {
    GateUpdate {
        name: "refs/heads/work".into(),
        old: None,
        new: Some(RefTarget::Changeset(new)),
        user: true,
    }
}

#[tokio::test]
async fn an_agent_never_writes_a_shared_ref() {
    let f = fx().await;
    let a = commit(&f.e, &[], &[("x", b"1")]).await;
    f.set_master(a).await;
    let b = commit(&f.e, &[a], &[("y", b"2")]).await;
    let c = ctx("agent:bot");
    for name in ["refs/heads/master", "refs/heads/feature", "refs/tags/t"] {
        let u = [GateUpdate {
            name: name.into(),
            old: None,
            new: Some(RefTarget::Changeset(b)),
            user: false,
        }];
        let err = f
            .gate
            .check(&gate_req(&c, &f.e, &u, GateMode::Push { force: false }))
            .await
            .unwrap_err();
        assert_eq!(code(&err), "AGENT_SCOPE_DENIED", "{name}");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }
    // And through a real update.
    let err = f.land("agent:bot", b).await.unwrap_err();
    assert_eq!(code(&err), "AGENT_SCOPE_DENIED");
    assert_eq!(f.tip().await, a);
    // A person is not affected.
    f.land(ALICE, b).await.unwrap();
}

#[tokio::test]
async fn agent_drafts_follow_scope_denied_paths_and_budgets() {
    let f = fx().await;
    let rules = policy_text(
        "[agents]\ndefault_scope = [\"src/**\", \"tests/**\"]\ndenied_paths = [\"src/secret/**\"]\n[agents.budgets]\nmax_changes_per_run = 2\nmax_paths_per_run = 3\n",
    );
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, rules.as_bytes()), ("base", b"0")],
    )
    .await;
    f.set_master(a).await;
    let c = ctx("agent:bot");
    let check = |cs: ChangesetId| {
        let u = [draft(cs)];
        let c = c.clone();
        let f = &f;
        async move {
            f.ingest(cs).await;
            f.gate
                .check(&gate_req(&c, &f.e, &u, GateMode::Push { force: false }))
                .await
        }
    };
    let ok = commit(&f.e, &[a], &[("src/a.rs", b"1"), ("tests/t.rs", b"2")]).await;
    check(ok).await.unwrap();
    // Outside the scope, in a denied path, and in the policy file: all AGENT_SCOPE_DENIED with the paths.
    for (path, why) in [
        ("README", "scope"),
        ("src/secret/k", "denied"),
        (POLICY_PATH, "policy"),
    ] {
        let bad = commit(&f.e, &[a], &[("src/a.rs", b"1"), (path, b"x")]).await;
        let err = check(bad).await.unwrap_err();
        assert_eq!(code(&err), "AGENT_SCOPE_DENIED", "{why}");
        assert!(
            paths(&err).iter().any(|p| p == path),
            "{why}: {:?}",
            paths(&err)
        );
    }
    // Two changes fit, three do not (max_changes_per_run = 2), and four paths do not (max 3).
    let one = commit(&f.e, &[a], &[("src/1", b"1")]).await;
    let two = commit(&f.e, &[one], &[("src/2", b"1")]).await;
    let three = commit(&f.e, &[two], &[("src/3", b"1")]).await;
    check(two).await.unwrap();
    let err = check(three).await.unwrap_err();
    assert_eq!(code(&err), "BUDGET_EXCEEDED");
    assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    let wide = commit(
        &f.e,
        &[a],
        &[
            ("src/a", b"1"),
            ("src/b", b"1"),
            ("src/c", b"1"),
            ("src/d", b"1"),
        ],
    )
    .await;
    assert_eq!(code(&check(wide).await.unwrap_err()), "BUDGET_EXCEEDED");
    // A person's draft is not held to the agent lists.
    let human = ctx(ALICE);
    let u = [draft(wide)];
    f.gate
        .check(&gate_req(&human, &f.e, &u, GateMode::Push { force: false }))
        .await
        .unwrap();
}

#[tokio::test]
async fn agents_can_be_switched_off_by_the_repository() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[(
            POLICY_PATH,
            policy_text("[agents]\nenabled = false\n").as_bytes(),
        )],
    )
    .await;
    f.set_master(a).await;
    let w = commit(&f.e, &[a], &[("src/a", b"1")]).await;
    f.ingest(w).await;
    let c = ctx("agent:bot");
    let u = [draft(w)];
    let err = f
        .gate
        .check(&gate_req(&c, &f.e, &u, GateMode::Push { force: false }))
        .await
        .unwrap_err();
    assert_eq!(code(&err), "AGENT_SCOPE_DENIED");
    assert!(paths(&err)[0].contains("switched off"));
}

#[tokio::test]
async fn an_agent_draft_under_an_unusable_policy_is_refused() {
    let f = fx().await;
    let a = commit(&f.e, &[], &[(POLICY_PATH, b"[[[")]).await;
    f.set_master(a).await;
    let w = commit(&f.e, &[a], &[("src/a", b"1")]).await;
    f.ingest(w).await;
    let c = ctx("agent:bot");
    let u = [draft(w)];
    let err = f
        .gate
        .check(&gate_req(&c, &f.e, &u, GateMode::Push { force: false }))
        .await
        .unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
}

// ------------------------------------------------------------------------------ the services

fn tokens() -> Arc<dyn asc_rpc::auth::Authorizer> {
    Arc::new(
        StaticTokens::new()
            .with_token("alice-w", ALICE, Access::Write)
            .with_token("alice-a", ALICE, Access::Admin)
            .with_token("bob-r", BOB, Access::Read)
            .with_token("bob-w", BOB, Access::Write)
            .with_token("agent-w", "agent:bot", Access::Write),
    )
}

fn call<T>(m: T, token: &str, op: bool) -> Request<T> {
    let mut r = Request::new(m);
    r.extensions_mut()
        .insert(Credential::bearer(BearerToken::new(token).unwrap()));
    if op {
        r.metadata_mut()
            .insert("x-asc-op-id", Uuid::new_v4().to_string().parse().unwrap());
    }
    r
}

fn repo_ref() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: "o/r".into(),
        id_mode: IdMode::Asc.to_wire(),
    })
}

struct Svc {
    f: Fx,
    svc: rpc::PolicyService,
}

async fn svc() -> Svc {
    let f = fx().await;
    let hooks = Arc::new(ReadHooks::default());
    let mutations = Arc::new(ServerMutations::new(
        f.reg.clone(),
        f.gated.hooks.clone(),
        hooks,
    ));
    let hub = Arc::new(rpc::PolicyHub::new(
        f.reg.clone(),
        f.policies.clone(),
        mutations,
    ));
    let svc = rpc::PolicyService::new(hub, tokens(), ServerInfo::default());
    Svc { f, svc }
}

use asc_rpc::services::policy_server::Policy as PolicyApi;

#[tokio::test]
async fn get_source_returns_the_file_and_what_it_means_for_the_caller() {
    use wire::GetPolicySourceRequest as Req;
    let s = svc().await;
    let absent = s
        .svc
        .get_source(call(
            Req {
                repo: repo_ref(),
                rev: String::new(),
            },
            "bob-r",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!absent.present && absent.text.is_empty());
    assert!(!absent.parsed.unwrap().present);

    let text = policy_text(
        "[trunk]\nrequire_approvals = 2\nrequire_checks = [\"build\"]\n[paths.\"src/**\"]\nwrite = [\"role:maintainer\"]\n",
    );
    let a = commit(&s.f.e, &[], &[(POLICY_PATH, text.as_bytes())]).await;
    s.f.set_master(a).await;
    let got = s
        .svc
        .get_source(call(
            Req {
                repo: repo_ref(),
                rev: String::new(),
            },
            "bob-r",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(got.present);
    assert_eq!(&got.text[..], text.as_bytes());
    assert_eq!(&got.source[..], a.as_bytes());
    let p = got.parsed.unwrap();
    assert!(p.present && p.error.is_empty());
    assert_eq!((p.principal_kind.as_str(), p.maintainer), ("user", false));
    assert_eq!(p.trunk.as_ref().unwrap().require_approvals, 2);
    assert_eq!(p.trunk.as_ref().unwrap().branch, "master");
    assert_eq!(p.paths.len(), 1);
    assert!(
        p.roles
            .iter()
            .any(|r| r.name == "maintainer" && r.members == ["user:alice"])
    );
    // The same file read by a maintainer says so.
    let by_alice = s
        .svc
        .get_source(call(
            Req {
                repo: repo_ref(),
                rev: "master".into(),
            },
            "alice-w",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(by_alice.parsed.unwrap().maintainer);
    // A revision that is not there, and a too long one.
    let e = s
        .svc
        .get_source(call(
            Req {
                repo: repo_ref(),
                rev: "nope".into(),
            },
            "bob-r",
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::NotFound);
    let e = s
        .svc
        .get_source(call(
            Req {
                repo: repo_ref(),
                rev: "x".repeat(5000),
            },
            "bob-r",
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    // No credential, no answer.
    let e = s
        .svc
        .get_source(Request::new(Req {
            repo: repo_ref(),
            rev: String::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn repo_get_policy_decides_paths_for_the_caller() {
    let s = svc().await;
    let rules = policy_text(
        "[paths.\"src/**\"]\nwrite = [\"role:maintainer\"]\n[agents]\ndefault_scope = [\"docs/**\"]\n[agents.budgets]\nmax_changes_per_run = 4\n",
    );
    let a = commit(&s.f.e, &[], &[(POLICY_PATH, rules.as_bytes())]).await;
    s.f.set_master(a).await;
    let hub = rpc::PolicyHub::new(
        s.f.reg.clone(),
        s.f.policies.clone(),
        Arc::new(ServerMutations::new(
            s.f.reg.clone(),
            s.f.gated.hooks.clone(),
            Arc::new(ReadHooks::default()),
        )),
    );
    let ask = |who: &str| {
        let c = ctx(who);
        let hub = &hub;
        let who = who.to_owned();
        async move {
            let _ = who;
            hub.get_policy(
                &c,
                wire::GetPolicyRequest {
                    repo: repo_ref(),
                    paths: vec![b"src/a.rs".to_vec().into(), b"docs/a.md".to_vec().into()],
                },
            )
            .await
            .unwrap()
        }
    };
    let bob = ask(BOB).await;
    assert_eq!(bob.decisions.len(), 2);
    assert!(!bob.decisions[0].allowed && bob.decisions[0].reason.contains("role:maintainer"));
    assert!(bob.decisions[1].allowed);
    assert_eq!(bob.agents.as_ref().unwrap().max_changes_per_run, 4);
    assert_eq!(bob.merge_strategy, "rebase");
    let alice = ask(ALICE).await;
    assert!(alice.maintainer && alice.decisions.iter().all(|d| d.allowed));
    let bot = ask("agent:bot").await;
    assert_eq!(bot.principal_kind, "agent");
    assert!(
        !bot.decisions[0].allowed && bot.decisions[1].allowed,
        "scope: docs only"
    );
    // Bounds.
    let too_many: Vec<bytes::Bytes> = (0..=MAX_EVAL_PATHS)
        .map(|i| format!("p{i}").into_bytes().into())
        .collect();
    let e = hub
        .get_policy(
            &ctx(BOB),
            wire::GetPolicyRequest {
                repo: repo_ref(),
                paths: too_many,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    let e = hub
        .get_policy(
            &ctx(BOB),
            wire::GetPolicyRequest {
                repo: repo_ref(),
                paths: vec![bytes::Bytes::new()],
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    let e = hub
        .get_policy(
            &ctx(BOB),
            wire::GetPolicyRequest {
                repo: repo_ref(),
                paths: vec![b"a\0b".to_vec().into()],
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn an_unusable_policy_is_reported_and_decides_nothing_is_allowed() {
    let s = svc().await;
    let a = commit(
        &s.f.e,
        &[],
        &[(
            POLICY_PATH,
            b"version = 9\n[roles]\nmaintainer = [\"user:a\"]\n",
        )],
    )
    .await;
    s.f.set_master(a).await;
    let hub = rpc::PolicyHub::new(
        s.f.reg.clone(),
        s.f.policies.clone(),
        Arc::new(ServerMutations::new(
            s.f.reg.clone(),
            s.f.gated.hooks.clone(),
            Arc::new(ReadHooks::default()),
        )),
    );
    let got = hub
        .get_policy(
            &ctx(BOB),
            wire::GetPolicyRequest {
                repo: repo_ref(),
                paths: vec![b"x".to_vec().into()],
            },
        )
        .await
        .unwrap();
    assert!(got.present && got.error.contains("version 9"), "{got:?}");
    assert!(!got.decisions[0].allowed);
}

#[tokio::test]
async fn validate_reports_errors_with_places_and_warnings_without_state() {
    use wire::ValidatePolicyRequest as Req;
    let s = svc().await;
    let bad = s
        .svc
        .validate(call(
            Req {
                repo: repo_ref(),
                text: b"version = 1\n[roles]\nmaintainer = [\"user:a\"]\n[trunk]\nbogus = 1\n"
                    .to_vec()
                    .into(),
            },
            "bob-r",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!bad.valid && bad.parsed.is_none());
    let d = &bad.diagnostics[0];
    assert_eq!(
        (d.severity, d.code.as_str(), d.line),
        (wire::PolicySeverity::Error as i32, "unknown_key", 5)
    );
    let big = s
        .svc
        .validate(call(
            Req {
                repo: repo_ref(),
                text: vec![b'#'; asc_core::policy::MAX_POLICY_BYTES + 1].into(),
            },
            "bob-r",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(big.diagnostics[0].code, "too_large");

    // bob is a plain user: a file that names alice only does not name him; with no policy in force
    // he could write anything, so that is a lockout warning for him.
    let only_alice = policy_text("");
    let ok = s
        .svc
        .validate(call(
            Req {
                repo: repo_ref(),
                text: only_alice.clone().into_bytes().into(),
            },
            "bob-r",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(ok.valid && !ok.caller_stays_maintainer);
    assert!(
        ok.diagnostics
            .iter()
            .any(|d| d.code == "lockout" && d.severity == wire::PolicySeverity::Warning as i32)
    );
    assert!(ok.parsed.unwrap().present);
    let alice = s
        .svc
        .validate(call(
            Req {
                repo: repo_ref(),
                text: only_alice.into_bytes().into(),
            },
            "alice-w",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(alice.valid && alice.caller_stays_maintainer && alice.diagnostics.is_empty());
    let agentish = s
        .svc
        .validate(call(
            Req {
                repo: repo_ref(),
                text: b"version = 1\n[roles]\nmaintainer = [\"user:alice\", \"agent:*\"]\n"
                    .to_vec()
                    .into(),
            },
            "alice-w",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        agentish
            .diagnostics
            .iter()
            .any(|d| d.code == "maintainer_includes_agents")
    );
    // Validation keeps nothing.
    assert_eq!(s.f.rows("SELECT COUNT(*) FROM audit_events").await, 0);
}

#[tokio::test]
async fn propose_makes_a_change_on_a_draft_ref_and_never_writes_trunk() {
    use wire::ProposePolicyRequest as Req;
    let s = svc().await;
    let a = commit(
        &s.f.e,
        &[],
        &[(POLICY_PATH, policy_text("").as_bytes()), ("x", b"1")],
    )
    .await;
    s.f.set_master(a).await;
    let new = policy_text("[trunk]\nrequire_approvals = 1\n");
    let base = bytes::Bytes::copy_from_slice(a.as_bytes());
    let made = s
        .svc
        .propose(call(
            Req {
                repo: repo_ref(),
                base: base.clone(),
                text: new.clone().into_bytes().into(),
                message: bytes::Bytes::new(),
            },
            "alice-w",
            true,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(made.validation.unwrap().valid);
    let cs = ChangesetId::from_bytes(made.changeset.as_ref().try_into().unwrap());
    // Trunk did not move; the change has the file and the default message.
    assert_eq!(s.f.tip().await, a);
    let engine = s.f.e.backend.engine();
    let body = engine.get_changeset(cs).await.unwrap();
    assert_eq!(body.parents, [a]);
    assert!(String::from_utf8_lossy(&body.message).starts_with("policy: update"));
    assert_eq!(body.author.name(), b"alice");
    let entry = engine
        .lookup(
            body.root,
            &asc_core::path::RepoPath::new(POLICY_PATH).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        engine
            .read_file(asc_core::id::FileId::from_bytes(entry.id), entry.size)
            .await
            .unwrap(),
        new.as_bytes()
    );
    // It sits on a draft ref and is a change in the index.
    assert!(!made.change.is_empty());
    let refs: i64 =
        s.f.rows("SELECT COUNT(*) FROM refs WHERE name LIKE '%/asc/policy-%'")
            .await;
    assert_eq!(refs, 1);
    // The proposal is audited.
    assert_eq!(s.f.rows("SELECT COUNT(*) FROM audit_events WHERE action = 'policy.proposed' AND scope_kind = 'repo'").await, 1);

    // The proposer can not land it unless the policy lets them: bob proposes, alice lands.
    let by_bob = s
        .svc
        .propose(call(
            Req {
                repo: repo_ref(),
                base: base.clone(),
                text: policy_text("[trunk]\nallow_force = true\n")
                    .into_bytes()
                    .into(),
                message: b"relax".to_vec().into(),
            },
            "bob-w",
            true,
        ))
        .await;
    // Bob is not named maintainer in the file he sends? He is not: lockout does not apply to a
    // non-maintainer, but the file is valid, so the proposal is made.
    let by_bob = by_bob.unwrap().into_inner();
    let landed_by_bob =
        s.f.land(
            BOB,
            ChangesetId::from_bytes(by_bob.changeset.as_ref().try_into().unwrap()),
        )
        .await;
    assert_eq!(code(&landed_by_bob.unwrap_err()), "POLICY_DENIED");
}

#[tokio::test]
async fn propose_refuses_bad_files_stale_bases_and_lockouts() {
    use wire::ProposePolicyRequest as Req;
    let s = svc().await;
    let a = commit(
        &s.f.e,
        &[],
        &[(POLICY_PATH, policy_text("").as_bytes()), ("x", b"1")],
    )
    .await;
    s.f.set_master(a).await;
    let base = bytes::Bytes::copy_from_slice(a.as_bytes());
    let send = |text: &str, base: bytes::Bytes, who: &'static str| {
        let req = Req {
            repo: repo_ref(),
            base,
            text: text.as_bytes().to_vec().into(),
            message: bytes::Bytes::new(),
        };
        s.svc.propose(call(req, who, true))
    };
    let e = send("version = 1\n[roles]\n", base.clone(), "alice-w")
        .await
        .unwrap_err();
    assert_eq!(code(&e), "POLICY_INVALID");
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    // Alice is a maintainer; a file that drops her would lock her out of the policy.
    let e = send(
        "version = 1\n[roles]\nmaintainer = [\"user:bob\"]\n",
        base.clone(),
        "alice-w",
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), "POLICY_LOCKOUT");
    assert_eq!(e.code(), tonic::Code::FailedPrecondition);
    // A moved trunk is a version conflict naming the new tip.
    let b = commit(&s.f.e, &[a], &[("y", b"2")]).await;
    s.f.land(ALICE, b).await.unwrap();
    let e = send(&policy_text(""), base.clone(), "alice-w")
        .await
        .unwrap_err();
    assert_eq!(code(&e), "VERSION_CONFLICT");
    assert_eq!(
        &status_rpc_error(&e).unwrap().info().actual[..],
        b.as_bytes()
    );
    // Oversized text and message, and a read-only caller.
    let big = "#".repeat(asc_core::policy::MAX_POLICY_BYTES + 1);
    assert_eq!(
        code(&send(&big, base.clone(), "alice-w").await.unwrap_err()),
        "POLICY_INVALID"
    );
    let e = s
        .svc
        .propose(call(
            Req {
                repo: repo_ref(),
                base: bytes::Bytes::copy_from_slice(b.as_bytes()),
                text: policy_text("").into_bytes().into(),
                message: vec![b'm'; 5000].into(),
            },
            "alice-w",
            true,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    let e = send(
        &policy_text(""),
        bytes::Bytes::copy_from_slice(b.as_bytes()),
        "bob-r",
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
    // No x-asc-op-id on a mutating call.
    let e = s
        .svc
        .propose(call(
            Req {
                repo: repo_ref(),
                base: bytes::Bytes::copy_from_slice(b.as_bytes()),
                text: policy_text("").into_bytes().into(),
                message: bytes::Bytes::new(),
            },
            "alice-w",
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert_eq!(s.f.tip().await, b);
}

#[tokio::test]
async fn evaluate_decides_writes_runs_and_landing_and_guards_other_principals() {
    use wire::{EvaluatePolicyRequest as Req, PolicyAction as A};
    let s = svc().await;
    let text = policy_text(
        "[trunk]\nrequire_approvals = 2\nrequire_checks = [\"build\"]\n[signing]\nrequire_signed = true\n[merge]\nstrategy = \"rebase-disjoint\"\n[paths.\"src/**\"]\nwrite = [\"role:maintainer\", \"agent:*\"]\n[agents]\ndefault_scope = [\"src/**\"]\n[agents.budgets]\nmax_changes_per_run = 2\n",
    );
    let a = commit(&s.f.e, &[], &[(POLICY_PATH, text.as_bytes())]).await;
    s.f.set_master(a).await;
    let ask = |action: A,
               who: &'static str,
               principal: Option<(&str, &str)>,
               paths: Vec<&str>,
               changes: u32| {
        let req = Req {
            repo: repo_ref(),
            rev: String::new(),
            principal: principal.map(|(k, n)| wire::PrincipalRef {
                kind: k.into(),
                name: n.into(),
            }),
            action: action as i32,
            paths: paths
                .into_iter()
                .map(|p| bytes::Bytes::copy_from_slice(p.as_bytes()))
                .collect(),
            run_changes: changes,
            run_paths: 0,
        };
        s.svc.evaluate(call(req, who, false))
    };
    // Self: writes.
    let me = ask(A::Write, "bob-r", None, vec!["src/a", "docs/a"], 0)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(me.principal.as_ref().unwrap().name, "bob");
    assert!(!me.decisions[0].allowed && me.decisions[1].allowed);
    // Self: landing.
    let land = ask(A::Land, "bob-r", None, vec![], 0)
        .await
        .unwrap()
        .into_inner()
        .land
        .unwrap();
    assert_eq!(
        (
            land.approvals,
            land.checks.clone(),
            land.signed,
            land.merge_strategy.as_str()
        ),
        (2, vec!["build".to_string()], true, "rebase-disjoint")
    );
    // Someone else: a non-maintainer is told nothing, and cannot tell who exists.
    let e = ask(A::Write, "bob-r", Some(("user", "alice")), vec!["src/a"], 0)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::NotFound);
    // A maintainer may ask about an agent's run and writes.
    let run = ask(A::Run, "alice-w", Some(("agent", "bot")), vec![], 3)
        .await
        .unwrap()
        .into_inner();
    assert!(
        !run.run_allowed && run.run_reason.contains("at most 2"),
        "{run:?}"
    );
    assert_eq!(run.principal.unwrap().kind, "agent");
    let fits = ask(A::Run, "alice-w", Some(("agent", "bot")), vec![], 2)
        .await
        .unwrap()
        .into_inner();
    assert!(fits.run_allowed);
    let w = ask(
        A::Write,
        "alice-w",
        Some(("agent", "bot")),
        vec!["src/a", "README"],
        0,
    )
    .await
    .unwrap()
    .into_inner();
    assert!(
        w.decisions[0].allowed && !w.decisions[1].allowed,
        "an agent keeps to its scope"
    );
    // A user's run has no budget.
    assert!(
        ask(A::Run, "bob-r", None, vec![], 999)
            .await
            .unwrap()
            .into_inner()
            .run_allowed
    );
    // Bad requests.
    assert_eq!(
        ask(A::Unspecified, "bob-r", None, vec![], 0)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    let long = "n".repeat(300);
    assert_eq!(
        ask(
            A::Write,
            "alice-w",
            Some(("user", long.as_str())),
            vec![],
            0
        )
        .await
        .unwrap_err()
        .code(),
        tonic::Code::InvalidArgument
    );
    // Without a policy in force nobody can ask about someone else.
    let t = svc().await;
    let req = Req {
        repo: repo_ref(),
        rev: String::new(),
        principal: Some(wire::PrincipalRef {
            kind: "user".into(),
            name: "bob".into(),
        }),
        action: A::Write as i32,
        ..Req::default()
    };
    assert_eq!(
        t.svc
            .evaluate(call(req, "alice-w", false))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    let own = Req {
        repo: repo_ref(),
        action: A::Land as i32,
        ..Req::default()
    };
    let l = t
        .svc
        .evaluate(call(own, "alice-w", false))
        .await
        .unwrap()
        .into_inner()
        .land
        .unwrap();
    assert_eq!((l.approvals, l.signed), (0, false));
}

#[tokio::test]
async fn an_update_never_escapes_an_unusable_policy_through_the_policy_it_brings() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, b"this is not toml ["), ("x", b"1")],
    )
    .await;
    f.set_master(a).await;
    // The replacement policy is valid and switches review off, but the update also writes another
    // file: it is no repair and is judged by nothing it brings.
    let off = commit(
        &f.e,
        &[a],
        &[(POLICY_PATH, policy_text("").as_bytes()), ("evil", b"1")],
    )
    .await;
    f.register(off, 1, 1, ALICE).await;
    f.review(1, 1, 0).await;
    let err = f.land(ALICE, off).await.unwrap_err();
    assert_eq!(code(&err), "POLICY_DENIED");
    assert!(
        paths(&err)[0].contains("cannot be used"),
        "{:?}",
        paths(&err)
    );
    assert_eq!(f.tip().await, a);
    // A force push that rewrites trunk, and a delete of trunk, are refused too.
    let del = f
        .gated
        .update_refs(
            &ctx(ALICE),
            wire::UpdateRefsRequest {
                updates: vec![wire::RefUpdate {
                    name: "refs/heads/master".into(),
                    expect: Some(wire::ref_update::Expect::Any(true)),
                    new: None,
                    ..wire::RefUpdate::default()
                }],
                mode: wire::UpdateMode::Force as i32,
                ..wire::UpdateRefsRequest::default()
            },
        )
        .await;
    assert!(del.is_err());
    // The host service relays calls of users the gate cannot see; it does not repair either.
    let host = admin_ctx(crate::authz::HOST_SERVICE_PRINCIPAL, "host");
    assert_eq!(
        code(&f.land_with(&host, off).await.unwrap_err()),
        "POLICY_DENIED"
    );
    assert_eq!(f.tip().await, a);
}

/// A call of `principal` whose credential `token` is judged by [`admin_tokens`].
fn admin_ctx(principal: &str, token: &str) -> CallCtx {
    let mut c = ctx(principal);
    c.admin = asc_rpc::auth::AdminProof::new(
        admin_tokens(),
        Some(asc_rpc::auth::Credential::bearer(
            asc_rpc::auth::BearerToken::new(token).unwrap(),
        )),
        "o/r",
    );
    c
}

/// Two tokens of one user (a read-only one and an admin one), and the host service.
fn admin_tokens() -> Arc<dyn asc_rpc::auth::Authorizer> {
    use asc_rpc::auth::{Access, StaticTokens};
    Arc::new(
        StaticTokens::new()
            .with_token("dana-read", "pat:dana", Access::Read)
            .with_token("dana-admin", "pat:dana", Access::Admin)
            .with_token("host", crate::authz::HOST_SERVICE_PRINCIPAL, Access::Admin),
    )
}

#[tokio::test]
async fn only_the_admin_credential_of_the_call_may_repair_an_unusable_policy_with_any_update() {
    let f = fx().await;
    let a = commit(
        &f.e,
        &[],
        &[(POLICY_PATH, b"this is not toml ["), ("x", b"1")],
    )
    .await;
    f.set_master(a).await;
    let fix = commit(
        &f.e,
        &[a],
        &[(POLICY_PATH, policy_text("").as_bytes()), ("y", b"2")],
    )
    .await;
    // No proof at all, and a read-only token of the same user, repair nothing.
    for c in [ctx("pat:dana"), admin_ctx("pat:dana", "dana-read")] {
        assert_eq!(
            code(&f.land_with(&c, fix).await.unwrap_err()),
            "POLICY_DENIED"
        );
    }
    assert_eq!(f.tip().await, a);
    // The admin credential of that call does.
    f.land_with(&admin_ctx("pat:dana", "dana-admin"), fix)
        .await
        .unwrap();
    assert_eq!(f.tip().await, fix);
}
