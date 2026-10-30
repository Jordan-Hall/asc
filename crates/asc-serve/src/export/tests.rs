//! Tests of the data export service.

use std::sync::Arc;
use std::time::Duration;

use asc_rpc::forwarded::Role;
use asc_rpc::server::{ServerInfo, wire};
use asc_rpc::services::data_export_server::DataExport;
use futures::StreamExt as _;
use tonic::Code;

use super::rpc::ExportService;
use super::{ExportHub, READY};
use crate::settings::testkit::{Kit, Recorder, claim, kit, op, req};

struct Fx {
    k: Kit,
    hub: Arc<ExportHub>,
    svc: ExportService,
    audit: Arc<Recorder>,
}

async fn fx() -> Fx {
    let k = kit().await;
    let audit = Arc::new(Recorder::default());
    let hub = ExportHub::new(k.pool.clone(), k.dir.path().join("exports"), audit.clone());
    let svc = ExportService::new(hub.clone(), k.authz.clone(), ServerInfo::default());
    Fx { k, hub, svc, audit }
}

fn me<T>(f: &Fx, msg: T, login: &str, role: Role, n: u32) -> tonic::Request<T> {
    claim(
        req(msg, "host-token", Some(&op(n))),
        &f.k,
        &format!("7:{login}"),
        role,
        &format!("user:{login}"),
    )
}

async fn seed(f: &Fx) {
    let p = &f.k.pool;
    for (id, owner, secret) in [
        ("ak_a", "alice", "sealed-alice"),
        ("ak_b", "bob", "sealed-bob"),
    ] {
        sqlx::query(
            "INSERT INTO ai_keys (id, owner_kind, owner_name, label, provider, sealed_key, key_gen, key_hint, state, created, updated) \
             VALUES ($1, 1, $2, $1, 4, $3, 1, 'abcd', 2, 1, 1)",
        )
        .bind(id.to_owned())
        .bind(owner.to_owned())
        .bind(secret.as_bytes().to_vec())
        .execute(p)
        .await
        .unwrap();
    }
    for (id, author, body) in [
        (1, "7:alice", "looks good"),
        (2, "8:bob", "bobs private note"),
        (3, "8:alicia", "alicia"),
    ] {
        sqlx::query("INSERT INTO review_comments (ns, id, thread, author, body, created) VALUES ($1, $2, 1, $3, $4, 5)")
            .bind(vec![1u8])
            .bind(id)
            .bind(author.to_owned())
            .bind(body.to_owned())
            .execute(p)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO auth_tokens (id, kind, principal, digest, label, created) VALUES ('t1', 'pat', 'pat:alice', $1, 'cli', 9)")
        .bind(vec![9u8; 32])
        .execute(p)
        .await
        .unwrap();
}

async fn ready(f: &Fx, id: &str) -> super::Job {
    for _ in 0..200 {
        let j = f.hub.get("alice", id).await.unwrap();
        if j.state != super::PENDING && j.state != super::RUNNING {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("export did not finish");
}

async fn download(f: &Fx, id: &str, login: &str) -> Result<Vec<u8>, tonic::Status> {
    let r = f
        .svc
        .download(me(
            f,
            wire::DownloadExportRequest { id: id.into() },
            login,
            Role::Write,
            50,
        ))
        .await?;
    let mut s = r.into_inner();
    let mut out = Vec::new();
    while let Some(c) = s.next().await {
        let c = c?;
        assert_eq!(c.offset as usize, out.len());
        out.extend_from_slice(&c.data);
        if c.last {
            break;
        }
    }
    Ok(out)
}

#[tokio::test(flavor = "current_thread")]
async fn an_export_holds_only_the_callers_rows_and_no_secret() {
    let f = fx().await;
    seed(&f).await;
    let job = f
        .svc
        .request_export(me(
            &f,
            wire::RequestExportRequest {},
            "alice",
            Role::Write,
            1,
        ))
        .await
        .unwrap()
        .into_inner()
        .job
        .unwrap();
    let j = ready(&f, &job.id).await;
    assert_eq!(j.state, READY, "{}", j.error);
    assert!(j.expires > j.finished && j.size > 0);
    let bytes = download(&f, &job.id, "alice").await.unwrap();
    assert_eq!(bytes.len() as i64, j.size);
    let text = String::from_utf8(bytes).unwrap();
    for must in [
        "\"table\":\"ai_keys\"",
        "ak_a",
        "looks good",
        "\"table\":\"access_tokens\"",
        "\"cli\"",
    ] {
        assert!(text.contains(must), "{must}\n{text}");
    }
    // Nothing of bob's or alicia's, and no secret column or value.
    for never in [
        "ak_b",
        "bobs private note",
        "alicia",
        "sealed-alice",
        "sealed-bob",
        "digest",
        "sealed_key",
        "secret_sha256",
    ] {
        assert!(!text.contains(never), "{never}");
    }
    // Every line is a JSON object.
    for l in text.lines() {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        assert!(v["table"].is_string() && v["row"].is_object());
    }
    // File mode and audit.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(f.hub.path(&job.id))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert_eq!(f.audit.of("export.requested").len(), 1);
    let dl = f.audit.of("export.downloaded");
    assert_eq!(
        (dl.len(), dl[0].1.as_str(), dl[0].2.as_deref()),
        (1, "7:alice", Some("host-service"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn another_persons_job_is_not_found_and_other_accounts_are_refused() {
    let f = fx().await;
    let job = f
        .svc
        .request_export(me(
            &f,
            wire::RequestExportRequest {},
            "alice",
            Role::Write,
            1,
        ))
        .await
        .unwrap()
        .into_inner()
        .job
        .unwrap();
    ready(&f, &job.id).await;
    // Bob asks for alice's job by id: not found, for get, download and delete.
    assert_eq!(
        f.svc
            .get_export(me(
                &f,
                wire::GetExportRequest { id: job.id.clone() },
                "bob",
                Role::Read,
                2
            ))
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        download(&f, &job.id, "bob").await.unwrap_err().code(),
        Code::NotFound
    );
    assert_eq!(
        f.svc
            .delete_export(me(
                &f,
                wire::DeleteExportRequest { id: job.id.clone() },
                "bob",
                Role::Write,
                3
            ))
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert!(
        f.svc
            .list_exports(me(&f, wire::ListExportsRequest {}, "bob", Role::Read, 4))
            .await
            .unwrap()
            .into_inner()
            .jobs
            .is_empty()
    );
    // A read claim cannot request; an organisation or instance claim is not the account; the host alone has no person.
    assert_eq!(
        f.svc
            .request_export(me(
                &f,
                wire::RequestExportRequest {},
                "carol",
                Role::Read,
                5
            ))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let org = claim(
        req(wire::ListExportsRequest {}, "host-token", None),
        &f.k,
        "9:owner",
        Role::Admin,
        "org:acme",
    );
    assert_eq!(
        f.svc.list_exports(org).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    let host = req(wire::ListExportsRequest {}, "host-token", None);
    assert_eq!(
        f.svc.list_exports(host).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    // Her own token works.
    assert!(
        f.svc
            .list_exports(req(wire::ListExportsRequest {}, "alice-token", None))
            .await
            .is_ok()
    );
    // A malformed id reaches no query.
    assert_eq!(
        f.svc
            .get_export(me(
                &f,
                wire::GetExportRequest { id: "../x".into() },
                "alice",
                Role::Read,
                6
            ))
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
}

#[tokio::test(flavor = "current_thread")]
async fn one_at_a_time_one_a_day_expiry_and_delete() {
    let f = fx().await;
    let ask = |n| {
        f.svc.request_export(me(
            &f,
            wire::RequestExportRequest {},
            "alice",
            Role::Write,
            n,
        ))
    };
    let job = ask(1).await.unwrap().into_inner().job.unwrap();
    // A second at once: busy (or, if the first already finished, too soon).
    let e = ask(2).await.unwrap_err();
    assert!(
        matches!(e.code(), Code::FailedPrecondition | Code::ResourceExhausted),
        "{e:?}"
    );
    ready(&f, &job.id).await;
    assert_eq!(ask(3).await.unwrap_err().code(), Code::ResourceExhausted);
    // Expiry deletes the file; the record says so; a download is refused.
    sqlx::query("UPDATE export_jobs SET expires = 1 WHERE id = $1")
        .bind(job.id.clone())
        .execute(&f.k.pool)
        .await
        .unwrap();
    assert_eq!(
        download(&f, &job.id, "alice").await.unwrap_err().code(),
        Code::FailedPrecondition
    );
    assert_eq!(f.hub.sweep().await, 1);
    assert!(!f.hub.path(&job.id).exists());
    assert_eq!(
        f.hub.get("alice", &job.id).await.unwrap().state,
        super::EXPIRED
    );
    // Delete removes the record and the file; again is not found.
    sqlx::query("UPDATE export_jobs SET state = $2, expires = $3 WHERE id = $1")
        .bind(job.id.clone())
        .bind(READY)
        .bind(crate::ai::clock::now() + 100)
        .execute(&f.k.pool)
        .await
        .unwrap();
    let del = |n| {
        f.svc.delete_export(me(
            &f,
            wire::DeleteExportRequest { id: job.id.clone() },
            "alice",
            Role::Write,
            n,
        ))
    };
    del(4).await.unwrap();
    assert_eq!(del(5).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(f.audit.of("export.deleted").len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_download_counts_as_bandwidth_of_the_persons_export_pseudo_repository() {
    use crate::settings::access::Subject;
    let f = fx().await;
    let usage = crate::usage::UsageHub::new(
        f.k.pool.clone(),
        crate::usage::UsageConfig::default(),
        f.audit.clone(),
    );
    f.hub.set_usage(usage.clone());
    let job = f
        .svc
        .request_export(me(
            &f,
            wire::RequestExportRequest {},
            "alice",
            Role::Write,
            1,
        ))
        .await
        .unwrap()
        .into_inner()
        .job
        .unwrap();
    let j = ready(&f, &job.id).await;
    let bytes = download(&f, &job.id, "alice").await.unwrap();
    assert_eq!(bytes.len() as i64, j.size);
    let now = crate::ai::clock::now();
    let want = u64::try_from(j.size).unwrap();
    for _ in 0..100 {
        let t = usage
            .totals(
                &Subject::User("alice".into()),
                crate::usage::Period::parse("", now).unwrap(),
                now,
            )
            .await
            .unwrap();
        if t.bandwidth_bytes >= want {
            assert_eq!(t.bandwidth_bytes, want);
            assert_eq!(
                t.repos
                    .iter()
                    .find(|r| r.repo == "alice/.data-export")
                    .unwrap()
                    .bandwidth_bytes,
                want
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the download was not counted");
}
