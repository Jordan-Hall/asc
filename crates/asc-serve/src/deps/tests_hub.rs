//! The service end to end: a repository, a mock OSV and registry, the hub, the gate and the RPC layer.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use asc_core::id::ChangesetId;
use asc_core::policy::DepSeverity;
use asc_rpc::IdMode;
use asc_rpc::auth::{Access, BearerToken, Credential, StaticTokens};
use asc_rpc::server::{CallCtx, RefsHandler, ServerInfo, wire};
use asc_rpc::services::deps_service_server::DepsService as DepsApi;
use tonic::{Code, Request};
use uuid::Uuid;

use super::REF_PREFIX;
use super::gate::DepsGateSlot;
use super::hub::{DepsHub, hex};
use super::model::Ecosystem;
use super::rpc::DepsRpc;
use super::tests::{Mock, clock, config, egress_for};
use crate::ingest::testutil::commit;
use crate::mutations::ServerMutations;
use crate::policy::gate::ServerGate;
use crate::policy::tests::{ALICE, BOB, Fx, code, ctx, fx, paths, policy_text, update_req};
use crate::read::ReadHooks;
use crate::update::{PipelineHooks, ServerRefs};

const T0: i64 = 1_800_000_000;
const SUM_OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SUM_NEW: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

struct H {
    f: Fx,
    hub: Arc<DepsHub>,
    mock: Mock,
    clock: Arc<AtomicI64>,
}

fn build(
    f: &Fx,
    mock: &Mock,
    server_url: Option<&str>,
) -> (Arc<DepsHub>, Arc<AtomicI64>, Arc<ServerMutations>) {
    let c = config(mock);
    let (clk, t) = clock(T0);
    let mutations = Arc::new(ServerMutations::new(
        f.reg.clone(),
        f.gated.hooks.clone(),
        Arc::new(ReadHooks::default()),
    ));
    let hub = DepsHub::new(
        f.reg.clone(),
        mutations.clone(),
        egress_for(&c),
        c,
        server_url.map(str::to_owned),
        clk,
    );
    (hub, t, mutations)
}

async fn setup() -> H {
    let f = fx().await;
    let mock = Mock::start().await;
    let (hub, clock, _) = build(&f, &mock, Some("https://asc.example.com"));
    H {
        f,
        hub,
        mock,
        clock,
    }
}

fn repo() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: "o/r".into(),
        id_mode: IdMode::Asc.to_wire(),
    })
}

/// A Cargo.lock whose workspace member depends on every package, with a checksum each.
fn lock(pkgs: &[(&str, &str)]) -> String {
    let mut s = String::from(
        "version = 3\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n",
    );
    for (n, _) in pkgs {
        s.push_str(&format!(" \"{n}\",\n"));
    }
    s.push_str("]\n");
    for (n, v) in pkgs {
        s.push_str(&format!(
            "\n[[package]]\nname = \"{n}\"\nversion = \"{v}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{SUM_OLD}\"\n"
        ));
    }
    s
}

fn manifest(pkgs: &[(&str, &str)]) -> String {
    let mut s = String::from("[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\n");
    for (n, v) in pkgs {
        s.push_str(&format!("{n} = \"{v}\"\n"));
    }
    s
}

fn crates_route(m: &Mock, name: &str, versions: &[(&str, &str)]) {
    let list: Vec<String> = versions
        .iter()
        .map(|(v, c)| format!(r#"{{"num":"{v}","yanked":false,"checksum":"{c}"}}"#))
        .collect();
    m.route(
        &format!("/api/v1/crates/{name}"),
        200,
        &format!(r#"{{"versions":[{}]}}"#, list.join(",")),
    );
}

impl H {
    /// Trunk with these files.
    async fn trunk(&self, files: &[(&str, &[u8])]) -> ChangesetId {
        let parents: Vec<ChangesetId> = self
            .hub
            .trunk(&self.f.e)
            .await
            .unwrap()
            .into_iter()
            .collect();
        let cs = commit(&self.f.e, &parents, files).await;
        self.f.set_master(cs).await;
        cs
    }

    async fn scan(&self, rev: &str, refresh: bool) -> wire::ScanDepsResponse {
        self.hub
            .scan(
                &ctx(ALICE),
                wire::ScanDepsRequest {
                    repo: repo(),
                    rev: rev.into(),
                    ecosystems: vec![],
                    refresh,
                },
            )
            .await
            .unwrap()
    }

    async fn findings(&self, states: &[wire::DepFindingState]) -> wire::ListDepFindingsResponse {
        self.hub
            .list_findings(
                &ctx(BOB),
                wire::ListDepFindingsRequest {
                    repo: repo(),
                    states: states.iter().map(|s| *s as i32).collect(),
                    ..wire::ListDepFindingsRequest::default()
                },
            )
            .await
            .unwrap()
    }

    async fn read_at(&self, cs: &[u8], path: &str) -> Option<String> {
        let id = crate::agents::repo::changeset_id(cs).unwrap();
        let root = self.hub.view.root_of(&self.f.e, id).await.unwrap();
        self.hub
            .view
            .read(&self.f.e, root, path, 1 << 20)
            .await
            .unwrap()
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
    }
}

fn state_of(f: &wire::DepFinding) -> wire::DepFindingState {
    wire::DepFindingState::try_from(f.state).unwrap()
}

// ------------------------------------------------------------------------------ scan

#[tokio::test]
async fn a_scan_reports_what_it_read_and_what_it_found_and_records_it() {
    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    let a = h
        .trunk(&[
            ("Cargo.toml", manifest(&[("serde", "1.0.100")]).as_bytes()),
            (
                "Cargo.lock",
                lock(&[("serde", "1.0.100"), ("regex", "1.5.0")]).as_bytes(),
            ),
            (
                "node_modules/x/package.json",
                b"{\"dependencies\":{\"y\":\"1.0.0\"}}",
            ),
            ("README.md", b"hi"),
        ])
        .await;
    let r = h.scan("", false).await;
    let names: Vec<_> = r
        .files
        .iter()
        .map(|f| String::from_utf8_lossy(&f.path).into_owned())
        .collect();
    assert_eq!(
        names,
        ["Cargo.lock", "Cargo.toml"],
        "node_modules is not entered"
    );
    assert_eq!(r.files[0].format, "Cargo.lock");
    assert_eq!(r.files[0].ecosystem, "crates.io");
    assert_eq!(r.files[0].packages, 2);
    assert_eq!(r.files[0].direct, 2);
    assert_eq!(
        r.packages, 2,
        "the manifest's requirement is the lockfile's package"
    );
    assert_eq!(r.findings.len(), 1);
    let f = &r.findings[0];
    assert!(f.id > 0 && f.direct);
    assert_eq!(
        (f.ecosystem.as_str(), f.name.as_str(), f.version.as_str()),
        ("crates.io", "serde", "1.0.100")
    );
    assert_eq!(&f.path[..], b"Cargo.lock");
    assert_eq!(f.fixed_in, "1.0.200");
    let adv = f.advisory.as_ref().unwrap();
    assert_eq!(
        (adv.id.as_str(), adv.severity),
        ("RUSTSEC-1", wire::DepSeverity::High as i32)
    );
    assert_eq!(state_of(f), wire::DepFindingState::Open);
    assert_eq!(r.lookup, wire::DepLookupStatus::Complete as i32);
    assert_eq!((r.unknown_packages, r.truncated), (0, false));
    assert_eq!(r.source, "osv-mirror");
    assert_eq!(&r.scanned[..], a.as_bytes());
    // Recorded: the same finding keeps its id and a later sighting.
    h.clock.fetch_add(60, Ordering::SeqCst);
    let again = h.scan("", false).await;
    assert_eq!(again.findings[0].id, f.id);
    assert_eq!(again.findings[0].first_seen, f.first_seen);
    assert_eq!(again.findings[0].last_seen, f.last_seen + 60);
    let listed = h.findings(&[]).await;
    assert_eq!(listed.findings.len(), 1);
    assert_eq!(listed.counts_by_state["open"], 1);
    assert_eq!(&listed.last_scanned[..], a.as_bytes());
    assert_eq!(listed.last_lookup, wire::DepLookupStatus::Complete as i32);
}

#[tokio::test]
async fn findings_are_listed_by_filter_and_in_pages() {
    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    h.mock.vuln(
        Ecosystem::CratesIo,
        "regex",
        "1.5.0",
        "RUSTSEC-2",
        "1.5.1",
        "LOW",
    );
    h.trunk(&[(
        "Cargo.lock",
        lock(&[("serde", "1.0.100"), ("regex", "1.5.0")]).as_bytes(),
    )])
    .await;
    let r = h.scan("", false).await;
    assert_eq!(r.findings.len(), 2);
    assert_eq!(r.findings[0].name, "serde", "worst first");
    let list = |req: wire::ListDepFindingsRequest| {
        let h = &h;
        async move {
            h.hub
                .list_findings(
                    &ctx(BOB),
                    wire::ListDepFindingsRequest {
                        repo: repo(),
                        ..req
                    },
                )
                .await
                .unwrap()
        }
    };
    let page1 = list(wire::ListDepFindingsRequest {
        limit: 1,
        ..Default::default()
    })
    .await;
    assert_eq!(page1.findings.len(), 1);
    assert!(!page1.next_page_token.is_empty());
    let page2 = list(wire::ListDepFindingsRequest {
        limit: 1,
        page_token: page1.next_page_token.clone(),
        ..Default::default()
    })
    .await;
    assert_eq!(page2.findings.len(), 1);
    assert!(page2.next_page_token.is_empty());
    assert_ne!(page1.findings[0].id, page2.findings[0].id);
    let high = list(wire::ListDepFindingsRequest {
        min_severity: wire::DepSeverity::High as i32,
        ..Default::default()
    })
    .await;
    assert_eq!(high.findings.len(), 1);
    assert_eq!(high.findings[0].name, "serde");
    let by_name = list(wire::ListDepFindingsRequest {
        name: "regex".into(),
        ..Default::default()
    })
    .await;
    assert_eq!(by_name.findings.len(), 1);
    let by_eco = list(wire::ListDepFindingsRequest {
        ecosystems: vec!["cargo".into()],
        ..Default::default()
    })
    .await;
    assert_eq!(by_eco.findings.len(), 2);
    let other = list(wire::ListDepFindingsRequest {
        ecosystems: vec!["npm".into()],
        ..Default::default()
    })
    .await;
    assert!(other.findings.is_empty());
    let fixed = h.findings(&[wire::DepFindingState::Fixed]).await;
    assert!(fixed.findings.is_empty());
    let bad = h
        .hub
        .list_findings(
            &ctx(BOB),
            wire::ListDepFindingsRequest {
                repo: repo(),
                ecosystems: vec!["cobol".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
    let bad = h
        .hub
        .list_findings(
            &ctx(BOB),
            wire::ListDepFindingsRequest {
                repo: repo(),
                page_token: "x".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn only_trunk_is_recorded_and_a_finding_is_fixed_when_trunk_no_longer_has_it() {
    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    let a = h
        .trunk(&[("Cargo.lock", lock(&[("serde", "1.0.100")]).as_bytes())])
        .await;
    // A revision that is not trunk is reported and not recorded.
    let w = commit(
        &h.f.e,
        &[a],
        &[(
            "Cargo.lock",
            lock(&[("serde", "1.0.100"), ("regex", "1.5.0")]).as_bytes(),
        )],
    )
    .await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "regex",
        "1.5.0",
        "RUSTSEC-9",
        "1.5.1",
        "HIGH",
    );
    h.f.ingest(w).await;
    let r = h.scan(&hex(w.as_bytes()), false).await;
    assert_eq!(r.findings.len(), 2);
    assert!(r.findings.iter().all(|f| f.id == 0));
    assert_eq!(&r.scanned[..], w.as_bytes());
    assert!(
        h.findings(&[]).await.findings.is_empty(),
        "nothing was recorded"
    );
    // Trunk scans record; trunk moves to a fixed version; the finding is fixed.
    let r = h.scan("", false).await;
    assert_eq!(r.findings.len(), 1);
    h.trunk(&[("Cargo.lock", lock(&[("serde", "1.0.200")]).as_bytes())])
        .await;
    let r = h.scan("", false).await;
    assert!(r.findings.is_empty());
    let listed = h.findings(&[]).await;
    assert_eq!(listed.counts_by_state["fixed"], 1);
    assert_eq!(listed.counts_by_state["open"], 0);
    assert_eq!(state_of(&listed.findings[0]), wire::DepFindingState::Fixed);
}

#[tokio::test]
async fn a_failed_lookup_is_reported_as_unknown_and_closes_no_finding() {
    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    h.trunk(&[("Cargo.lock", lock(&[("serde", "1.0.100")]).as_bytes())])
        .await;
    let first = h.scan("", false).await;
    assert_eq!(first.findings.len(), 1);
    h.mock.with(|s| s.fail_osv = true);
    h.clock.fetch_add(7 * 3600, Ordering::SeqCst);
    let r = h.scan("", false).await;
    assert_eq!(r.lookup, wire::DepLookupStatus::Failed as i32);
    assert_eq!(r.unknown_packages, 1);
    assert!(
        r.findings.is_empty(),
        "not reported clean either: the status says what it is"
    );
    assert!(!r.lookup_errors.is_empty());
    let listed = h.findings(&[]).await;
    assert_eq!(
        listed.counts_by_state["open"], 1,
        "the earlier finding is still open"
    );
    assert_eq!(listed.counts_by_state["fixed"], 0);
    assert_eq!(listed.last_lookup, wire::DepLookupStatus::Failed as i32);
    // CocoaPods has no advisory data: counted as unknown, but the lookup did not fail.
    h.mock.with(|s| s.fail_osv = false);
    h.trunk(&[(
        "Podfile.lock",
        b"PODS:\n  - Alamofire (5.6.1)\n\nDEPENDENCIES:\n  - Alamofire\n",
    )])
    .await;
    let r = h.scan("", false).await;
    assert_eq!(r.unknown_packages, 1);
    assert_eq!(r.lookup, wire::DepLookupStatus::Complete as i32);
}

#[tokio::test]
async fn a_finding_is_dismissed_by_a_person_and_stays_dismissed() {
    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    h.trunk(&[("Cargo.lock", lock(&[("serde", "1.0.100")]).as_bytes())])
        .await;
    let id = h.scan("", false).await.findings[0].id;
    let dismiss = |reason: wire::DepDismissReason, id: u64, note: &str| {
        let h = &h;
        let note = note.to_owned();
        async move {
            h.hub
                .dismiss(
                    &ctx(ALICE),
                    wire::DismissDepFindingRequest {
                        repo: repo(),
                        id,
                        reason: reason as i32,
                        note,
                    },
                )
                .await
        }
    };
    let f = dismiss(wire::DepDismissReason::NotUsed, id, "dead code")
        .await
        .unwrap()
        .finding
        .unwrap();
    assert_eq!(state_of(&f), wire::DepFindingState::Dismissed);
    assert_eq!(
        (f.dismissed_by.as_str(), f.note.as_str()),
        (ALICE, "dead code")
    );
    assert_eq!(f.dismiss_reason, wire::DepDismissReason::NotUsed as i32);
    assert!(f.dismissed_at > 0);
    let again = h.scan("", false).await;
    assert_eq!(again.findings.len(), 1);
    assert_eq!(
        state_of(&again.findings[0]),
        wire::DepFindingState::Dismissed,
        "a rescan does not reopen it"
    );
    assert!(
        h.findings(&[wire::DepFindingState::Open])
            .await
            .findings
            .is_empty()
    );
    assert_eq!(h.findings(&[]).await.counts_by_state["dismissed"], 1);
    let f = dismiss(wire::DepDismissReason::Unspecified, id, "")
        .await
        .unwrap()
        .finding
        .unwrap();
    assert_eq!(state_of(&f), wire::DepFindingState::Open);
    assert!(f.dismissed_by.is_empty());
    let e = dismiss(wire::DepDismissReason::NotUsed, id, &"n".repeat(1025))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let e = dismiss(wire::DepDismissReason::NotUsed, 999, "")
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
    let e = dismiss(wire::DepDismissReason::NotUsed, 0, "")
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn the_repository_settings_on_trunk_decide_what_is_reported() {
    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    h.mock.vuln(
        Ecosystem::Npm,
        "left-pad",
        "1.0.0",
        "GHSA-np",
        "1.3.0",
        "HIGH",
    );
    h.trunk(&[
        ("Cargo.lock", lock(&[("serde", "1.0.100")]).as_bytes()),
        ("package-lock.json", b"{\"lockfileVersion\":3,\"packages\":{\"node_modules/left-pad\":{\"version\":\"1.0.0\"}}}"),
        (
            ".asc/deps.toml",
            b"version = 1\necosystems = [\"npm\"]\n[[ignore]]\nname = \"left-pad\"\nadvisories = [\"GHSA-np\"]\n",
        ),
    ])
    .await;
    // The file restricts to npm and ignores the npm advisory: nothing is reported.
    let r = h.scan("", false).await;
    assert!(r.findings.is_empty());
    assert_eq!(r.files.len(), 1);
    // A request can ask for another ecosystem.
    let r = h
        .hub
        .scan(
            &ctx(ALICE),
            wire::ScanDepsRequest {
                repo: repo(),
                ecosystems: vec!["cargo".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.findings.len(), 1);
    // A change to the settings in a revision under review does not change what that scan reports.
    let a = h.hub.trunk(&h.f.e).await.unwrap().unwrap();
    let w = commit(&h.f.e, &[a], &[(".asc/deps.toml", b"version = 1\n")]).await;
    h.f.ingest(w).await;
    let r = h
        .hub
        .scan(
            &ctx(ALICE),
            wire::ScanDepsRequest {
                repo: repo(),
                rev: hex(w.as_bytes()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(r.findings.is_empty(), "trunk's settings bind the scan of w");
    // An unusable settings file is reported by GetConfig; the scan uses the defaults.
    h.trunk(&[(".asc/deps.toml", b"version = 9\n")]).await;
    let got = h
        .hub
        .get_config(
            &ctx(BOB),
            wire::GetDepsConfigRequest {
                repo: repo(),
                rev: String::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(got.source, "default");
    assert!(got.error.contains("version"), "{}", got.error);
    assert_eq!(got.text, "version = 9\n");
    assert_eq!(got.supported.len(), 12);
}

// ------------------------------------------------------------------------------ updates

async fn cargo_world(h: &H) -> ChangesetId {
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    crates_route(
        &h.mock,
        "serde",
        &[
            ("1.0.100", SUM_OLD),
            ("1.0.200", SUM_NEW),
            ("2.0.0", SUM_NEW),
        ],
    );
    crates_route(&h.mock, "regex", &[("1.5.0", SUM_OLD), ("1.5.1", SUM_NEW)]);
    h.trunk(&[
        (
            "Cargo.toml",
            manifest(&[("serde", "1.0.100"), ("regex", "1.5.0")]).as_bytes(),
        ),
        (
            "Cargo.lock",
            lock(&[("serde", "1.0.100"), ("regex", "1.5.0")]).as_bytes(),
        ),
    ])
    .await
}

#[tokio::test]
async fn updates_name_the_version_each_package_moves_to_and_what_that_fixes() {
    let h = setup().await;
    cargo_world(&h).await;
    let list = |security: bool, ignored: bool| {
        let h = &h;
        async move {
            h.hub
                .list_updates(
                    &ctx(BOB),
                    wire::ListDepUpdatesRequest {
                        repo: repo(),
                        security_only: security,
                        include_ignored: ignored,
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
        }
    };
    let all = list(false, false).await;
    assert_eq!(all.packages, 2);
    assert_eq!(all.unknown_packages, 0);
    let by: Vec<_> = all
        .updates
        .iter()
        .map(|u| {
            (
                u.name.as_str(),
                u.current.as_str(),
                u.latest.as_str(),
                u.target.as_str(),
            )
        })
        .collect();
    assert_eq!(
        by,
        [
            ("regex", "1.5.0", "1.5.1", "1.5.1"),
            ("serde", "1.0.100", "2.0.0", "1.0.200")
        ]
    );
    let serde = &all.updates[1];
    assert_eq!(
        serde.kind,
        wire::DepUpdateKind::Major as i32,
        "what is available"
    );
    assert_eq!(serde.fixes, ["RUSTSEC-1"]);
    assert!(serde.unfixed.is_empty());
    assert_eq!(serde.group, "cargo-serde");
    assert!(serde.direct && !serde.ignored);
    let sec = list(true, false).await;
    assert_eq!(sec.updates.len(), 1);
    assert_eq!(sec.updates[0].name, "serde");
    // A registry that cannot be asked is unknown, with its reason.
    h.mock.route("/api/v1/crates/regex", 500, "");
    h.clock.fetch_add(3601, Ordering::SeqCst);
    let r = list(false, false).await;
    assert_eq!(r.unknown_packages, 1);
    assert!(
        r.lookup_errors
            .iter()
            .any(|e| e.contains("crates.io registry")),
        "{:?}",
        r.lookup_errors
    );
    let regex = r.updates.iter().find(|u| u.name == "regex").unwrap();
    assert!(regex.error.contains("500") && regex.target.is_empty());
}

#[tokio::test]
async fn a_proposal_is_a_change_on_an_agent_deps_ref_and_never_trunk() {
    let h = setup().await;
    let trunk = cargo_world(&h).await;
    let propose = |req: wire::ProposeDepUpdateRequest| {
        let h = &h;
        async move {
            h.hub
                .propose_update(
                    &ctx(ALICE),
                    wire::ProposeDepUpdateRequest {
                        repo: repo(),
                        ..req
                    },
                )
                .await
        }
    };
    // Dry run: the plan, nothing recorded.
    let dry = propose(wire::ProposeDepUpdateRequest {
        dry_run: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(dry.proposals.len(), 2);
    assert!(
        dry.proposals
            .iter()
            .all(|p| p.changeset.is_empty() && p.r#ref.starts_with(REF_PREFIX))
    );
    assert_eq!(&dry.base[..], trunk.as_bytes());
    // Real.
    let r = propose(Default::default()).await.unwrap();
    assert_eq!(r.proposals.len(), 2);
    assert_eq!(h.f.tip().await, trunk, "trunk did not move");
    let serde = r
        .proposals
        .iter()
        .find(|p| p.group == "cargo-serde")
        .unwrap();
    assert!(
        serde
            .r#ref
            .starts_with("refs/heads/agent/deps/cargo-serde-"),
        "{}",
        serde.r#ref
    );
    assert_eq!(
        serde.r#ref.len(),
        "refs/heads/agent/deps/cargo-serde-".len() + 8
    );
    assert!(!serde.changeset.is_empty() && !serde.change.is_empty());
    assert_eq!(serde.updates[0].fixes, ["RUSTSEC-1"]);
    assert_eq!(serde.files, [bytes::Bytes::from_static(b"Cargo.lock")]);
    assert!(
        serde.message.contains("fixes RUSTSEC-1") && serde.message.contains("1.0.100 -> 1.0.200")
    );
    assert!(
        serde
            .notes
            .iter()
            .any(|n| n.contains("cargo update -p serde")),
        "{:?}",
        serde.notes
    );
    let lock_text = h.read_at(&serde.changeset, "Cargo.lock").await.unwrap();
    assert!(lock_text.contains("version = \"1.0.200\""));
    assert!(lock_text.contains(&format!("checksum = \"{SUM_NEW}\"")));
    assert!(
        lock_text.contains("version = \"1.5.0\""),
        "regex is not part of this change"
    );
    let manifest_text = h.read_at(&serde.changeset, "Cargo.toml").await.unwrap();
    assert_eq!(
        manifest_text,
        manifest(&[("serde", "1.0.100"), ("regex", "1.5.0")]),
        "the requirement admits the fix"
    );
    // Asked again: the same moves are already proposed.
    let again = propose(Default::default()).await.unwrap();
    assert!(again.proposals.is_empty(), "{:?}", again.proposals);
    assert_eq!(again.skipped.len(), 2);
    assert!(
        again
            .skipped
            .iter()
            .all(|s| s.reason.contains("already proposed")),
        "{:?}",
        again.skipped
    );
}

#[tokio::test]
async fn a_proposal_can_be_narrowed_limited_and_held_back_by_the_settings() {
    let h = setup().await;
    cargo_world(&h).await;
    let propose = |req: wire::ProposeDepUpdateRequest| {
        let h = &h;
        async move {
            h.hub
                .propose_update(
                    &ctx(ALICE),
                    wire::ProposeDepUpdateRequest {
                        repo: repo(),
                        dry_run: true,
                        ..req
                    },
                )
                .await
        }
    };
    let sec = propose(wire::ProposeDepUpdateRequest {
        security_only: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(sec.proposals.len(), 1);
    assert_eq!(sec.proposals[0].group, "cargo-serde");
    let sel = propose(wire::ProposeDepUpdateRequest {
        selectors: vec![wire::DepSelector {
            ecosystem: "cargo".into(),
            name: "regex".into(),
        }],
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(sel.proposals.len(), 1);
    assert_eq!(sel.proposals[0].group, "cargo-regex");
    let bad = propose(wire::ProposeDepUpdateRequest {
        selectors: vec![wire::DepSelector {
            ecosystem: "cobol".into(),
            name: "x".into(),
        }],
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
    let one = propose(wire::ProposeDepUpdateRequest {
        max_groups: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(one.proposals.len(), 1);
    assert!(
        one.skipped
            .iter()
            .any(|s| s.reason.contains("over the limit")),
        "{:?}",
        one.skipped
    );
    // The limit on the size of a step: a major update is not proposed unless the limit allows it.
    let major = propose(wire::ProposeDepUpdateRequest {
        selectors: vec![wire::DepSelector {
            ecosystem: "cargo".into(),
            name: "serde".into(),
        }],
        max_kind: wire::DepUpdateKind::Major as i32,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(major.proposals[0].updates[0].target, "2.0.0");
    // Settings on trunk: grouping, an ignore, and a limit.
    let a = h.hub.trunk(&h.f.e).await.unwrap().unwrap();
    let _ = a;
    h.trunk(&[(
        ".asc/deps.toml",
        b"version = 1\ngrouping = \"ecosystem\"\n[[ignore]]\nname = \"regex\"\nreason = \"pinned\"\n",
    )])
    .await;
    let r = propose(Default::default()).await.unwrap();
    assert_eq!(r.proposals.len(), 1);
    assert_eq!(r.proposals[0].group, "cargo");
    assert_eq!(r.proposals[0].updates.len(), 1);
    assert!(
        r.skipped
            .iter()
            .any(|s| s.name == "regex" && s.reason.contains("ignored")),
        "{:?}",
        r.skipped
    );
    // Settings that cannot be read refuse a proposal rather than guess.
    h.trunk(&[(".asc/deps.toml", b"version = 1\nbogus = true\n")])
        .await;
    let e = propose(Default::default()).await.unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
    assert!(e.message().contains("bogus"), "{}", e.message());
}

#[tokio::test]
async fn a_requirement_that_does_not_admit_the_new_version_is_rewritten_with_the_lock() {
    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "0.9.0",
        "RUSTSEC-7",
        "1.0.0",
        "CRITICAL",
    );
    crates_route(&h.mock, "serde", &[("0.9.0", SUM_OLD), ("1.0.0", SUM_NEW)]);
    h.trunk(&[
        ("Cargo.toml", manifest(&[("serde", "0.9")]).as_bytes()),
        ("Cargo.lock", lock(&[("serde", "0.9.0")]).as_bytes()),
    ])
    .await;
    let r = h
        .hub
        .propose_update(
            &ctx(ALICE),
            wire::ProposeDepUpdateRequest {
                repo: repo(),
                max_kind: wire::DepUpdateKind::Major as i32,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.proposals.len(), 1, "{:?}", r.skipped);
    let p = &r.proposals[0];
    assert_eq!(p.files.len(), 2);
    assert_eq!(
        h.read_at(&p.changeset, "Cargo.toml").await.unwrap(),
        manifest(&[("serde", "1.0")])
    );
    assert!(
        h.read_at(&p.changeset, "Cargo.lock")
            .await
            .unwrap()
            .contains("version = \"1.0.0\"")
    );
    // With the limit at minor the update is held back, with the reason.
    let held = h
        .hub
        .propose_update(
            &ctx(ALICE),
            wire::ProposeDepUpdateRequest {
                repo: repo(),
                dry_run: true,
                security_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(held.proposals.is_empty());
    assert!(
        held.skipped[0].reason.contains("major") && held.skipped[0].reason.contains("minor"),
        "{:?}",
        held.skipped
    );
}

// ------------------------------------------------------------------------------ settings

#[tokio::test]
async fn settings_are_proposed_with_their_workflow_and_never_written_to_trunk() {
    let h = setup().await;
    let a = h.trunk(&[("README.md", b"hi")]).await;
    let got = h
        .hub
        .get_config(
            &ctx(BOB),
            wire::GetDepsConfigRequest {
                repo: repo(),
                rev: String::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(got.source, "default");
    assert!(got.workflow.is_empty() && got.error.is_empty());
    let cfg = wire::DepsConfig {
        version: 1,
        ecosystems: vec!["cargo".into()],
        schedule: "0 4 * * 1".into(),
        max_proposals: 3,
        ..Default::default()
    };
    let set = |c: wire::DepsConfig, dry: bool| {
        let h = &h;
        async move {
            h.hub
                .set_config(
                    &ctx(ALICE),
                    wire::SetDepsConfigRequest {
                        repo: repo(),
                        config: Some(c),
                        dry_run: dry,
                    },
                )
                .await
        }
    };
    let dry = set(cfg.clone(), true).await.unwrap();
    assert!(dry.text.contains("schedule = \"0 4 * * 1\"") && !dry.workflow.is_empty());
    assert!(dry.changeset.is_empty() && dry.r#ref.is_empty());
    let done = set(cfg.clone(), false).await.unwrap();
    assert!(
        done.r#ref.starts_with("refs/heads/agent/deps/config-"),
        "{}",
        done.r#ref
    );
    assert!(!done.changeset.is_empty());
    assert_eq!(h.f.tip().await, a, "trunk did not move");
    assert_eq!(
        h.read_at(&done.changeset, ".asc/deps.toml").await.unwrap(),
        done.text
    );
    let flow = h
        .read_at(&done.changeset, ".asc/workflows/deps.toml")
        .await
        .unwrap();
    assert_eq!(flow, done.workflow);
    let wf = asc_core::workflow::Workflow::parse("deps", flow.as_bytes()).unwrap();
    assert_eq!(
        wf.steps[0].command[..5],
        ["asc", "deps", "propose", "--remote", "https://asc.example.com/o/r"]
    );
    assert!(flow.contains("https://asc.example.com"));
    // The same settings again: already proposed, nothing new.
    let twice = set(cfg.clone(), false).await.unwrap();
    assert!(twice.changeset.is_empty());
    // Landed (by a person), they are the settings in force; removing the schedule removes the workflow.
    let c = commit(
        &h.f.e,
        &[a],
        &[
            (".asc/deps.toml", done.text.as_bytes()),
            (".asc/workflows/deps.toml", done.workflow.as_bytes()),
        ],
    )
    .await;
    h.f.set_master(c).await;
    let got = h
        .hub
        .get_config(
            &ctx(BOB),
            wire::GetDepsConfigRequest {
                repo: repo(),
                rev: String::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(got.source, "file");
    assert_eq!(got.config.as_ref().unwrap().schedule, "0 4 * * 1");
    assert_eq!(got.workflow, done.workflow);
    let none = set(
        wire::DepsConfig {
            version: 1,
            ..Default::default()
        },
        false,
    )
    .await
    .unwrap();
    assert!(none.workflow.is_empty());
    assert!(
        h.read_at(&none.changeset, ".asc/workflows/deps.toml")
            .await
            .is_none(),
        "the generated workflow is removed"
    );
    assert!(h.read_at(&none.changeset, ".asc/deps.toml").await.is_some());
    // Bad settings and a missing public URL.
    let bad = set(
        wire::DepsConfig {
            max_proposals: 99,
            ..cfg.clone()
        },
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);
    let nourl = {
        let (hub, _, _) = build(&h.f, &h.mock, None);
        hub.set_config(
            &ctx(ALICE),
            wire::SetDepsConfigRequest {
                repo: repo(),
                config: Some(cfg),
                dry_run: false,
            },
        )
        .await
        .unwrap_err()
    };
    assert_eq!(nourl.code(), Code::FailedPrecondition);
    assert!(nourl.message().contains("public_url"));
    // A workflow file the operator wrote is not removed.
    let own = commit(
        &h.f.e,
        &[c],
        &[(".asc/workflows/deps.toml", b"description = \"mine\"\n")],
    )
    .await;
    h.f.set_master(own).await;
    let kept = set(
        wire::DepsConfig {
            version: 1,
            ecosystems: vec!["npm".into()],
            ..Default::default()
        },
        false,
    )
    .await
    .unwrap();
    assert!(
        h.read_at(&kept.changeset, ".asc/workflows/deps.toml")
            .await
            .unwrap()
            .contains("mine")
    );
}

// ------------------------------------------------------------------------------ the landing gate

struct G {
    h: H,
    refs: ServerRefs,
}

/// A gate that asks the hub (when `wired`), over the same repository.
async fn gated(wired: bool) -> G {
    let h = setup().await;
    let slot = Arc::new(DepsGateSlot::default());
    if wired {
        let check: Arc<dyn super::gate::DepsCheck> = h.hub.clone();
        assert!(slot.set(check));
    }
    let gate = Arc::new(
        ServerGate::new(
            h.f.reg.clone(),
            h.f.policies.clone(),
            h.f.scanner.clone(),
            h.f.signing.clone(),
        )
        .with_deps(slot),
    );
    let hooks = Arc::new(PipelineHooks::default());
    hooks.set_gate(gate).ok().unwrap();
    let refs = ServerRefs::new(h.f.reg.clone(), hooks);
    G { h, refs }
}

impl G {
    async fn land(&self, to: ChangesetId) -> Result<wire::UpdateRefsResponse, tonic::Status> {
        self.refs
            .update_refs(
                &ctx(ALICE),
                update_req(
                    "refs/heads/master",
                    wire::ref_update::Expect::Any(true),
                    to,
                    wire::UpdateMode::Land,
                ),
            )
            .await
    }

    /// Trunk with the policy `extra` and a clean lockfile; returns the tip.
    async fn base(&self, extra: &str) -> ChangesetId {
        let policy = policy_text(extra);
        self.h
            .trunk(&[
                (".asc/policy.toml", policy.as_bytes()),
                ("Cargo.lock", lock(&[("regex", "1.5.0")]).as_bytes()),
                ("src/main.rs", b"fn main() {}"),
            ])
            .await
    }

    async fn change(&self, parent: ChangesetId, files: &[(&str, &[u8])]) -> ChangesetId {
        let cs = commit(&self.h.f.e, &[parent], files).await;
        self.h.f.ingest(cs).await;
        cs
    }
}

#[tokio::test]
async fn the_policy_refuses_a_landing_that_adds_a_vulnerable_dependency() {
    let g = gated(true).await;
    g.h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    g.h.mock.vuln(
        Ecosystem::CratesIo,
        "log",
        "0.4.0",
        "RUSTSEC-2",
        "0.4.1",
        "MODERATE",
    );
    let a = g.base("[deps]\nblock_severity = \"high\"\n").await;
    // Adds a high advisory: refused, naming it.
    let bad = g
        .change(
            a,
            &[(
                "Cargo.lock",
                lock(&[("regex", "1.5.0"), ("serde", "1.0.100")]).as_bytes(),
            )],
        )
        .await;
    let e = g.land(bad).await.unwrap_err();
    assert_eq!(code(&e), "POLICY_DENIED");
    let lines = paths(&e);
    assert!(
        lines[0].contains("advisories at or above high"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("crates.io serde 1.0.100 RUSTSEC-1 (high)")),
        "{lines:?}"
    );
    assert_eq!(g.h.f.tip().await, a, "nothing landed");
    // Below the threshold: allowed.
    let moderate = g
        .change(
            a,
            &[(
                "Cargo.lock",
                lock(&[("regex", "1.5.0"), ("log", "0.4.0")]).as_bytes(),
            )],
        )
        .await;
    g.land(moderate).await.unwrap();
    // A package that was already there is not "added".
    let a2 = g.h.f.tip().await;
    let same = g
        .change(
            a2,
            &[(
                "Cargo.lock",
                lock(&[("regex", "1.5.0"), ("log", "0.4.0"), ("other", "1.0.0")]).as_bytes(),
            )],
        )
        .await;
    g.land(same).await.unwrap();
}

#[tokio::test]
async fn a_dismissed_advisory_does_not_block_a_landing() {
    let g = gated(true).await;
    g.h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "CRITICAL",
    );
    let a = g.base("[deps]\nblock_severity = \"high\"\n").await;
    // The maintainers looked at it first, on a branch scan that recorded the finding.
    let w = g
        .change(
            a,
            &[(
                "Cargo.lock",
                lock(&[("regex", "1.5.0"), ("serde", "1.0.100")]).as_bytes(),
            )],
        )
        .await;
    assert!(g.land(w).await.is_err());
    let entry = g.h.f.e.clone();
    let pool = g.h.f.reg.pool().clone();
    let adv = {
        let r =
            g.h.hub
                .osv
                .lookup(
                    &[super::osv::Query {
                        eco: Ecosystem::CratesIo,
                        name: "serde".into(),
                        version: "1.0.100".into(),
                    }],
                    false,
                )
                .await;
        match r.lookups.into_iter().next().unwrap() {
            super::model::Lookup::Known(mut a) => a.remove(0),
            other => panic!("{other:?}"),
        }
    };
    let rows = super::store::record_scan(
        &pool,
        entry.key.0,
        &super::store::Scan {
            found: &[super::store::NewFinding {
                ecosystem: "crates.io".into(),
                name: "serde".into(),
                version: "1.0.100".into(),
                path: "Cargo.lock".into(),
                direct: true,
                advisory: adv,
                fixed_in: "1.0.200".into(),
            }],
            unjudged: &Default::default(),
            ecosystems: &Default::default(),
            complete: false,
        },
        1,
    )
    .await
    .unwrap();
    super::store::set_state(
        &pool,
        entry.key.0,
        rows[0].id,
        super::store::State::Dismissed,
        1,
        "ok",
        "x",
        2,
        None,
    )
    .await
    .unwrap();
    g.land(w).await.unwrap();
}

#[tokio::test]
async fn an_unknown_answer_refuses_the_landing_unless_the_policy_accepts_it() {
    let g = gated(true).await;
    let a = g.base("[deps]\nblock_severity = \"high\"\n").await;
    g.h.mock.with(|s| s.fail_osv = true);
    let change = g
        .change(
            a,
            &[(
                "Cargo.lock",
                lock(&[("regex", "1.5.0"), ("serde", "1.0.100")]).as_bytes(),
            )],
        )
        .await;
    let e = g.land(change).await.unwrap_err();
    assert_eq!(code(&e), "POLICY_DENIED");
    let lines = paths(&e);
    assert!(lines[0].contains("lookup failed"), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("serde 1.0.100")),
        "{lines:?}"
    );
    // A change that touches no dependency file does not need the lookup.
    let code_only = g.change(a, &[("src/main.rs", b"fn main() { }")]).await;
    g.land(code_only).await.unwrap();
    assert_eq!(
        g.h.mock.count("POST"),
        2,
        "only the first change was looked up (and asked twice)"
    );
    // The policy may accept an unknown answer.
    let tip = g.h.f.tip().await;
    let relaxed = commit(
        &g.h.f.e,
        &[tip],
        &[(
            ".asc/policy.toml",
            policy_text("[deps]\nblock_severity = \"high\"\nblock_unknown = false\n").as_bytes(),
        )],
    )
    .await;
    g.h.f.ingest(relaxed).await;
    g.land(relaxed).await.unwrap();
    let t2 = g.h.f.tip().await;
    let change = g
        .change(
            t2,
            &[(
                "Cargo.lock",
                lock(&[("regex", "1.5.0"), ("serde", "1.0.100")]).as_bytes(),
            )],
        )
        .await;
    g.land(change).await.unwrap();
}

#[tokio::test]
async fn a_policy_that_blocks_cannot_be_enforced_without_the_scanner_so_it_refuses() {
    let g = gated(false).await;
    let a = g.base("[deps]\nblock_severity = \"low\"\n").await;
    let change = g
        .change(
            a,
            &[(
                "Cargo.lock",
                lock(&[("regex", "1.5.0"), ("serde", "1.0.100")]).as_bytes(),
            )],
        )
        .await;
    let e = g.land(change).await.unwrap_err();
    assert_eq!(code(&e), "POLICY_DENIED");
    assert!(paths(&e)[0].contains("switched off"), "{:?}", paths(&e));
    let other = g.change(a, &[("src/main.rs", b"fn main() { }")]).await;
    g.land(other).await.unwrap();
    // A policy that never mentions advisories asks nothing of anyone.
    let g = gated(false).await;
    let a = g.base("").await;
    let change = g
        .change(
            a,
            &[(
                "Cargo.lock",
                lock(&[("regex", "1.5.0"), ("serde", "1.0.100")]).as_bytes(),
            )],
        )
        .await;
    g.land(change).await.unwrap();
}

// ------------------------------------------------------------------------------ who may call

fn tokens() -> Arc<dyn asc_rpc::auth::Authorizer> {
    Arc::new(
        StaticTokens::new()
            .with_token("alice-w", ALICE, Access::Write)
            .with_token("alice-a", ALICE, Access::Admin)
            .with_token("bob-r", BOB, Access::Read)
            .with_token("bob-w", BOB, Access::Write),
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

#[tokio::test]
async fn readers_read_maintainers_write_and_nobody_else_does() {
    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    crates_route(
        &h.mock,
        "serde",
        &[("1.0.100", SUM_OLD), ("1.0.200", SUM_NEW)],
    );
    let policy = policy_text("");
    h.trunk(&[
        (".asc/policy.toml", policy.as_bytes()),
        ("Cargo.toml", manifest(&[("serde", "1.0.100")]).as_bytes()),
        ("Cargo.lock", lock(&[("serde", "1.0.100")]).as_bytes()),
    ])
    .await;
    let svc = DepsRpc::new(h.hub.clone(), tokens(), ServerInfo::default());
    let scan = |t: &str| {
        call(
            wire::ScanDepsRequest {
                repo: repo(),
                ..Default::default()
            },
            t,
            false,
        )
    };
    // A reader cannot scan; a writer who is no maintainer cannot; the maintainer (alice, by the policy)
    // can, and so can an administrator.
    let e = svc.scan(scan("bob-r")).await.unwrap_err();
    assert!(
        matches!(e.code(), Code::PermissionDenied | Code::Unauthenticated),
        "{e}"
    );
    let e = svc.scan(scan("bob-w")).await.unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(e.message().contains("maintainer"), "{}", e.message());
    assert_eq!(
        svc.scan(scan("alice-w"))
            .await
            .unwrap()
            .into_inner()
            .findings
            .len(),
        1
    );
    assert_eq!(
        svc.scan(scan("alice-a"))
            .await
            .unwrap()
            .into_inner()
            .findings
            .len(),
        1
    );
    // Reads need only read.
    let list = call(
        wire::ListDepFindingsRequest {
            repo: repo(),
            ..Default::default()
        },
        "bob-r",
        false,
    );
    assert_eq!(
        svc.list_findings(list)
            .await
            .unwrap()
            .into_inner()
            .findings
            .len(),
        1
    );
    let upd = call(
        wire::ListDepUpdatesRequest {
            repo: repo(),
            ..Default::default()
        },
        "bob-r",
        false,
    );
    assert_eq!(
        svc.list_updates(upd)
            .await
            .unwrap()
            .into_inner()
            .updates
            .len(),
        1
    );
    let cfg = call(
        wire::GetDepsConfigRequest {
            repo: repo(),
            rev: String::new(),
        },
        "bob-r",
        false,
    );
    assert_eq!(
        svc.get_config(cfg).await.unwrap().into_inner().source,
        "default"
    );
    // No credential, no answer.
    let e = svc
        .list_findings(Request::new(wire::ListDepFindingsRequest {
            repo: repo(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert!(
        matches!(e.code(), Code::Unauthenticated | Code::PermissionDenied),
        "{e}"
    );
    // Writes need an operation id as well as the role.
    let p = wire::ProposeDepUpdateRequest {
        repo: repo(),
        ..Default::default()
    };
    let e = svc
        .propose_update(call(p.clone(), "alice-w", false))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let e = svc
        .propose_update(call(p.clone(), "bob-w", true))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    let ok = svc
        .propose_update(call(p, "alice-w", true))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ok.proposals.len(), 1);
    assert!(ok.proposals[0].r#ref.starts_with(REF_PREFIX));
    // A dry run is a read of the plan and needs no operation id.
    let dry = wire::ProposeDepUpdateRequest {
        repo: repo(),
        dry_run: true,
        ..Default::default()
    };
    assert!(
        svc.propose_update(call(dry, "alice-w", false))
            .await
            .is_ok()
    );
    let set = wire::SetDepsConfigRequest {
        repo: repo(),
        config: Some(wire::DepsConfig::default()),
        dry_run: false,
    };
    let e = svc
        .set_config(call(set.clone(), "bob-w", true))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(svc.set_config(call(set, "alice-a", true)).await.is_ok());
    let d = wire::DismissDepFindingRequest {
        repo: repo(),
        id: 1,
        reason: wire::DepDismissReason::Tolerable as i32,
        note: String::new(),
    };
    let e = svc
        .dismiss(call(d.clone(), "bob-w", true))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(svc.dismiss(call(d, "alice-w", true)).await.is_ok());
    let _ = (DepSeverity::Low, CallCtx::clone);
}
