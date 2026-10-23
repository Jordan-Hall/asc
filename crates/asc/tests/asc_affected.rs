//! G11: `Affected` against turbo on the last 300 ASC master commits (SPEC §17 "Affected vs
//! turbo", spike S10). Run with
//! `cargo test -p asc --test asc_affected -- --ignored` (needs bun and network for
//! `bunx turbo@2.10.11`; add `--release` for the §12A.8 latency limits at full strictness).
//!
//! `D:\dev\monorepo` is only read. Turbo runs in a `git clone --shared` temporary copy that is
//! checked out at each commit, never in ASC itself. The engine answers the same query that
//! `asc affected --type node` makes, on a temporary store the test imports ASC's master into.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use asc_core::git::ObjectKind;
use asc_core::id::{ChangesetId, GitOid};
use asc_core::path::RefName;
use asc_engine::Engine;
use asc_engine::derive::{self, GitMapRow, GitMapSink};
use asc_engine::projects::AffectedQuery;
use asc_git::import::{ImportMap, ImportPolicy, Importer, Tip};
use asc_git::odb::Odb;
use asc_git::realgit::RealGit;
use asc_graph::model::Ecosystem;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{self, Objects, decode_verified, slice_range};
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use futures::stream::{self, BoxStream, StreamExt as _};

const ASC: &str = r"D:\dev\monorepo";
const TURBO: &str = "turbo@2.10.11";
/// How many master commits are compared.
const COMMITS: usize = 300;
/// The share of commits on which the two sets must be equal.
const EQUAL_SHARE: f64 = 0.95;
/// §12A.8 / §2: `Affected` is under 300 ms warm and under 2 s cold. Debug builds are an order of
/// magnitude slower, so they get ten times the limit.
const WARM_MAX: Duration = Duration::from_millis(300);
const COLD_MAX: Duration = Duration::from_secs(2);
/// Bounds on one external process's output.
const MAX_OUTPUT: usize = 64 << 20;

// ------------------------------------------------------------------------------ stand-ins

/// A ref store with no refs; the import and the queries never touch asc refs.
struct NoRefs;

#[async_trait]
impl Refs for NoRefs {
    async fn get(&self, _: RepoKey, _: &RefName) -> asc_store::refs::Result<Option<RefEntry>> {
        Ok(None)
    }
    async fn list(&self, _: RepoKey, _: &str) -> asc_store::refs::Result<Vec<RefEntry>> {
        Ok(Vec::new())
    }
    async fn update(&self, _: RepoKey, _: &RefTxn) -> asc_store::refs::Result<()> {
        Err(RefError::Corrupt("unused".into()))
    }
    async fn log(
        &self,
        _: RepoKey,
        _: &RefName,
        _: u32,
    ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
        Ok(Vec::new())
    }
}

fn kind_byte(k: ObjectKind) -> u8 {
    match k {
        ObjectKind::Blob => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Commit => 3,
        ObjectKind::Tag => 4,
    }
}

/// The git map in memory.
#[derive(Default)]
struct MemMap {
    rows: Mutex<HashMap<(GitOid, u8), [u8; 32]>>,
}

#[async_trait]
impl GitMapSink for MemMap {
    async fn put(&self, rows: &[GitMapRow]) -> derive::Result<()> {
        let mut map = self.rows.lock().expect("map lock");
        for r in rows {
            map.insert((r.git, kind_byte(r.kind)), r.asc);
        }
        Ok(())
    }
}

#[async_trait]
impl ImportMap for MemMap {
    async fn get(&self, git: GitOid, kind: ObjectKind) -> derive::Result<Option<[u8; 32]>> {
        Ok(self
            .rows
            .lock()
            .expect("map lock")
            .get(&(git, kind_byte(kind)))
            .copied())
    }
}

#[derive(Clone, Copy)]
struct Slot {
    offset: u64,
    len: u32,
    written: std::time::SystemTime,
}

struct LogInner {
    file: File,
    end: u64,
    index: HashMap<ObjKey, Slot>,
}

/// An append-only log file with an in-memory index: one file instead of one per object (which
/// is slow on Windows), and bounded RSS.
struct LogStore(Mutex<LogInner>);

impl LogStore {
    fn create(path: &Path) -> std::io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self(Mutex::new(LogInner {
            file,
            end: 0,
            index: HashMap::new(),
        })))
    }

    fn read(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        let mut g = self.0.lock().expect("store lock");
        let Some(slot) = g.index.get(key).copied() else {
            return Ok(None);
        };
        let mut buf = vec![0; slot.len as usize];
        let read = g
            .file
            .seek(SeekFrom::Start(slot.offset))
            .and_then(|_| g.file.read_exact(&mut buf));
        read.map_err(|e| objects::Error::Url(e.to_string()))?;
        Ok(Some(Bytes::from(buf)))
    }
}

#[async_trait]
impl Objects for LogStore {
    async fn get(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        self.read(key)?
            .map(|stored| decode_verified(key, &stored))
            .transpose()
    }
    async fn get_stored(&self, key: &ObjKey) -> objects::Result<Option<Bytes>> {
        self.read(key)
    }
    async fn get_range(
        &self,
        key: &ObjKey,
        r: std::ops::Range<u64>,
    ) -> objects::Result<Option<Bytes>> {
        Ok(self.get(key).await?.map(|data| slice_range(&data, r)))
    }
    async fn put(&self, key: &ObjKey, stored: Bytes) -> objects::Result<()> {
        decode_verified(key, &stored)?;
        let mut g = self.0.lock().expect("store lock");
        if g.index.contains_key(key) {
            return Ok(());
        }
        let offset = g.end;
        let wrote = g
            .file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| g.file.write_all(&stored));
        wrote.map_err(|e| objects::Error::Url(e.to_string()))?;
        g.end += stored.len() as u64;
        g.index.insert(
            *key,
            Slot {
                offset,
                len: stored.len() as u32,
                written: std::time::SystemTime::now(),
            },
        );
        Ok(())
    }
    async fn has_many(&self, keys: &[ObjKey]) -> objects::Result<Vec<bool>> {
        let g = self.0.lock().expect("store lock");
        Ok(keys.iter().map(|k| g.index.contains_key(k)).collect())
    }
    async fn delete(&self, key: &ObjKey) -> objects::Result<()> {
        self.0.lock().expect("store lock").index.remove(key);
        Ok(())
    }
    fn list(
        &self,
        prefix: KeyPrefix,
    ) -> BoxStream<'_, objects::Result<(ObjKey, std::time::SystemTime)>> {
        let items: Vec<_> = self
            .0
            .lock()
            .expect("store lock")
            .index
            .iter()
            .filter(|(k, _)| prefix.dir().is_none_or(|d| k.class().dir() == d))
            .map(|(k, s)| Ok((*k, s.written)))
            .collect();
        stream::iter(items).boxed()
    }
}

// ------------------------------------------------------------------------ external tools

/// Runs `cmd` and returns its output, failing on a non-zero exit or output past the bound.
fn run(what: &str, cmd: &mut Command) -> Output {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{what}: spawn failed: {e}"));
    assert!(
        out.status.success(),
        "{what} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    assert!(
        out.stdout.len() <= MAX_OUTPUT,
        "{what}: output past {MAX_OUTPUT} bytes"
    );
    out
}

/// A git command in `dir` with git's configuration isolated from the user's.
fn git(real: &RealGit, dir: &Path) -> Command {
    let mut cmd = real.command().expect("git command");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

/// The git directory of a work tree or bare repository.
fn git_dir_of(path: &Path) -> PathBuf {
    let dot = path.join(".git");
    if dot.is_dir() { dot } else { path.to_owned() }
}

fn parse_oid(hex: &str) -> GitOid {
    hex.trim()
        .parse()
        .unwrap_or_else(|e| panic!("bad oid {hex:?}: {e}"))
}

/// The package paths `turbo ls --affected` reports for `base..head`, in the checked-out clone.
fn turbo_affected(clone: &Path, base: GitOid, head: GitOid) -> BTreeSet<String> {
    let bunx = which_bunx();
    let out = run(
        "turbo ls --affected",
        Command::new(bunx)
            .args([TURBO, "ls", "--affected", "--output=json"])
            .current_dir(clone)
            .env("TURBO_SCM_BASE", base.to_string())
            .env("TURBO_SCM_HEAD", head.to_string())
            .env("TURBO_TELEMETRY_DISABLED", "1")
            .env("DO_NOT_TRACK", "1")
            .env("TURBO_NO_UPDATE_NOTIFIER", "1"),
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        panic!("turbo printed no JSON for {head}: {text}");
    };
    let doc: serde_json::Value = serde_json::from_str(&text[start..=end]).expect("turbo JSON");
    doc["packages"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("turbo JSON without packages.items: {doc}"))
        .iter()
        .map(|p| p["path"].as_str().expect("package path").replace('\\', "/"))
        .collect()
}

/// Whether `base...head` edits the root `package.json` or `bun.lock`.
fn root_files_changed(real: &RealGit, clone: &Path, base: GitOid, head: GitOid) -> bool {
    let out = run(
        "git diff --name-only",
        git(real, clone)
            .args(["diff", "--name-only", "-z"])
            .arg(format!("{base}...{head}")),
    );
    out.stdout
        .split(|&b| b == 0)
        .any(|p| p == b"package.json" || p == b"bun.lock")
}

/// Whether every project asc adds over turbo is explained by a documented difference: a gitlink
/// edit (turbo's clone does not check submodules out), a workspace `peerDependencies` edge
/// (turbo's graph leaves those out), a lockfile edit when the root files changed (SPEC §12A.4),
/// or a dependency on another explained project. `extras` maps a project root to its display id
/// and reasons.
fn extras_explained(
    real: &RealGit,
    clone: &Path,
    head: GitOid,
    extras: &HashMap<String, (String, Vec<asc_graph::affected::Reason>)>,
    root_changed: bool,
) -> bool {
    use asc_graph::affected::Reason;
    let peer_only = |root: &str, dep: &str| -> bool {
        let out = git(real, clone)
            .args(["show", &format!("{head}:{root}/package.json")])
            .output();
        let Ok(out) = out else { return false };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            return false;
        };
        let has = |t: &str| doc.get(t).and_then(|v| v.get(dep)).is_some();
        has("peerDependencies")
            && !has("dependencies")
            && !has("devDependencies")
            && !has("optionalDependencies")
    };
    let mut ok: BTreeSet<String> = BTreeSet::new();
    loop {
        let mut grew = false;
        for (root, (id, reasons)) in extras {
            if ok.contains(id) {
                continue;
            }
            let all = reasons.iter().all(|r| match r {
                Reason::Gitlink(_) => true,
                Reason::Lockfile => root_changed,
                Reason::Dep(d) => {
                    ok.contains(d) || peer_only(root, d.strip_prefix("node:").unwrap_or(d))
                }
                _ => false,
            });
            if all {
                ok.insert(id.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    extras.values().all(|(id, _)| ok.contains(id))
}

fn which_bunx() -> &'static str {
    if cfg!(windows) { "bunx.cmd" } else { "bunx" }
}

// ----------------------------------------------------------------------------- the test

#[tokio::test(flavor = "multi_thread")]
#[ignore = "reads D:\\dev\\monorepo, needs bun and network (bunx turbo@2.10.11)"]
async fn affected_matches_turbo_on_asc() {
    let asc = Path::new(ASC);
    assert!(asc.exists(), "{ASC} is missing");
    let real = RealGit::resolve(None, None).expect("real git");
    let src = git_dir_of(asc);

    // The last COMMITS first-parent steps of master: commit i has first parent i + 1.
    let listed = run(
        "git rev-list",
        git(&real, asc)
            .args(["rev-list", "--first-parent", "-n"])
            .arg((COMMITS + 1).to_string())
            .arg("refs/heads/master"),
    );
    let commits: Vec<GitOid> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(parse_oid)
        .collect();
    assert!(commits.len() > 1, "ASC master has no history");
    let steps = commits.len() - 1;

    let scratch = tempfile::tempdir().expect("temp dir");
    // Turbo runs on a shared clone, never in ASC itself.
    let clone = scratch.path().join("clone");
    run(
        "git clone --shared",
        git(&real, scratch.path())
            .args(["clone", "-q", "--shared", "--no-checkout"])
            .arg(asc)
            .arg(&clone),
    );

    // Import master into a temporary store.
    let objects = Arc::new(LogStore::create(&scratch.path().join("store.log")).expect("store"));
    let engine = Engine::new(objects.clone(), Arc::new(NoRefs), RepoKey(1));
    let map = MemMap::default();
    let odb = Odb::open(&src).expect("open ASC odb");
    let tips = [Tip {
        name: b"refs/heads/master".to_vec(),
        oid: commits[0],
    }];
    let started = Instant::now();
    let report = Importer::new(
        &engine,
        objects.clone(),
        &odb,
        &map,
        ImportPolicy::default(),
    )
    .import_commits(&tips)
    .await
    .expect("import ASC master");
    eprintln!(
        "asc_affected: imported {} commits in {:.1}s",
        report.commits,
        started.elapsed().as_secs_f64()
    );

    let cs = |oid: GitOid| -> ChangesetId {
        let id = map
            .rows
            .lock()
            .expect("map lock")
            .get(&(oid, kind_byte(ObjectKind::Commit)))
            .copied();
        ChangesetId::from_bytes(id.unwrap_or_else(|| panic!("commit {oid} was not imported")))
    };

    let mut equal = 0usize;
    let mut expected = 0usize;
    let mut explained = 0usize;
    let mut diffs = Vec::new();
    let mut cold = None;
    for i in 0..steps {
        let (head, base) = (commits[i], commits[i + 1]);
        run(
            "git checkout",
            git(&real, &clone)
                .args(["checkout", "-q", "--force", "--detach"])
                .arg(head.to_string()),
        );
        let want = turbo_affected(&clone, base, head);

        let query = AffectedQuery {
            base: cs(base),
            head: cs(head),
            no_merge_base: false,
            target: None,
            ecosystems: vec![Ecosystem::Node],
            select: Vec::new(),
            with_hashes: false,
        };
        let t = Instant::now();
        let result = engine
            .affected(&query)
            .await
            .unwrap_or_else(|e| panic!("affected {head}: {e}"));
        let took = t.elapsed();
        if cold.is_none() {
            cold = Some((took, query.clone()));
        }
        // Turbo lists packages, not the workspace root, so the root project is left out.
        let graph = engine.project_graph(cs(head)).await.expect("project graph");
        let got: BTreeSet<String> = result
            .projects
            .iter()
            .filter_map(|a| graph.graph.projects.iter().find(|p| p.key == a.project.key))
            .filter_map(|p| {
                p.root
                    .as_ref()
                    .map(|r| String::from_utf8_lossy(r.as_bytes()).into_owned())
            })
            .collect();

        let missing: Vec<&String> = want.difference(&got).collect();
        let extra: Vec<&String> = got.difference(&want).collect();
        let root_changed = root_files_changed(&real, &clone, base, head);
        let extras_by_root: HashMap<String, (String, Vec<asc_graph::affected::Reason>)> = result
            .projects
            .iter()
            .filter_map(|a| {
                let p = graph
                    .graph
                    .projects
                    .iter()
                    .find(|p| p.key == a.project.key)?;
                let root = String::from_utf8_lossy(p.root.as_ref()?.as_bytes()).into_owned();
                extra
                    .iter()
                    .any(|e| **e == root)
                    .then(|| (root, (a.project.id.to_string(), a.reasons.clone())))
            })
            .collect();
        if missing.is_empty() && extra.is_empty() {
            equal += 1;
        } else if missing.is_empty()
            && extras_explained(&real, &clone, head, &extras_by_root, root_changed)
        {
            // Equal up to the documented differences (gitlinks, peer edges, lockfile).
            equal += 1;
            explained += 1;
        } else {
            let why: Vec<String> = result
                .projects
                .iter()
                .filter(|a| {
                    graph.graph.projects.iter().any(|p| {
                        p.key == a.project.key
                            && p.root
                                .as_ref()
                                .is_some_and(|r| extra.iter().any(|e| e.as_bytes() == r.as_bytes()))
                    })
                })
                .map(|a| format!("{} {:?}", a.project.id, a.reasons))
                .collect();
            diffs.push(format!(
                "{head}: asc misses {missing:?}, asc adds {extra:?} because {why:?}"
            ));
        }
        // SPEC §12A.4: a root `package.json` or `bun.lock` edit affects only the members whose
        // dependency closure it changes, where turbo marks every package (spike S10). Those
        // commits are an expected difference, not a superset failure.
        if root_changed {
            expected += 1;
            eprintln!(
                "asc_affected: root package.json or bun.lock changed at {head}: turbo {} packages, asc {}",
                want.len(),
                got.len()
            );
            continue;
        }
        assert!(
            missing.is_empty(),
            "asc is not a superset of turbo at {head} (base {base}): missing {missing:?}",
        );
    }

    for d in &diffs {
        eprintln!("asc_affected: difference {d}");
    }
    let share = equal as f64 / steps as f64;
    eprintln!(
        "asc_affected: equal on {equal} of {steps} commits ({:.1}%, {explained} of them only up to documented differences); {expected} root-file commits differ by design",
        share * 100.0
    );
    assert!(
        share >= EQUAL_SHARE,
        "equal on only {equal} of {steps} commits ({:.1}%)",
        share * 100.0
    );

    // Latency: the first query after the import is the cold one (it derives the manifest sets);
    // the same query again is warm.
    let (cold_took, query) = cold.expect("at least one step");
    let t = Instant::now();
    engine.affected(&query).await.expect("warm affected");
    let warm_took = t.elapsed();
    eprintln!("asc_affected: cold {cold_took:?}, warm {warm_took:?}");
    let scale = if cfg!(debug_assertions) { 10 } else { 1 };
    assert!(
        cold_took < COLD_MAX * scale,
        "cold Affected took {cold_took:?}"
    );
    assert!(
        warm_took < WARM_MAX * scale,
        "warm Affected took {warm_took:?}"
    );
}
