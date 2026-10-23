//! M-meta (PLAN U71, SPEC §18): a mirror-mode `asc serve` answers the metadata RPCs, and for 500
//! random (rev, path) pairs every answer equals the matching git CLI output.
//!
//! The fixture is a real git repository (a Cargo monorepo with merges, deletions, an executable
//! file and nested directories) that the server mirrors with the production mirror code. The
//! server is reached over a loopback gRPC listener behind the real authorizer, in git id mode.
//! The git CLI output is the oracle; every git process goes through `asc_git::realgit`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use asc_git::realgit::RealGit;
use asc_rpc::auth::{Access, StaticTokens};
use asc_rpc::server::{RepoName, ServerInfo, wire};
use asc_rpc::services::projects_client::ProjectsClient;
use asc_rpc::services::repo_client::RepoClient;
use asc_serve::mirror::{Mirror, MirrorConfig, MirrorTarget};
use asc_serve::registry::{Extensions, NewRepo, Registry, RepoMode};
use asc_serve::{build_routes, migrate, projects, read};
use futures::TryStreamExt as _;
use tonic::transport::{Channel, Server};
use tonic::{Code, Request};

const REPO: &str = "asc/dds";
const TOKEN: &str = "meta-token";
/// The pairs the milestone names.
const PAIRS: usize = 500;
const COMMITS: usize = 64;
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

// ------------------------------------------------------------------------------ git oracle

fn git_cmd(dir: &Path, args: &[&str]) -> std::process::Command {
    let real = RealGit::resolve(None, None).expect("git is installed");
    let mut cmd = real.command().expect("a git command");
    cmd.arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off", "-c", "core.autocrlf=false"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

fn git_raw(dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Vec<u8> {
    let mut cmd = git_cmd(dir, args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_raw(dir, args, &[]))
        .expect("utf-8 git output")
        .trim_end_matches('\n')
        .to_owned()
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git_cmd(dir, args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

// ------------------------------------------------------------------------------ fixture

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

struct Work {
    dir: PathBuf,
    files: BTreeMap<String, Vec<String>>,
    clock: i64,
    n: usize,
}

impl Work {
    fn write(&mut self, path: &str) {
        let full = self.dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let lines = &self.files[path];
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(full, body).unwrap();
    }

    fn edit(&mut self, rng: &mut Rng, path: &str) {
        self.n += 1;
        let n = self.n;
        let lines = self.files.entry(path.to_owned()).or_default();
        let fresh = format!("line {n} {:x}", rng.next() & 0xffff);
        match rng.below(4) {
            _ if lines.is_empty() => lines.push(fresh),
            0 => {
                let at = rng.below(lines.len() + 1);
                lines.insert(at, fresh);
            }
            1 => {
                let at = rng.below(lines.len());
                lines[at] = fresh;
            }
            2 if lines.len() > 2 => {
                let at = rng.below(lines.len());
                lines.remove(at);
            }
            _ => lines.push(fresh),
        }
        self.write(path);
    }

    fn remove(&mut self, path: &str) {
        self.files.remove(path);
        std::fs::remove_file(self.dir.join(path)).unwrap();
    }

    fn commit(&mut self, msg: &str) {
        self.clock += 60;
        let date = format!("{} +0000", 1_700_000_000 + self.clock);
        let envs = [
            ("GIT_AUTHOR_NAME", "Ada".to_owned()),
            ("GIT_AUTHOR_EMAIL", "ada@example.test".to_owned()),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_NAME", "Grace".to_owned()),
            ("GIT_COMMITTER_EMAIL", "grace@example.test".to_owned()),
            ("GIT_COMMITTER_DATE", date),
        ];
        git_raw(&self.dir, &["add", "-A"], &[]);
        git_raw(
            &self.dir,
            &["commit", "-q", "--allow-empty", "-m", msg],
            &envs,
        );
    }

    fn merge(&mut self, branch: &str, msg: &str) {
        self.clock += 60;
        let date = format!("{} +0000", 1_700_000_000 + self.clock);
        let envs = [
            ("GIT_AUTHOR_NAME", "Ada".to_owned()),
            ("GIT_AUTHOR_EMAIL", "ada@example.test".to_owned()),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_NAME", "Grace".to_owned()),
            ("GIT_COMMITTER_EMAIL", "grace@example.test".to_owned()),
            ("GIT_COMMITTER_DATE", date),
        ];
        git_raw(
            &self.dir,
            &["merge", "--no-ff", "-q", "-m", msg, branch],
            &envs,
        );
        // The merge brought in the branch's files: reload what the model tracks.
        let listing = git(&self.dir, &["ls-files"]);
        self.files = listing
            .lines()
            .map(|p| {
                let body = std::fs::read_to_string(self.dir.join(p)).unwrap();
                (p.to_owned(), body.lines().map(str::to_owned).collect())
            })
            .collect();
    }
}

const CRATES: [&str; 3] = ["a", "b", "c"];

/// A real git repository with `COMMITS` commits on `master`, side branches merged with
/// `--no-ff`, deletions, an executable file, a lightweight tag and a second branch.
fn build_upstream(root: &Path) -> PathBuf {
    let work_dir = root.join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    git_raw(&work_dir, &["init", "-q", "--initial-branch=master"], &[]);
    git_raw(&work_dir, &["config", "core.filemode", "true"], &[]);
    let mut w = Work {
        dir: work_dir.clone(),
        files: BTreeMap::new(),
        clock: 0,
        n: 0,
    };
    let mut rng = Rng(SEED);
    for c in CRATES {
        let deps = if c == "a" {
            "\n[dependencies]\nb = { path = \"../b\" }\n"
        } else {
            ""
        };
        w.files.insert(
            format!("{c}/Cargo.toml"),
            format!("[package]\nname = \"{c}\"\nversion = \"0.1.0\"\n{deps}")
                .lines()
                .map(str::to_owned)
                .collect(),
        );
        w.write(&format!("{c}/Cargo.toml"));
        w.edit(&mut rng, &format!("{c}/src/lib.rs"));
    }
    w.edit(&mut rng, "docs/readme.md");
    w.edit(&mut rng, "tools/run.sh");
    w.commit("initial");
    git_raw(
        &work_dir,
        &["update-index", "--chmod=+x", "tools/run.sh"],
        &[],
    );
    w.commit("make the tool executable");

    let mutable: Vec<String> = [
        "a/src/lib.rs",
        "a/src/one.rs",
        "a/src/deep/er/two.rs",
        "b/src/lib.rs",
        "b/src/three.rs",
        "c/src/lib.rs",
        "c/data/blob.txt",
        "docs/readme.md",
        "docs/guide/intro.md",
        "tools/run.sh",
        "notes.txt",
    ]
    .map(str::to_owned)
    .to_vec();
    let mut side = 0usize;
    for i in 0..COMMITS {
        if i % 13 == 12 {
            // A side branch touching its own files, merged back with a merge commit.
            side += 1;
            let name = format!("side{side}");
            git_raw(&work_dir, &["checkout", "-q", "-b", &name], &[]);
            for k in 0..2 {
                w.edit(&mut rng, &format!("b/src/side{side}_{k}.rs"));
                w.edit(&mut rng, &format!("docs/side{side}.md"));
                w.commit(&format!("side {side} commit {k}"));
            }
            git_raw(&work_dir, &["checkout", "-q", "master"], &[]);
            w.edit(&mut rng, "notes.txt");
            w.commit(&format!("master work during side {side}"));
            w.merge(&name, &format!("Merge branch '{name}'"));
            continue;
        }
        for _ in 0..1 + rng.below(3) {
            let p = rng.pick(&mutable).clone();
            if w.files.contains_key(&p) && rng.below(8) == 0 && !p.ends_with("lib.rs") {
                w.remove(&p);
            } else {
                w.edit(&mut rng, &p);
            }
        }
        w.commit(&format!("commit {i}"));
        if i == 20 {
            git_raw(&work_dir, &["tag", "v1"], &[]);
        }
        if i == 30 {
            git_raw(&work_dir, &["branch", "release", "HEAD"], &[]);
        }
    }
    let bare = root.join("up.git");
    git_raw(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work_dir.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
        &[],
    );
    bare
}

// ------------------------------------------------------------------------------ server

#[derive(Clone)]
struct Server_ {
    _tmp: Arc<tempfile::TempDir>,
    up: PathBuf,
    repo: RepoClient<Channel>,
    projects: ProjectsClient<Channel>,
    anon: RepoClient<Channel>,
}

fn rq<T>(m: T) -> Request<T> {
    let mut r = Request::new(m);
    r.metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    r
}

fn repo_ref() -> Option<wire::RepoRef> {
    Some(wire::RepoRef {
        name: REPO.into(),
        id_mode: wire::IdMode::Git as i32,
    })
}

async fn start() -> Server_ {
    let tmp = tempfile::tempdir().unwrap();
    let up = build_upstream(tmp.path());

    let pool = migrate::connect("sqlite::memory:", 1).await.unwrap();
    migrate::migrate(&pool).await.unwrap();
    let registry = Arc::new(Registry::new(pool, "memory:///").unwrap());
    let name = RepoName::parse(REPO).unwrap();
    registry
        .create(
            &name,
            &NewRepo {
                mode: RepoMode::Mirror,
                ..NewRepo::default()
            },
        )
        .await
        .unwrap();

    let ext = Extensions::new();
    asc_serve::update::register(&ext, &registry);
    read::register(&ext, &registry);
    let _warmer = projects::register(&ext, &registry);

    let real = RealGit::resolve(None, None).expect("git is installed");
    let mirror = Mirror::new(
        registry.clone(),
        &ext.pipeline,
        real,
        MirrorConfig {
            dir: tmp.path().join("mirrors"),
            secret: None,
            forgejo_url: None,
            allowed_hosts: Vec::new(),
            interval: Duration::from_secs(3600),
            fetch_timeout: Duration::from_secs(300),
        },
    );
    // A local upstream is operator-configured; a client could not name it.
    mirror
        .sources()
        .set_configured(
            &name,
            MirrorTarget {
                url: up.to_str().unwrap().to_owned(),
                token: None,
            },
        )
        .unwrap();
    let report = mirror.sync(&name).await.unwrap();
    assert!(report.commits > COMMITS, "{report:?}");
    // A second sync of an unchanged upstream imports nothing (the webhook/poll path).
    assert_eq!(mirror.sync(&name).await.unwrap().commits, 0);

    let auth = StaticTokens::new().with_token(TOKEN, "reader", Access::Read);
    let routes = build_routes(&ext, Arc::new(auth), ServerInfo::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let incoming = futures::stream::unfold(listener, |l| async move {
        let r = l.accept().await.map(|(s, _)| s);
        Some((r, l))
    });
    tokio::spawn(async move {
        let _ = Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming)
            .await;
    });
    let channel = Channel::from_shared(format!("http://127.0.0.1:{port}"))
        .unwrap()
        .connect_lazy();
    Server_ {
        _tmp: Arc::new(tmp),
        up,
        repo: RepoClient::new(channel.clone()).max_decoding_message_size(64 << 20),
        projects: ProjectsClient::new(channel.clone()),
        anon: RepoClient::new(channel),
    }
}

// ------------------------------------------------------------------------------ helpers

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Bytes {
    assert_eq!(s.len() % 2, 0);
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect::<Vec<u8>>()
        .into()
}

fn lines(s: &str) -> Vec<String> {
    s.lines().map(str::to_owned).collect()
}

async fn log_ids(s: &mut Server_, rev: &str, path: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let req = wire::LogRequest {
            repo: repo_ref(),
            include: vec![rev.to_owned()],
            paths: path.map(str::to_owned).into_iter().collect(),
            page_token: token.clone(),
            ..Default::default()
        };
        let msgs: Vec<wire::LogResponse> = s
            .repo
            .log(rq(req))
            .await
            .unwrap()
            .into_inner()
            .try_collect()
            .await
            .unwrap();
        for m in &msgs {
            out.extend(m.entries.iter().map(|e| hex(&e.id)));
        }
        token = msgs
            .last()
            .map(|m| m.next_page_token.clone())
            .unwrap_or_default();
        if token.is_empty() {
            return out;
        }
    }
}

async fn resolve(s: &mut Server_, specs: &[&str]) -> Vec<String> {
    s.repo
        .resolve(rq(wire::ResolveRequest {
            repo: repo_ref(),
            revspecs: specs.iter().map(|x| (*x).to_owned()).collect(),
        }))
        .await
        .unwrap()
        .into_inner()
        .ids
        .iter()
        .map(|b| hex(b))
        .collect()
}

/// `git blame --porcelain` as per-line (commit, original line, original path).
fn git_blame(up: &Path, rev: &str, path: &str) -> Vec<(String, u32, String)> {
    let text =
        String::from_utf8(git_raw(up, &["blame", "--porcelain", rev, "--", path], &[])).unwrap();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    let mut cur: Option<(String, u32)> = None;
    for line in text.lines() {
        if line.starts_with('\t') {
            let (sha, orig) = cur.take().expect("a header precedes each line");
            let file = names[&sha].clone();
            out.push((sha, orig, file));
            continue;
        }
        let first = line.split(' ').next().unwrap_or("");
        if cur.is_none() && first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let mut it = line.split(' ');
            let sha = it.next().unwrap().to_owned();
            let orig: u32 = it.next().unwrap().parse().unwrap();
            cur = Some((sha, orig));
        } else if let Some(f) = line.strip_prefix("filename ") {
            let sha = cur.as_ref().expect("filename follows a header").0.clone();
            names.insert(sha, f.to_owned());
        }
    }
    out
}

/// `git log --format=%H -- path` for asc's changeset model: a changeset's changes are its diff
/// against the first parent, so a merge is listed when that diff touches the path (git's default
/// history simplification would drop a merge that equals its second parent there). The oracle
/// is git's own first-parent merge diff, filtered by the same path-or-directory prefix rule.
fn git_path_log(up: &Path, rev: &str, path: &str) -> Vec<String> {
    path_log(up, rev, path, false)
}

type Touched = Vec<(String, Vec<String>)>;

/// The commits reachable from `rev` with the files each changes against its first parent,
/// newest first in git's order. One git process per (rev, mode); the 500 pairs reuse them.
fn touched(up: &Path, rev: &str, first_parent: bool) -> Arc<Touched> {
    type Memo = std::sync::Mutex<HashMap<(String, bool), Arc<Touched>>>;
    static MEMO: std::sync::OnceLock<Memo> = std::sync::OnceLock::new();
    let memo = MEMO.get_or_init(Default::default);
    if let Some(t) = memo.lock().unwrap().get(&(rev.to_owned(), first_parent)) {
        return t.clone();
    }
    let mut args = vec![
        "log",
        "--format=@@%H",
        "--diff-merges=first-parent",
        "--name-only",
    ];
    if first_parent {
        args.push("--first-parent");
    }
    args.push(rev);
    let text = git(up, &args);
    let mut out: Touched = Vec::new();
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("@@") {
            out.push((h.to_owned(), Vec::new()));
        } else if !line.is_empty() {
            out.last_mut().unwrap().1.push(line.to_owned());
        }
    }
    let t = Arc::new(out);
    memo.lock()
        .unwrap()
        .insert((rev.to_owned(), first_parent), t.clone());
    t
}

fn path_log(up: &Path, rev: &str, path: &str, first_parent: bool) -> Vec<String> {
    let prefix = format!("{path}/");
    touched(up, rev, first_parent)
        .iter()
        .filter(|(_, files)| files.iter().any(|f| f == path || f.starts_with(&prefix)))
        .map(|(id, _)| id.clone())
        .collect()
}

fn rev_list_parents(up: &Path, rev: &str) -> Arc<Vec<String>> {
    static MEMO: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Arc<Vec<String>>>>> =
        std::sync::OnceLock::new();
    let memo = MEMO.get_or_init(Default::default);
    if let Some(t) = memo.lock().unwrap().get(rev) {
        return t.clone();
    }
    let t = Arc::new(lines(&git(up, &["rev-list", "--parents", rev])));
    memo.lock().unwrap().insert(rev.to_owned(), t.clone());
    t
}

fn mode_str(m: u32) -> String {
    format!("{m:06o}")
}

// ------------------------------------------------------------------------------ tests

/// Checks the (rev, path) pairs `range` of the sample (one task's share) and returns how many
/// files were blamed and how many path-limited logs were not empty.
async fn run_pairs(
    mut s: Server_,
    all_commits: Arc<Vec<String>>,
    range: std::ops::Range<usize>,
) -> (usize, usize) {
    let up = s.up.clone();
    // Each pair draws from its own stream, so the sample does not depend on the task split.
    let mut trees: HashMap<String, Vec<String>> = HashMap::new();
    let mut blamed = 0usize;
    let mut logged_with_path = 0usize;
    for i in range {
        let mut rng = Rng(SEED ^ (i as u64 + 1).wrapping_mul(0xD6E8_FEB8_6659_FD93));
        rng.next();
        let rev = all_commits[rng.below(all_commits.len())].clone();
        let files = trees
            .entry(rev.clone())
            .or_insert_with(|| lines(&git(&up, &["ls-tree", "-r", "--name-only", &rev])))
            .clone();
        // Mostly files that exist at the revision, sometimes a path that does not.
        let path = if files.is_empty() || rng.below(10) == 0 {
            "nothing/here.rs".to_owned()
        } else {
            rng.pick(&files).clone()
        };
        let exists = files.contains(&path);
        let ctx = format!("pair {i}: {rev} {path}");

        // Log --format=%H, with and without the path. The server orders by (generation, id), so
        // the set equals git's and the order is a valid topological one; the path-limited log
        // is the same order restricted to the commits git says touch the path.
        let all_ids = log_ids(&mut s, &rev, None).await;
        let rev_list = rev_list_parents(&up, &rev);
        let parent_map: HashMap<&str, Vec<&str>> = rev_list
            .iter()
            .map(|l| {
                let mut f = l.split(' ');
                (f.next().unwrap(), f.collect())
            })
            .collect();
        assert_eq!(
            all_ids.iter().map(String::as_str).collect::<BTreeSet<_>>(),
            parent_map.keys().copied().collect::<BTreeSet<_>>(),
            "Log set {ctx}"
        );
        assert_eq!(
            all_ids.len(),
            parent_map.len(),
            "Log has no duplicates {ctx}"
        );
        let pos: HashMap<&str, usize> = all_ids
            .iter()
            .enumerate()
            .map(|(i, c)| (c.as_str(), i))
            .collect();
        for (c, ps) in &parent_map {
            assert!(
                ps.iter().all(|p| pos[c] < pos[p]),
                "Log is topological {ctx}"
            );
        }
        let want: BTreeSet<String> = git_path_log(&up, &rev, &path).into_iter().collect();
        let got = log_ids(&mut s, &rev, Some(&path)).await;
        assert_eq!(
            got.iter().cloned().collect::<BTreeSet<_>>(),
            want,
            "Log {ctx}"
        );
        assert_eq!(got.len(), want.len(), "Log has no duplicates {ctx}");
        let expect: Vec<&String> = all_ids.iter().filter(|c| want.contains(*c)).collect();
        assert_eq!(got.iter().collect::<Vec<_>>(), expect, "Log order {ctx}");
        logged_with_path += usize::from(!want.is_empty());
        if i % 25 == 0 {
            let first_parent = lines(&git(&up, &["log", "--first-parent", "--format=%H", &rev]));
            let msgs: Vec<wire::LogResponse> = s
                .repo
                .log(rq(wire::LogRequest {
                    repo: repo_ref(),
                    include: vec![rev.clone()],
                    first_parent: true,
                    ..Default::default()
                }))
                .await
                .unwrap()
                .into_inner()
                .try_collect()
                .await
                .unwrap();
            let got: Vec<String> = msgs
                .iter()
                .flat_map(|m| m.entries.iter().map(|e| hex(&e.id)))
                .collect();
            assert_eq!(got, first_parent, "Log --first-parent {ctx}");
        }

        // MergeBase and IsAncestor against another random commit.
        let other = all_commits[rng.below(all_commits.len())].clone();
        // Every commit of the fixture descends from one root, so a merge base always exists.
        let want_all = lines(&git(&up, &["merge-base", "--all", &rev, &other]));
        let want_base: BTreeSet<String> = want_all.iter().cloned().collect();
        let mb = s
            .repo
            .merge_base(rq(wire::MergeBaseRequest {
                repo: repo_ref(),
                a: unhex(&rev),
                b: vec![unhex(&other)],
                all: true,
            }))
            .await
            .unwrap()
            .into_inner();
        let got_base: BTreeSet<String> = mb.bases.iter().map(|b| hex(b)).collect();
        assert_eq!(got_base, want_base, "MergeBase {ctx} vs {other}");
        let single = s
            .repo
            .merge_base(rq(wire::MergeBaseRequest {
                repo: repo_ref(),
                a: unhex(&rev),
                b: vec![unhex(&other)],
                all: false,
            }))
            .await
            .unwrap()
            .into_inner();
        let got_single = single.bases.iter().map(|b| hex(b)).collect::<Vec<_>>();
        assert_eq!(got_single.len(), 1, "MergeBase single {ctx} vs {other}");
        if want_all.len() == 1 {
            assert_eq!(got_single, want_all, "MergeBase single {ctx} vs {other}");
        } else {
            assert_eq!(
                got_single,
                [git(&up, &["merge-base", &rev, &other])],
                "MergeBase single {ctx} vs {other}"
            );
        }
        let anc = s
            .repo
            .is_ancestor(rq(wire::IsAncestorRequest {
                repo: repo_ref(),
                ancestor: unhex(&rev),
                descendant: unhex(&other),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            anc.result,
            git_ok(&up, &["merge-base", "--is-ancestor", &rev, &other]),
            "IsAncestor {ctx} vs {other}"
        );

        // Diff --name-status between the commit's first parent and the commit.
        let parents = rev_list_parents(&up, &rev);
        let parent = parents[0].split(' ').nth(1).map(str::to_owned);
        if let Some(parent) = parent {
            let want = git(
                &up,
                &[
                    "diff",
                    "--name-status",
                    "--no-renames",
                    &parent,
                    &rev,
                    "--",
                    &path,
                ],
            );
            let msgs: Vec<wire::DiffResponse> = s
                .repo
                .diff(rq(wire::DiffRequest {
                    repo: repo_ref(),
                    base: parent.clone(),
                    head: rev.clone(),
                    paths: vec![path.clone()],
                    format: wire::DiffFormat::Status as i32,
                    ..Default::default()
                }))
                .await
                .unwrap()
                .into_inner()
                .try_collect()
                .await
                .unwrap();
            let text: Vec<u8> = msgs.iter().flat_map(|m| m.text.to_vec()).collect();
            assert_eq!(
                String::from_utf8(text).unwrap().trim_end_matches('\n'),
                want,
                "Diff {ctx}"
            );
        }

        // Stat, ReadTree and ReadFile for the path at the revision.
        let st = s
            .repo
            .stat(rq(wire::StatRequest {
                repo: repo_ref(),
                rev: rev.clone(),
                paths: vec![Bytes::from(path.clone().into_bytes())],
                objects: vec![],
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(st.exists, [exists], "Stat exists {ctx}");
        if exists {
            let e = &st.entries[0];
            let ls = git(&up, &["ls-tree", "-l", &rev, "--", &path]);
            let mut f = ls.split_whitespace();
            let (mode, _kind, oid) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
            let size: u64 = f.next().unwrap().parse().unwrap();
            assert_eq!(mode_str(e.mode), mode, "Stat mode {ctx}");
            assert_eq!(hex(&e.git), oid, "Stat oid {ctx}");
            assert_eq!(e.size, size, "Stat size {ctx}");

            let mut data = Vec::new();
            let mut total = 0;
            let parts: Vec<wire::ReadFileResponse> = s
                .repo
                .read_file(rq(wire::ReadFileRequest {
                    repo: repo_ref(),
                    at: Some(wire::read_file_request::At::RevPath(format!(
                        "{rev}:{path}"
                    ))),
                    ..Default::default()
                }))
                .await
                .unwrap()
                .into_inner()
                .try_collect()
                .await
                .unwrap();
            for p in parts {
                data.extend_from_slice(&p.data);
                total = p.total_size;
            }
            let want = git_raw(&up, &["cat-file", "blob", &format!("{rev}:{path}")], &[]);
            assert_eq!(data, want, "ReadFile {ctx}");
            assert_eq!(total, want.len() as u64, "ReadFile total {ctx}");

            // Blame: every line's commit, original line and original path.
            let want = git_blame(&up, &rev, &path);
            let parts: Vec<wire::BlameResponse> = s
                .repo
                .blame(rq(wire::BlameRequest {
                    repo: repo_ref(),
                    rev: rev.clone(),
                    path: Bytes::from(path.clone().into_bytes()),
                    ..Default::default()
                }))
                .await
                .unwrap()
                .into_inner()
                .try_collect()
                .await
                .unwrap();
            let mut got: Vec<(String, u32, String)> = Vec::new();
            for h in parts.iter().flat_map(|p| &p.hunks) {
                assert_eq!(
                    h.start as usize,
                    got.len(),
                    "Blame hunks are contiguous {ctx}"
                );
                for k in 0..h.len {
                    got.push((
                        hex(&h.origin),
                        h.orig_start + k + 1,
                        String::from_utf8(h.orig_path.to_vec()).unwrap(),
                    ));
                }
            }
            assert_eq!(got, want, "Blame {ctx}");
            blamed += 1;
        }

        // ReadTree of the directory holding the path (non-recursive and recursive).
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
        let spec = if dir.is_empty() {
            rev.clone()
        } else {
            format!("{rev}:{dir}")
        };
        if exists || dir.is_empty() {
            let want = lines(&git(&up, &["ls-tree", "-l", &spec]));
            let mut want_rows: Vec<(String, String, String, u64)> = want
                .iter()
                .map(|l| {
                    let (meta, name) = l.split_once('\t').unwrap();
                    let mut f = meta.split_whitespace();
                    let mode = f.next().unwrap().to_owned();
                    f.next();
                    let oid = f.next().unwrap().to_owned();
                    let size = f.next().unwrap().parse().unwrap_or(0);
                    (name.to_owned(), mode, oid, size)
                })
                .collect();
            want_rows.sort();
            let parts: Vec<wire::ReadTreeResponse> = s
                .repo
                .read_tree(rq(wire::ReadTreeRequest {
                    repo: repo_ref(),
                    rev: rev.clone(),
                    path: Bytes::from(dir.to_owned().into_bytes()),
                    ..Default::default()
                }))
                .await
                .unwrap()
                .into_inner()
                .try_collect()
                .await
                .unwrap();
            let mut got_rows: Vec<(String, String, String, u64)> = parts
                .iter()
                .flat_map(|p| &p.entries)
                .map(|e| {
                    (
                        String::from_utf8(e.name.to_vec()).unwrap(),
                        mode_str(e.mode),
                        hex(&e.git),
                        e.size,
                    )
                })
                .collect();
            got_rows.sort();
            assert_eq!(got_rows, want_rows, "ReadTree {ctx}");
            let tree_git = git(
                &up,
                &[
                    "rev-parse",
                    &if dir.is_empty() {
                        format!("{rev}^{{tree}}")
                    } else {
                        spec.clone()
                    },
                ],
            );
            assert_eq!(
                parts.first().map(|p| hex(&p.tree_git)).unwrap_or_default(),
                tree_git,
                "ReadTree oid {ctx}"
            );

            // LastCommits: the newest commit touching each entry of the directory.
            let names: Vec<Bytes> = want_rows
                .iter()
                .take(6)
                .map(|r| Bytes::from(r.0.clone().into_bytes()))
                .collect();
            if !names.is_empty() {
                let lc = s
                    .repo
                    .last_commits(rq(wire::LastCommitsRequest {
                        repo: repo_ref(),
                        rev: rev.clone(),
                        dir: Bytes::from(dir.to_owned().into_bytes()),
                        names: names.clone(),
                    }))
                    .await
                    .unwrap()
                    .into_inner();
                for n in &names {
                    let name = String::from_utf8(n.to_vec()).unwrap();
                    let full = if dir.is_empty() {
                        name.clone()
                    } else {
                        format!("{dir}/{name}")
                    };
                    let want = path_log(&up, &rev, &full, true)
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                    let got = lc
                        .by_name
                        .get(&name)
                        .map(|e| hex(&e.id))
                        .unwrap_or_default();
                    assert_eq!(got, want, "LastCommits {name} in {dir:?} {ctx}");
                }
            }
        }

        // ReadChangeset: header, parents, tree and the first-parent changes.
        if i % 5 == 0 {
            let rc = s
                .repo
                .read_changeset(rq(wire::ReadChangesetRequest {
                    repo: repo_ref(),
                    rev: rev.clone(),
                }))
                .await
                .unwrap()
                .into_inner();
            let h = rc.header.unwrap();
            assert_eq!(hex(&h.id), rev, "ReadChangeset id {ctx}");
            let want_parents: Vec<String> =
                parents[0].split(' ').skip(1).map(str::to_owned).collect();
            assert_eq!(
                h.parents.iter().map(|p| hex(p)).collect::<Vec<_>>(),
                want_parents
            );
            assert_eq!(
                hex(&rc.root_git),
                git(&up, &["rev-parse", &format!("{rev}^{{tree}}")])
            );
            let a = h.author.unwrap();
            assert_eq!(
                format!(
                    "{} <{}> {}",
                    String::from_utf8_lossy(&a.name),
                    String::from_utf8_lossy(&a.email),
                    a.time
                ),
                git(&up, &["log", "-1", "--format=%an <%ae> %at", &rev]),
                "ReadChangeset author {ctx}"
            );
            let c = h.committer.unwrap();
            assert_eq!(
                format!(
                    "{} <{}> {}",
                    String::from_utf8_lossy(&c.name),
                    String::from_utf8_lossy(&c.email),
                    c.time
                ),
                git(&up, &["log", "-1", "--format=%cn <%ce> %ct", &rev]),
                "ReadChangeset committer {ctx}"
            );
            assert_eq!(
                String::from_utf8_lossy(&h.message).trim_end(),
                git(&up, &["log", "-1", "--format=%B", &rev]).trim_end(),
                "ReadChangeset message {ctx}"
            );
            let base = match want_parents.first() {
                Some(p) => p.clone(),
                None => "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned(),
            };
            let want: BTreeSet<String> = lines(&git(
                &up,
                &["diff", "--name-status", "--no-renames", &base, &rev],
            ))
            .into_iter()
            .collect();
            let got: BTreeSet<String> = rc
                .changes
                .iter()
                .map(|c| format!("{}\t{}", c.status, String::from_utf8_lossy(&c.path)))
                .collect();
            assert_eq!(got, want, "ReadChangeset changes {ctx}");
        }
    }
    (blamed, logged_with_path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn metadata_rpcs_equal_the_git_cli_on_500_random_pairs() {
    let mut s = start().await;
    let up = s.up.clone();

    // ---- Version
    let v = s
        .repo
        .version(Request::new(wire::VersionRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(!v.server_version.is_empty());
    assert!(v.features.iter().any(|f| f == "projects.v1") || v.features.is_empty());

    // ---- ListRefs equals for-each-ref
    // `ListRefs` speaks asc ids; the git map turns them into the oids the CLI prints.
    let asc_ref = Some(wire::RepoRef {
        name: REPO.into(),
        id_mode: wire::IdMode::Asc as i32,
    });
    let listed = s
        .repo
        .list_refs(rq(wire::ListRefsRequest {
            repo: asc_ref.clone(),
            limit: 1000,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    let asc_ids: Vec<Bytes> = listed
        .refs
        .iter()
        .map(|r| match r.target.as_ref().and_then(|t| t.t.as_ref()) {
            Some(wire::ref_target::T::Changeset(c)) => c.clone(),
            other => panic!("unexpected ref target {other:?}"),
        })
        .collect();
    let git_ids = s
        .repo
        .git_map(rq(wire::GitMapRequest {
            repo: asc_ref,
            asc: asc_ids,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .asc_to_git;
    let got: BTreeMap<String, String> = listed
        .refs
        .iter()
        .zip(&git_ids)
        .map(|(r, g)| (r.name.clone(), hex(g)))
        .collect();
    let want: BTreeMap<String, String> = git(
        &up,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads",
            "refs/tags",
        ],
    )
    .lines()
    .map(|l| {
        let (o, n) = l.split_once(' ').unwrap();
        (n.to_owned(), o.to_owned())
    })
    .collect();
    assert_eq!(got, want, "ListRefs");
    assert!(want.contains_key("refs/heads/master"));
    assert!(want.contains_key("refs/tags/v1"));

    // ---- the commits and files to sample
    let all_commits: Vec<String> = lines(&git(&up, &["rev-list", "--all"]));
    assert!(all_commits.len() > COMMITS);
    // ---- GitMap: every git commit maps to an asc id that maps back to it
    for chunk in all_commits.chunks(200) {
        let git_ids: Vec<Bytes> = chunk.iter().map(|c| unhex(c)).collect();
        let m = s
            .repo
            .git_map(rq(wire::GitMapRequest {
                repo: Some(wire::RepoRef {
                    name: REPO.into(),
                    id_mode: wire::IdMode::Asc as i32,
                }),
                git: git_ids.clone(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(m.git_to_asc.len(), chunk.len());
        assert!(m.git_to_asc.iter().all(|c| c.len() == 32));
        let back = s
            .repo
            .git_map(rq(wire::GitMapRequest {
                repo: Some(wire::RepoRef {
                    name: REPO.into(),
                    id_mode: wire::IdMode::Asc as i32,
                }),
                asc: m.git_to_asc.clone(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(back.asc_to_git, git_ids, "GitMap round trip");
    }
    // An unknown git oid maps to nothing.
    let unknown = s
        .repo
        .git_map(rq(wire::GitMapRequest {
            repo: Some(wire::RepoRef {
                name: REPO.into(),
                id_mode: wire::IdMode::Asc as i32,
            }),
            git: vec![Bytes::from(vec![7u8; 20])],
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(unknown.git_to_asc, [Bytes::new()]);

    // ---- Resolve equals rev-parse for the revision grammar
    let master = git(&up, &["rev-parse", "master"]);
    for spec in [
        "master".to_owned(),
        "refs/heads/master".to_owned(),
        "HEAD".to_owned(),
        "v1".to_owned(),
        "release".to_owned(),
        "master~1".to_owned(),
        "master~7".to_owned(),
        "master^1".to_owned(),
        format!("{master}~3"),
    ] {
        let want = git(&up, &["rev-parse", &spec.replace("HEAD", "master")]);
        let got = resolve(&mut s, &[&spec]).await;
        assert_eq!(got, [want], "Resolve {spec}");
    }
    assert_eq!(resolve(&mut s, &["no-such-ref"]).await, [String::new()]);

    // ---- 500 random (rev, path) pairs, five tasks of a hundred
    let all_commits = Arc::new(all_commits);
    let tasks: Vec<_> = (0..5)
        .map(|k| {
            tokio::spawn(run_pairs(
                s.clone(),
                all_commits.clone(),
                k * PAIRS / 5..(k + 1) * PAIRS / 5,
            ))
        })
        .collect();
    let (mut blamed, mut logged_with_path) = (0, 0);
    for t in tasks {
        let (b, l) = t.await.unwrap();
        blamed += b;
        logged_with_path += l;
    }
    assert!(blamed > 200, "too few blamed files: {blamed}");
    assert!(
        logged_with_path > 200,
        "too few path logs: {logged_with_path}"
    );
}

#[tokio::test]
async fn affected_equals_the_changed_paths_and_the_dependency_closure_from_git() {
    let mut s = start().await;
    let up = s.up.clone();
    let mut rng = Rng(SEED ^ 0x5555);
    let all: Vec<String> = lines(&git(&up, &["rev-list", "--first-parent", "master"]));
    // `a` depends on `b` by path; `c` is independent.
    let crate_of = |p: &str| {
        p.split('/')
            .next()
            .filter(|c| CRATES.contains(c))
            .map(str::to_owned)
    };
    for i in 0..60 {
        // Never the root commit as base: it adds the manifests themselves.
        let head = all[rng.below(all.len() - 2)].clone();
        let base = all[rng.below(all.len() - 1)].clone();
        let r = s
            .projects
            .affected(rq(wire::AffectedRequest {
                repo: repo_ref(),
                base: base.clone(),
                head: head.clone(),
                no_merge_base: true,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        let changed = lines(&git(&up, &["diff", "--name-only", &base, &head]));
        assert_eq!(
            r.changed_paths as usize,
            changed.len(),
            "pair {i}: changed paths"
        );
        assert_eq!(hex(&r.base), base);
        assert_eq!(hex(&r.head), head);
        let mut want: BTreeSet<String> = changed.iter().filter_map(|p| crate_of(p)).collect();
        if want.contains("b") {
            want.insert("a".into());
        }
        let got: BTreeSet<String> = r
            .projects
            .iter()
            .map(|p| {
                p.project
                    .as_ref()
                    .unwrap()
                    .id
                    .trim_start_matches("rust:")
                    .to_owned()
            })
            .collect();
        assert!(!r.all);
        assert_eq!(got, want, "pair {i}: Affected {base}..{head}");
        let direct: BTreeSet<String> = r
            .projects
            .iter()
            .filter(|p| p.direct)
            .map(|p| {
                p.project
                    .as_ref()
                    .unwrap()
                    .id
                    .trim_start_matches("rust:")
                    .to_owned()
            })
            .collect();
        let want_direct: BTreeSet<String> = changed.iter().filter_map(|p| crate_of(p)).collect();
        assert_eq!(direct, want_direct, "pair {i}: direct projects");
    }
    // With the merge base on, a branch tip against master matches git's merge base.
    let release = git(&up, &["rev-parse", "release"]);
    let r = s
        .projects
        .affected(rq(wire::AffectedRequest {
            repo: repo_ref(),
            base: release.clone(),
            head: git(&up, &["rev-parse", "master"]),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        hex(&r.merge_base),
        git(&up, &["merge-base", &release, "master"])
    );
}

#[tokio::test]
async fn every_rpc_is_authorised_and_bad_input_is_refused() {
    let mut s = start().await;
    let master = git(&s.up, &["rev-parse", "master"]);
    // No token: refused before any repository work.
    let e = s
        .anon
        .resolve(Request::new(wire::ResolveRequest {
            repo: repo_ref(),
            revspecs: vec![master.clone()],
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    let e = s
        .projects
        .affected(Request::new(wire::AffectedRequest {
            repo: repo_ref(),
            base: master.clone(),
            head: master.clone(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);
    // A wrong token is refused too.
    let mut bad = Request::new(wire::ListRefsRequest {
        repo: repo_ref(),
        ..Default::default()
    });
    bad.metadata_mut()
        .insert("authorization", "Bearer nope".parse().unwrap());
    let e = s.anon.list_refs(bad).await.unwrap_err();
    assert!(matches!(
        e.code(),
        Code::Unauthenticated | Code::PermissionDenied
    ));
    // Hostile paths and revisions are refused, never followed.
    for path in ["../x", "a/../../x", "/abs", "a//b", ".git/config"] {
        let r = s
            .repo
            .read_file(rq(wire::ReadFileRequest {
                repo: repo_ref(),
                at: Some(wire::read_file_request::At::RevPath(format!(
                    "{master}:{path}"
                ))),
                ..Default::default()
            }))
            .await;
        let failed = match r {
            Err(_) => true,
            Ok(st) => st.into_inner().try_collect::<Vec<_>>().await.is_err(),
        };
        assert!(failed, "ReadFile {path}");
    }
    let e = s
        .repo
        .resolve(rq(wire::ResolveRequest {
            repo: repo_ref(),
            revspecs: vec![":/anything".into()],
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::InvalidArgument);
}
