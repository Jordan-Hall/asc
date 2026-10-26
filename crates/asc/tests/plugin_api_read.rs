//! P02 acceptance: `Workspace.ReadFile` reads a file at `rev:path`, a `FileId` or a stage (0-3)
//! from the local store (never the work tree), in ranges, and refuses what it must. A real
//! `asc daemon run` serves the RPC on a private endpoint (`ASC_DAEMON_ENDPOINT`), so the user's own
//! daemon is never involved.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use asc_core::codec::Enveloped as _;
use asc_core::content::{Body, ingest};
use asc_core::id::FileId;
use asc_core::object::{Change, Kind};
use asc_core::path::RepoPath;
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_rpc::ipc::{IpcEndpoint, WorkspaceClient, connect};
use asc_rpc::server::wire;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects as _;
use bytes::Bytes;
use support::{Fixture, text};
use tonic::Code;
use tonic::transport::Channel;
use wire::ws_read_file_request::At;

const A0: &[u8] = b"l1\nl2\nl3\n";
const A_FEAT: &[u8] = b"l1\nl2 feat\nl3\n";
const A_OTHER: &[u8] = b"l1\nl2 other\nl3\n";

struct Daemon {
    child: std::process::Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Rig {
    f: Fixture,
    ep: String,
    client: WorkspaceClient<Channel>,
    ws: String,
    big: Vec<u8>,
    _daemon: Daemon,
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn asc(f: &Fixture, ep: &str, args: &[&str]) -> Output {
    let mut c: Command = f.asc();
    c.args(args)
        .env("ASC_NO_DAEMON", "1")
        .env("ASC_DAEMON_ENDPOINT", ep)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    c.output().expect("spawn asc")
}

fn endpoint_name(f: &Fixture) -> String {
    if cfg!(windows) {
        format!(
            "asc-p02-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    } else {
        f.sandbox
            .tmp
            .path()
            .join("ipc")
            .join("d.sock")
            .to_string_lossy()
            .into_owned()
    }
}

/// `master` at c0 (which holds the chunked `big.bin`), `feat` (checked out) at c1 and `other` at
/// c2, which conflict on `a.txt`. A daemon serves the workspace.
async fn rig() -> Rig {
    let f = Fixture::new().await;
    let c = f
        .commit(&[], &[("a.txt", A0), ("b.txt", b"b"), ("dir/x.txt", b"x")])
        .await;
    // A chunked file, stored the way `asc add` stores it.
    let big: Vec<u8> = (0..5_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let got = ingest(std::io::Cursor::new(&big)).unwrap();
    let Body::Chunked { list, chunks } = got.body else {
        panic!("expected a chunked file");
    };
    for (id, data) in &chunks {
        f.objects
            .put(&ObjKey::Chunk(*id), encode_stored(data))
            .await
            .unwrap();
    }
    let envelope = Bytes::from(list.to_envelope());
    f.objects
        .put(&ObjKey::ChunkList(got.file), encode_stored(&envelope))
        .await
        .unwrap();
    let base = f.engine.get_changeset(c).await.unwrap().root;
    let change = Change::write(Kind::File, got.file, got.size, got.git, None).unwrap();
    let root = f
        .engine
        .apply(base, &[(RepoPath::new("big.bin").unwrap(), change)])
        .await
        .unwrap();
    let sig = Signature::new(
        b"Test".to_vec(),
        b"test@example.com".to_vec(),
        1_700_000_001,
        0,
    )
    .unwrap();
    let c0 = f
        .engine
        .create_changeset(
            NewChangeset {
                parents: vec![c],
                author: sig.clone(),
                committer: sig,
                message: b"big\n".to_vec(),
                extra: Vec::new(),
                root,
                copies: Vec::new(),
            },
            PathPolicy::Native { windows_safe: true },
        )
        .await
        .unwrap();
    let c1 = f.commit(&[c0], &[("a.txt", A_FEAT)]).await;
    let c2 = f.commit(&[c0], &[("a.txt", A_OTHER)]).await;
    f.set_branch("master", c0).await;
    f.set_branch("feat", c1).await;
    f.set_branch("other", c2).await;
    f.checkout_branch("feat", Some(c1)).await;
    write(&f.root, "a.txt", A_FEAT);
    write(&f.root, "b.txt", b"b");

    let ep = endpoint_name(&f);
    let child = f
        .asc()
        .args(["daemon", "run"])
        .env("ASC_DAEMON_ENDPOINT", &ep)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the daemon");
    let daemon = Daemon { child };
    let mut client = WorkspaceClient::new(connect(&IpcEndpoint::from_raw(ep.clone())));
    let mut up = false;
    for _ in 0..200 {
        if client.ping(wire::PingRequest {}).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(up, "the daemon did not come up");
    let ws = f.ws.to_string();
    client
        .mount(wire::MountRequest { ws_id: ws.clone() })
        .await
        .unwrap();
    Rig {
        f,
        ep,
        client,
        ws,
        big,
        _daemon: daemon,
    }
}

fn at_rev(rev: &str, path: &str) -> At {
    At::RevPath(wire::WsRevPath {
        rev: rev.into(),
        path: path.as_bytes().to_vec().into(),
    })
}

fn at_stage(stage: u32, path: &str) -> At {
    At::Stage(wire::WsStagePath {
        stage,
        path: path.as_bytes().to_vec().into(),
    })
}

fn at_id(id: FileId) -> At {
    At::FileId(Bytes::copy_from_slice(id.as_bytes()))
}

/// Reads to the end of the stream: the bytes and the `total_size` every message carried.
async fn read(
    r: &mut Rig,
    at: At,
    offset: u64,
    length: u64,
) -> Result<(Vec<u8>, u64), tonic::Status> {
    let mut s = r
        .client
        .read_file(wire::WsReadFileRequest {
            ws_id: r.ws.clone(),
            at: Some(at),
            offset,
            length,
        })
        .await?
        .into_inner();
    let (mut data, mut total) = (Vec::new(), None);
    while let Some(m) = tokio::time::timeout(Duration::from_secs(60), s.message())
        .await
        .expect("stream stalled")?
    {
        assert!(m.data.len() <= 64 * 1024);
        assert_eq!(*total.get_or_insert(m.total_size), m.total_size);
        data.extend_from_slice(&m.data);
    }
    Ok((data, total.expect("at least one message")))
}

async fn code(r: &mut Rig, at: At) -> Code {
    read(r, at, 0, 0).await.unwrap_err().code()
}

#[tokio::test]
async fn a_revision_path_reads_the_stored_file_not_the_work_tree() {
    let mut r = rig().await;
    let (d, t) = read(&mut r, at_rev("", "a.txt"), 0, 0).await.unwrap();
    assert_eq!((d.as_slice(), t), (A_FEAT, A_FEAT.len() as u64));
    let (d, _) = read(&mut r, at_rev("master", "a.txt"), 0, 0).await.unwrap();
    assert_eq!(d, A0);
    let (d, _) = read(&mut r, at_rev("other", "a.txt"), 0, 0).await.unwrap();
    assert_eq!(d, A_OTHER);
    let (d, _) = read(&mut r, at_rev("master", "dir/x.txt"), 0, 0)
        .await
        .unwrap();
    assert_eq!(d, b"x");
    // The work tree is never read: an edit does not show.
    write(&r.f.root, "a.txt", b"dirty");
    let (d, _) = read(&mut r, at_rev("HEAD", "a.txt"), 0, 0).await.unwrap();
    assert_eq!(d, A_FEAT);
    // The CLI prints the same bytes.
    let cli = asc(&r.f, &r.ep, &["cat-file", "blob", "master:a.txt"]);
    assert_eq!(cli.stdout, A0, "{}", text(&cli.stderr));
}

#[tokio::test]
async fn ranges_are_clamped_and_chunked_files_stream() {
    let mut r = rig().await;
    let (d, t) = read(&mut r, at_rev("master", "a.txt"), 3, 4).await.unwrap();
    assert_eq!((d.as_slice(), t), (&A0[3..7], A0.len() as u64));
    let (d, t) = read(&mut r, at_rev("master", "a.txt"), 6, 100)
        .await
        .unwrap();
    assert_eq!((d.as_slice(), t), (&A0[6..], A0.len() as u64));
    let (d, t) = read(&mut r, at_rev("master", "a.txt"), 500, 0)
        .await
        .unwrap();
    assert_eq!((d.len(), t), (0, A0.len() as u64));

    let big = r.big.clone();
    let (d, t) = read(&mut r, at_rev("master", "big.bin"), 0, 0)
        .await
        .unwrap();
    assert_eq!(t, big.len() as u64);
    assert!(d == big, "the whole chunked file");
    let (d, _) = read(&mut r, at_rev("master", "big.bin"), 4_194_000, 5_000)
        .await
        .unwrap();
    assert!(
        d == big[4_194_000..4_199_000],
        "a range across a chunk edge"
    );
    let (d, _) = read(&mut r, at_rev("master", "big.bin"), 4_999_990, 0)
        .await
        .unwrap();
    assert!(d == big[4_999_990..], "the tail");
}

#[tokio::test]
async fn a_file_id_reads_from_the_store() {
    let mut r = rig().await;
    let (d, _) = read(&mut r, at_id(FileId::of(A_OTHER)), 0, 0)
        .await
        .unwrap();
    assert_eq!(d, A_OTHER);
    assert_eq!(
        code(&mut r, at_id(FileId::of(b"never stored"))).await,
        Code::NotFound
    );
}

#[tokio::test]
async fn stages_read_the_index_and_the_conflict_sides() {
    let mut r = rig().await;
    let (d, _) = read(&mut r, at_stage(0, "a.txt"), 0, 0).await.unwrap();
    assert_eq!(d, A_FEAT);
    assert_eq!(code(&mut r, at_stage(2, "a.txt")).await, Code::NotFound);

    let out = asc(&r.f, &r.ep, &["merge", "other"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    for (stage, want) in [(1, A0), (2, A_FEAT), (3, A_OTHER)] {
        let (d, _) = read(&mut r, at_stage(stage, "a.txt"), 0, 0).await.unwrap();
        assert_eq!(d, want, "stage {stage}");
    }
    assert_eq!(code(&mut r, at_stage(0, "a.txt")).await, Code::NotFound);
    // An untouched path is still at stage 0.
    let (d, _) = read(&mut r, at_stage(0, "b.txt"), 0, 0).await.unwrap();
    assert_eq!(d, b"b");
}

#[tokio::test]
async fn bad_requests_are_refused() {
    let mut r = rig().await;
    assert_eq!(code(&mut r, at_rev("", "nope.txt")).await, Code::NotFound);
    assert_eq!(
        code(&mut r, at_rev("", "dir")).await,
        Code::FailedPrecondition
    );
    for bad in [
        at_rev("-x", "a.txt"),
        at_rev("", ""),
        at_rev("", "../a.txt"),
        at_rev("", "/etc/passwd"),
        at_stage(4, "a.txt"),
        At::FileId(Bytes::from_static(b"short")),
    ] {
        assert_eq!(code(&mut r, bad).await, Code::InvalidArgument);
    }
    // No `at`, and a workspace that is not mounted.
    let e = r
        .client
        .read_file(wire::WsReadFileRequest {
            ws_id: r.ws.clone(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
    let e = r
        .client
        .read_file(wire::WsReadFileRequest {
            ws_id: "00000000-0000-0000-0000-000000000000".into(),
            at: Some(at_rev("", "a.txt")),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_ne!(e.code(), Code::Ok);
}
