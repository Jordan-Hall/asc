//! `Releases`: notes from landed changes, the next release, creating (with provenance and checksums),
//! drafts and publishing, paging, and who may do what.

use std::sync::Arc;

use asc_core::id::{ChangesetId, RefTarget};
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::forwarded::Role;
use asc_rpc::server::wire;
use asc_rpc::services::releases_server::Releases;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use tonic::Code;

use super::rpc::ReleasesService;
use super::{category, next_tag, valid_tag};
use crate::ingest::testutil::{sig, tree};
use crate::review::Reviews;
use crate::review::tests::{Fx, cid, fx, repo_ref, req};
use crate::review::tests_acting::claimkit::{self, host};

const READER: &str = "reader-token";
const WRITER: &str = "writer-token";

fn svc(f: &Fx) -> ReleasesService {
    let auth = StaticTokens::new()
        .with_token(READER, "42:alice", Access::Read)
        .with_token(WRITER, "44:carol", Access::Write);
    ReleasesService::new(
        Arc::new(Reviews::new(f.reg.clone())),
        Arc::new(auth),
        asc_rpc::server::ServerInfo::default(),
    )
}

async fn commit_with(
    f: &Fx,
    parent: Option<ChangesetId>,
    file: &str,
    message: &str,
) -> ChangesetId {
    let engine = f.e.backend.engine();
    let base =
        match parent {
            Some(p) => engine.get_changeset(p).await.unwrap().root,
            None => asc_engine::Engine::<
                asc_store::bucket::BucketObjects,
                asc_store::sqlrefs::SqlRefs,
            >::empty_tree_id(),
        };
    let root = tree(&f.e, base, &[(file, file.as_bytes())]).await;
    engine
        .create_changeset(
            NewChangeset {
                parents: parent.into_iter().collect(),
                author: sig(),
                committer: sig(),
                message: message.as_bytes().to_vec(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap()
}

async fn set_ref(f: &Fx, name: &str, cs: ChangesetId) {
    let txn = RefTxn {
        updates: vec![RefUpdate {
            name: asc_core::path::RefName::new(name.as_bytes()).unwrap(),
            expect: Expect::Any,
            new: Some(RefTarget::Changeset(cs)),
            expires_at: None,
        }],
        actor: Actor {
            id: "t".into(),
            via: "test".into(),
        },
        reason: "test".into(),
        op_id: uuid::Uuid::new_v4(),
    };
    f.reg.refs().update(f.e.key, &txn).await.unwrap();
}

/// base <- a "feat: add search" <- b "fix: crash on empty input" <- c "update the docs" <- d (skipped).
/// Trunk is at d, branch `v1` at base.
async fn world() -> (Fx, [ChangesetId; 5]) {
    let f = fx().await;
    let base = commit_with(&f, None, "base", "initial").await;
    let a = commit_with(&f, Some(base), "a", "feat: add search").await;
    let b = commit_with(&f, Some(a), "b", "fix: crash on empty input").await;
    let c = commit_with(&f, Some(b), "c", "update the docs").await;
    let d = commit_with(&f, Some(c), "d", "chore: tidy\n\nRelease-Note: skip").await;
    crate::graph_sql::index_for_tests(&f.e, f.reg.pool(), &[base, a, b, c, d]).await;
    set_ref(&f, "refs/heads/master", d).await;
    set_ref(&f, "refs/heads/v1", base).await;
    (f, [base, a, b, c, d])
}

fn create(tag: &str) -> wire::CreateReleaseRequest {
    wire::CreateReleaseRequest {
        repo: repo_ref(),
        tag: tag.into(),
        target: "v1".into(),
        ..Default::default()
    }
}

async fn make(f: &Fx, m: wire::CreateReleaseRequest) -> Result<wire::Release, tonic::Status> {
    svc(f)
        .create(req(m, WRITER, true))
        .await
        .map(|r| r.into_inner().release.unwrap())
}

fn asset(name: &str) -> wire::ReleaseAsset {
    wire::ReleaseAsset {
        name: name.into(),
        size: 12,
        sha256: "ab".repeat(32),
        url: format!("https://dl.example/{name}"),
        content_type: "application/gzip".into(),
    }
}

#[test]
fn tags_categories_and_the_next_tag() {
    assert!(valid_tag("v1.2.3") && valid_tag("release/2026-10") && valid_tag("1.0.0-rc.1+build"));
    for bad in [
        "",
        "-x",
        "/x",
        ".x",
        "x/",
        "x.",
        "a//b",
        "a..b",
        "a b",
        "a\nb",
        &"x".repeat(129),
    ] {
        assert!(!valid_tag(bad), "{bad:?}");
    }
    assert_eq!(category("feat: add x"), Some("added"));
    assert_eq!(category("Add x"), Some("added"));
    assert_eq!(category("fix: y"), Some("fixed"));
    assert_eq!(category("Bugfix y"), Some("fixed"));
    assert_eq!(category("tidy up"), Some("changed"));
    assert_eq!(category("feat: x\n\nRelease-Note: fixed"), Some("fixed"));
    assert_eq!(category("fix: x\n\nRelease-Note: skip"), None);
    assert_eq!(next_tag("v2.18.0"), "v2.18.1");
    assert_eq!(next_tag("2.0.9"), "2.0.10");
    for none in ["", "v2", "v2.1", "v1.2.3.4", "v1.x.3", "latest", "v1.2.-3"] {
        assert_eq!(next_tag(none), "", "{none:?}");
    }
}

#[tokio::test]
async fn notes_are_written_from_the_changes_since_the_previous_release() {
    let (f, [_base, _a, b, c, _d]) = world().await;
    // A change known to the index: b is alice's change, c an agent's.
    for (cs, change, actor) in [(b, 7u8, "42:alice"), (c, 8u8, "agent:helper")] {
        sqlx::query(
            "INSERT INTO change_versions (ns, change, v, cs, parent_change, at, actor, op_id) VALUES ($1, $2, 1, $3, NULL, 1, $4, $5)",
        )
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(cid(change).as_bytes().to_vec())
        .bind(cs.as_bytes().to_vec())
        .bind(actor)
        .bind(vec![change])
        .execute(f.reg.pool())
        .await
        .unwrap();
    }
    // Before any release the notes start at the first changeset.
    let all = svc(&f)
        .draft_notes(req(
            wire::DraftReleaseNotesRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            READER,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        all.total, 4,
        "initial, add, fix and docs; the skipped one is left out"
    );
    // The release at `v1` (base): the notes since it are three changes.
    make(&f, create("v1.0.0")).await.unwrap();
    let got = svc(&f)
        .draft_notes(req(
            wire::DraftReleaseNotesRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            READER,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.from_tag, "v1.0.0");
    let seen: Vec<(&str, &str, &str, bool)> = got
        .notes
        .iter()
        .map(|n| {
            (
                n.category.as_str(),
                n.subject.as_str(),
                n.author.as_str(),
                n.by_agent,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("changed", "update the docs", "helper", true),
            ("fixed", "fix: crash on empty input", "alice", false),
            ("added", "feat: add search", "A", false),
        ]
    );
    assert_eq!(got.notes[1].change.as_ref(), cid(7).as_bytes());
    assert!(
        got.notes[2].change.is_empty(),
        "a changeset no change names"
    );
    assert!(!got.truncated);
    let md = &got.markdown;
    let (added, changed, fixed) = (
        md.find("### Added").unwrap(),
        md.find("### Changed").unwrap(),
        md.find("### Fixed").unwrap(),
    );
    assert!(added < changed && changed < fixed, "{md}");
    assert!(
        md.contains("- feat: add search (A)") && md.contains("change 07070707"),
        "{md}"
    );
    // An explicit range.
    let one = svc(&f)
        .draft_notes(req(
            wire::DraftReleaseNotesRequest {
                repo: repo_ref(),
                from: "v1".into(),
                to: "master".into(),
            },
            READER,
            false,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((one.total, one.from_tag.as_str()), (3, "v1"));
    // An unknown revision is not found.
    let err = svc(&f)
        .draft_notes(req(
            wire::DraftReleaseNotesRequest {
                repo: repo_ref(),
                from: "nope".into(),
                ..Default::default()
            },
            READER,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn the_next_release_counts_the_changes_since_the_latest_and_proposes_a_tag() {
    let (f, _) = world().await;
    let s = svc(&f);
    let ask_as = |token: &'static str| {
        s.next(req(
            wire::NextReleaseRequest { repo: repo_ref() },
            token,
            false,
        ))
    };
    let ask = || ask_as(READER);
    let none = ask().await.unwrap().into_inner();
    assert_eq!(
        (none.latest_tag.as_str(), none.proposed_tag.as_str()),
        ("", "")
    );
    assert_eq!(none.changes_since, 5, "every changeset is new");
    make(&f, create("v1.4.2")).await.unwrap();
    let got = ask().await.unwrap().into_inner();
    assert_eq!(
        (
            got.latest_tag.as_str(),
            got.proposed_tag.as_str(),
            got.changes_since
        ),
        ("v1.4.2", "v1.4.3", 4)
    );
    // A draft is named, but is not the latest.
    make(
        &f,
        wire::CreateReleaseRequest {
            draft: true,
            ..create("v1.4.3")
        },
    )
    .await
    .unwrap();
    // The tag of a draft is for writers: a reader is not told it exists.
    let got = ask().await.unwrap().into_inner();
    assert_eq!(
        (got.latest_tag.as_str(), got.draft_tag.as_str()),
        ("v1.4.2", "")
    );
    let got = ask_as(WRITER).await.unwrap().into_inner();
    assert_eq!(
        (got.latest_tag.as_str(), got.draft_tag.as_str()),
        ("v1.4.2", "v1.4.3")
    );
}

#[tokio::test]
async fn a_release_keeps_its_provenance_and_the_checksums_of_its_assets() {
    let (f, [base, ..]) = world().await;
    // The run that built it: 3 jobs passed, 1 failed, 1 skipped.
    sqlx::query(
        "INSERT INTO check_runs (ns, id, source, external_id, name, change, version, status, created, url) \
         VALUES ($1, 1, 'actions', '4218', 'release', $2, 0, 3, 1, 'https://ci.example/runs/4218')",
    )
    .bind(f.e.ns.as_bytes().to_vec())
    .bind(Vec::<u8>::new())
    .execute(f.reg.pool())
    .await
    .unwrap();
    for (i, status) in [3i64, 3, 3, 4, 6].into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO check_jobs (ns, id, run, name, status) VALUES ($1, $2, 1, $3, $4)",
        )
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(i64::try_from(i).unwrap() + 1)
        .bind(format!("job{i}"))
        .bind(status)
        .execute(f.reg.pool())
        .await
        .unwrap();
    }
    // The release commit is signed by the server's key.
    sqlx::query(
        "INSERT INTO change_signatures (repo_id, changeset, signature, key_id, status, signed_at) VALUES ($1, $2, 'sig', 'server', $3, 1)",
    )
    .bind(f.e.key.0)
    .bind(base.as_bytes().to_vec())
    .bind(wire::VerificationStatus::Valid as i64)
    .execute(f.reg.pool())
    .await
    .unwrap();
    let r = make(
        &f,
        wire::CreateReleaseRequest {
            title: "First".into(),
            notes: "Hello".into(),
            channel: wire::ReleaseChannel::PreRelease as i32,
            build_source: "actions".into(),
            build_run: "4218".into(),
            assets: vec![asset("asc-linux.tar.gz"), asset("asc-macos.tar.gz")],
            ..create("v1.0.0-rc.1")
        },
    )
    .await
    .unwrap();
    assert_eq!(
        (r.tag.as_str(), r.title.as_str(), r.notes.as_str()),
        ("v1.0.0-rc.1", "First", "Hello")
    );
    assert_eq!(r.channel, wire::ReleaseChannel::PreRelease as i32);
    assert!(!r.draft && r.published > 0);
    assert_eq!(r.target.as_ref(), base.as_bytes());
    assert_eq!((r.created_by.as_str(), r.created_by_id), ("carol", 44));
    assert_eq!(
        r.changes, 1,
        "the release is cut from base: only base itself is new"
    );
    let p = r.provenance.clone().unwrap();
    assert_eq!(
        (
            p.build_source.as_str(),
            p.build_run.as_str(),
            p.build_url.as_str(),
            p.built_from.as_str()
        ),
        ("actions", "4218", "https://ci.example/runs/4218", "v1")
    );
    assert_eq!((p.checks_passed, p.checks_total), (3, 4));
    assert_eq!((p.signed_by.as_str(), p.signature_valid), ("server", true));
    assert_eq!(r.assets.len(), 2);
    assert_eq!(r.assets[0].sha256, "ab".repeat(32));
    // A title defaults to the tag, and a build run nobody reported is not found.
    let plain = make(&f, create("v1.0.0")).await.unwrap();
    assert_eq!(plain.title, "v1.0.0");
    assert_eq!(plain.previous_tag, "v1.0.0-rc.1");
    assert_eq!(plain.provenance.unwrap().signed_by, "server");
    let err = make(
        &f,
        wire::CreateReleaseRequest {
            build_source: "actions".into(),
            build_run: "9999".into(),
            ..create("v1.0.1")
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    // The tag is taken once.
    let err = make(&f, create("v1.0.0")).await.unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);
    // It reads back the same.
    let got = svc(&f)
        .get(req(
            wire::GetReleaseRequest {
                repo: repo_ref(),
                tag: "v1.0.0-rc.1".into(),
            },
            READER,
            false,
        ))
        .await
        .unwrap()
        .into_inner()
        .release
        .unwrap();
    assert_eq!(got, r);
}

#[tokio::test]
async fn bad_releases_are_refused_before_anything_is_stored() {
    let (f, _) = world().await;
    let refused = |m: wire::CreateReleaseRequest| {
        let f = &f;
        async move { make(f, m).await.unwrap_err().code() }
    };
    for bad in ["", "a b", "-x", "a..b"] {
        assert_eq!(refused(create(bad)).await, Code::InvalidArgument, "{bad:?}");
    }
    let mut cases: Vec<wire::CreateReleaseRequest> = Vec::new();
    cases.push(wire::CreateReleaseRequest {
        title: "a\nb".into(),
        ..create("t1")
    });
    cases.push(wire::CreateReleaseRequest {
        notes: "x".repeat(64 * 1024 + 1),
        ..create("t2")
    });
    cases.push(wire::CreateReleaseRequest {
        channel: wire::ReleaseChannel::Draft as i32,
        ..create("t3")
    });
    cases.push(wire::CreateReleaseRequest {
        assets: vec![wire::ReleaseAsset {
            sha256: "AB".repeat(32),
            ..asset("x")
        }],
        ..create("t4")
    });
    cases.push(wire::CreateReleaseRequest {
        assets: vec![wire::ReleaseAsset {
            url: "http://dl.example/x".into(),
            ..asset("x")
        }],
        ..create("t5")
    });
    cases.push(wire::CreateReleaseRequest {
        assets: vec![wire::ReleaseAsset {
            name: "dir/x".into(),
            ..asset("x")
        }],
        ..create("t6")
    });
    cases.push(wire::CreateReleaseRequest {
        assets: vec![asset("x"), asset("x")],
        ..create("t7")
    });
    cases.push(wire::CreateReleaseRequest {
        assets: (0..101).map(|i| asset(&format!("f{i}"))).collect(),
        ..create("t8")
    });
    cases.push(wire::CreateReleaseRequest {
        build_source: "actions".into(),
        ..create("t9")
    });
    for c in cases {
        let tag = c.tag.clone();
        assert_eq!(refused(c).await, Code::InvalidArgument, "{tag}");
    }
    let n: i64 = sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM releases")
        .fetch_one(f.reg.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
    // An unknown target is not found.
    let err = make(
        &f,
        wire::CreateReleaseRequest {
            target: "nope".into(),
            ..create("t10")
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn a_draft_is_for_writers_until_it_is_published() {
    let (f, _) = world().await;
    make(&f, create("v1.0.0")).await.unwrap();
    make(
        &f,
        wire::CreateReleaseRequest {
            draft: true,
            ..create("v1.1.0")
        },
    )
    .await
    .unwrap();
    let s = svc(&f);
    let get = |tag: &str, token: &str| {
        s.get(req(
            wire::GetReleaseRequest {
                repo: repo_ref(),
                tag: tag.into(),
            },
            token,
            false,
        ))
    };
    let list = |token: &str, channel: wire::ReleaseChannel| {
        s.list(req(
            wire::ListReleasesRequest {
                repo: repo_ref(),
                channel: channel as i32,
                ..Default::default()
            },
            token,
            false,
        ))
    };
    // A reader sees the published one only; a writer sees both, and the counts follow.
    assert_eq!(
        get("v1.1.0", READER).await.unwrap_err().code(),
        Code::NotFound
    );
    assert!(
        get("v1.1.0", WRITER)
            .await
            .unwrap()
            .into_inner()
            .release
            .unwrap()
            .draft
    );
    let r = list(READER, wire::ReleaseChannel::Unspecified)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        r.releases
            .iter()
            .map(|x| x.tag.as_str())
            .collect::<Vec<_>>(),
        ["v1.0.0"]
    );
    assert_eq!((r.stable, r.pre_release, r.draft), (1, 0, 0));
    let w = list(WRITER, wire::ReleaseChannel::Unspecified)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        w.releases
            .iter()
            .map(|x| x.tag.as_str())
            .collect::<Vec<_>>(),
        ["v1.1.0", "v1.0.0"]
    );
    assert_eq!((w.stable, w.draft), (1, 1));
    assert!(
        list(READER, wire::ReleaseChannel::Draft)
            .await
            .unwrap()
            .into_inner()
            .releases
            .is_empty()
    );
    assert_eq!(
        list(WRITER, wire::ReleaseChannel::Draft)
            .await
            .unwrap()
            .into_inner()
            .releases
            .len(),
        1
    );
    // Only a writer publishes, and only once.
    let publish = |token: &str| {
        s.publish(req(
            wire::PublishReleaseRequest {
                repo: repo_ref(),
                tag: "v1.1.0".into(),
            },
            token,
            true,
        ))
    };
    assert_eq!(
        publish(READER).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    let done = publish(WRITER).await.unwrap().into_inner().release.unwrap();
    assert!(!done.draft && done.published > 0);
    assert_eq!(
        publish(WRITER).await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    assert!(get("v1.1.0", READER).await.is_ok());
    let missing = svc(&f)
        .publish(req(
            wire::PublishReleaseRequest {
                repo: repo_ref(),
                tag: "nope".into(),
            },
            WRITER,
            true,
        ))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);
}

#[tokio::test]
async fn releases_page_newest_first_and_filter_by_channel() {
    let (f, _) = world().await;
    for (tag, ch) in [
        ("v1", wire::ReleaseChannel::Stable),
        ("v2-rc", wire::ReleaseChannel::PreRelease),
        ("v2", wire::ReleaseChannel::Stable),
    ] {
        make(
            &f,
            wire::CreateReleaseRequest {
                channel: ch as i32,
                ..create(tag)
            },
        )
        .await
        .unwrap();
    }
    let s = svc(&f);
    let page = |token: Option<String>, channel: wire::ReleaseChannel, limit: u32| {
        s.list(req(
            wire::ListReleasesRequest {
                repo: repo_ref(),
                channel: channel as i32,
                limit,
                page_token: token.unwrap_or_default(),
            },
            READER,
            false,
        ))
    };
    let first = page(None, wire::ReleaseChannel::Unspecified, 2)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        first
            .releases
            .iter()
            .map(|x| x.tag.as_str())
            .collect::<Vec<_>>(),
        ["v2", "v2-rc"]
    );
    assert!(!first.next_page_token.is_empty());
    assert_eq!((first.stable, first.pre_release), (2, 1));
    let second = page(
        Some(first.next_page_token),
        wire::ReleaseChannel::Unspecified,
        2,
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        second
            .releases
            .iter()
            .map(|x| x.tag.as_str())
            .collect::<Vec<_>>(),
        ["v1"]
    );
    assert!(second.next_page_token.is_empty());
    let pre = page(None, wire::ReleaseChannel::PreRelease, 0)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(pre.releases.len(), 1);
    // A page token of another call is refused.
    let err = page(Some("junk".into()), wire::ReleaseChannel::Unspecified, 2)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn who_may_ask_and_who_may_write() {
    let (f, _) = world().await;
    let s = svc(&f);
    assert_eq!(
        s.create(req(create("v9"), READER, true))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        s.create(req(create("v9"), "no-such-token", true))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    assert_eq!(
        s.list(req(
            wire::ListReleasesRequest {
                repo: repo_ref(),
                ..Default::default()
            },
            "no-such-token",
            false
        ))
        .await
        .unwrap_err()
        .code(),
        Code::Unauthenticated
    );
    // The hub's claims: another repository's grants nothing; a read claim cannot create, a write claim can.
    let (a, v) = claimkit::authorizer();
    let s = ReleasesService::new(
        Arc::new(Reviews::new(f.reg.clone())),
        a,
        asc_rpc::server::ServerInfo::default(),
    );
    assert_eq!(
        s.create(host(
            &v,
            create("v9"),
            "44:carol",
            Role::Admin,
            "o/other",
            true
        ))
        .await
        .unwrap_err()
        .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        s.create(host(&v, create("v9"), "44:carol", Role::Read, "o/r", true))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let made = s
        .create(host(&v, create("v9"), "44:carol", Role::Write, "o/r", true))
        .await
        .unwrap()
        .into_inner()
        .release
        .unwrap();
    assert_eq!(
        (made.created_by.as_str(), made.created_by_id),
        ("carol", 44)
    );
}
