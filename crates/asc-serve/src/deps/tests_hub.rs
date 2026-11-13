//! The service end to end: a repository, a mock OSV and registry, the hub, the gate and the RPC layer.

use std::collections::HashMap;
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
use super::config::Config;
use super::gate::DepsGateSlot;
use super::hub::{DepsHub, hex};
use super::intel::IntelSource;
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
    build_with(f, mock, server_url, |_| {})
}

/// [`build`] with the operator settings edited first. The exploitation feeds accept the small files tests
/// write.
fn build_with(
    f: &Fx,
    mock: &Mock,
    server_url: Option<&str>,
    edit: impl FnOnce(&mut Config),
) -> (Arc<DepsHub>, Arc<AtomicI64>, Arc<ServerMutations>) {
    let mut c = config(mock);
    edit(&mut c);
    let (clk, t) = clock(T0);
    let mutations = Arc::new(ServerMutations::new(
        f.reg.clone(),
        f.gated.hooks.clone(),
        Arc::new(ReadHooks::default()),
    ));
    let intel = IntelSource::new(egress_for(&c), &c, clk.clone()).with_min_rows(1, 1);
    let mut hub = DepsHub::new(
        f.reg.clone(),
        mutations.clone(),
        egress_for(&c),
        c,
        server_url.map(str::to_owned),
        clk,
    );
    Arc::get_mut(&mut hub)
        .expect("nothing else holds the hub yet")
        .intel = intel;
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
    // The two paths a lockfile move checks the checksum against: the version's own document and the
    // sparse index, which also says what the version depends on.
    let mut index = String::new();
    for (v, c) in versions {
        m.route(
            &format!("/api/v1/crates/{name}/{v}"),
            200,
            &format!(r#"{{"version":{{"num":"{v}","yanked":false,"checksum":"{c}"}}}}"#),
        );
        index.push_str(&format!(
            r#"{{"name":"{name}","vers":"{v}","deps":[],"cksum":"{c}","features":{{}},"yanked":false}}"#
        ));
        index.push('\n');
    }
    let path: Vec<String> = super::facts::crates_index_path(name).unwrap();
    m.route(&format!("/{}", path.join("/")), 200, &index);
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
    assert_eq!(
        r.packages_by_ecosystem,
        HashMap::from([("crates.io".to_owned(), 2)])
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
    assert_eq!(listed.packages_checked, 2);
    assert_eq!(
        listed.packages_by_ecosystem,
        HashMap::from([("crates.io".to_owned(), 2)])
    );
    assert_eq!(listed.last_lookup, wire::DepLookupStatus::Complete as i32);
    assert_eq!(listed.packages_unknown, 0);
    assert_eq!(
        listed.open_by_severity,
        HashMap::from([
            ("critical".to_owned(), 0),
            ("high".to_owned(), 1),
            ("moderate".to_owned(), 0),
            ("low".to_owned(), 0),
            ("unknown".to_owned(), 0),
        ]),
        "every severity is present, counted over the open findings"
    );
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
    assert!(
        h.findings(&[])
            .await
            .open_by_severity
            .values()
            .all(|n| *n == 0),
        "a dismissed finding is not counted as open"
    );
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
            .any(|n| n.contains("crates.io API and index agree on the checksum")),
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
        [
            "asc",
            "deps",
            "propose",
            "--remote",
            "https://asc.example.com/o/r"
        ]
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
    gate_of(setup().await, wired).await
}

/// A gate over `h`, asking its hub when `wired`.
async fn gate_of(h: H, wired: bool) -> G {
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

#[tokio::test]
async fn scan_status_says_what_a_running_scan_has_done() {
    use super::osv::Query;
    use super::progress::{Phase, Tick};

    let h = setup().await;
    h.mock.vuln(
        Ecosystem::CratesIo,
        "serde",
        "1.0.100",
        "RUSTSEC-1",
        "1.0.200",
        "HIGH",
    );
    h.trunk(&[(
        "Cargo.lock",
        lock(&[("serde", "1.0.100"), ("regex", "1.5.0")]).as_bytes(),
    )])
    .await;
    let idle = h.hub.scan_status(&ctx(BOB)).await.unwrap();
    assert!(!idle.running);
    assert_eq!(idle.phase, wire::DepScanPhase::Unspecified as i32);
    assert_eq!((idle.started_at, idle.eta_seconds), (0, 0));

    // The lookup counts a step per package version asked and one per advisory record read.
    let tick = Tick::default();
    let queries: Vec<Query> = [("serde", "1.0.100"), ("regex", "1.5.0")]
        .iter()
        .map(|(n, v)| Query {
            eco: Ecosystem::CratesIo,
            name: (*n).into(),
            version: (*v).into(),
        })
        .collect();
    h.hub.osv.lookup_with(&queries, true, Some(&tick)).await;
    assert_eq!(tick.get(), (3, 3), "two versions and one record");

    // A scan in the advisory phase: the bar and the estimate come from its steps.
    let key = h.hub.entry(&ctx(ALICE)).await.unwrap().key.0;
    let g = h.hub.scans.start(key, T0);
    g.phase(Phase::Advisories, T0);
    g.tick().add_total(4);
    g.tick().add_done(2);
    h.clock.store(T0 + 10, Ordering::SeqCst);
    let s = h.hub.scan_status(&ctx(BOB)).await.unwrap();
    assert!(s.running);
    assert_eq!(s.phase, wire::DepScanPhase::Advisories as i32);
    assert_eq!((s.done, s.total, s.started_at), (2, 4, T0));
    assert_eq!(s.permille, 525);
    assert_eq!(s.eta_seconds, 10, "two steps in 10 s, two left");
    drop(g);

    // A finished scan leaves nothing running.
    h.scan("", false).await;
    let s = h.hub.scan_status(&ctx(BOB)).await.unwrap();
    assert!(!s.running);
    assert_eq!(
        s.last_took_seconds, 0,
        "the clock did not move during the scan"
    );
}

// ------------------------------------------------------------------------------ KEV and EPSS

const KEV_FILE: &str = r#"{"catalogVersion":"2027.01.14","dateReleased":"2027-01-14T00:00:00Z","vulnerabilities":[
{"cveID":"CVE-2024-0001","dateAdded":"2024-03-01","dueDate":"2024-03-22","knownRansomwareCampaignUse":"Known"},
{"cveID":"CVE-2024-0009","dateAdded":"2024-04-01","dueDate":"2024-04-22","knownRansomwareCampaignUse":"Unknown"}]}"#;
const EPSS_FILE: &str = "#model_version:v2025.03.14,score_date:2027-01-14T00:00:00+0000\n\
cve,epss,percentile\n\
CVE-2024-0001,0.2,0.7\n\
CVE-2024-0002,0.9,0.99\n\
CVE-2024-0003,0.1,0.5\n";

/// Which advisory belongs to which package: serde is KEV-listed with a low EPSS, regex is not in KEV
/// and has the highest EPSS, itoa has a low EPSS, log has no row anywhere.
const INTEL_PKGS: [(&str, &str, &str); 4] = [
    ("serde", "1.0.100", "CVE-2024-0001"),
    ("regex", "1.5.0", "CVE-2024-0002"),
    ("itoa", "1.0.1", "CVE-2024-0003"),
    ("log", "0.4.1", "CVE-2024-0004"),
];

struct Feeds {
    h: H,
    _dir: tempfile::TempDir,
    path: String,
}

/// A hub with the feeds that are given loaded (and nothing else).
async fn feed_hub(kev: Option<&str>, epss: Option<&str>, max_packages: Option<usize>) -> Feeds {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().into_owned();
    let kev_path = dir.path().join("kev.json");
    let epss_path = dir.path().join("epss.csv");
    if let Some(k) = kev {
        std::fs::write(&kev_path, k).unwrap();
    }
    if let Some(e) = epss {
        std::fs::write(&epss_path, e).unwrap();
    }
    let f = fx().await;
    let mock = Mock::start().await;
    let (hub, clock, _) = build_with(&f, &mock, Some("https://asc.example.com"), |c| {
        c.kev_file = kev.map(|_| kev_path.clone());
        c.epss_file = epss.map(|_| epss_path.clone());
        if let Some(n) = max_packages {
            c.max_packages = n;
        }
    });
    hub.intel().refresh().await;
    let h = H {
        f,
        hub,
        mock,
        clock,
    };
    Feeds { h, _dir: dir, path }
}

/// A scanned repository of the four packages above, with the feeds that are given loaded.
async fn with_feeds(kev: Option<&str>, epss: Option<&str>, max_packages: Option<usize>) -> Feeds {
    let Feeds { h, _dir, path } = feed_hub(kev, epss, max_packages).await;
    for (name, version, id) in INTEL_PKGS {
        h.mock
            .vuln(Ecosystem::CratesIo, name, version, id, "9.9.9", "HIGH");
    }
    let pkgs: Vec<(&str, &str)> = INTEL_PKGS.iter().map(|(n, v, _)| (*n, *v)).collect();
    h.trunk(&[("Cargo.lock", lock(&pkgs).as_bytes())]).await;
    h.scan("", false).await;
    Feeds { h, _dir, path }
}

impl H {
    async fn list(
        &self,
        req: wire::ListDepFindingsRequest,
    ) -> Result<wire::ListDepFindingsResponse, tonic::Status> {
        self.hub
            .list_findings(
                &ctx(BOB),
                wire::ListDepFindingsRequest {
                    repo: repo(),
                    ..req
                },
            )
            .await
    }
}

fn intel_of<'a>(r: &'a wire::ListDepFindingsResponse, name: &str) -> &'a wire::DepIntel {
    r.findings
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no finding for {name}"))
        .advisory
        .as_ref()
        .unwrap()
        .intel
        .as_ref()
        .expect("every finding carries intel")
}

fn status_of(i: &wire::DepIntel) -> wire::DepIntelStatus {
    wire::DepIntelStatus::try_from(i.status).unwrap()
}

fn exploited() -> wire::ListDepFindingsRequest {
    wire::ListDepFindingsRequest {
        sort: wire::DepFindingSort::Exploited as i32,
        ..Default::default()
    }
}

/// The sealed scope of a plain EXPLOITED listing of the test repository.
fn exploited_scope() -> String {
    let hash = super::hub::filter_hash(
        &[],
        &[],
        super::store::severity_code(None),
        "",
        wire::DepFindingSort::Exploited,
        false,
    );
    format!("Deps.ListFindings.exploited|{}|{hash:016x}", ctx(BOB).repo)
}

fn names(r: &wire::ListDepFindingsResponse) -> Vec<&str> {
    r.findings.iter().map(|f| f.name.as_str()).collect()
}

#[tokio::test]
async fn intel_is_listed_not_listed_or_unknown_and_never_guessed() {
    use wire::DepIntelStatus::{Listed, NotListed, Unknown};
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let r = t.h.list(Default::default()).await.unwrap();
    let serde = intel_of(&r, "serde");
    assert_eq!(status_of(serde), Listed);
    assert_eq!(serde.cve, "CVE-2024-0001");
    assert!(serde.kev && serde.kev_ransomware);
    assert_eq!(
        (serde.kev_added.as_str(), serde.kev_due.as_str()),
        ("2024-03-01", "2024-03-22")
    );
    assert!((serde.epss - 0.2).abs() < 1e-9 && (serde.epss_percentile - 0.7).abs() < 1e-9);
    assert!(
        serde.epss_date.starts_with("2027-01-14"),
        "{}",
        serde.epss_date
    );
    let regex = intel_of(&r, "regex");
    assert_eq!(status_of(regex), Listed, "an EPSS row alone lists it");
    assert!(!regex.kev && (regex.epss - 0.9).abs() < 1e-9);
    let log = intel_of(&r, "log");
    assert_eq!(
        status_of(log),
        NotListed,
        "every configured feed is loaded and has no row"
    );
    assert_eq!((log.epss, log.kev), (0.0, false));

    // The feeds age out: nothing is known then, and nothing is called clean.
    t.h.clock.fetch_add(8 * 86_400, Ordering::SeqCst);
    let r = t.h.list(Default::default()).await.unwrap();
    for name in ["serde", "regex", "itoa", "log"] {
        let i = intel_of(&r, name);
        assert_eq!(status_of(i), Unknown, "{name} after the feeds went stale");
        assert!(!i.kev && i.epss == 0.0);
    }
    assert_eq!(
        r.kev_feed.as_ref().unwrap().state,
        wire::DepFeedState::Unknown as i32
    );
    assert_eq!(r.open_exploited, 0, "an untrusted KEV counts nothing");
}

#[tokio::test]
async fn a_feed_that_is_off_or_not_loaded_gives_unknown() {
    use wire::DepIntelStatus::{Listed, NotListed, Unknown};
    // Neither feed configured.
    let none = with_feeds(None, None, None).await;
    let r = none.h.list(Default::default()).await.unwrap();
    for name in ["serde", "log"] {
        assert_eq!(status_of(intel_of(&r, name)), Unknown);
    }
    for feed in [&r.kev_feed, &r.epss_feed] {
        assert_eq!(
            feed.as_ref().unwrap().state,
            wire::DepFeedState::Unconfigured as i32
        );
    }
    // Only KEV configured and loaded: the advisories it does not list are not listed.
    let kev_only = with_feeds(Some(KEV_FILE), None, None).await;
    let r = kev_only.h.list(Default::default()).await.unwrap();
    assert_eq!(status_of(intel_of(&r, "regex")), NotListed);
    assert_eq!(status_of(intel_of(&r, "serde")), Listed);
    // EPSS configured, but its file is not there: unknown, not clean.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("epss.csv");
    let f = fx().await;
    let mock = Mock::start().await;
    let (hub, _, _) = build_with(&f, &mock, None, |c| c.epss_file = Some(missing.clone()));
    hub.intel().refresh().await;
    let adv = super::model::Advisory {
        id: "CVE-2024-0004".into(),
        aliases: vec![],
        summary: String::new(),
        severity: None,
        cvss: 0.0,
        fixed: vec![],
        url: String::new(),
        affected: vec![],
    };
    assert_eq!(status_of(&hub.intel_view().of(&adv)), Unknown);
}

#[tokio::test]
async fn the_feeds_are_reported_without_a_url_or_path() {
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let r = t.h.list(Default::default()).await.unwrap();
    let kev = r.kev_feed.as_ref().unwrap();
    assert_eq!(kev.state, wire::DepFeedState::Current as i32);
    assert_eq!(kev.version, "2027.01.14");
    assert_eq!((kev.count, kev.loaded_at, kev.age_seconds), (2, T0, 0));
    let epss = r.epss_feed.as_ref().unwrap();
    assert_eq!(epss.state, wire::DepFeedState::Current as i32);
    assert_eq!((epss.version.as_str(), epss.count), ("v2025.03.14", 3));
    t.h.clock.fetch_add(90, Ordering::SeqCst);
    let r = t.h.list(Default::default()).await.unwrap();
    assert_eq!(r.kev_feed.as_ref().unwrap().age_seconds, 90);
    let text = format!("{r:?}");
    assert!(
        !text.contains(&t.path) && !text.contains("kev.json"),
        "{text}"
    );
    assert_eq!(r.open_exploited, 1);
}

#[tokio::test]
async fn the_exploited_order_is_kev_then_epss_then_cvss_and_pages_over_two_calls() {
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let all = t.h.list(exploited()).await.unwrap();
    assert_eq!(names(&all), ["serde", "regex", "itoa", "log"]);
    assert!(all.next_page_token.is_empty());
    let newest = t.h.list(Default::default()).await.unwrap();
    let ids: Vec<u64> = newest.findings.iter().map(|f| f.id).collect();
    assert!(
        ids.windows(2).all(|w| w[0] > w[1]),
        "newest stays the default, by id: {ids:?}"
    );
    let newest_names: Vec<String> = names(&newest).iter().map(|n| (*n).to_owned()).collect();

    let first =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 3,
            ..exploited()
        })
        .await
        .unwrap();
    assert_eq!(names(&first), ["serde", "regex", "itoa"]);
    assert!(!first.next_page_token.is_empty());
    let second =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 3,
            page_token: first.next_page_token.clone(),
            ..exploited()
        })
        .await
        .unwrap();
    assert_eq!(names(&second), ["log"]);
    assert!(second.next_page_token.is_empty());

    // The default order still pages by id.
    let n1 =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 3,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(names(&n1), newest_names[..3]);
    let n2 =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 3,
            page_token: n1.next_page_token,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(names(&n2), newest_names[3..]);
}

#[tokio::test]
async fn a_bad_or_foreign_page_token_is_refused() {
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let page = |sort: i32, token: &str| wire::ListDepFindingsRequest {
        limit: 2,
        sort,
        page_token: token.to_owned(),
        ..Default::default()
    };
    let exp = wire::DepFindingSort::Exploited as i32;
    let new = wire::DepFindingSort::Newest as i32;
    let newest_token = t.h.list(page(new, "")).await.unwrap().next_page_token;
    let exploited_token = t.h.list(page(exp, "")).await.unwrap().next_page_token;
    assert!(!newest_token.is_empty() && !exploited_token.is_empty());
    for (sort, token) in [
        (exp, newest_token.as_str()),
        (new, exploited_token.as_str()),
        (exp, "not a token"),
        (exp, "AAAA"),
    ] {
        let e = t.h.list(page(sort, token)).await.unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument, "{sort} {token}");
    }
    // A token that is genuine but names an offset that is not a page of the list.
    let who = ctx(BOB).principal.id.clone();
    let scope = exploited_scope();
    for payload in ["0", "-1", "01", "", "1x", "+1", " 1"] {
        let token = crate::pagetoken::seal(&scope, &who, payload);
        let e = t.h.list(page(exp, &token)).await.unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument, "payload {payload:?}");
    }
    // The same token for another caller is refused too.
    let other = crate::pagetoken::seal(&scope, "user:someone-else", "2");
    assert_eq!(
        t.h.list(page(exp, &other)).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    // And a good offset is accepted.
    let good = crate::pagetoken::seal(&scope, &who, "2");
    assert_eq!(
        names(&t.h.list(page(exp, &good)).await.unwrap()),
        ["itoa", "log"]
    );
}

#[tokio::test]
async fn an_offset_at_or_past_the_end_is_an_empty_last_page() {
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let who = ctx(BOB).principal.id.clone();
    let scope = exploited_scope();
    for payload in ["4", "5", "99", "18446744073709551615"] {
        let token = crate::pagetoken::seal(&scope, &who, payload);
        let r =
            t.h.list(wire::ListDepFindingsRequest {
                page_token: token,
                ..exploited()
            })
            .await
            .unwrap_or_else(|e| panic!("{payload}: {e}"));
        assert!(r.findings.is_empty(), "{payload}");
        assert!(r.next_page_token.is_empty(), "{payload}");
        assert_eq!(r.counts_by_state["open"], 4, "the totals still come");
    }
    // A token that overflows an offset is still refused.
    let token = crate::pagetoken::seal(&scope, &who, "18446744073709551616");
    let e =
        t.h.list(wire::ListDepFindingsRequest {
            page_token: token,
            ..exploited()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn only_exploited_without_a_usable_kev_feed_is_a_failed_precondition_that_names_the_state() {
    // Not configured.
    let none = with_feeds(None, Some(EPSS_FILE), None).await;
    let only = wire::ListDepFindingsRequest {
        only_exploited: true,
        ..Default::default()
    };
    let e = none.h.list(only.clone()).await.unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
    assert!(
        e.message().contains("KEV") && e.message().contains("not configured"),
        "{}",
        e.message()
    );
    // The sort alone still answers: it only orders.
    let r = none.h.list(exploited()).await.unwrap();
    assert_eq!(r.findings.len(), 4);
    // Configured and loaded, then too old.
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    t.h.list(only.clone()).await.unwrap();
    t.h.clock.fetch_add(8 * 86_400, Ordering::SeqCst);
    let e = t.h.list(only).await.unwrap_err();
    assert_eq!(e.code(), Code::FailedPrecondition);
    assert!(e.message().contains("too old"), "{}", e.message());
    assert!(
        !e.message().contains(&t.path) && !e.message().contains("kev.json"),
        "{}",
        e.message()
    );
}

#[tokio::test]
async fn the_feed_status_and_the_data_of_one_view_always_agree() {
    let t = feed_hub(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let stale = 7 * 86_400;
    for step in [0, 1, stale - 1, stale, stale + 1, stale + 2, 3 * stale] {
        t.h.clock.store(T0 + step, Ordering::SeqCst);
        let v = t.h.hub.intel_view();
        assert_eq!(
            v.kev_usable(),
            v.kev.state == super::intel::FeedState::Current,
            "KEV at +{step}"
        );
        assert_eq!(
            v.intel.epss.is_some(),
            v.epss.state == super::intel::FeedState::Current,
            "EPSS at +{step}"
        );
        let (intel, status) = t.h.hub.intel().snapshot();
        assert_eq!(
            intel.kev.is_some(),
            status.kev.state == super::intel::FeedState::Current
        );
    }
}

#[tokio::test]
async fn only_the_in_memory_orders_take_a_scan_slot() {
    use std::time::Duration;
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let first =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!first.next_page_token.is_empty());
    // Hold every slot, as running scans would.
    let mut held = Vec::new();
    for _ in 0..t.h.hub.cfg.max_concurrent_scans {
        held.push(t.h.hub.permit().await.unwrap());
    }
    let wait = Duration::from_millis(300);
    // The default order and the plain filters need none after the first page (the first one counts what KEV
    // lists, which reads many rows, so it queues).
    let r = tokio::time::timeout(
        wait,
        t.h.list(wire::ListDepFindingsRequest {
            limit: 1,
            page_token: first.next_page_token.clone(),
            ..Default::default()
        }),
    )
    .await
    .expect("a later NEWEST page does not queue")
    .unwrap();
    assert_eq!(r.open_exploited, 0, "only the first page counts");
    assert!(
        tokio::time::timeout(wait, t.h.list(Default::default()))
            .await
            .is_err(),
        "the first NEWEST page queues for its count"
    );
    // Sorting by exploitation, or keeping only what KEV lists, queues like a scan.
    assert!(
        tokio::time::timeout(wait, t.h.list(exploited()))
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(
            wait,
            t.h.list(wire::ListDepFindingsRequest {
                only_exploited: true,
                ..Default::default()
            })
        )
        .await
        .is_err()
    );
    drop(held);
    assert_eq!(t.h.list(exploited()).await.unwrap().findings.len(), 4);
}

#[tokio::test]
async fn a_page_token_is_bound_to_the_filters_it_was_issued_under() {
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let first =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 1,
            ..exploited()
        })
        .await
        .unwrap();
    let token = first.next_page_token;
    assert!(!token.is_empty());
    // The same filters continue.
    let r =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 1,
            page_token: token.clone(),
            ..exploited()
        })
        .await
        .unwrap();
    assert_eq!(r.findings.len(), 1);
    // Other filters, a flag or a name refuse it.
    for other in [
        wire::ListDepFindingsRequest {
            name: "serde".into(),
            ..exploited()
        },
        wire::ListDepFindingsRequest {
            only_exploited: true,
            ..exploited()
        },
        wire::ListDepFindingsRequest {
            states: vec![wire::DepFindingState::Dismissed as i32],
            ..exploited()
        },
        wire::ListDepFindingsRequest {
            ecosystems: vec!["crates.io".into()],
            ..exploited()
        },
        wire::ListDepFindingsRequest {
            min_severity: wire::DepSeverity::High as i32,
            ..exploited()
        },
    ] {
        let e =
            t.h.list(wire::ListDepFindingsRequest {
                page_token: token.clone(),
                ..other
            })
            .await
            .unwrap_err();
        assert_eq!(e.code(), Code::InvalidArgument);
    }
}

#[test]
fn the_cut_says_when_it_cut() {
    let mut rows: Vec<u32> = (0..5).collect();
    assert!(!super::hub::cut(&mut rows, 5));
    assert_eq!(rows.len(), 5);
    let mut rows: Vec<u32> = (0..6).collect();
    assert!(super::hub::cut(&mut rows, 5));
    assert_eq!(rows, [0, 1, 2, 3, 4]);
    let mut rows: Vec<u32> = Vec::new();
    assert!(!super::hub::cut(&mut rows, 0));
}

#[tokio::test]
async fn a_list_that_fits_is_not_marked_truncated_and_the_count_is_for_the_first_page() {
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let r = t.h.list(exploited()).await.unwrap();
    assert!(!r.truncated);
    assert_eq!(r.open_exploited, 1);
    let r =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 1,
            ..exploited()
        })
        .await
        .unwrap();
    assert_eq!(r.open_exploited, 1, "the first page counts");
    let later =
        t.h.list(wire::ListDepFindingsRequest {
            limit: 1,
            page_token: r.next_page_token,
            ..exploited()
        })
        .await
        .unwrap();
    assert_eq!(later.open_exploited, 0, "a later page does not");
    assert!(!later.truncated);
}

// ------------------------------------------------------------------------------ who may see which repository

/// Tokens as a forge-backed authorizer would answer them: a person who has no access to a repository is
/// refused the same way whether it is private or does not exist.
struct Scoped {
    inner: StaticTokens,
    /// The only repository `CAROL` may name.
    carol_repo: &'static str,
}

const CAROL: &str = "44:carol";

#[async_trait::async_trait]
impl asc_rpc::auth::Authorizer for Scoped {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<asc_rpc::auth::Principal, asc_rpc::auth::AuthError> {
        let p = self.inner.authorize(token, repo, access).await?;
        if p.id == CAROL && repo != self.carol_repo {
            return Err(asc_rpc::auth::AuthError::Forbidden);
        }
        Ok(p)
    }
}

#[tokio::test]
async fn a_person_without_access_gets_one_answer_for_a_private_repository_and_a_missing_one() {
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let auth = Arc::new(Scoped {
        inner: StaticTokens::new()
            .with_token("bob-r", BOB, Access::Read)
            .with_token("carol-r", CAROL, Access::Read),
        carol_repo: "somewhere/else",
    });
    let svc = DepsRpc::new(t.h.hub.clone(), auth, ServerInfo::default());
    let named = |name: &str| {
        Some(wire::RepoRef {
            name: name.into(),
            id_mode: IdMode::Asc.to_wire(),
        })
    };
    // Bob reads what exists.
    let ok = svc
        .sbom(call(
            wire::SbomRequest {
                repo: repo(),
                ..Default::default()
            },
            "bob-r",
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ok.components, 4);
    let mut answers = Vec::new();
    // "o/r" exists and carol may not read it; "no/such" does not exist.
    for name in ["o/r", "no/such"] {
        let sbom = svc
            .sbom(call(
                wire::SbomRequest {
                    repo: named(name),
                    ..Default::default()
                },
                "carol-r",
                false,
            ))
            .await
            .unwrap_err();
        let plain = svc
            .list_findings(call(
                wire::ListDepFindingsRequest {
                    repo: named(name),
                    ..Default::default()
                },
                "carol-r",
                false,
            ))
            .await
            .unwrap_err();
        let sorted = svc
            .list_findings(call(
                wire::ListDepFindingsRequest {
                    repo: named(name),
                    sort: wire::DepFindingSort::Exploited as i32,
                    only_exploited: true,
                    ..Default::default()
                },
                "carol-r",
                false,
            ))
            .await
            .unwrap_err();
        assert_eq!(sbom.code(), plain.code(), "{name}");
        assert_eq!(sorted.code(), plain.code(), "{name}");
        assert_eq!(sbom.message(), plain.message(), "{name}");
        assert_eq!(sorted.message(), plain.message(), "{name}");
        answers.push((sbom.code(), sbom.message().to_owned()));
    }
    assert_eq!(answers[0], answers[1], "private and missing look alike");
    assert_eq!(answers[0].0, Code::PermissionDenied);
}

// ------------------------------------------------------------------------------ the gate on exploitation

/// A gate over a repository whose hub has the given feeds, and nothing scanned. The directory holds the
/// feed files and must outlive the test.
async fn gate_feeds(kev: Option<&str>, epss: Option<&str>) -> (G, tempfile::TempDir) {
    let Feeds { h, _dir, .. } = feed_hub(kev, epss, None).await;
    (gate_of(h, true).await, _dir)
}

fn files_adding(extra: &[(&str, &str)]) -> Vec<(&'static str, Vec<u8>)> {
    let mut all = vec![("regex", "1.5.0")];
    all.extend_from_slice(extra);
    vec![("Cargo.lock", lock(&all).into_bytes())]
}

async fn land_adding(
    g: &G,
    base: ChangesetId,
    extra: &[(&str, &str)],
) -> Result<(), tonic::Status> {
    let files = files_adding(extra);
    let files: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    let cs = g.change(base, &files).await;
    g.land(cs).await.map(|_| ())
}

fn advisory(h: &H, name: &str, version: &str, id: &str, sev: &str) {
    h.mock
        .vuln(Ecosystem::CratesIo, name, version, id, "9.9.9", sev);
}

#[tokio::test]
async fn a_kev_listed_advisory_blocks_a_landing_that_adds_it() {
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0001", "LOW");
    advisory(&g.h, "log", "0.4.1", "CVE-2024-0004", "CRITICAL");
    let a = g.base("[deps]\nblock_exploited = true\n").await;
    let files = files_adding(&[("serde", "1.0.100")]);
    let files: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    let bad = g.change(a, &files).await;
    let e = g.land(bad).await.unwrap_err();
    assert_eq!(code(&e), "POLICY_DENIED");
    let lines = paths(&e);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("crates.io serde 1.0.100 CVE-2024-0001")
                && l.contains("known exploited")),
        "{lines:?}"
    );
    let text = lines.join("\\n");
    assert!(
        !text.contains("kev.json") && !text.contains("epss.csv") && !text.contains("http"),
        "{text}"
    );
    assert_eq!(g.h.f.tip().await, a, "nothing landed");
    // Severity alone does not block: only the new key is set, and log is not on the list.
    land_adding(&g, a, &[("log", "0.4.1")]).await.unwrap();
}

#[tokio::test]
async fn an_epss_score_at_the_threshold_blocks_and_one_below_passes() {
    // serde has the score 0.9, log has 0.1.
    let kev = None;
    for (threshold, serde_blocked) in [("0.9", true), ("0.95", false)] {
        let (g, _keep) = gate_feeds(kev, Some(EPSS_FILE)).await;
        advisory(&g.h, "serde", "1.0.100", "CVE-2024-0002", "LOW");
        advisory(&g.h, "log", "0.4.1", "CVE-2024-0003", "LOW");
        let a = g
            .base(&format!("[deps]\nepss_threshold = {threshold}\n"))
            .await;
        let r = land_adding(&g, a, &[("serde", "1.0.100")]).await;
        if serde_blocked {
            let e = r.unwrap_err();
            assert_eq!(code(&e), "POLICY_DENIED");
            let lines = paths(&e);
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("serde 1.0.100 CVE-2024-0002") && l.contains("EPSS 0.900")),
                "{lines:?}"
            );
            // Below the threshold passes.
            let tip = g.h.f.tip().await;
            land_adding(&g, tip, &[("log", "0.4.1")]).await.unwrap();
        } else {
            r.unwrap();
        }
    }
}

#[tokio::test]
async fn a_feed_that_is_missing_or_stale_refuses_and_names_the_feed() {
    // Neither feed configured; the policy needs both. block_unknown does not matter.
    let (g, _keep) = gate_feeds(None, None).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0001", "LOW");
    let a = g
        .base("[deps]\nblock_exploited = true\nepss_threshold = 0.5\nblock_unknown = false\n")
        .await;
    let e = land_adding(&g, a, &[("serde", "1.0.100")])
        .await
        .unwrap_err();
    assert_eq!(code(&e), "POLICY_DENIED");
    let lines = paths(&e);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("KEV feed is not configured")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("EPSS feed is not configured")),
        "{lines:?}"
    );
    // A change that adds nothing with an advisory is not judged by a feed, so it lands.
    land_adding(&g, a, &[("log", "0.4.1")]).await.unwrap();

    // Loaded, then too old.
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    advisory(&g.h, "log", "0.4.1", "CVE-2024-0004", "LOW");
    let a = g
        .base("[deps]\nblock_exploited = true\nepss_threshold = 0.5\n")
        .await;
    land_adding(&g, a, &[("log", "0.4.1")]).await.unwrap();
    let tip = g.h.f.tip().await;
    g.h.clock.fetch_add(8 * 86_400, Ordering::SeqCst);
    let e = land_adding(&g, tip, &[("log", "0.4.1"), ("itoa", "1.0.1")]).await;
    // `log` is already on trunk and `itoa` has no advisory: nothing is judged.
    e.unwrap();
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0009", "LOW");
    let tip = g.h.f.tip().await;
    let e = land_adding(
        &g,
        tip,
        &[("log", "0.4.1"), ("itoa", "1.0.1"), ("serde", "1.0.100")],
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), "POLICY_DENIED");
    let lines = paths(&e);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("KEV feed is not loaded, or its copy is too old")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("EPSS feed is not loaded, or its copy is too old")),
        "{lines:?}"
    );
    let text = lines.join("\\n");
    assert!(
        !text.contains("kev.json") && !text.contains("http"),
        "{text}"
    );

    // Only the feed the policy names is needed: KEV is stale, the policy asks for EPSS alone.
    let (g, _keep) = gate_feeds(None, Some(EPSS_FILE)).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0003", "LOW");
    let a = g.base("[deps]\nepss_threshold = 0.5\n").await;
    land_adding(&g, a, &[("serde", "1.0.100")]).await.unwrap();
}

#[tokio::test]
async fn a_dismissed_advisory_does_not_block_on_exploitation() {
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0001", "LOW");
    let a = g.base("[deps]\nblock_exploited = true\n").await;
    assert!(land_adding(&g, a, &[("serde", "1.0.100")]).await.is_err());
    let adv = match g
        .h
        .hub
        .osv
        .lookup(
            &[super::osv::Query {
                eco: Ecosystem::CratesIo,
                name: "serde".into(),
                version: "1.0.100".into(),
            }],
            false,
        )
        .await
        .lookups
        .remove(0)
    {
        super::model::Lookup::Known(mut a) => a.remove(0),
        other => panic!("{other:?}"),
    };
    let entry = g.h.f.e.clone();
    let pool = g.h.f.reg.pool().clone();
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
                fixed_in: "9.9.9".into(),
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
    land_adding(&g, a, &[("serde", "1.0.100")]).await.unwrap();
}

#[tokio::test]
async fn without_the_new_keys_the_gate_is_what_it_was() {
    // No `[deps]` at all: nothing is judged, with or without feeds, however exploited the advisory.
    let (g, _keep) = gate_feeds(None, None).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0001", "CRITICAL");
    let a = g.base("").await;
    land_adding(&g, a, &[("serde", "1.0.100")]).await.unwrap();
    // The severity gate alone does not look at the feeds: a KEV-listed moderate advisory passes a "high"
    // gate, and a missing feed does not refuse.
    let (g, _keep) = gate_feeds(None, None).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0001", "MODERATE");
    let a = g
        .base(
            "[deps]\nblock_severity = \"high\"
",
        )
        .await;
    land_adding(&g, a, &[("serde", "1.0.100")]).await.unwrap();
    // A zero threshold and `block_exploited = false` are off.
    let (g, _keep) = gate_feeds(None, None).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0001", "MODERATE");
    let a = g
        .base("[deps]\nblock_exploited = false\nepss_threshold = 0\n")
        .await;
    land_adding(&g, a, &[("serde", "1.0.100")]).await.unwrap();
}

#[tokio::test]
async fn the_exploitation_keys_and_the_severity_gate_work_together_or_apart() {
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0001", "HIGH");
    advisory(&g.h, "log", "0.4.1", "CVE-2024-0004", "CRITICAL");
    advisory(&g.h, "itoa", "1.0.1", "CVE-2024-0003", "LOW");
    let a = g
        .base("[deps]\nblock_severity = \"critical\"\nblock_exploited = true\n")
        .await;
    // High and on KEV: below the severity bar, refused by the new key alone.
    let e = land_adding(&g, a, &[("serde", "1.0.100")])
        .await
        .unwrap_err();
    let lines = paths(&e);
    assert!(
        !lines.iter().any(|l| l.contains("advisories at or above")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("known exploited")),
        "{lines:?}"
    );
    // Critical and not on KEV: refused by the severity bar alone.
    let e = land_adding(&g, a, &[("log", "0.4.1")]).await.unwrap_err();
    let lines = paths(&e);
    assert!(
        lines[0].contains("advisories at or above critical"),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("known exploited")),
        "{lines:?}"
    );
    // Neither: passes.
    land_adding(&g, a, &[("itoa", "1.0.1")]).await.unwrap();
}

#[tokio::test]
async fn both_gates_report_when_one_advisory_trips_both() {
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0001", "CRITICAL");
    let a = g
        .base("[deps]\nblock_severity = \"high\"\nblock_exploited = true\n")
        .await;
    let e = land_adding(&g, a, &[("serde", "1.0.100")])
        .await
        .unwrap_err();
    let lines = paths(&e);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("advisories at or above high")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("treats as exploited")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("(known exploited)")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn only_exploited_keeps_what_kev_lists_and_open_exploited_ignores_dismissed() {
    let t = with_feeds(Some(KEV_FILE), Some(EPSS_FILE), None).await;
    let only = wire::ListDepFindingsRequest {
        only_exploited: true,
        ..Default::default()
    };
    let r = t.h.list(only.clone()).await.unwrap();
    assert_eq!(names(&r), ["serde"], "an EPSS row alone is not KEV");
    assert_eq!(r.open_exploited, 1);
    let both =
        t.h.list(wire::ListDepFindingsRequest {
            only_exploited: true,
            ..exploited()
        })
        .await
        .unwrap();
    assert_eq!(names(&both), ["serde"]);
    let id = r.findings[0].id;
    t.h.hub
        .dismiss(
            &ctx(ALICE),
            wire::DismissDepFindingRequest {
                repo: repo(),
                id,
                reason: wire::DepDismissReason::Tolerable as i32,
                note: String::new(),
            },
        )
        .await
        .unwrap();
    let r = t.h.list(only).await.unwrap();
    assert_eq!(
        names(&r),
        ["serde"],
        "the filter still shows the dismissed finding"
    );
    assert_eq!(state_of(&r.findings[0]), wire::DepFindingState::Dismissed);
    assert_eq!(r.open_exploited, 0, "a dismissed finding is not open");
}

// ------------------------------------------------------------------------------ Sbom

fn sbom_req(rev: &str, ecosystems: &[&str]) -> wire::SbomRequest {
    wire::SbomRequest {
        repo: repo(),
        rev: rev.into(),
        ecosystems: ecosystems.iter().map(|e| (*e).to_owned()).collect(),
    }
}

#[tokio::test]
async fn an_sbom_lists_the_components_of_a_revision_and_sends_nothing_anywhere() {
    let h = setup().await;
    let cs = h
        .trunk(&[(
            "Cargo.lock",
            lock(&[("serde", "1.0.100"), ("regex", "1.5.0")]).as_bytes(),
        )])
        .await;
    let before = h.mock.hits();
    let r = h.hub.sbom(&ctx(BOB), sbom_req("", &[])).await.unwrap();
    assert_eq!(r.scanned.as_ref(), cs.as_bytes());
    assert!(!r.truncated);
    assert!(r.notes.is_empty(), "{:?}", r.notes);
    let doc: serde_json::Value = serde_json::from_slice(&r.document).unwrap();
    assert_eq!(doc["bomFormat"], "CycloneDX");
    assert_eq!(doc["specVersion"], "1.6");
    let comps = doc["components"].as_array().unwrap();
    assert_eq!(comps.len(), r.components as usize);
    let purls: Vec<&str> = comps.iter().filter_map(|c| c["purl"].as_str()).collect();
    assert!(purls.contains(&"pkg:cargo/serde@1.0.100"), "{purls:?}");
    assert!(purls.contains(&"pkg:cargo/regex@1.5.0"), "{purls:?}");
    // The same revision gives the same bytes, named by id and with its ecosystem.
    let again = h
        .hub
        .sbom(&ctx(BOB), sbom_req(&hex(cs.as_bytes()), &["crates.io"]))
        .await
        .unwrap();
    assert_eq!(again.document, r.document);
    assert_eq!(
        h.mock.hits(),
        before,
        "no advisory source, registry or feed was asked"
    );
    // No scan was recorded.
    assert!(h.findings(&[]).await.findings.is_empty());
}

#[tokio::test]
async fn an_sbom_refuses_an_unknown_ecosystem_and_an_unknown_revision() {
    let h = setup().await;
    h.trunk(&[("Cargo.lock", lock(&[("serde", "1.0.100")]).as_bytes())])
        .await;
    let e = h
        .hub
        .sbom(&ctx(BOB), sbom_req("", &["cobol"]))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let e = h
        .hub
        .sbom(&ctx(BOB), sbom_req("no-such-rev", &[]))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::NotFound);
}

#[tokio::test]
async fn an_sbom_needs_read_access_to_the_repository() {
    let h = setup().await;
    h.trunk(&[("Cargo.lock", lock(&[("serde", "1.0.100")]).as_bytes())])
        .await;
    let svc = DepsRpc::new(h.hub.clone(), tokens(), ServerInfo::default());
    for token in ["bob-r", "bob-w", "alice-w"] {
        let ok = svc
            .sbom(call(sbom_req("", &[]), token, false))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ok.components, 1, "{token}");
    }
    let e = svc.sbom(Request::new(sbom_req("", &[]))).await.unwrap_err();
    assert!(
        matches!(e.code(), Code::Unauthenticated | Code::PermissionDenied),
        "{e}"
    );
    let e = svc
        .sbom(call(sbom_req("", &[]), "unknown-token", false))
        .await
        .unwrap_err();
    assert!(
        matches!(e.code(), Code::Unauthenticated | Code::PermissionDenied),
        "{e}"
    );
}

#[tokio::test]
async fn an_sbom_flags_a_truncated_list() {
    let t = with_feeds(None, None, Some(2)).await;
    let r = t.h.hub.sbom(&ctx(BOB), sbom_req("", &[])).await.unwrap();
    assert!(r.truncated, "four packages exceed a limit of two");
    assert_eq!(r.components, 2);
    let doc: serde_json::Value = serde_json::from_slice(&r.document).unwrap();
    assert_eq!(doc["components"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn an_incomplete_scan_refuses_the_exploitation_gate_whatever_block_unknown_says() {
    // The advisory lookup fails.
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    let a = g
        .base("[deps]\nblock_exploited = true\nblock_unknown = false\n")
        .await;
    g.h.mock.with(|s| s.fail_osv = true);
    let e = land_adding(&g, a, &[("serde", "1.0.100")])
        .await
        .unwrap_err();
    assert_eq!(code(&e), "POLICY_DENIED");
    let lines = paths(&e);
    assert!(
        lines.iter().any(|l| l.contains("lookup failed")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("serde 1.0.100")),
        "{lines:?}"
    );
    // The same with the severity key alone is accepted, as before.
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    let a = g
        .base("[deps]\nblock_severity = \"high\"\nblock_unknown = false\n")
        .await;
    g.h.mock.with(|s| s.fail_osv = true);
    land_adding(&g, a, &[("serde", "1.0.100")]).await.unwrap();
    // More packages than the gate looks up.
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    let a = g
        .base("[deps]\nepss_threshold = 0.5\nblock_unknown = false\n")
        .await;
    let names: Vec<String> = (0..=super::hub::MAX_GATE_PACKAGES)
        .map(|i| format!("pkg{i}"))
        .collect();
    let extra: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "1.0.0")).collect();
    let e = land_adding(&g, a, &extra).await.unwrap_err();
    assert_eq!(code(&e), "POLICY_DENIED");
    assert!(
        paths(&e).iter().any(|l| l.contains("cannot be checked")),
        "{:?}",
        paths(&e)
    );
}

#[tokio::test]
async fn an_advisory_without_a_cve_alias_is_not_listed_while_both_feeds_are_current() {
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    // The id is no CVE and the advisory has no aliases: neither feed knows it, and both are current.
    advisory(&g.h, "serde", "1.0.100", "RUSTSEC-2024-9999", "CRITICAL");
    let a = g
        .base("[deps]\nblock_exploited = true\nepss_threshold = 0.1\n")
        .await;
    land_adding(&g, a, &[("serde", "1.0.100")]).await.unwrap();
}

#[tokio::test]
async fn a_zero_threshold_with_block_exploited_off_means_off_at_the_gate() {
    let (g, _keep) = gate_feeds(Some(KEV_FILE), Some(EPSS_FILE)).await;
    advisory(&g.h, "serde", "1.0.100", "CVE-2024-0002", "CRITICAL");
    let a = g
        .base("[deps]\nblock_exploited = false\nepss_threshold = 0\n")
        .await;
    // The advisory has EPSS 0.9 and is critical; nothing is switched on.
    land_adding(&g, a, &[("serde", "1.0.100")]).await.unwrap();
}
