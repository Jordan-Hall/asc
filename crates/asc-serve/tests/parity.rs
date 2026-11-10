//! Row-level parity over every list and search RPC (STATUS work unit 12, FEATURE-ROADMAP DoD 5).
//!
//! One table: each list or search RPC the server serves, called by a repository reader, a writer
//! and an administrator, by the host service carrying a signed `read` claim for the subject of
//! the call, and with no token at all. Each row pins, per caller, one of:
//!
//! * `O`: answered (or, where the row names it, stopped past the gate by the request itself, such
//!   as an object id that does not exist; an outsider never gets that far);
//! * `E`: answered with nothing of the subject: no hit, no row and no count (the filtering
//!   RPCs, which search or count over what the caller may read);
//! * `-`: refused without a trace of the subject: `NOT_FOUND`, `PERMISSION_DENIED` or
//!   `UNAUTHENTICATED`, never an `INVALID_ARGUMENT` about the request or an internal error.
//!
//! The repository has a pushed and indexed commit, so a reader's answer has something in it. A
//! change to any decision fails this test, so a visibility change is always deliberate.
//!
//! `Workspace/Search` is the local daemon's IPC and is not served by `asc serve`; it is not here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/serve_rig.rs"]
mod rig;

use std::sync::Arc;

use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::forwarded::{ForwardedVerifier, HOST_SERVICE_PRINCIPAL, Role};
use asc_rpc::server::wire;
use asc_serve::authz::forwarded::ForwardedAuthorizer;
use bytes::{Buf, BufMut};
use futures::StreamExt;
use prost::Message;
use rig::{Options, READ_TOKEN, REPO, WRITE_TOKEN};
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::transport::Channel;

const ORG: &str = "acme";
const KEY: [u8; 32] = [7; 32];
const ADMIN_TOKEN: &str = "parity-admin-0123456789";
const HOST_TOKEN: &str = "parity-host-0123456789";
const WORD: &str = "parity_marmot_word";

/// Who calls, in the order of a row's `want`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Caller {
    /// `11:bob`, who may read the repository.
    Read,
    /// `12:alice`, who may write it.
    Write,
    /// `13:dana`, who administers it.
    Admin,
    /// The host service with a signed `read` claim for `14:carol` on the row's subject.
    Claim,
    /// No token.
    Anon,
}

const CALLERS: [Caller; 5] = [
    Caller::Read,
    Caller::Write,
    Caller::Admin,
    Caller::Claim,
    Caller::Anon,
];

// ------------------------------------------------------------------------- the raw codec

/// Sends the bytes of an encoded request and keeps the answer's bytes: one client for every RPC.
#[derive(Clone, Copy, Default)]
struct Raw;

impl Codec for Raw {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = Raw;
    type Decoder = Raw;

    fn encoder(&mut self) -> Self::Encoder {
        Raw
    }

    fn decoder(&mut self) -> Self::Decoder {
        Raw
    }
}

impl Encoder for Raw {
    type Item = Vec<u8>;
    type Error = tonic::Status;

    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), tonic::Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for Raw {
    type Item = Vec<u8>;
    type Error = tonic::Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, tonic::Status> {
        let mut out = vec![0; src.remaining()];
        src.copy_to_slice(&mut out);
        Ok(Some(out))
    }
}

// ---------------------------------------------------------------------------- the table

/// Whether the messages of an answer show anything of the subject.
type Shows = fn(&[Vec<u8>]) -> bool;

/// One RPC.
struct Row {
    path: &'static str,
    body: Vec<u8>,
    stream: bool,
    /// What a claim is made for: `owner/name`, `org:<name>`, `user:<login>` or `instance`.
    subject: String,
    /// One of `O`, `E` and `-` per caller, in the order of [`CALLERS`].
    want: &'static str,
    /// An error message that shows the call got past the gate (the object it names is absent).
    admitted: Option<&'static str>,
    /// Whether the answer's messages show anything of the subject; rows with an `E` have one.
    shows: Option<Shows>,
}

fn row(path: &'static str, msg: &impl Message, subject: &str, want: &'static str) -> Row {
    assert_eq!(want.len(), CALLERS.len(), "{path}");
    Row {
        path,
        body: msg.encode_to_vec(),
        stream: false,
        subject: subject.to_owned(),
        want,
        admitted: None,
        shows: None,
    }
}

impl Row {
    fn admitted(mut self, message: &'static str) -> Self {
        self.admitted = Some(message);
        self
    }

    fn streaming(mut self) -> Self {
        self.stream = true;
        self
    }

    fn shows(mut self, f: Shows) -> Self {
        self.shows = Some(f);
        self
    }
}

fn repo() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: REPO.to_owned(),
        ..Default::default()
    })
}

fn owner(kind: wire::OwnerKind, name: &str) -> Option<wire::OwnerRef> {
    Some(wire::OwnerRef {
        kind: kind.into(),
        name: name.to_owned(),
    })
}

fn contributors_shown(msgs: &[Vec<u8>]) -> bool {
    msgs.iter().any(|m| {
        let r = wire::ListOwnerContributorsResponse::decode(m.as_slice()).unwrap();
        !r.contributors.is_empty() || r.landed > 0 || r.counted_repos > 0
    })
}

fn cross_hits_shown(msgs: &[Vec<u8>]) -> bool {
    msgs.iter().any(|m| {
        let r = wire::CrossSearchResponse::decode(m.as_slice()).unwrap();
        !r.repos.is_empty() || r.searched_repos > 0 || r.unindexed_repos > 0
    })
}

fn search_hits_shown(msgs: &[Vec<u8>]) -> bool {
    msgs.iter().any(|m| {
        !wire::SearchResponse::decode(m.as_slice())
            .unwrap()
            .files
            .is_empty()
    })
}

fn code_search() -> Row {
    row(
        "/asc.v1.CodeIntel/Search",
        &wire::SearchRequest {
            scope: Some(wire::Scope {
                repo: repo(),
                at: Some(wire::scope::At::Rev("refs/heads/master".to_owned())),
                ..Default::default()
            }),
            pattern: WORD.to_owned(),
            ..Default::default()
        },
        REPO,
        "OOOO-",
    )
    .streaming()
    .shows(search_hits_shown)
}

#[allow(clippy::too_many_lines)] // one table
fn rows() -> Vec<Row> {
    use wire::OwnerKind::{Org, Repo, User};
    let org = &format!("org:{ORG}");
    vec![
        // Agent runs: any reader of the repository; the tokens that act for agents, its admins.
        row(
            "/asc.v1.Agents/ListRuns",
            &wire::ListAgentRunsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Agents/ListRequests",
            &wire::ListAgentRequestsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Agents/ListCheckpoints",
            &wire::ListAgentCheckpointsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        )
        .admitted("invalid id"),
        row(
            "/asc.v1.Agents/ListAgents",
            &wire::ListRepoAgentsRequest { repo: repo() },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.AgentTokens/ListTokens",
            &wire::ListAgentTokensRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "--O--",
        ),
        // AI keys: the owner's administrators; a user is their own. A key that does not exist
        // answers alike for everyone.
        row(
            "/asc.v1.AiKeyShares/ListShares",
            &wire::ListAiKeySharesRequest {
                key_id: "k1".to_owned(),
            },
            "user:bob",
            "-----",
        ),
        row(
            "/asc.v1.AiKeyShares/ListSharedWithMe",
            &wire::ListSharedAiKeysRequest {
                grantee: owner(User, "bob"),
            },
            "user:bob",
            "O----",
        ),
        row(
            "/asc.v1.AiProviders/ListKeys",
            &wire::ListAiKeysRequest {
                owner: owner(User, "bob"),
            },
            "user:bob",
            "O----",
        ),
        row(
            "/asc.v1.AiProviders/ListKeys",
            &wire::ListAiKeysRequest {
                owner: owner(Org, ORG),
            },
            org,
            "-----",
        ),
        row(
            "/asc.v1.AiProviders/ListModels",
            &wire::ListAiModelsRequest {
                id: "k1".to_owned(),
            },
            "user:bob",
            "-----",
        ),
        row(
            "/asc.v1.AiProviders/ListFeatures",
            &wire::ListAiFeaturesRequest {
                owner: owner(User, "bob"),
            },
            "user:bob",
            "O----",
        ),
        row(
            "/asc.v1.AiProviders/ListActivity",
            &wire::ListAiActivityRequest {
                owner: owner(User, "bob"),
                ..Default::default()
            },
            "user:bob",
            "O----",
        )
        .admitted("key_id is required"),
        // The audit log of a repository: its administrators. The catalogue of actions: any
        // signed-in caller.
        row(
            "/asc.v1.Audit/List",
            &wire::ListAuditRequest {
                scope: owner(Repo, REPO),
                ..Default::default()
            },
            REPO,
            "--O--",
        ),
        row(
            "/asc.v1.Audit/ListActions",
            &wire::ListAuditActionsRequest {},
            "instance",
            "OOOO-",
        ),
        // Site administration: nobody here administers the site.
        row(
            "/asc.v1.Backups/ListBackups",
            &wire::ListBackupsRequest::default(),
            "instance",
            "-----",
        ),
        row(
            "/asc.v1.Backups/ListTargets",
            &wire::ListBackupTargetsRequest {},
            "instance",
            "-----",
        ),
        row(
            "/asc.v1.Maintenance/ListAgentRunners",
            &wire::ListAgentRunnersRequest {},
            "instance",
            "-----",
        ),
        row(
            "/asc.v1.Storage/ListRepoPlacements",
            &wire::ListRepoPlacementsRequest {
                repos: vec![REPO.to_owned()],
            },
            "instance",
            "-----",
        ),
        // Checks and their history: any reader.
        row(
            "/asc.v1.Ci/ListRuns",
            &wire::ListRunsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.CiInsights/ListFailureGroups",
            &wire::ListFailureGroupsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.CiInsights/ListFlakyTests",
            &wire::ListFlakyTestsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Cloud/ListWorkspaces",
            &wire::ListWorkspacesRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        // The caller's own exports; the host's claim is for carol, so carol's.
        row(
            "/asc.v1.DataExport/ListExports",
            &wire::ListExportsRequest {},
            "user:carol",
            "OOOO-",
        ),
        row(
            "/asc.v1.DepsService/ListFindings",
            &wire::ListDepFindingsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.DepsService/ListUpdates",
            &wire::ListDepUpdatesRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Inbox/List",
            &wire::InboxListRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Insights/ListContributors",
            &wire::ListContributorsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        // Counts over the named repositories the caller may read; the rest leave no trace.
        row(
            "/asc.v1.Insights/ListOwnerContributors",
            &wire::ListOwnerContributorsRequest {
                owner: ORG.to_owned(),
                repos: vec![REPO.to_owned()],
                ..Default::default()
            },
            org,
            "OOOOE",
        )
        .shows(contributors_shown),
        row(
            "/asc.v1.MergeQueue/List",
            &wire::ListQueueRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        // Organisation settings: its members (the hub's claim says who they are).
        row(
            "/asc.v1.OrgSettings/ListAgents",
            &wire::ListOrgAgentsRequest {
                org: ORG.to_owned(),
            },
            org,
            "---O-",
        ),
        // Path rules: the repository's administrators; teams: the organisation's.
        row(
            "/asc.v1.PathRules/ListPathRules",
            &wire::ListPathRulesRequest {
                repo: REPO.to_owned(),
            },
            REPO,
            "--O--",
        ),
        row(
            "/asc.v1.PathRules/ListTeams",
            &wire::ListTeamsRequest {
                org: ORG.to_owned(),
            },
            org,
            "-----",
        ),
        row(
            "/asc.v1.Releases/List",
            &wire::ListReleasesRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Repo/ListRefs",
            &wire::ListRefsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Repo/FindLanding",
            &wire::FindLandingRequest {
                repo: repo(),
                changeset: vec![0xab; 32].into(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Review/ListThreads",
            &wire::ListThreadsRequest {
                repo: repo(),
                change: vec![0xab; 16].into(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        )
        .admitted("unknown change"),
        row(
            "/asc.v1.Review/ListOverrides",
            &wire::ListOverridesRequest {
                repo: repo(),
                change: vec![0xab; 16].into(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        )
        .admitted("unknown change"),
        // Secret scanning: the rules are public to readers, the findings to administrators.
        row(
            "/asc.v1.SecretScan/ListRules",
            &wire::ListSecretRulesRequest { repo: repo() },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.SecretScan/ListFindings",
            &wire::ListSecretFindingsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "--O--",
        ),
        // A person's sessions: theirs alone.
        row(
            "/asc.v1.Sessions/ListSessions",
            &wire::ListSessionsRequest {
                login: "bob".to_owned(),
            },
            "user:bob",
            "O----",
        ),
        // Signing keys of a user who has registered none: the same "no such user" for all but
        // the user.
        row(
            "/asc.v1.Signing/ListKeys",
            &wire::ListSigningKeysRequest {
                owner: "bob".to_owned(),
            },
            "user:bob",
            "O----",
        ),
        row(
            "/asc.v1.Stacks/ListRefLog",
            &wire::ListRefLogRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        // Storage: owner and repository administrators.
        row(
            "/asc.v1.Storage/ListTargets",
            &wire::ListStorageTargetsRequest {
                owner: owner(Org, ORG),
            },
            org,
            "-----",
        ),
        row(
            "/asc.v1.Storage/ListMigrations",
            &wire::ListStorageMigrationsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "--O--",
        ),
        // An organisation's spend: its administrators.
        row(
            "/asc.v1.Usage/ListSpend",
            &wire::ListSpendRequest {
                owner: owner(Org, ORG),
                ..Default::default()
            },
            org,
            "-----",
        ),
        row(
            "/asc.v1.Usage/ListSpend",
            &wire::ListSpendRequest {
                owner: owner(User, "bob"),
                ..Default::default()
            },
            "user:bob",
            "O----",
        ),
        // Workflows: readers see definitions and runs; secret names are for administrators.
        row(
            "/asc.v1.Workflows/ListDefinitions",
            &wire::ListWorkflowDefinitionsRequest { repo: repo() },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Workflows/ListRuns",
            &wire::ListWorkflowRunsRequest {
                repo: repo(),
                ..Default::default()
            },
            REPO,
            "OOOO-",
        ),
        row(
            "/asc.v1.Workflows/ListSecrets",
            &wire::ListWorkflowSecretsRequest { repo: repo() },
            REPO,
            "--O--",
        ),
        code_search(),
        // Search across an organisation: hits from what the caller may read, nothing else. A
        // claim is for one subject and an organisation is not a repository, so a claim reads
        // nothing here; the hub names the repositories its person may read and searches as itself.
        row(
            "/asc.v1.CrossSearch/Search",
            &wire::CrossSearchRequest {
                target: Some(wire::CrossSearchTarget {
                    scope: owner(Org, ORG),
                    ..Default::default()
                }),
                pattern: WORD.to_owned(),
                ..Default::default()
            },
            org,
            "OOOEE",
        )
        .streaming()
        .shows(cross_hits_shown),
    ]
}

// ----------------------------------------------------------------------------- the calls

/// One call: the messages of the answer, or the code and message of a refusal.
async fn call(
    channel: &Channel,
    row: &Row,
    caller: Caller,
) -> Result<Vec<Vec<u8>>, (tonic::Code, String)> {
    let mut req = tonic::Request::new(row.body.clone());
    let token = match caller {
        Caller::Read => Some(READ_TOKEN),
        Caller::Write => Some(WRITE_TOKEN),
        Caller::Admin => Some(ADMIN_TOKEN),
        Caller::Claim => Some(HOST_TOKEN),
        Caller::Anon => None,
    };
    if let Some(t) = token {
        req.metadata_mut()
            .insert("authorization", format!("Bearer {t}").parse().unwrap());
    }
    if caller == Caller::Claim {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claim = ForwardedVerifier::new(KEY.to_vec()).unwrap().sign(
            "14:carol",
            Role::Read,
            &row.subject,
            i64::try_from(now).unwrap(),
        );
        for (k, v) in claim {
            req.metadata_mut().insert(k, v.parse().unwrap());
        }
    }
    let mut grpc = tonic::client::Grpc::new(channel.clone());
    grpc.ready().await.unwrap();
    let path = tonic::codegen::http::uri::PathAndQuery::from_static(row.path);
    let fail = |s: tonic::Status| (s.code(), s.message().to_owned());
    if row.stream {
        let mut stream = grpc
            .server_streaming(req, path, Raw)
            .await
            .map_err(fail)?
            .into_inner();
        let mut out = Vec::new();
        while let Some(m) = stream.next().await {
            out.push(m.map_err(fail)?);
        }
        Ok(out)
    } else {
        grpc.unary(req, path, Raw)
            .await
            .map(|r| vec![r.into_inner()])
            .map_err(fail)
    }
}

/// The letter `got` earns in `row`'s table, or `None` for an answer no caller may get.
fn grade(row: &Row, got: &Result<Vec<Vec<u8>>, (tonic::Code, String)>) -> Option<char> {
    match got {
        Ok(msgs) => match row.shows {
            Some(shows) if !shows(msgs) => Some('E'),
            _ => Some('O'),
        },
        Err((_, m)) if row.admitted == Some(m.as_str()) => Some('O'),
        Err((
            tonic::Code::NotFound | tonic::Code::PermissionDenied | tonic::Code::Unauthenticated,
            _,
        )) => Some('-'),
        Err(_) => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_list_and_search_rpc_answers_exactly_the_callers_the_platform_allows() {
    let tokens = StaticTokens::new()
        .with_token(READ_TOKEN, "11:bob", Access::Read)
        .with_token(WRITE_TOKEN, "12:alice", Access::Write)
        .with_token(ADMIN_TOKEN, "13:dana", Access::Admin)
        .with_token(HOST_TOKEN, HOST_SERVICE_PRINCIPAL, Access::Admin);
    let auth = Arc::new(ForwardedAuthorizer::new(
        Arc::new(tokens),
        ForwardedVerifier::new(KEY.to_vec()).unwrap(),
    ));
    let rig = rig::start_with(
        Options {
            codeintel: true,
            ..Options::default()
        },
        auth,
    )
    .await;
    let source = format!("pub fn {WORD}() -> u32 {{\n    1\n}}\n");
    let c1 = rig.commit(None, &[("src/lib.rs", &source)]).await;
    rig.push(None, c1).await;
    let channel = Channel::from_shared(rig.base.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let search = code_search();
    rig::until("the pushed commit to be searchable", 90, || async {
        matches!(call(&channel, &search, Caller::Read).await, Ok(m) if search_hits_shown(&m))
    })
    .await;

    let mut wrong = Vec::new();
    for row in rows() {
        for (caller, want) in CALLERS.into_iter().zip(row.want.chars()) {
            let got = call(&channel, &row, caller).await;
            if grade(&row, &got) != Some(want) {
                wrong.push(format!(
                    "{} as {caller:?}: want {want}, got {got:?}",
                    row.path
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The table itself: every list and search RPC in the protos is a row, so a new one cannot be
/// added without deciding who sees it.
#[test]
fn the_table_covers_every_list_and_search_rpc() {
    let protos = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto/asc/v1");
    let table: std::collections::BTreeSet<&str> = rows().iter().map(|r| r.path).collect();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&protos).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("proto") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let mut service = "";
        for line in text.lines().map(str::trim) {
            if let Some(rest) = line.strip_prefix("service ") {
                service = rest.split_whitespace().next().unwrap_or_default();
            } else if let Some(rest) = line.strip_prefix("rpc ") {
                let method = rest.split('(').next().unwrap_or_default().trim();
                let listing = ["List", "Search", "Find"]
                    .iter()
                    .any(|p| method.starts_with(p));
                let full = format!("/asc.v1.{service}/{method}");
                if listing && service != "Workspace" && !table.contains(full.as_str()) {
                    missing.push(full);
                }
            }
        }
    }
    assert!(missing.is_empty(), "rows missing: {missing:?}");
}
