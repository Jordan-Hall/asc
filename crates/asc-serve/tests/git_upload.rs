//! U81: `git clone`, `fetch` and `ls-remote` against `asc serve` (SPEC §25; PLAN U81), with real
//! git >= 2.40 as the client and real `git upload-pack` behind the routes.
//!
//! The server is the production wiring: `GitViewService`, `git_http::register`, the credential
//! edge and `limits::guarded_routes` (admission, class `git`), served by tonic's server over a
//! loopback socket, so peers, `Retry-After` and the streaming bodies are the real ones.
#![cfg(feature = "git-compat")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::io::{Cursor, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use asc_core::codec::Enveloped;
use asc_core::content::{Body as Content, ingest};
use asc_core::git::{ObjectKind, export_tag, object_oid};
use asc_core::id::{ChangesetId, RefTarget, TagId};
use asc_core::object::{Change, Kind, SortedMap, TagRecord};
use asc_core::path::{RefName, RepoPath};
use asc_core::sig::Signature;
use asc_engine::commit::{NewChangeset, PathPolicy};
use asc_git::realgit::RealGit;
use asc_rpc::auth::{Access, AuthError, Authorizer, BearerToken, Principal};
use asc_rpc::server::{RepoName, ServerInfo};
use asc_serve::config::ServeConfig;
use asc_serve::git_view::GitViewService;
use asc_serve::http::Metrics;
use asc_serve::limits::{Admission, guarded_routes};
use asc_serve::registry::{Extensions, NewRepo, Registry, RepoEntry, RepoMode};
use asc_serve::{git_http, migrate};
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::{Actor, Expect, RefTxn, RefUpdate, Refs};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use uuid::Uuid;

const READ: &str = "tok-read-S3CR3T";
const WRITE: &str = "tok-write-S3CR3T";
const OUTSIDE: &str = "tok-outside-S3CR3T";
const REVOKED: &str = "tok-revoked-S3CR3T";
const WEB: &str = "acme/web";
const MIRROR: &str = "acme/mirror";

// ------------------------------------------------------------------------------ authorizer

/// Tokens with per-repo grants, and repos anonymous may read.
struct Acl {
    public: Vec<&'static str>,
    log: Mutex<Vec<(String, Access)>>,
}

impl Acl {
    fn grants(token: &str) -> Option<(&'static str, Vec<(&'static str, Access)>)> {
        match token {
            READ => Some(("reader", vec![(WEB, Access::Read), (MIRROR, Access::Read)])),
            WRITE => Some((
                "writer",
                vec![(WEB, Access::Write), (MIRROR, Access::Write)],
            )),
            OUTSIDE => Some(("outsider", vec![])),
            _ => None,
        }
    }
}

#[async_trait]
impl Authorizer for Acl {
    async fn authorize(
        &self,
        token: Option<&BearerToken>,
        repo: &str,
        access: Access,
    ) -> Result<Principal, AuthError> {
        self.log.lock().unwrap().push((repo.to_owned(), access));
        let public = access == Access::Read && self.public.contains(&repo);
        let Some(token) = token else {
            return if public {
                Ok(Principal {
                    id: "anonymous".into(),
                })
            } else {
                Err(AuthError::Unauthenticated)
            };
        };
        let (id, repos) = Self::grants(token.expose()).ok_or(AuthError::Unauthenticated)?;
        let allowed = repos.iter().any(|(r, max)| *r == repo && access <= *max);
        if allowed || public {
            Ok(Principal { id: id.to_owned() })
        } else {
            Err(AuthError::Forbidden)
        }
    }
}

// ------------------------------------------------------------------------------ audit capture

type Records = Arc<Mutex<Vec<HashMap<String, String>>>>;

struct Capture(Records);

struct Fields(HashMap<String, String>);

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target() == "asc_git_audit"
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, e: &tracing::Event<'_>) {
        let mut f = Fields(HashMap::new());
        e.record(&mut f);
        self.0.lock().unwrap().push(f.0);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

fn audit_records() -> Records {
    static RECORDS: OnceLock<Records> = OnceLock::new();
    RECORDS
        .get_or_init(|| {
            let records: Records = Arc::default();
            let _ = tracing::subscriber::set_global_default(Capture(records.clone()));
            records
        })
        .clone()
}

// ------------------------------------------------------------------------------ the server

struct Env {
    dir: tempfile::TempDir,
    registry: Arc<Registry>,
    web: Arc<RepoEntry>,
    mirror: Arc<RepoEntry>,
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    acl: Arc<Acl>,
    view_dir: PathBuf,
}

fn sig() -> Signature {
    Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap()
}

impl Env {
    /// A server with the git layer live (unless `extra` disables it), `acme/web` as a primary
    /// repo and `acme/mirror` as a mirror-mode repo with a bare mirror on disk.
    async fn start(extra: &str, public: &[&'static str]) -> Self {
        let _ = audit_records();
        let dir = tempfile::tempdir().unwrap();
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let db = format!("sqlite:{}?mode=rwc", fwd(&dir.path().join("asc.db")));
        let cfg = ServeConfig::parse(&format!(
            "store = \"memory:///\"\ndb = {db:?}\ncache = {{ disk_dir = {:?}, disk = \"1GiB\" }}\n{extra}",
            fwd(&dir.path().join("cache"))
        ))
        .unwrap();
        let pool = migrate::connect(&db, 4).await.unwrap();
        migrate::migrate(&pool).await.unwrap();
        let registry = Arc::new(Registry::new(pool.clone(), "memory:///").unwrap());
        let web = registry
            .create(&RepoName::parse(WEB).unwrap(), &NewRepo::default())
            .await
            .unwrap();
        let mirror = registry
            .create(
                &RepoName::parse(MIRROR).unwrap(),
                &NewRepo {
                    mode: RepoMode::Mirror,
                    default_branch: "trunk".into(),
                },
            )
            .await
            .unwrap();
        let mirrors = dir.path().join("cache").join("mirrors");
        std::fs::create_dir_all(&mirrors).unwrap();

        let metrics = Arc::new(Metrics::new());
        let ext = Extensions::new();
        let views = GitViewService::start(&cfg, pool.clone(), Some(mirrors.clone())).unwrap();
        let view_dir = dir.path().join("cache").join("gitview");
        if let Some(views) = views {
            git_http::register(&ext, views, registry.clone(), &metrics, &cfg);
        }
        let acl = Arc::new(Acl {
            public: public.to_vec(),
            log: Mutex::default(),
        });
        let admission = Arc::new(Admission::new(&cfg.limits));
        let routes = guarded_routes(&ext, acl.clone(), admission, ServerInfo::default());
        let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = incoming.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = Server::builder()
                .accept_http1(true)
                .add_routes(routes)
                .serve_with_incoming(incoming)
                .await;
        });
        Self {
            dir,
            registry,
            web,
            mirror,
            addr,
            metrics,
            acl,
            view_dir,
        }
    }

    fn url(&self, name: &str) -> String {
        format!("http://{}/{name}.git", self.addr)
    }

    fn url_with(&self, name: &str, user: &str, secret: &str) -> String {
        format!("http://{user}:{secret}@{}/{name}.git", self.addr)
    }

    async fn file(&self, repo: &RepoEntry, content: &[u8]) -> Change {
        let got = ingest(Cursor::new(content)).unwrap();
        let Content::Inline(b) = got.body else {
            unreachable!("test files are small")
        };
        repo.objects
            .put(&ObjKey::File(got.file), encode_stored(&b))
            .await
            .unwrap();
        Change::write(Kind::File, got.file, got.size, got.git, None).unwrap()
    }

    async fn commit(&self, parent: Option<ChangesetId>, files: &[(&str, &[u8])]) -> ChangesetId {
        let repo = &self.web;
        let engine = repo.backend.engine();
        let mut changes = Vec::new();
        for (path, content) in files {
            changes.push((
                RepoPath::new(*path).unwrap(),
                self.file(repo, content).await,
            ));
        }
        let base =
            match parent {
                Some(p) => engine.get_changeset(p).await.unwrap().root,
                None => asc_engine::Engine::<
                    asc_store::bucket::BucketObjects,
                    asc_store::sqlrefs::SqlRefs,
                >::empty_tree_id(),
            };
        let root = engine.apply(base, &changes).await.unwrap();
        engine
            .create_changeset(
                NewChangeset {
                    parents: parent.into_iter().collect(),
                    author: sig(),
                    committer: sig(),
                    message: b"msg\n".to_vec(),
                    extra: vec![],
                    root,
                    copies: vec![],
                },
                PathPolicy::Native { windows_safe: true },
            )
            .await
            .unwrap()
    }

    async fn set_ref(&self, name: &str, target: RefTarget) {
        let txn = RefTxn {
            updates: vec![RefUpdate {
                name: RefName::new(name.as_bytes().to_vec()).unwrap(),
                expect: Expect::Any,
                new: Some(target),
                expires_at: None,
            }],
            actor: Actor {
                id: "test".into(),
                via: "test".into(),
            },
            reason: "test".into(),
            op_id: Uuid::new_v4(),
        };
        self.registry
            .refs()
            .update(self.web.key, &txn)
            .await
            .unwrap();
    }

    async fn tag(&self, name: &str, target: ChangesetId) -> TagId {
        let record = TagRecord {
            target,
            name: name.as_bytes().to_vec(),
            tagger: Some(sig()),
            message: b"release\n".to_vec(),
            extra: SortedMap::new(vec![]).unwrap(),
        };
        let id = record.id();
        self.web
            .objects
            .put(&ObjKey::Tag(id), encode_stored(&record.to_envelope()))
            .await
            .unwrap();
        self.set_ref(&format!("refs/tags/{name}"), RefTarget::Tag(id))
            .await;
        id
    }

    async fn git_id(&self, cs: ChangesetId) -> String {
        self.web
            .backend
            .engine()
            .git_commit_of(cs)
            .await
            .unwrap()
            .to_string()
    }

    /// `a <- b <- c` on `master`, `topic` at `a`, annotated `v1` at `b`, and private refs that
    /// must never be advertised.
    async fn seed(&self) -> Seed {
        let a = self.commit(None, &[("README", b"one\n")]).await;
        let b = self
            .commit(Some(a), &[("README", b"two\n"), ("src/lib.txt", b"lib\n")])
            .await;
        let c = self
            .commit(
                Some(b),
                &[("README", b"three\n"), ("src/lib.txt", b"lib2\n")],
            )
            .await;
        self.set_ref("refs/heads/master", RefTarget::Changeset(c))
            .await;
        self.set_ref("refs/heads/topic", RefTarget::Changeset(a))
            .await;
        let tag = self.tag("v1", b).await;
        self.set_ref("refs/users/7/heads/private", RefTarget::Changeset(c))
            .await;
        self.set_ref("refs/pull/1/head", RefTarget::Changeset(c))
            .await;
        self.set_ref("refs/snapshots/s1", RefTarget::Changeset(c))
            .await;
        Seed { a, b, c, tag }
    }

    /// The bare mirror of `acme/mirror`: `trunk`, plus a commit only a pull ref reaches.
    fn seed_mirror(&self, git: &Git) -> MirrorIds {
        let dir = self
            .dir
            .path()
            .join("cache")
            .join("mirrors")
            .join(format!("{}.git", self.mirror.key.0));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| git.sync(&dir, args, &[]).stdout.trim().to_owned();
        run(&["init", "-q", "--bare", "."]);
        let tree = run(&["hash-object", "-t", "tree", "-w", "--stdin"]);
        let trunk = run(&["commit-tree", &tree, "-m", "trunk"]);
        let hidden = run(&[
            "commit-tree",
            &tree,
            "-p",
            &trunk,
            "-m",
            "only on a pull ref",
        ]);
        run(&["update-ref", "refs/heads/trunk", &trunk]);
        run(&["update-ref", "refs/pull/1/head", &hidden]);
        run(&["update-ref", "refs/users/9/heads/x", &hidden]);
        run(&["update-ref", "refs/snapshots/z", &hidden]);
        run(&["symbolic-ref", "HEAD", "refs/heads/trunk"]);
        MirrorIds { trunk, hidden }
    }
}

struct Seed {
    a: ChangesetId,
    b: ChangesetId,
    c: ChangesetId,
    tag: TagId,
}

struct MirrorIds {
    trunk: String,
    hidden: String,
}

// ------------------------------------------------------------------------------ real git

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

struct Git {
    real: RealGit,
    home: tempfile::TempDir,
}

impl Git {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("gitconfig"),
            "[user]\n\tname = T\n\temail = t@x\n[init]\n\tdefaultBranch = master\n[core]\n\tautocrlf = false\n[advice]\n\tdetachedHead = false\n",
        )
        .unwrap();
        Self {
            real: RealGit::resolve(None, None).expect("real git >= 2.40"),
            home,
        }
    }

    fn work(&self, name: &str) -> PathBuf {
        let p = self.home.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sync(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = self.real.command().unwrap();
        cmd.args(["-c", "credential.helper="])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.path().join("gitconfig"))
            .env("HOME", self.home.path())
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        Run {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    async fn run(&self, cwd: &Path, args: &[&str]) -> Run {
        self.run_env(cwd, args, &[]).await
    }

    async fn run_env(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
        tokio::task::block_in_place(|| self.sync(cwd, args, env))
    }
}

fn lines(s: &str) -> Vec<String> {
    s.lines().map(str::to_owned).collect()
}

/// `(oid, ref)` pairs of `git ls-remote` output, sorted.
fn refs_of(out: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(o, r)| (o.to_owned(), r.to_owned()))
        .collect();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

// ------------------------------------------------------------------------------ raw http

struct Raw {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Raw {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn basic(user: &str, secret: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{secret}"))
    )
}

async fn read_response(mut s: tokio::net::TcpStream) -> Raw {
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut buf)).await;
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(buf.len(), |p| p + 4);
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut body = buf[split..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    Raw { status, head, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(p) = b.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(std::str::from_utf8(&b[..p]).unwrap_or("0").trim(), 16)
            .unwrap_or(0);
        if size == 0 || b.len() < p + 2 + size {
            break;
        }
        out.extend_from_slice(&b[p + 2..p + 2 + size]);
        b = &b[(p + 2 + size + 2).min(b.len())..];
    }
    out
}

fn request(
    method: &str,
    target: &str,
    host: SocketAddr,
    auth: Option<&str>,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut r = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(a) = auth {
        r.push_str(&format!("Authorization: {a}\r\n"));
    }
    for (k, v) in extra {
        r.push_str(&format!("{k}: {v}\r\n"));
    }
    if method == "POST" {
        r.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    r.push_str("\r\n");
    let mut bytes = r.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

async fn raw(addr: SocketAddr, req: Vec<u8>) -> Raw {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(&req).await.unwrap();
    read_response(s).await
}

const UPLOAD_TYPE: (&str, &str) = ("Content-Type", "application/x-git-upload-pack-request");

fn pkt(s: &str) -> String {
    format!("{:04x}{s}", s.len() + 4)
}

// ------------------------------------------------------------------------------ tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clone_fetch_and_ls_remote_with_every_protocol_version() {
    let env = Env::start("", &[WEB]).await;
    let s = env.seed().await;
    let git = Git::new();
    let (ca, cb, cc) = (
        env.git_id(s.a).await,
        env.git_id(s.b).await,
        env.git_id(s.c).await,
    );
    let tag_oid = {
        let rec = TagRecord {
            target: s.b,
            name: b"v1".to_vec(),
            tagger: Some(sig()),
            message: b"release\n".to_vec(),
            extra: SortedMap::new(vec![]).unwrap(),
        };
        assert_eq!(rec.id(), s.tag);
        let target = env.web.backend.engine().git_commit_of(s.b).await.unwrap();
        object_oid(ObjectKind::Tag, &export_tag(&rec, target).unwrap()).to_string()
    };
    let url = env.url(WEB);
    let cwd = git.work("lsr");

    // The advertisement is the same for v0, v1 and v2 (`ls-refs`): shared refs only, with the
    // ids of the git map.
    let mut seen = Vec::new();
    for v in ["0", "1", "2"] {
        let r = git
            .run_env(
                &cwd,
                &["-c", &format!("protocol.version={v}"), "ls-remote", &url],
                &[("GIT_TRACE_PACKET", "1")],
            )
            .await;
        assert!(r.ok, "ls-remote v{v}: {}", r.stderr);
        match v {
            "2" => assert!(r.stderr.contains("version 2"), "{}", r.stderr),
            "1" => assert!(r.stderr.contains("version 1"), "{}", r.stderr),
            _ => assert!(!r.stderr.contains("git< version"), "{}", r.stderr),
        }
        seen.push(refs_of(&r.stdout));
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1], seen[2]);
    let names: Vec<&str> = seen[0].iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "HEAD",
            "refs/heads/master",
            "refs/heads/topic",
            "refs/tags/v1",
            "refs/tags/v1^{}"
        ]
    );
    let id_of = |n: &str| seen[0].iter().find(|(_, r)| r == n).unwrap().0.clone();
    assert_eq!(id_of("refs/heads/master"), cc);
    assert_eq!(id_of("HEAD"), cc);
    assert_eq!(id_of("refs/heads/topic"), ca);
    assert_eq!(id_of("refs/tags/v1^{}"), cb);
    assert_eq!(id_of("refs/tags/v1"), tag_oid);

    // A clone (v2 by default) checks out the head with the right content and passes fsck.
    let clone = git.work("clone");
    let r = git.run(&clone, &["clone", "-q", &url, "c"]).await;
    assert!(r.ok, "clone: {}", r.stderr);
    let c = clone.join("c");
    assert_eq!(
        std::fs::read_to_string(c.join("README")).unwrap(),
        "three\n"
    );
    assert_eq!(
        std::fs::read_to_string(c.join("src").join("lib.txt")).unwrap(),
        "lib2\n"
    );
    let head = git.run(&c, &["rev-parse", "HEAD"]).await;
    assert_eq!(head.stdout.trim(), cc);
    assert!(git.run(&c, &["fsck", "--strict"]).await.ok);
    let tags = git.run(&c, &["tag"]).await;
    assert_eq!(tags.stdout.trim(), "v1", "auto-followed annotated tag");

    // Clones with the older protocols work, and a later push to master reaches `fetch`.
    for v in ["0", "1"] {
        let dir = git.work(&format!("clone-v{v}"));
        let r = git
            .run(
                &dir,
                &[
                    "-c",
                    &format!("protocol.version={v}"),
                    "clone",
                    "-q",
                    &url,
                    "c",
                ],
            )
            .await;
        assert!(r.ok, "clone v{v}: {}", r.stderr);
        assert_eq!(
            git.run(&dir.join("c"), &["rev-parse", "HEAD"])
                .await
                .stdout
                .trim(),
            cc
        );
    }
    let d = env
        .commit(Some(s.c), &[("README", b"four\n"), ("new/file", b"n\n")])
        .await;
    env.set_ref("refs/heads/master", RefTarget::Changeset(d))
        .await;
    let r = git.run(&c, &["fetch", "-q", "origin"]).await;
    assert!(r.ok, "fetch: {}", r.stderr);
    let after = git.run(&c, &["rev-parse", "origin/master"]).await;
    assert_eq!(after.stdout.trim(), env.git_id(d).await);
    assert!(
        git.run(&c, &["merge", "-q", "--ff-only", "origin/master"])
            .await
            .ok
    );
    assert_eq!(
        std::fs::read_to_string(c.join("new").join("file")).unwrap(),
        "n\n"
    );
    assert!(git.run(&c, &["fsck", "--strict"]).await.ok);

    // Private refs are never advertised: no `refs/users`, `refs/pull`, `refs/snapshots`.
    let all = git.run(&c, &["ls-remote", &url]).await;
    for hidden in ["refs/users", "refs/pull", "refs/snapshots"] {
        assert!(!all.stdout.contains(hidden), "{}", all.stdout);
    }
    assert_eq!(lines(&all.stdout).len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shallow_and_partial_clones_and_the_lazy_fetch_that_follows() {
    let env = Env::start("", &[WEB]).await;
    let s = env.seed().await;
    let git = Git::new();
    let url = env.url(WEB);
    let cc = env.git_id(s.c).await;

    // --depth 1
    let w = git.work("depth");
    let r = git
        .run(&w, &["clone", "-q", "--depth", "1", &url, "c"])
        .await;
    assert!(r.ok, "depth 1: {}", r.stderr);
    let c = w.join("c");
    let count = git.run(&c, &["rev-list", "--count", "HEAD"]).await;
    assert_eq!(count.stdout.trim(), "1");
    assert!(c.join(".git").join("shallow").is_file());
    assert_eq!(git.run(&c, &["rev-parse", "HEAD"]).await.stdout.trim(), cc);

    // --deepen
    let r = git.run(&c, &["fetch", "-q", "--deepen", "1"]).await;
    assert!(r.ok, "deepen: {}", r.stderr);
    assert_eq!(
        git.run(&c, &["rev-list", "--count", "HEAD"])
            .await
            .stdout
            .trim(),
        "2"
    );

    // --unshallow
    let r = git.run(&c, &["fetch", "-q", "--unshallow"]).await;
    assert!(r.ok, "unshallow: {}", r.stderr);
    assert_eq!(
        git.run(&c, &["rev-list", "--count", "HEAD"])
            .await
            .stdout
            .trim(),
        "3"
    );
    assert!(!c.join(".git").join("shallow").exists());
    assert!(git.run(&c, &["fsck", "--strict"]).await.ok);

    // --shallow-since (every commit has the same date: the cut keeps them all or none, but the
    // option must be accepted and answered)
    let w = git.work("since");
    let r = git
        .run(
            &w,
            &["clone", "-q", "--shallow-since=2023-01-01", &url, "c"],
        )
        .await;
    assert!(r.ok, "shallow-since: {}", r.stderr);
    let w = git.work("exclude");
    let r = git
        .run(&w, &["clone", "-q", "--shallow-exclude=v1", &url, "c"])
        .await;
    assert!(r.ok, "shallow-exclude: {}", r.stderr);
    assert_eq!(
        git.run(&w.join("c"), &["rev-list", "--count", "HEAD"])
            .await
            .stdout
            .trim(),
        "1",
        "history above the tag only"
    );

    // --filter=blob:none, then the lazy fetch of the blobs a checkout needs
    let w = git.work("blobless");
    let r = git
        .run(
            &w,
            &[
                "clone",
                "-q",
                "--no-checkout",
                "--filter=blob:none",
                &url,
                "c",
            ],
        )
        .await;
    assert!(r.ok, "blob:none: {}", r.stderr);
    let c = w.join("c");
    let missing = git
        .run(&c, &["rev-list", "--objects", "--missing=print", "HEAD"])
        .await;
    assert!(
        missing.stdout.lines().any(|l| l.starts_with('?')),
        "blobs were left out: {}",
        missing.stdout
    );
    let r = git.run(&c, &["checkout", "-q", "master"]).await;
    assert!(r.ok, "lazy blob fetch: {}", r.stderr);
    assert_eq!(
        std::fs::read_to_string(c.join("README")).unwrap(),
        "three\n"
    );

    // --filter=tree:0, then trees and blobs on demand
    let w = git.work("treeless");
    let r = git
        .run(
            &w,
            &["clone", "-q", "--no-checkout", "--filter=tree:0", &url, "c"],
        )
        .await;
    assert!(r.ok, "tree:0: {}", r.stderr);
    let c = w.join("c");
    let shown = git.run(&c, &["show", "HEAD:src/lib.txt"]).await;
    assert!(shown.ok, "lazy tree and blob fetch: {}", shown.stderr);
    assert_eq!(shown.stdout, "lib2\n");

    // blob:limit
    let w = git.work("limit");
    let r = git
        .run(&w, &["clone", "-q", "--filter=blob:limit=2", &url, "c"])
        .await;
    assert!(r.ok, "blob:limit: {}", r.stderr);
    assert_eq!(
        std::fs::read_to_string(w.join("c").join("README")).unwrap(),
        "three\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn include_tag_follows_an_annotated_tag_when_only_a_branch_is_fetched() {
    let env = Env::start("", &[WEB]).await;
    let s = env.seed().await;
    let git = Git::new();
    let w = git.work("single");
    let r = git
        .run(
            &w,
            &[
                "clone",
                "-q",
                "--single-branch",
                "--branch",
                "master",
                &env.url(WEB),
                "c",
            ],
        )
        .await;
    assert!(r.ok, "{}", r.stderr);
    let c = w.join("c");
    let tags = git.run(&c, &["tag"]).await;
    assert_eq!(
        tags.stdout.trim(),
        "v1",
        "include-tag brought the tag along"
    );
    let peeled = git.run(&c, &["rev-parse", "v1^{commit}"]).await;
    assert_eq!(peeled.stdout.trim(), env.git_id(s.b).await);
    let r = git
        .run(
            &c,
            &["fetch", "-q", "origin", "refs/tags/v1:refs/tags/again"],
        )
        .await;
    assert!(r.ok, "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_repo_challenges_then_the_credential_helper_works() {
    let env = Env::start("", &[]).await;
    env.seed().await;
    let git = Git::new();
    let url = env.url(WEB);
    let w = git.work("anon");

    // No credential and no helper: git is challenged, finds nothing to answer with and stops.
    let r = git.run(&w, &["ls-remote", &url]).await;
    assert!(!r.ok);
    assert!(
        r.stderr.contains("could not read Username") || r.stderr.contains("Authentication failed"),
        "{}",
        r.stderr
    );

    // A helper answers after the challenge; git then stores the working credential.
    let helper = git.home.path().join("helper.sh");
    let log = git.home.path().join("helper.log");
    let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\necho \"$1\" >> \"{}\"\nif [ \"$1\" = get ]; then\n  echo username=x-access-token\n  echo password={READ}\nfi\ncat >/dev/null\n",
            fwd(&log)
        ),
    )
    .unwrap();
    let helper_arg = format!("credential.helper=!sh {}", fwd(&helper));
    let r = git
        .run(&w, &["-c", &helper_arg, "clone", "-q", &url, "c"])
        .await;
    assert!(r.ok, "{}", r.stderr);
    assert!(!r.stderr.contains(READ));
    let asked = std::fs::read_to_string(&log).unwrap();
    assert_eq!(asked.lines().collect::<Vec<_>>(), ["get", "store"]);
    assert_eq!(
        std::fs::read_to_string(w.join("c").join("README")).unwrap(),
        "three\n"
    );

    // The deploy-token user names are hints; the secret decides.
    for user in ["git", "oauth2", "gitlab-ci-token", ""] {
        let d = git.work(&format!("user-{user}"));
        // git sends no credential for an empty user name in the URL on every platform, so that
        // case carries the same Basic header by hand.
        let header = format!("http.extraHeader=Authorization: {}", basic(user, READ));
        let r = if user.is_empty() {
            git.run(&d, &["-c", &header, "clone", "-q", &env.url(WEB), "c"])
                .await
        } else {
            git.run(&d, &["clone", "-q", &env.url_with(WEB, user, READ), "c"])
                .await
        };
        assert!(r.ok, "user {user:?}: {}", r.stderr);
    }

    // A revoked token: 401 again and the helper is told to erase it.
    let helper2 = git.home.path().join("helper2.sh");
    let log2 = git.home.path().join("helper2.log");
    std::fs::write(
        &helper2,
        format!(
            "#!/bin/sh\necho \"$1\" >> \"{}\"\nif [ \"$1\" = get ]; then\n  echo username=x-access-token\n  echo password={REVOKED}\nfi\ncat >/dev/null\n",
            fwd(&log2)
        ),
    )
    .unwrap();
    let helper_arg = format!("credential.helper=!sh {}", fwd(&helper2));
    let r = git.run(&w, &["-c", &helper_arg, "ls-remote", &url]).await;
    assert!(!r.ok);
    assert!(r.stderr.contains("Authentication failed"), "{}", r.stderr);
    assert!(!r.stderr.contains(REVOKED));
    let asked = std::fs::read_to_string(&log2).unwrap();
    assert_eq!(asked.lines().collect::<Vec<_>>(), ["get", "erase"]);

    // A valid token with no read access gets the answer of a missing repository.
    let r = git
        .run(&w, &["ls-remote", &env.url_with(WEB, "git", OUTSIDE)])
        .await;
    assert!(!r.ok);
    assert!(
        r.stderr.contains("not found") || r.stderr.contains("404"),
        "{}",
        r.stderr
    );
    let missing = git
        .run(&w, &["ls-remote", &env.url_with("acme/ghost", "git", READ)])
        .await;
    assert!(!missing.ok);
    assert!(
        missing.stderr.contains("not found") || missing.stderr.contains("404"),
        "{}",
        missing.stderr
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anonymous_read_works_for_what_the_authorizer_calls_public() {
    let env = Env::start("", &[WEB]).await;
    env.seed().await;
    let git = Git::new();
    let w = git.work("anon");
    let r = git.run(&w, &["clone", "-q", &env.url(WEB), "c"]).await;
    assert!(r.ok, "{}", r.stderr);
    // The private mirror repo still challenges an anonymous client.
    let r = git.run(&w, &["ls-remote", &env.url(MIRROR)]).await;
    assert!(!r.ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receive_pack_needs_write_and_a_mirror_is_read_only() {
    let env = Env::start("", &[]).await;
    env.seed().await;
    let git = Git::new();
    env.seed_mirror(&git);
    let adv = |name: &str| format!("/{name}.git/info/refs?service=git-receive-pack");

    // A read deploy token clones and gets 403 on the receive-pack advertisement.
    let r = raw(
        env.addr,
        request(
            "GET",
            &adv(WEB),
            env.addr,
            Some(&basic("git", READ)),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.head);
    assert!(r.text().contains("does not allow pushing"));
    let w = git.work("push");
    assert!(
        git.run(&w, &["clone", "-q", &env.url_with(WEB, "git", READ), "c"])
            .await
            .ok
    );
    let c = w.join("c");
    let r = git.run(&c, &["push", "origin", "HEAD:refs/heads/x"]).await;
    assert!(!r.ok);
    assert!(r.stderr.contains("403"), "{}", r.stderr);

    // Anonymous: the challenge, so a helper is asked. No credential at all on a private repo.
    let r = raw(
        env.addr,
        request("GET", &adv(WEB), env.addr, None, &[], b""),
    )
    .await;
    assert_eq!(r.status, 401);
    assert!(
        r.header("www-authenticate")
            .unwrap()
            .starts_with("Basic realm=")
    );

    // No read access: the same 404 as a missing repo.
    let r = raw(
        env.addr,
        request(
            "GET",
            &adv(WEB),
            env.addr,
            Some(&basic("git", OUTSIDE)),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 404);

    // A write token passes this gate; the push itself is served by U82.
    let r = raw(
        env.addr,
        request(
            "GET",
            &adv(WEB),
            env.addr,
            Some(&basic("git", WRITE)),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(
        r.header("content-type").as_deref(),
        Some("application/x-git-receive-pack-advertisement")
    );

    // A mirror says why, to a writer as well.
    for token in [READ, WRITE] {
        let r = raw(
            env.addr,
            request(
                "GET",
                &adv(MIRROR),
                env.addr,
                Some(&basic("git", token)),
                &[],
                b"",
            ),
        )
        .await;
        assert_eq!(r.status, 403);
        assert!(r.text().contains("read-only mirror"), "{}", r.text());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mirror_clones_and_its_hidden_refs_stay_hidden() {
    let env = Env::start("", &[]).await;
    let git = Git::new();
    let ids = env.seed_mirror(&git);
    let url = env.url_with(MIRROR, "git", READ);
    let w = git.work("mirror");

    for v in ["0", "1", "2"] {
        let r = git
            .run(
                &w,
                &["-c", &format!("protocol.version={v}"), "ls-remote", &url],
            )
            .await;
        assert!(r.ok, "v{v}: {}", r.stderr);
        let refs = refs_of(&r.stdout);
        let names: Vec<&str> = refs.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, ["HEAD", "refs/heads/trunk"], "v{v}");
        assert!(!r.stdout.contains(&ids.hidden), "v{v}: {}", r.stdout);
    }
    let r = git.run(&w, &["clone", "-q", &url, "c"]).await;
    assert!(r.ok, "{}", r.stderr);
    let c = w.join("c");
    assert_eq!(
        git.run(&c, &["rev-parse", "HEAD"]).await.stdout.trim(),
        ids.trunk
    );
    // The commit only a pull ref reaches did not come along.
    let has = git.run(&c, &["cat-file", "-e", &ids.hidden]).await;
    assert!(!has.ok, "the hidden commit must not be in the clone");

    // A `want` for it is refused by protocols that check wants against what was advertised.
    let r = git
        .run(
            &c,
            &["-c", "protocol.version=0", "fetch", "origin", &ids.hidden],
        )
        .await;
    assert!(!r.ok, "a want for an unadvertised object is refused");
    assert!(
        !git.run(&c, &["cat-file", "-e", &ids.hidden]).await.ok,
        "and nothing was written"
    );
    // The same under protocol v2, which would let a client want any object that exists: a
    // mirror is served over v0/v1, so the want is refused there too.
    let r = git
        .run(
            &c,
            &["-c", "protocol.version=2", "fetch", "origin", &ids.hidden],
        )
        .await;
    assert!(!r.ok, "a v2 client is served v0 by a mirror: {}", r.stderr);
    assert!(!git.run(&c, &["cat-file", "-e", &ids.hidden]).await.ok);
    // A derived view is still served over v2 (the shallow and partial tests rely on it).
    let r = git
        .run(&w, &["-c", "protocol.version=2", "ls-remote", &url])
        .await;
    assert!(r.ok, "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_paths_are_404_and_start_no_child() {
    let env = Env::start("", &[]).await;
    env.seed().await;
    let before: Vec<_> = std::fs::read_dir(&env.view_dir)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    let long = "n".repeat(101);
    let auth = basic("git", READ);
    let bad = [
        "/acme/%2e%2e/info/refs?service=git-upload-pack".to_owned(),
        "/%2e%2e/web/info/refs?service=git-upload-pack".to_owned(),
        "/acme/web%00/info/refs?service=git-upload-pack".to_owned(),
        "/acme/we%5cb/info/refs?service=git-upload-pack".to_owned(),
        "/acme/we%2fb/info/refs?service=git-upload-pack".to_owned(),
        "/acme/%252e%252e/info/refs?service=git-upload-pack".to_owned(),
        "/acme//web/info/refs?service=git-upload-pack".to_owned(),
        "/acme/.hidden/info/refs?service=git-upload-pack".to_owned(),
        "/acme/web./info/refs?service=git-upload-pack".to_owned(),
        format!("/acme/{long}/info/refs?service=git-upload-pack"),
        "/acme/web.git/info/refs/?service=git-upload-pack".to_owned(),
        "/acme/web.git/objects/info/packs".to_owned(),
        "/acme/web.git/HEAD".to_owned(),
        "/acme/web.git/git-upload-archive".to_owned(),
        "/acme/web.git/extra/git-upload-pack".to_owned(),
    ];
    for target in &bad {
        let r = raw(
            env.addr,
            request("GET", target, env.addr, Some(&auth), &[], b""),
        )
        .await;
        assert_eq!(r.status, 404, "{target}: {}", r.head);
    }
    // A raw backslash is not even a valid request target.
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/we\\b/info/refs?service=git-upload-pack",
            env.addr,
            Some(&auth),
            &[],
            b"",
        ),
    )
    .await;
    assert!(matches!(r.status, 400 | 404), "{}", r.head);
    // A 100-byte name is a name (and is simply not there).
    let r = raw(
        env.addr,
        request(
            "GET",
            &format!(
                "/acme/{}/info/refs?service=git-upload-pack",
                "n".repeat(100)
            ),
            env.addr,
            Some(&auth),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 404);

    // Services other than the two, and the dumb protocol's missing service: 403, no child.
    for target in [
        "/acme/web.git/info/refs?service=git-upload-archive",
        "/acme/web.git/info/refs?service=git-receive-pack%00",
        "/acme/web.git/info/refs?service=GIT-UPLOAD-PACK",
        "/acme/web.git/info/refs",
    ] {
        let r = raw(
            env.addr,
            request("GET", target, env.addr, Some(&auth), &[], b""),
        )
        .await;
        assert_eq!(r.status, 403, "{target}: {}", r.head);
    }
    // A token in the query string is a 400 before anything else.
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack&token=abc",
            env.addr,
            None,
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 400);
    // The wrong method on a route.
    let r = raw(
        env.addr,
        request(
            "POST",
            "/acme/web.git/info/refs?service=git-upload-pack",
            env.addr,
            Some(&auth),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 405);

    let after: Vec<_> = std::fs::read_dir(&env.view_dir)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert_eq!(
        before, after,
        "no view was built or opened for a refused request"
    );
    let rendered = env.metrics.render();
    assert!(
        rendered.contains("asc_git_rejected_total{reason=\"bad_path\"}"),
        "{rendered}"
    );
}

fn gzip_zeros(megabytes: usize) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let zeros = vec![0u8; 1 << 20];
    for _ in 0..megabytes {
        e.write_all(&zeros).unwrap();
    }
    e.finish().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_bodies_are_bounded_typed_and_probed() {
    let env = Env::start("[limits.git]\nfetch_body_bytes = \"256KiB\"\n", &[]).await;
    env.seed().await;
    let auth = basic("git", READ);
    let post = |extra: &[(&str, &str)], body: &[u8]| {
        request(
            "POST",
            "/acme/web.git/git-upload-pack",
            env.addr,
            Some(&auth),
            extra,
            body,
        )
    };

    // The bare `0000` probe is answered at once, without a child, and charges nothing to a view.
    let r = raw(env.addr, post(&[UPLOAD_TYPE], b"0000")).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert!(r.body.is_empty());
    assert_eq!(
        r.header("content-type").as_deref(),
        Some("application/x-git-upload-pack-result")
    );
    assert!(r.header("cache-control").unwrap().contains("no-cache"));
    assert!(
        !env.view_dir.join(format!("{}.git", env.web.key.0)).exists(),
        "no view for a probe"
    );

    // Content type and encoding.
    let r = raw(env.addr, post(&[("Content-Type", "text/plain")], b"0000")).await;
    assert_eq!(r.status, 415);
    let r = raw(env.addr, post(&[], b"0000")).await;
    assert_eq!(r.status, 415);
    let r = raw(
        env.addr,
        post(&[UPLOAD_TYPE, ("Content-Encoding", "br")], b"0000"),
    )
    .await;
    assert_eq!(r.status, 415);

    // A huge `have` list: over the cap on the wire.
    let mut haves = String::new();
    while haves.len() < 300 * 1024 {
        haves.push_str(&pkt("have 0123456789012345678901234567890123456789\n"));
    }
    let r = raw(env.addr, post(&[UPLOAD_TYPE], haves.as_bytes())).await;
    assert_eq!(r.status, 413, "{}", r.head);

    // A small gzip body that inflates to 128 MiB: cut at the cap, never inflated whole.
    let bomb = gzip_zeros(128);
    assert!(
        bomb.len() < 256 * 1024,
        "the bomb is small on the wire: {}",
        bomb.len()
    );
    let started = Instant::now();
    let r = raw(
        env.addr,
        post(&[UPLOAD_TYPE, ("Content-Encoding", "gzip")], &bomb),
    )
    .await;
    assert_eq!(r.status, 413, "{}", r.head);
    assert!(started.elapsed() < Duration::from_secs(10));
    // Damaged gzip.
    let r = raw(
        env.addr,
        post(&[UPLOAD_TYPE, ("Content-Encoding", "gzip")], b"not gzip"),
    )
    .await;
    assert_eq!(r.status, 400);
    // An empty body.
    let r = raw(env.addr, post(&[UPLOAD_TYPE], b"")).await;
    assert_eq!(r.status, 400);

    let rendered = env.metrics.render();
    assert!(
        rendered.contains("asc_git_rejected_total{reason=\"body_size\"}"),
        "{rendered}"
    );
    assert!(
        rendered.contains("asc_git_rejected_total{reason=\"content_type\"}"),
        "{rendered}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gzip_request_that_git_itself_sends_is_inflated_and_served() {
    // git gzips a negotiation larger than 1 KiB: a clone of a repo with many refs-worth of
    // `have`s is not needed; a deepen with many wants is. Drive it by hand with a real v0 request.
    let env = Env::start("", &[]).await;
    let s = env.seed().await;
    let auth = basic("git", READ);
    let want = env.git_id(s.c).await;
    let body = format!(
        "{}{}0000{}",
        pkt(&format!("want {want} side-band-64k ofs-delta agent=t\n")),
        "",
        pkt("done\n")
    );
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(body.as_bytes()).unwrap();
    let gz = e.finish().unwrap();
    let r = raw(
        env.addr,
        request(
            "POST",
            "/acme/web.git/git-upload-pack",
            env.addr,
            Some(&auth),
            &[UPLOAD_TYPE, ("Content-Encoding", "gzip")],
            &gz,
        ),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(
        r.header("content-type").as_deref(),
        Some("application/x-git-upload-pack-result")
    );
    let text = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        text.contains("PACK"),
        "a pack came back: {} bytes",
        r.body.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_client_is_cut_by_the_idle_and_wall_limits() {
    let env = Env::start(
        "[limits.git]\nrequest_timeout = \"6s\"\nidle_timeout = \"1s\"\n",
        &[],
    )
    .await;
    env.seed().await;
    let auth = basic("git", READ);
    let mut s = tokio::net::TcpStream::connect(env.addr).await.unwrap();
    let head = format!(
        "POST /acme/web.git/git-upload-pack HTTP/1.1\r\nHost: {}\r\nAuthorization: {auth}\r\nContent-Type: application/x-git-upload-pack-request\r\nContent-Length: 1000\r\n\r\n0032want",
        env.addr
    );
    s.write_all(head.as_bytes()).await.unwrap();
    let started = Instant::now();
    let r = read_response(s).await;
    assert_eq!(r.status, 408, "{}", r.head);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cut by the idle limit, not left to the wall: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fifth_concurrent_fetch_of_one_principal_is_429() {
    let env = Env::start("", &[]).await;
    env.seed().await;
    let auth = basic("git", READ);
    // One request first, so admission knows this token (it is charged by principal from then on).
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            env.addr,
            Some(&auth),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 200);

    let mut held = Vec::new();
    for _ in 0..4 {
        let mut s = tokio::net::TcpStream::connect(env.addr).await.unwrap();
        let head = format!(
            "POST /acme/web.git/git-upload-pack HTTP/1.1\r\nHost: {}\r\nAuthorization: {auth}\r\nContent-Type: application/x-git-upload-pack-request\r\nContent-Length: 1000\r\n\r\n0032want",
            env.addr
        );
        s.write_all(head.as_bytes()).await.unwrap();
        held.push(s);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let r = raw(
        env.addr,
        request(
            "POST",
            "/acme/web.git/git-upload-pack",
            env.addr,
            Some(&auth),
            &[UPLOAD_TYPE],
            b"0000",
        ),
    )
    .await;
    assert_eq!(r.status, 429, "{}", r.head);
    assert!(r.header("retry-after").is_some(), "{}", r.head);
    // Discovery is `cheap` and is not held back by the fetch slots.
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            env.addr,
            Some(&auth),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 200);
    // Another principal is not affected.
    let other = basic("git", WRITE);
    let r = raw(
        env.addr,
        request(
            "POST",
            "/acme/web.git/git-upload-pack",
            env.addr,
            Some(&other),
            &[UPLOAD_TYPE],
            b"0000",
        ),
    )
    .await;
    assert_eq!(r.status, 200);
    // Freed slots are usable again.
    drop(held);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let r = raw(
        env.addr,
        request(
            "POST",
            "/acme/web.git/git-upload-pack",
            env.addr,
            Some(&auth),
            &[UPLOAD_TYPE],
            b"0000",
        ),
    )
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_by_config_serves_the_stub_in_a_build_with_the_layer() {
    let env = Env::start("[git]\nenabled = false\n", &[WEB]).await;
    env.seed().await;
    // Anonymous: the challenge, even for a repo the authorizer calls public.
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            env.addr,
            None,
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 401);
    assert!(
        r.header("www-authenticate")
            .unwrap()
            .starts_with("Basic realm=")
    );
    // Authenticated and allowed to read: 403 with the flag and the text.
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            env.addr,
            Some(&basic("git", READ)),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(r.header("x-asc-git").as_deref(), Some("disabled"));
    assert_eq!(r.text().trim(), "asc: git protocol disabled on this server");
    // No read access: 404 as before.
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            env.addr,
            Some(&basic("git", OUTSIDE)),
            &[],
            b"",
        ),
    )
    .await;
    // `acme/web` is public in this table, so the outsider may read it; a repo that is not
    // public and not granted is 404.
    assert_eq!(r.status, 403);
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/other.git/info/refs?service=git-upload-pack",
            env.addr,
            Some(&basic("git", OUTSIDE)),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 404);
    // Real git fails on it with the 403.
    let git = Git::new();
    let r = git
        .run(
            &git.work("w"),
            &["ls-remote", &env.url_with(WEB, "git", READ)],
        )
        .await;
    assert!(!r.ok);
    assert!(r.stderr.contains("403"), "{}", r.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_request_is_audited_and_counted_without_a_credential() {
    let records = audit_records();
    let env = Env::start("", &[]).await;
    env.seed().await;
    let git = Git::new();
    let w = git.work("audit");
    let url = env.url_with(WEB, "x-access-token", READ);
    let r = git.run(&w, &["clone", "-q", &url, "c"]).await;
    assert!(r.ok, "{}", r.stderr);
    let r = git
        .run(&w, &["-c", "protocol.version=0", "ls-remote", &url])
        .await;
    assert!(r.ok);
    // The 404 and a refusal are audited too.
    let _ = raw(
        env.addr,
        request(
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            env.addr,
            Some(&basic("git", OUTSIDE)),
            &[],
            b"",
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let all = records.lock().unwrap().clone();
    let mine: Vec<_> = all
        .iter()
        .filter(|r| r.get("repo").map(String::as_str) == Some(WEB))
        .collect();
    assert!(!mine.is_empty());
    for rec in &mine {
        for v in rec.values() {
            assert!(
                !v.contains(READ) && !v.contains(OUTSIDE),
                "credential in audit: {v}"
            );
            assert!(!v.contains("Basic "), "{v}");
        }
        assert!(
            rec.contains_key("op_id") && rec.contains_key("outcome") && rec.contains_key("peer")
        );
    }
    let fetch = mine
        .iter()
        .find(|r| {
            r.get("method").map(String::as_str) == Some("POST")
                && r.get("principal").map(String::as_str) == Some("reader")
                && r.get("version").map(String::as_str) == Some("v2")
        })
        .expect("the fetch POST was audited");
    assert_eq!(fetch["principal"], "reader");
    assert_eq!(fetch["service"], "git-upload-pack");
    assert_eq!(fetch["version"], "v2");
    assert_eq!(fetch["outcome"], "ok");
    assert_eq!(fetch["status"], "200");
    assert!(fetch["bytes_out"].parse::<u64>().unwrap() > 0);
    assert!(fetch["bytes_in"].parse::<u64>().unwrap() > 0);
    assert!(
        mine.iter()
            .any(|r| r.get("version").map(String::as_str) == Some("v0")),
        "the v0 listing is labelled v0"
    );
    assert!(
        mine.iter().any(
            |r| r.get("outcome").map(String::as_str) == Some("rejected:no_read")
                && r.get("principal").map(String::as_str) == Some("-")
        ),
        "the refused request is audited with its reason"
    );

    let metrics = env.metrics.render();
    assert!(
        metrics.contains(
            "asc_git_requests_total{service=\"git-upload-pack\",version=\"v2\",status=\"200\"}"
        ),
        "{metrics}"
    );
    assert!(
        metrics.contains("asc_git_bytes_total{dir=\"out\"}"),
        "{metrics}"
    );
    assert!(metrics.contains("asc_git_child_seconds_count"), "{metrics}");
    assert!(
        metrics.contains("asc_git_rejected_total{reason=\"no_read\"}"),
        "{metrics}"
    );
    assert!(!metrics.contains(READ));

    // The authorizer was asked with the repo name and read access for the fetch.
    assert!(
        env.acl
            .log
            .lock()
            .unwrap()
            .contains(&(WEB.to_owned(), Access::Read))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_repo_answers_503_with_retry_after_while_its_view_builds() {
    let env = Env::start("", &[]).await;
    env.seed().await;
    // Enough graph rows that the estimate passes the inline threshold (as in the view tests).
    let rows = asc_git::compat::INLINE_BUILD_OBJECTS / asc_serve::git_view::OBJECTS_PER_CHANGESET + 1;
    let mut tx = env.registry.pool().begin().await.unwrap();
    for i in 0..rows {
        let mut cs = [0u8; 32];
        cs[..8].copy_from_slice(&i.to_be_bytes());
        sqlx::query("INSERT INTO cs_graph (ns, cs, gen, parents) VALUES ($1, $2, 0, $3)")
            .bind(env.web.ns.as_bytes().to_vec())
            .bind(cs.to_vec())
            .bind(Vec::<u8>::new())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let auth = basic("git", READ);
    let r = raw(
        env.addr,
        request(
            "GET",
            "/acme/web.git/info/refs?service=git-upload-pack",
            env.addr,
            Some(&auth),
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(r.status, 503, "{}", r.head);
    assert!(r.header("retry-after").is_some());
    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let r = raw(
            env.addr,
            request(
                "GET",
                "/acme/web.git/info/refs?service=git-upload-pack",
                env.addr,
                Some(&auth),
                &[],
                b"",
            ),
        )
        .await;
        if r.status == 200 {
            ok = true;
            break;
        }
        assert_eq!(r.status, 503);
    }
    assert!(ok, "the background build finishes and the repo is served");
}
