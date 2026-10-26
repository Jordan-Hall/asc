//! Tests of the `Ci` service.

use asc_rpc::server::wire;
use asc_rpc::services::ci_server::Ci;
use bytes::Bytes;
use tonic::Status;

use crate::review::tests::{ADMIN, BOB, CAROL, Fx, cbytes, code, fx, repo_ref, req};

use wire::CiStatus as C;

fn job(name: &str, status: C) -> wire::ReportJob {
    wire::ReportJob {
        name: name.into(),
        status: status as i32,
        ..Default::default()
    }
}

fn run_req(id: &str, change: u8, status: C, jobs: Vec<wire::ReportJob>) -> wire::ReportRunRequest {
    wire::ReportRunRequest {
        repo: repo_ref(),
        source: "actions".into(),
        external_id: id.into(),
        name: "ci".into(),
        change: if change == 0 {
            Bytes::new()
        } else {
            cbytes(change)
        },
        status: status as i32,
        jobs,
        url: "https://ci.example/runs/1".into(),
        ..Default::default()
    }
}

async fn report(f: &Fx, m: wire::ReportRunRequest) -> Result<wire::CiRun, Status> {
    f.ci.report_run(req(m, ADMIN, false))
        .await
        .map(|r| r.into_inner().run.unwrap())
}

async fn runs(f: &Fx, change: u8, status: C) -> wire::ListRunsResponse {
    f.ci.list_runs(req(
        wire::ListRunsRequest {
            repo: repo_ref(),
            change: if change == 0 {
                Bytes::new()
            } else {
                cbytes(change)
            },
            status: status as i32,
            ..Default::default()
        },
        BOB,
        false,
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn append(
    f: &Fx,
    run: u64,
    job: u64,
    data: &[u8],
) -> Result<wire::AppendJobLogResponse, Status> {
    f.ci.append_job_log(req(
        wire::AppendJobLogRequest {
            repo: repo_ref(),
            run_id: run,
            job_id: job,
            data: Bytes::copy_from_slice(data),
        },
        ADMIN,
        true,
    ))
    .await
    .map(tonic::Response::into_inner)
}

async fn log(f: &Fx, run: u64, job: u64, max: u32, since: u64) -> wire::JobLogResponse {
    f.ci.job_log(req(
        wire::JobLogRequest {
            repo: repo_ref(),
            run_id: run,
            job_id: job,
            max_bytes: max,
            since_offset: since,
        },
        BOB,
        false,
    ))
    .await
    .unwrap()
    .into_inner()
}

#[tokio::test]
async fn a_run_is_reported_updated_and_listed_with_its_jobs() {
    let f = fx().await;
    let r = report(
        &f,
        run_req(
            "7",
            1,
            C::Running,
            vec![job("build", C::Running), job("test", C::Queued)],
        ),
    )
    .await
    .unwrap();
    assert_eq!((r.id, r.version, r.status), (1, 2, C::Running as i32));
    assert_eq!(
        r.jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(),
        ["build", "test"]
    );
    assert!(r.created > 0);

    // Reporting again updates in place: same ids, new statuses.
    let r2 = report(
        &f,
        run_req(
            "7",
            1,
            C::Failure,
            vec![job("build", C::Success), job("test", C::Failure)],
        ),
    )
    .await
    .unwrap();
    assert_eq!((r2.id, r2.status), (1, C::Failure as i32));
    assert_eq!(
        r2.jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
        r.jobs.iter().map(|j| j.id).collect::<Vec<_>>()
    );
    assert_eq!(r2.jobs[1].status, C::Failure as i32);

    // A late "running" report does not reopen a finished run or job.
    let late = report(
        &f,
        run_req("7", 1, C::Running, vec![job("test", C::Running)]),
    )
    .await
    .unwrap();
    assert_eq!(
        (late.status, late.jobs[1].status),
        (C::Failure as i32, C::Failure as i32)
    );

    // Another run on another change, and one on no change.
    report(&f, run_req("8", 3, C::Success, vec![]))
        .await
        .unwrap();
    report(&f, run_req("9", 0, C::Queued, vec![]))
        .await
        .unwrap();
    let all = runs(&f, 0, C::Unspecified).await;
    assert_eq!(all.runs.iter().map(|r| r.id).collect::<Vec<_>>(), [3, 2, 1]);
    assert_eq!(runs(&f, 1, C::Unspecified).await.runs.len(), 1);
    assert_eq!(runs(&f, 0, C::Success).await.runs[0].external_id, "8");
    assert!(runs(&f, 2, C::Unspecified).await.runs.is_empty());
    assert_eq!(runs(&f, 1, C::Unspecified).await.runs[0].jobs.len(), 2);
}

#[tokio::test]
async fn reports_are_validated() {
    let f = fx().await;
    let bad = |m: wire::ReportRunRequest| {
        let f = &f;
        async move { code(&report(f, m).await.unwrap_err()) }
    };
    let ok = || run_req("1", 1, C::Queued, vec![]);
    let mut m = ok();
    m.source = "a b".into();
    assert_eq!(bad(m).await, tonic::Code::InvalidArgument);
    let mut m = ok();
    m.url = "http://plain.example/".into();
    assert_eq!(bad(m).await, tonic::Code::InvalidArgument);
    let mut m = ok();
    m.external_id = String::new();
    assert_eq!(bad(m).await, tonic::Code::InvalidArgument);
    let mut m = ok();
    m.version = 1;
    m.change = Bytes::new();
    assert_eq!(bad(m).await, tonic::Code::InvalidArgument);
    let mut m = ok();
    m.version = 9;
    assert_eq!(bad(m).await, tonic::Code::InvalidArgument);
    assert_eq!(
        bad(run_req("1", 9, C::Queued, vec![])).await,
        tonic::Code::NotFound
    );
    assert_eq!(
        bad(run_req(
            "1",
            1,
            C::Queued,
            vec![job("a", C::Queued), job("a", C::Queued)]
        ))
        .await,
        tonic::Code::InvalidArgument
    );
    // A run cannot move to another change.
    report(&f, ok()).await.unwrap();
    assert_eq!(
        bad(run_req("1", 3, C::Queued, vec![])).await,
        tonic::Code::InvalidArgument
    );
    // Only a service credential reports.
    let e = f.ci.report_run(req(ok(), CAROL, false)).await.unwrap_err();
    assert!(
        matches!(
            code(&e),
            tonic::Code::PermissionDenied | tonic::Code::Unauthenticated
        ),
        "{e:?}"
    );
}

#[tokio::test]
async fn runs_page_newest_first() {
    let f = fx().await;
    for i in 0..5 {
        report(&f, run_req(&format!("r{i}"), 1, C::Success, vec![]))
            .await
            .unwrap();
    }
    let page = |token: &str| {
        f.ci.list_runs(req(
            wire::ListRunsRequest {
                repo: repo_ref(),
                limit: 2,
                page_token: token.into(),
                ..Default::default()
            },
            BOB,
            false,
        ))
    };
    let p1 = page("").await.unwrap().into_inner();
    assert_eq!(p1.runs.iter().map(|r| r.id).collect::<Vec<_>>(), [5, 4]);
    let p2 = page(&p1.next_page_token).await.unwrap().into_inner();
    assert_eq!(p2.runs.iter().map(|r| r.id).collect::<Vec<_>>(), [3, 2]);
    let p3 = page(&p2.next_page_token).await.unwrap().into_inner();
    assert_eq!(p3.runs.iter().map(|r| r.id).collect::<Vec<_>>(), [1]);
    assert!(p3.next_page_token.is_empty());
}

#[tokio::test]
async fn a_job_log_is_appended_tailed_and_followed() {
    let f = fx().await;
    let r = report(
        &f,
        run_req("1", 1, C::Running, vec![job("build", C::Running)]),
    )
    .await
    .unwrap();
    let (run, j) = (r.id, r.jobs[0].id);
    assert_eq!(append(&f, run, j, b"one\ntwo\n").await.unwrap().size, 8);
    assert_eq!(append(&f, run, j, b"three\nfo").await.unwrap().size, 16);
    assert_eq!(append(&f, run, j, b"ur\n").await.unwrap().size, 19);

    // The whole log, then a tail cut forward to a line start, then a tail that happens to start one.
    let all = log(&f, run, j, 0, 0).await;
    assert_eq!(all.data.as_ref(), b"one\ntwo\nthree\nfour\n");
    assert_eq!(
        (all.offset, all.next_offset, all.size, all.truncated),
        (0, 19, 19, false)
    );
    let tail = log(&f, run, j, 10, 0).await;
    assert_eq!(tail.data.as_ref(), b"four\n");
    assert_eq!(
        (tail.offset, tail.next_offset, tail.truncated),
        (14, 19, true)
    );
    let exact = log(&f, run, j, 5, 0).await;
    assert_eq!(exact.data.as_ref(), b"four\n");
    // A tail inside one long line keeps the line's tail (nothing to cut at).
    let one = log(&f, run, j, 3, 0).await;
    assert_eq!(one.data.as_ref(), b"");
    assert_eq!(one.next_offset, 19);

    // Following from an offset reads what came after.
    let more = log(&f, run, j, 0, 8).await;
    assert_eq!(more.data.as_ref(), b"three\nfour\n");
    assert_eq!((more.offset, more.next_offset), (8, 19));
    assert_eq!(log(&f, run, j, 4, 8).await.data.as_ref(), b"thre");
    assert!(log(&f, run, j, 0, 19).await.data.is_empty());
    assert_eq!(
        f.ci.job_log(req(
            wire::JobLogRequest {
                repo: repo_ref(),
                run_id: run,
                job_id: 99,
                ..Default::default()
            },
            BOB,
            false,
        ))
        .await
        .unwrap_err()
        .code(),
        tonic::Code::NotFound
    );
    // The job shows its size.
    assert_eq!(
        runs(&f, 1, C::Unspecified).await.runs[0].jobs[0].log_size,
        19
    );
}

#[tokio::test]
async fn appends_are_idempotent_bounded_and_admin_only() {
    let f = fx().await;
    let r = report(
        &f,
        run_req("1", 1, C::Running, vec![job("build", C::Running)]),
    )
    .await
    .unwrap();
    let (run, j) = (r.id, r.jobs[0].id);
    let msg = wire::AppendJobLogRequest {
        repo: repo_ref(),
        run_id: run,
        job_id: j,
        data: Bytes::from_static(b"x\n"),
    };
    let first = req(msg.clone(), ADMIN, true);
    let mut again = req(msg.clone(), ADMIN, false);
    again.metadata_mut().insert(
        "x-asc-op-id",
        first.metadata().get("x-asc-op-id").unwrap().clone(),
    );
    assert_eq!(
        f.ci.append_job_log(first).await.unwrap().into_inner().size,
        2
    );
    assert_eq!(
        f.ci.append_job_log(again).await.unwrap().into_inner().size,
        2
    );
    assert_eq!(log(&f, run, j, 0, 0).await.data.as_ref(), b"x\n");

    assert_eq!(
        code(&append(&f, run, j, b"").await.unwrap_err()),
        tonic::Code::InvalidArgument
    );
    let big = vec![b'a'; super::MAX_APPEND + 1];
    assert_eq!(
        code(&append(&f, run, j, &big).await.unwrap_err()),
        tonic::Code::InvalidArgument
    );
    assert_eq!(
        code(&append(&f, run, 99, b"x").await.unwrap_err()),
        tonic::Code::NotFound
    );
    let e =
        f.ci.append_job_log(req(msg.clone(), CAROL, true))
            .await
            .unwrap_err();
    assert!(
        matches!(
            code(&e),
            tonic::Code::PermissionDenied | tonic::Code::Unauthenticated
        ),
        "{e:?}"
    );
    // No op id, no append.
    let e =
        f.ci.append_job_log(req(msg, ADMIN, false))
            .await
            .unwrap_err();
    assert_eq!(code(&e), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn a_full_log_drops_later_bytes() {
    let f = fx().await;
    let r = report(
        &f,
        run_req("1", 1, C::Running, vec![job("build", C::Running)]),
    )
    .await
    .unwrap();
    let (run, j) = (r.id, r.jobs[0].id);
    // Fill the job's log to just under the cap with a row, as many appends would.
    sqlx::query("UPDATE check_jobs SET log_size = $2 WHERE ns = $1")
        .bind(f.e.ns.as_bytes().to_vec())
        .bind(super::MAX_LOG - 1)
        .execute(f.reg.pool())
        .await
        .unwrap();
    let a = append(&f, run, j, b"too much").await.unwrap();
    assert!(a.dropped);
    assert_eq!(i64::try_from(a.size).unwrap(), super::MAX_LOG - 1);
}
