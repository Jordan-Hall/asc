//! The `SecretScan` service: who may call it, what a scan finds, that a finding never holds the
//! value, and how a finding moves between states. The gate that refuses a push is tested with the
//! policy (`policy/tests.rs`); this file is the service on top of the same fixture.

use std::sync::Arc;

use asc_rpc::IdMode;
use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::secret_scan_server::SecretScan as SecretScanApi;
use sha2::{Digest, Sha256};
use tonic::{Code, Request};
use uuid::Uuid;

use super::rpc::{SecretScanHub, SecretScanService};
use crate::ingest::testutil::commit;
use crate::policy::tests::{ALICE, BOB, Fx, fx, policy_text, token};

fn tokens() -> Arc<dyn asc_rpc::auth::Authorizer> {
    Arc::new(
        StaticTokens::new()
            .with_token("admin", ALICE, Access::Admin)
            .with_token("writer", BOB, Access::Write)
            .with_token("reader", "44:carol", Access::Read),
    )
}

fn call<T>(m: T, token: Option<&str>, op: bool) -> Request<T> {
    let mut r = Request::new(m);
    if let Some(t) = token {
        r.extensions_mut()
            .insert(Credential::bearer(BearerToken::new(t).unwrap()));
    }
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
    svc: SecretScanService,
}

async fn svc() -> Svc {
    let f = fx().await;
    let hub = Arc::new(SecretScanHub::new(
        f.reg.clone(),
        f.scanner.clone(),
        f.policies.clone(),
    ));
    let svc = SecretScanService::new(hub, tokens(), ServerInfo::default());
    Svc { f, svc }
}

fn scan_rev(rev: &str) -> wire::ScanSecretsRequest {
    wire::ScanSecretsRequest {
        repo: repo_ref(),
        at: Some(wire::scan_secrets_request::At::Rev(rev.into())),
        paths: vec![],
    }
}

fn list_all() -> wire::ListSecretFindingsRequest {
    wire::ListSecretFindingsRequest {
        repo: repo_ref(),
        ..wire::ListSecretFindingsRequest::default()
    }
}

/// A repository whose trunk holds one file with a github-shaped token (put there without the gate,
/// as a repository that existed before scanning was switched on) and one clean file.
async fn with_a_planted_token() -> (Svc, String) {
    let s = svc().await;
    let t = token();
    let body = format!("name = \"svc\"\ngithub = \"{t}\"\n");
    let c = commit(
        &s.f.e,
        &[],
        &[
            ("conf/app.toml", body.as_bytes()),
            ("src/main.rs", b"fn main() {}\n"),
        ],
    )
    .await;
    s.f.set_master(c).await;
    (s, t)
}

#[tokio::test]
async fn rules_are_listed_for_a_reader_and_the_other_calls_need_admin() {
    let s = svc().await;
    let rules = s
        .svc
        .list_rules(call(
            wire::ListSecretRulesRequest { repo: repo_ref() },
            Some("reader"),
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    let github = rules
        .rules
        .iter()
        .find(|r| r.id == "github-token")
        .expect("the built-in github rule");
    assert!(github.builtin && github.enabled);
    assert_eq!(github.severity, wire::SecretSeverity::Critical as i32);
    assert!(rules.block_on_push, "blocking is the default");
    // No credential: refused. A writer is not an admin.
    let err = s
        .svc
        .list_rules(call(
            wire::ListSecretRulesRequest { repo: repo_ref() },
            None,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
    for who in ["reader", "writer"] {
        let err = s
            .svc
            .scan(call(scan_rev("master"), Some(who), false))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied, "{who} scans");
        let err = s
            .svc
            .list_findings(call(list_all(), Some(who), false))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied, "{who} lists");
        let err = s
            .svc
            .update_finding(call(
                wire::UpdateSecretFindingRequest {
                    repo: repo_ref(),
                    id: 1,
                    state: wire::SecretFindingState::Revoked as i32,
                    note: String::new(),
                },
                Some(who),
                true,
            ))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied, "{who} updates");
    }
    let err = s
        .svc
        .scan(call(scan_rev("master"), None, false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
}

#[tokio::test]
async fn a_scan_finds_the_token_by_place_and_rule_and_nothing_carries_the_value() {
    let (s, t) = with_a_planted_token().await;
    let resp = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.files_scanned, 2);
    assert!(!resp.truncated);
    assert_eq!(resp.findings.len(), 1, "{:?}", resp.findings);
    let f = &resp.findings[0];
    assert_eq!(f.rule_id, "github-token");
    assert_eq!(f.path.as_ref(), b"conf/app.toml");
    assert_eq!(f.line, 2);
    assert_eq!(f.state, wire::SecretFindingState::Open as i32);
    assert!(
        f.preview.len() <= 5,
        "at most two characters and the mask: {:?}",
        f.preview
    );
    assert!(f.preview.ends_with("***"));
    // The fingerprint is a keyed hash: 64 hex characters that are not the plain hash of the value.
    assert_eq!(f.fingerprint.len(), 64);
    assert!(f.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
    let plain: String = Sha256::digest(t.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_ne!(f.fingerprint, plain);
    // The value is in none of the answer, its Debug form or the stored row.
    let rest = &t[4..];
    assert!(!format!("{resp:?}").contains(rest));
    let stored: Vec<(String, String)> =
        sqlx::query_as("SELECT preview, fingerprint FROM secret_findings")
            .fetch_all(s.f.reg.pool())
            .await
            .unwrap();
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].0.contains(rest) && !stored[0].1.contains(rest));
}

#[tokio::test]
async fn scanning_again_keeps_one_finding_and_its_first_sighting() {
    let (s, _) = with_a_planted_token().await;
    let first = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner()
        .findings;
    let second = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner()
        .findings;
    assert_eq!((first.len(), second.len()), (1, 1));
    assert_eq!(first[0].id, second[0].id);
    assert_eq!(first[0].fingerprint, second[0].fingerprint);
    assert_eq!(first[0].first_seen, second[0].first_seen);
    assert_eq!(
        s.f.rows("SELECT COUNT(*) FROM secret_findings").await,
        1,
        "the same secret in the same file is one finding"
    );
}

#[tokio::test]
async fn paths_narrow_a_scan_and_bad_requests_are_refused() {
    let (s, _) = with_a_planted_token().await;
    let mut req = scan_rev("master");
    req.paths = vec!["src".into()];
    let resp = s
        .svc
        .scan(call(req, Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert!(resp.findings.is_empty());
    assert_eq!(resp.files_scanned, 1);
    // Nothing named: refused. An unknown revision: not found. Too many paths: refused.
    let err = s
        .svc
        .scan(call(
            wire::ScanSecretsRequest {
                repo: repo_ref(),
                at: None,
                paths: vec![],
            },
            Some("admin"),
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let err = s
        .svc
        .scan(call(scan_rev("no-such-branch"), Some("admin"), false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    let mut req = scan_rev("master");
    req.paths = (0..65).map(|n| format!("p{n}")).collect();
    let err = s
        .svc
        .scan(call(req, Some("admin"), false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    // A change id is 16 bytes.
    let err = s
        .svc
        .scan(call(
            wire::ScanSecretsRequest {
                repo: repo_ref(),
                at: Some(wire::scan_secrets_request::At::Change(vec![1, 2, 3].into())),
                paths: vec![],
            },
            Some("admin"),
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_range_scans_only_what_it_adds() {
    let s = svc().await;
    let a = commit(&s.f.e, &[], &[("base", b"0")]).await;
    s.f.set_master(a).await;
    let t = token();
    let body = format!("github = \"{t}\"\n");
    let bad = commit(&s.f.e, &[a], &[("conf/app.toml", body.as_bytes())]).await;
    s.f.ingest(bad).await;
    let range = |base: String, head: String| wire::ScanSecretsRequest {
        repo: repo_ref(),
        at: Some(wire::scan_secrets_request::At::Range(
            wire::SecretScanRange { base, head },
        )),
        paths: vec![],
    };
    let found = s
        .svc
        .scan(call(
            range(a.to_string(), bad.to_string()),
            Some("admin"),
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(found.findings.len(), 1);
    assert_eq!(found.findings[0].rule_id, "github-token");
    // The base itself adds nothing.
    let none = s
        .svc
        .scan(call(
            range(a.to_string(), a.to_string()),
            Some("admin"),
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(none.findings.is_empty());
}

#[tokio::test]
async fn a_finding_is_listed_filtered_and_moved_between_states_with_a_note() {
    let (s, _) = with_a_planted_token().await;
    let id = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner()
        .findings[0]
        .id;
    let list = |req: wire::ListSecretFindingsRequest| {
        let svc = &s.svc;
        async move {
            svc.list_findings(call(req, Some("admin"), false))
                .await
                .unwrap()
                .into_inner()
        }
    };
    let all = list(list_all()).await;
    assert_eq!(all.findings.len(), 1);
    assert_eq!(all.counts_by_state.get("open"), Some(&1));
    // Filters: by rule, by state, by severity.
    let mut by_rule = list_all();
    by_rule.rule_ids = vec!["aws-access-key".into()];
    assert!(list(by_rule).await.findings.is_empty());
    let mut by_state = list_all();
    by_state.states = vec![wire::SecretFindingState::Revoked as i32];
    assert!(list(by_state).await.findings.is_empty());
    let mut by_sev = list_all();
    by_sev.min_severity = wire::SecretSeverity::Critical as i32;
    assert_eq!(list(by_sev).await.findings.len(), 1);

    let update = |state: wire::SecretFindingState, note: &str| {
        let svc = &s.svc;
        let note = note.to_owned();
        async move {
            svc.update_finding(call(
                wire::UpdateSecretFindingRequest {
                    repo: repo_ref(),
                    id,
                    state: state as i32,
                    note,
                },
                Some("admin"),
                true,
            ))
            .await
        }
    };
    let done = update(wire::SecretFindingState::FalsePositive, "a test fixture")
        .await
        .unwrap()
        .into_inner()
        .finding
        .unwrap();
    assert_eq!(done.state, wire::SecretFindingState::FalsePositive as i32);
    assert_eq!(done.note, "a test fixture");
    assert_eq!(done.resolved_by, ALICE);
    assert!(done.resolved_at > 0);
    let after = list(list_all()).await;
    assert_eq!(after.counts_by_state.get("false_positive"), Some(&1));
    assert_eq!(after.counts_by_state.get("open").copied().unwrap_or(0), 0);
    // A scan does not reopen a resolved finding.
    let again = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        again.findings[0].state,
        wire::SecretFindingState::FalsePositive as i32
    );
    // Reopening clears who resolved it and when.
    let open = update(wire::SecretFindingState::Open, "")
        .await
        .unwrap()
        .into_inner()
        .finding
        .unwrap();
    assert_eq!(open.state, wire::SecretFindingState::Open as i32);
    assert!(open.resolved_by.is_empty() && open.resolved_at == 0);
    // The change is audited with the rule and the state, never the value.
    let details: Vec<String> = sqlx::query_scalar(
        "SELECT details_json FROM audit_events WHERE action = 'secret.finding.updated' AND scope_kind = 'repo'",
    )
    .fetch_all(s.f.reg.pool())
    .await
    .unwrap();
    assert_eq!(details.len(), 2);
    assert!(details.iter().all(|d| d.contains("github-token")));
    assert!(details.iter().all(|d| !d.contains(&token()[4..])));
}

#[tokio::test]
async fn updates_refuse_bad_input_and_need_an_operation_id() {
    let (s, _) = with_a_planted_token().await;
    let id = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner()
        .findings[0]
        .id;
    let req = |id: u64, state: i32, note: &str| wire::UpdateSecretFindingRequest {
        repo: repo_ref(),
        id,
        state,
        note: note.into(),
    };
    let revoked = wire::SecretFindingState::Revoked as i32;
    // Unknown finding.
    let err = s
        .svc
        .update_finding(call(req(999, revoked, ""), Some("admin"), true))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    // Id zero, no state, a state that does not exist, a long note, a control character.
    for (r, why) in [
        (req(0, revoked, ""), "id zero"),
        (req(id, 0, ""), "no state"),
        (req(id, 99, ""), "unknown state"),
        (req(id, revoked, &"x".repeat(1025)), "long note"),
        (req(id, revoked, "bad\u{7}note"), "control character"),
    ] {
        let err = s
            .svc
            .update_finding(call(r, Some("admin"), true))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument, "{why}");
    }
    // A mutating call without an operation id is refused and changes nothing.
    let err = s
        .svc
        .update_finding(call(req(id, revoked, ""), Some("admin"), false))
        .await
        .unwrap_err();
    assert_ne!(err.code(), Code::Ok);
    let still = s
        .svc
        .list_findings(call(list_all(), Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        still.counts_by_state.get("open"),
        Some(&1),
        "nothing was changed"
    );
    assert_eq!(
        still.findings[0].state,
        wire::SecretFindingState::Open as i32
    );
}

#[tokio::test]
async fn list_filters_are_bounded_and_pages_resume() {
    let s = svc().await;
    let mut many = list_all();
    many.rule_ids = (0..65).map(|n| format!("r{n}")).collect();
    let err = s
        .svc
        .list_findings(call(many, Some("admin"), false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let mut bad_state = list_all();
    bad_state.states = vec![42];
    let err = s
        .svc
        .list_findings(call(bad_state, Some("admin"), false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    let mut bad_sev = list_all();
    bad_sev.min_severity = 42;
    let err = s
        .svc
        .list_findings(call(bad_sev, Some("admin"), false))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    // Three secrets in three files, a page of two, then the rest.
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for n in 0..3u8 {
        let t = format!("ghp_{}{}", "q7Zk3MxR9vTb2LpWd8NcYf4HgJa6SeUo5Ei", n);
        files.push((format!("k{n}.env"), format!("TOKEN={t}\n").into_bytes()));
    }
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let c = commit(&s.f.e, &[], &refs).await;
    s.f.set_master(c).await;
    let found = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(found.findings.len(), 3, "{:?}", found.findings);
    let mut req = list_all();
    req.limit = 2;
    let page = s
        .svc
        .list_findings(call(req.clone(), Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.findings.len(), 2);
    assert!(!page.next_page_token.is_empty());
    req.page_token = page.next_page_token.clone();
    let rest = s
        .svc
        .list_findings(call(req, Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(rest.findings.len(), 1);
    assert!(rest.next_page_token.is_empty());
    let mut ids: Vec<u64> = page
        .findings
        .iter()
        .chain(&rest.findings)
        .map(|f| f.id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "no finding twice, none missed");
}

#[tokio::test]
async fn the_policy_in_force_shapes_the_rules_and_allowed_paths() {
    let (s, _) = with_a_planted_token().await;
    // A trunk policy with a custom rule and an allowed path.
    let p = policy_text(
        "[secrets]\nallow_paths = [\"conf/**\"]\n[secrets.rules.acme]\npattern = \"ACME-[0-9A-F]{16}\"\nseverity = \"high\"\n",
    );
    let tip = s.f.tip().await;
    let c = commit(&s.f.e, &[tip], &[(".asc/policy.toml", p.as_bytes())]).await;
    s.f.set_master(c).await;
    let rules = s
        .svc
        .list_rules(call(
            wire::ListSecretRulesRequest { repo: repo_ref() },
            Some("reader"),
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        rules
            .rules
            .iter()
            .any(|r| r.id == "custom.acme" && !r.builtin)
    );
    assert_eq!(rules.allow_paths, ["conf/**"]);
    // The allowed path is not reported by a scan.
    let resp = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert!(resp.findings.is_empty(), "{:?}", resp.findings);
    // An unreadable policy never switches scanning off: the safe defaults apply, so the allowed path is
    // not allowed any more and the token is reported again.
    let bad = commit(
        &s.f.e,
        &[c],
        &[(
            ".asc/policy.toml",
            b"version = 1
[secrets
",
        )],
    )
    .await;
    s.f.set_master(bad).await;
    let resp = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.findings.len(), 1, "{:?}", resp.findings);
}

#[tokio::test]
async fn a_tree_walk_cut_short_says_the_scan_was_truncated() {
    let mut s = svc().await;
    let scanner = Arc::new(super::Scanner::new(
        s.f.reg.clone(),
        super::Config {
            max_files: 1,
            ..super::Config::default()
        },
    ));
    let hub = Arc::new(SecretScanHub::new(
        s.f.reg.clone(),
        scanner,
        s.f.policies.clone(),
    ));
    s.svc = SecretScanService::new(hub, tokens(), ServerInfo::default());
    let names: Vec<String> = (0..6).map(|i| format!("f{i}.txt")).collect();
    let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x
"[..])).collect();
    let c = commit(&s.f.e, &[], &files).await;
    s.f.set_master(c).await;
    let resp = s
        .svc
        .scan(call(scan_rev("master"), Some("admin"), false))
        .await
        .unwrap()
        .into_inner();
    assert!(resp.truncated, "a scan that left files unvisited must say so");
}
