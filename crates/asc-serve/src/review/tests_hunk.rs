//! `Review.HunkContext`: the chain of definitions around a line, for a TSRX component with its
//! control-flow scopes, for Rust, and for a file with no grammar; and its refusals.

use asc_rpc::forwarded::Role;
use asc_rpc::server::wire;
use asc_rpc::services::review_server::Review;
use bytes::Bytes;
use tonic::Code;

use super::tests::{ALICE, Fx, cbytes, fx, repo_ref, req};
use super::tests_acting::claimkit::{self, host};
use crate::ingest::testutil::commit;

const TSRX: &[u8] = b"type Props = { users: string[] };\n\nfunction UserList({ users }: Props) @{\n  <ul>\n    @for (const user of users) {\n      @if (user) {\n        <li>{user}</li>\n      }\n    }\n  </ul>\n}\n";
const RUST: &[u8] =
    b"pub struct Parser;\nimpl Parser {\n    pub fn parse(&self) -> u32 {\n        1\n    }\n}\n";

async fn with_files(f: &Fx) {
    let cs = commit(
        &f.e,
        &[],
        &[
            ("src/List.tsrx", TSRX),
            ("src/lib.rs", RUST),
            ("notes.txt", b"plain text\n"),
        ],
    )
    .await;
    sqlx::query(
        "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) VALUES ($1, $2, 3, $3, NULL, 1, '42:alice', $4)",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(super::tests::cid(1).as_bytes().to_vec())
    .bind(cs.as_bytes().to_vec())
    .bind(vec![1u8, 3, 7])
    .execute(f.reg.pool())
    .await
    .unwrap();
}

async fn ctx(
    f: &Fx,
    token: &str,
    path: &str,
    lines: &[u32],
) -> Result<wire::HunkContextResponse, tonic::Status> {
    f.review
        .hunk_context(req(
            wire::HunkContextRequest {
                repo: repo_ref(),
                change: cbytes(1),
                version: 0,
                path: Bytes::copy_from_slice(path.as_bytes()),
                lines: lines.to_vec(),
            },
            token,
            false,
        ))
        .await
        .map(tonic::Response::into_inner)
}

fn names(l: &wire::HunkContextLine) -> Vec<&str> {
    l.chain.iter().map(|e| e.name.as_str()).collect()
}

#[tokio::test]
async fn a_hunk_header_names_the_component_and_the_scopes_around_the_line() {
    let f = fx().await;
    with_files(&f).await;
    let r = ctx(&f, ALICE, "src/List.tsrx", &[1, 4, 7, 11])
        .await
        .unwrap();
    assert_eq!(r.language, "tsrx");
    assert!(
        names(&r.lines[0]).is_empty() || names(&r.lines[0]) == ["Props"],
        "{:?}",
        r.lines[0]
    );
    assert_eq!(names(&r.lines[1]), ["UserList"]);
    assert_eq!(r.lines[1].chain[0].kind, "component");
    assert_eq!(
        names(&r.lines[2]),
        ["UserList", "@for (const user of users)", "@if (user)"]
    );
    assert_eq!(r.lines[2].chain[2].kind, "scope");
    let rust = ctx(&f, ALICE, "src/lib.rs", &[4]).await.unwrap();
    assert_eq!(rust.language, "rust");
    assert_eq!(names(&rust.lines[0]), ["Parser", "parse"]);
    let plain = ctx(&f, ALICE, "notes.txt", &[1]).await.unwrap();
    assert_eq!(plain.language, "");
    assert!(plain.lines[0].chain.is_empty());
}

#[tokio::test]
async fn hunk_context_refuses_what_it_cannot_answer() {
    let f = fx().await;
    with_files(&f).await;
    assert_eq!(
        ctx(&f, ALICE, "missing.rs", &[1]).await.unwrap_err().code(),
        Code::NotFound
    );
    assert_eq!(
        ctx(&f, ALICE, "", &[1]).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    let many: Vec<u32> = (1..=201).collect();
    assert_eq!(
        ctx(&f, ALICE, "src/lib.rs", &many)
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let unknown = f
        .review
        .hunk_context(req(
            wire::HunkContextRequest {
                repo: repo_ref(),
                change: cbytes(9),
                version: 0,
                path: Bytes::from_static(b"a"),
                lines: vec![1],
            },
            ALICE,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), Code::NotFound);
    let old = f
        .review
        .hunk_context(req(
            wire::HunkContextRequest {
                repo: repo_ref(),
                change: cbytes(1),
                version: 99,
                path: Bytes::from_static(b"a"),
                lines: vec![1],
            },
            ALICE,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(old.code(), Code::InvalidArgument);
    // Another repository's claim reads nothing.
    let (a, v) = claimkit::authorizer();
    let svc = super::rpc::ReviewService::new(
        std::sync::Arc::new(super::Reviews::new(f.reg.clone())),
        a,
        asc_rpc::server::ServerInfo::default(),
    );
    let m = || wire::HunkContextRequest {
        repo: repo_ref(),
        change: cbytes(1),
        version: 0,
        path: Bytes::from_static(b"src/lib.rs"),
        lines: vec![4],
    };
    assert_eq!(
        svc.hunk_context(host(&v, m(), "42:alice", Role::Admin, "o/other", false))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert!(
        svc.hunk_context(host(&v, m(), "42:alice", Role::Read, "o/r", false))
            .await
            .is_ok()
    );
}
