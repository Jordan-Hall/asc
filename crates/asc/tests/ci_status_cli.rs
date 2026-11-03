//! `asc ci status` and `asc ci log` against a stand-in `Ci` service on loopback: the request each
//! command builds, the newest run of each workflow, the exit code of each verdict, `-w` waiting
//! for a run to end, `-f` following a log until its job ends, and server text that cannot reach
//! the terminal as control sequences.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use asc_rpc::server::wire;
use asc_rpc::services::ci_server::{Ci, CiServer};
use bytes::Bytes;
use support::{Sandbox, text};
use tonic::{Request, Response, Status};

/// The change `[0xab; 16]` as letters.
const CHANGE: &str = "popopopopopopopopopopopopopopopo";

#[derive(Default)]
struct Seen {
    list: Vec<wire::ListRunsRequest>,
    log: Vec<wire::JobLogRequest>,
}

#[derive(Clone, Default)]
struct Fake {
    seen: Arc<Mutex<Seen>>,
    /// The answer to each `ListRuns`, in turn; the last one repeats.
    lists: Arc<Vec<Vec<wire::CiRun>>>,
    /// The log of job 10, which grows by a line for each read.
    log: Arc<Vec<&'static str>>,
}

fn job(id: u64, name: &str, status: wire::CiStatus) -> wire::CiJob {
    wire::CiJob {
        id,
        name: name.into(),
        status: status as i32,
        started: 1_700_000_000,
        finished: if status == wire::CiStatus::Running {
            0
        } else {
            1_700_000_042
        },
        ..Default::default()
    }
}

fn run(id: u64, name: &str, status: wire::CiStatus, stale: bool) -> wire::CiRun {
    wire::CiRun {
        id,
        source: "hub".into(),
        external_id: id.to_string(),
        name: name.into(),
        change: Bytes::from_static(&[0xab; 16]),
        status: status as i32,
        stale,
        url: format!("https://hub.example.org/acme/app/actions/runs/{id}"),
        jobs: vec![job(id * 10, "build", status)],
        ..Default::default()
    }
}

#[tonic::async_trait]
impl Ci for Fake {
    async fn list_runs(
        &self,
        r: Request<wire::ListRunsRequest>,
    ) -> Result<Response<wire::ListRunsResponse>, Status> {
        let mut seen = self.seen.lock().unwrap();
        seen.list.push(r.into_inner());
        let i = (seen.list.len() - 1).min(self.lists.len().saturating_sub(1));
        Ok(Response::new(wire::ListRunsResponse {
            runs: self.lists.get(i).cloned().unwrap_or_default(),
            next_page_token: String::new(),
        }))
    }

    async fn job_log(
        &self,
        r: Request<wire::JobLogRequest>,
    ) -> Result<Response<wire::JobLogResponse>, Status> {
        let mut seen = self.seen.lock().unwrap();
        let req = r.into_inner();
        let n = seen.log.len();
        seen.log.push(req.clone());
        let whole: String = self.log.iter().take(n + 1).copied().collect();
        let from = (req.since_offset as usize).min(whole.len());
        Ok(Response::new(wire::JobLogResponse {
            data: Bytes::from(whole.as_bytes()[from..].to_vec()),
            offset: from as u64,
            next_offset: whole.len() as u64,
            size: whole.len() as u64,
            truncated: false,
        }))
    }

    async fn report_run(
        &self,
        _: Request<wire::ReportRunRequest>,
    ) -> Result<Response<wire::ReportRunResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }

    async fn append_job_log(
        &self,
        _: Request<wire::AppendJobLogRequest>,
    ) -> Result<Response<wire::AppendJobLogResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }

    async fn stats(
        &self,
        _: Request<wire::CiStatsRequest>,
    ) -> Result<Response<wire::CiStatsResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }

    async fn compare_with_main(
        &self,
        _: Request<wire::CompareWithMainRequest>,
    ) -> Result<Response<wire::CompareWithMainResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }
}

struct Server {
    addr: SocketAddr,
    fake: Fake,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(fake: Fake) -> Self {
        let svc = fake.clone();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let incoming = futures::stream::unfold(listener, |l| async move {
                    let r = l.accept().await.map(|(s, _)| s);
                    Some((r, l))
                });
                let _ = tonic::transport::Server::builder()
                    .add_service(CiServer::new(svc))
                    .serve_with_incoming_shutdown(incoming, async {
                        let _ = stop_rx.await;
                    })
                    .await;
            });
        });
        Self {
            addr: addr_rx.recv().unwrap(),
            fake,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("asc+http://{}/acme/app", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn asc(sb: &Sandbox, cwd: &Path, server: &Server, args: &[&str]) -> Output {
    let mut cmd = sb.asc(cwd);
    cmd.env("ASC_TOKEN", "secret-token-value")
        .env("ASC_NO_DAEMON", "1")
        .args(args)
        .args(["--remote", &server.url()]);
    cmd.output().expect("spawn asc")
}

fn setup(
    lists: Vec<Vec<wire::CiRun>>,
    log: Vec<&'static str>,
) -> (Server, Sandbox, std::path::PathBuf) {
    let server = Server::start(Fake {
        lists: Arc::new(lists),
        log: Arc::new(log),
        ..Default::default()
    });
    let sb = Sandbox::new();
    let cwd = sb.dir("w");
    (server, sb, cwd)
}

fn no_controls(s: &str) {
    assert!(
        !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t'),
        "control characters reached the output: {s:?}"
    );
}

#[test]
fn the_newest_run_of_each_workflow_decides_and_a_stale_one_does_not() {
    use wire::CiStatus as S;
    let mut lint = run(2, "lint\u{1b}[2J", S::Failure, true);
    lint.jobs.clear();
    let (server, sb, cwd) = setup(
        vec![vec![
            run(3, "ci", S::Success, false),
            lint,
            run(1, "ci", S::Failure, false),
        ]],
        vec![],
    );
    let out = asc(&sb, &cwd, &server, &["ci", "status", CHANGE]);
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert!(so.starts_with("success   #3 ci (hub)\n"), "{so}");
    assert!(
        so.contains("  https://hub.example.org/acme/app/actions/runs/3\n"),
        "{so}"
    );
    assert!(so.contains("  success   #30 build  42s\n"), "{so}");
    assert!(
        so.contains("failure   #2 lint\\x1b[2J (hub)  stale\n"),
        "{so}"
    );
    assert!(!so.contains("#1 "), "a re-run replaces the older run: {so}");
    no_controls(&so);
    let seen = server.fake.seen.lock().unwrap();
    let req = &seen.list[0];
    assert_eq!(req.change.as_ref(), [0xab; 16]);
    assert_eq!((req.limit, req.status), (30, 0));
    assert_eq!(req.repo.as_ref().unwrap().name, "acme/app");
}

#[test]
fn a_failure_ends_1_and_a_running_run_ends_8() {
    use wire::CiStatus as S;
    let (server, sb, cwd) = setup(vec![vec![run(4, "ci", S::Failure, false)]], vec![]);
    let out = asc(&sb, &cwd, &server, &["ci", "status", CHANGE, "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let line = text(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(line.trim()).expect("one JSON line");
    assert_eq!(v["id"], 4);
    assert_eq!(v["change"], CHANGE);
    drop(server);

    let (server, sb, cwd) = setup(vec![vec![run(5, "ci", S::Running, false)]], vec![]);
    let out = asc(&sb, &cwd, &server, &["ci", "status", CHANGE]);
    assert_eq!(out.status.code(), Some(8), "{}", text(&out.stderr));
    assert!(text(&out.stdout).starts_with("running   #5 ci (hub)"));
}

#[test]
fn outside_a_workspace_every_change_is_listed() {
    let (server, sb, cwd) = setup(vec![vec![]], vec![]);
    let out = asc(&sb, &cwd, &server, &["ci", "status"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "no CI runs\n");
    assert!(server.fake.seen.lock().unwrap().list[0].change.is_empty());

    let out = asc(&sb, &cwd, &server, &["ci", "status", "--limit", "0"]);
    assert_eq!(out.status.code(), Some(129));
}

#[test]
fn watch_waits_for_the_run_to_end() {
    use wire::CiStatus as S;
    let (server, sb, cwd) = setup(
        vec![
            vec![run(6, "ci", S::Running, false)],
            vec![run(6, "ci", S::Success, false)],
        ],
        vec![],
    );
    let out = asc(&sb, &cwd, &server, &["ci", "status", CHANGE, "-w"]);
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert!(
        se.contains("running   #6 ci (hub)") && se.contains("success   #6 ci (hub)"),
        "{se}"
    );
    assert!(se.contains("#6  success   #60 build"), "{se}");
    assert!(so.starts_with("success   #6 ci (hub)"), "{so}");
    assert_eq!(server.fake.seen.lock().unwrap().list.len(), 2);
}

#[test]
fn a_log_is_followed_until_its_job_ends() {
    use wire::CiStatus as S;
    let mut running = run(1, "ci", S::Running, false);
    running.jobs = vec![job(10, "build", S::Running)];
    let mut done = run(1, "ci", S::Success, false);
    done.jobs = vec![job(10, "build", S::Success)];
    let (server, sb, cwd) = setup(
        vec![vec![running], vec![done]],
        vec![
            "\x1b[32mstart\x1b[0m\r\n",
            "step \x1b]0;owned\x07\n",
            "end\n",
        ],
    );
    let out = asc(&sb, &cwd, &server, &["ci", "log", "1", "10", "-f"]);
    let (so, se) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "{se}");
    assert_eq!(so, "start\nstep \\x1b]0;owned\\x07\nend\n");
    no_controls(&so);
    let seen = server.fake.seen.lock().unwrap();
    let offsets: Vec<_> = seen.log.iter().map(|r| r.since_offset).collect();
    assert_eq!(offsets, [0, 16, 32]);
    assert!(seen.log.iter().all(|r| r.run_id == 1 && r.job_id == 10));
}
