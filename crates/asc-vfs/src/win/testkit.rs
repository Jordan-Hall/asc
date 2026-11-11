//! What the tests of both Windows backends share: a tree built in memory, the state database the
//! switch intent lives in, a placeholder clock, a recording journal, and a child process that holds
//! a file open. Each backend's lab adds its provider on top.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use asc_core::codec::Enveloped;
use asc_core::git::blob_oid;
use asc_core::id::{FileId, GitOid, TreeId};
use asc_core::object::{Change, Entry, Kind, Tree};
use asc_core::path::{Name, RepoPath};
use asc_state::state::repos::{NewRepo, RemoteKind};
use asc_state::state::workspaces::{self, NewWorkspace};
use asc_state::{AscHome, RepoId, StateDb, WorkspaceId};
use asc_store::bucket::BucketObjects;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_wc::journal::Op;
use asc_wc::redirect::Redirections;
use asc_wc::switch::{Dirty, Mode, Plan, plan};

use super::engine::Store;
use super::{Config, JournalSink, TimeSource};
use crate::core::{CoreFs, Options};
use crate::policy::HydrationPolicy;

/// The clone time of every placeholder a switch has not stamped.
pub const EPOCH_TIME: i64 = 1_700_000_000;
/// The workspace uuid of the tests.
pub const WORKSPACE: [u8; 16] = [
    0x6b, 0x1f, 0x3a, 0x90, 0x11, 0x22, 0x4c, 0x33, 0x84, 0x55, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
];

pub type Files<'a> = &'a [(&'a str, &'a [u8])];

pub fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

pub fn entry(name: &str, kind: Kind, id: [u8; 32], size: u64) -> Entry {
    Entry {
        name: Name::new(name).unwrap(),
        kind,
        id,
        size,
        git: GitOid::from_bytes([0; 20]),
    }
}

pub async fn put(store: &BucketObjects, key: ObjKey, data: &[u8]) {
    store.put(&key, encode_stored(data)).await.unwrap();
}

pub async fn tree(store: &BucketObjects, mut entries: Vec<Entry>) -> TreeId {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let t = Tree::new(entries, true).unwrap();
    put(store, ObjKey::Tree(t.id()), &t.to_envelope()).await;
    t.id()
}

/// The modification time of `p` in Unix seconds.
pub fn mtime(p: &Path) -> i64 {
    let t = std::fs::metadata(p).unwrap().modified().unwrap();
    i64::try_from(t.duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap()
}

/// `entry_times`: the placeholder time of a path, the clone time unless a switch stamped it.
#[derive(Default)]
pub struct Clock {
    stamped: Mutex<HashMap<Vec<u8>, i64>>,
}

impl Clock {
    pub fn stamp(&self, path: &RepoPath, t: i64) {
        self.stamped
            .lock()
            .unwrap()
            .insert(path.as_bytes().to_vec(), t);
    }
}

impl TimeSource for Clock {
    fn times(&self, paths: &[RepoPath]) -> io::Result<Vec<i64>> {
        let m = self.stamped.lock().unwrap();
        Ok(paths
            .iter()
            .map(|p| m.get(p.as_bytes()).copied().unwrap_or(EPOCH_TIME))
            .collect())
    }
}

/// A journal that remembers every hint.
#[derive(Default)]
pub struct Rec {
    hints: Mutex<Vec<(Op, Vec<u8>)>>,
    lost: Mutex<u32>,
}

impl JournalSink for Rec {
    fn record(&self, op: Op, path: &[u8]) {
        self.hints.lock().unwrap().push((op, path.to_vec()));
    }

    fn lost(&self) {
        *self.lost.lock().unwrap() += 1;
    }
}

impl Rec {
    pub fn clear(&self) {
        self.hints.lock().unwrap().clear();
    }

    pub fn has(&self, op: Op, path: &str) -> bool {
        self.hints
            .lock()
            .unwrap()
            .iter()
            .any(|(o, p)| *o == op && p == path.as_bytes())
    }

    pub fn any_under(&self, prefix: &str) -> bool {
        self.hints
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p)| p.starts_with(prefix.as_bytes()))
    }

    pub fn dump(&self) -> Vec<(Op, String)> {
        self.hints
            .lock()
            .unwrap()
            .iter()
            .map(|(o, p)| (*o, String::from_utf8_lossy(p).into_owned()))
            .collect()
    }

    pub fn lost_count(&self) -> u32 {
        *self.lost.lock().unwrap()
    }

    /// Waits up to ten seconds for a hint.
    pub fn wait_for(&self, op: Op, path: &str) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if self.has(op, path) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

enum N {
    F(Vec<u8>),
    D(BTreeMap<String, N>),
}

fn insert(map: &mut BTreeMap<String, N>, path: &str, data: &[u8]) {
    match path.split_once('/') {
        None => {
            map.insert(path.to_owned(), N::F(data.to_vec()));
        }
        Some((d, rest)) => {
            let e = map
                .entry(d.to_owned())
                .or_insert_with(|| N::D(BTreeMap::new()));
            if let N::D(m) = e {
                insert(m, rest, data);
            }
        }
    }
}

fn write_tree(
    rt: &tokio::runtime::Runtime,
    store: &BucketObjects,
    m: &BTreeMap<String, N>,
    prefix: &str,
    missing: &[&str],
) -> TreeId {
    let mut entries = Vec::new();
    for (name, n) in m {
        let full = format!("{prefix}{name}");
        match n {
            N::F(data) => {
                let id = FileId::of(data);
                if !missing.contains(&full.as_str()) {
                    rt.block_on(put(store, ObjKey::File(id), data));
                }
                entries.push(entry(name, Kind::File, *id.as_bytes(), data.len() as u64));
            }
            N::D(sub) => {
                let id = write_tree(rt, store, sub, &format!("{full}/"), missing);
                entries.push(entry(name, Kind::Dir, *id.as_bytes(), 0));
            }
        }
    }
    rt.block_on(tree(store, entries))
}

/// Everything a lab needs but the provider.
pub struct Kit {
    pub rt: tokio::runtime::Runtime,
    pub dir: tempfile::TempDir,
    pub store: Arc<BucketObjects>,
    pub journal: Arc<Rec>,
    pub clock: Arc<Clock>,
    pub db: StateDb,
    pub ws: WorkspaceId,
    pub deny: String,
    _home: tempfile::TempDir,
}

/// A temporary directory under `ASC_TEST_TMP` when set (a drive with room), else the system's.
pub fn tempdir() -> tempfile::TempDir {
    match std::env::var_os("ASC_TEST_TMP") {
        Some(base) => {
            let _ = std::fs::create_dir_all(&base);
            tempfile::tempdir_in(base).unwrap()
        }
        None => tempfile::tempdir().unwrap(),
    }
}

impl Kit {
    pub fn new(deny: &str) -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let home = tempdir();
        let (db, ws) = rt.block_on(async {
            let h = AscHome::open(home.path().join("home")).unwrap();
            let db = StateDb::open(&h).await.unwrap();
            let repo = asc_state::state::repos::insert(
                &db,
                &NewRepo {
                    id: RepoId::new(),
                    name: "r".into(),
                    ns: vec![1, 2, 3],
                    remote_url: None,
                    remote_kind: RemoteKind::Server,
                    default_branch: "master".into(),
                    windows_safe: true,
                },
            )
            .await
            .unwrap();
            let ws = WorkspaceId::new();
            workspaces::insert(
                &db,
                &NewWorkspace {
                    id: ws,
                    repo,
                    path: "/w".into(),
                    mount: "/w".into(),
                    mode: workspaces::Mode::Virtual,
                    head: "refs/heads/master".into(),
                    base_cs: None,
                    sparse: None,
                    ephemeral_until: None,
                },
            )
            .await
            .unwrap();
            (db, ws)
        });
        Self {
            rt,
            dir: tempdir(),
            store: Arc::new(BucketObjects::memory()),
            journal: Arc::new(Rec::default()),
            clock: Arc::new(Clock::default()),
            db,
            ws,
            deny: deny.to_owned(),
            _home: home,
        }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().join("ws")
    }

    pub fn at(&self, rel: &str) -> PathBuf {
        let mut b = self.root();
        for c in rel.split('/') {
            b.push(c);
        }
        b
    }

    /// Stores the objects of `files` (except those named in `missing`) and the trees.
    pub fn tree_missing(&self, files: Files<'_>, missing: &[&str]) -> TreeId {
        let mut top = BTreeMap::new();
        for (path, data) in files {
            insert(&mut top, path, data);
        }
        write_tree(&self.rt, &self.store, &top, "", missing)
    }

    pub fn tree(&self, files: Files<'_>) -> TreeId {
        self.tree_missing(files, &[])
    }

    pub fn core(&self, root: TreeId) -> Arc<CoreFs> {
        let opts = Options {
            redirections: Redirections::parse("node_modules\n"),
            policy: if self.deny.is_empty() {
                HydrationPolicy::allow_all()
            } else {
                HydrationPolicy::parse(&self.deny)
            },
            ..Options::windows()
        };
        Arc::new(CoreFs::new(self.store.clone(), root, opts))
    }

    pub fn config(&self, guid: [u8; 16], core: Arc<CoreFs>) -> Config {
        Config {
            workspace: guid,
            core,
            objects: self.store.clone(),
            local: None,
            times: self.clock.clone(),
            journal: self.journal.clone(),
            redirections: Redirections::parse("node_modules\n"),
            rt: self.rt.handle().clone(),
        }
    }

    pub fn store_handle(&self) -> Store {
        Store {
            db: self.db.clone(),
            ws: self.ws,
        }
    }

    /// The plan of `old` to `new`, as `asc_wc::switch::plan` makes it from the leaf diff.
    pub fn plan(&self, old: Files<'_>, new: Files<'_>) -> Plan {
        let old: BTreeMap<&str, &[u8]> = old.iter().copied().collect();
        let new_m: BTreeMap<&str, &[u8]> = new.iter().copied().collect();
        let mut diff = Vec::new();
        for (path, data) in &new_m {
            if old.get(path) != Some(data) {
                diff.push((
                    p(path),
                    Change::write(
                        Kind::File,
                        FileId::of(data),
                        data.len() as u64,
                        blob_oid(data),
                        None,
                    )
                    .unwrap(),
                ));
            }
        }
        for path in old.keys() {
            if !new_m.contains_key(path) {
                diff.push((p(path), Change::Delete));
            }
        }
        plan(&diff, &Dirty::default(), Mode::Safe)
    }

    /// Stamps every path a switch from `old` to `new` changes with `t`, as the daemon does, and
    /// returns the plan and the extras.
    pub fn stamp(
        &self,
        old: Files<'_>,
        new: Files<'_>,
        extra: &[&str],
        t: i64,
    ) -> (Plan, BTreeSet<RepoPath>) {
        let plan = self.plan(old, new);
        let extra: BTreeSet<RepoPath> = extra.iter().map(|s| p(s)).collect();
        for (path, _) in &plan.updates {
            self.clock.stamp(path, t);
        }
        for path in plan.deletes.iter().chain(extra.iter()) {
            self.clock.stamp(path, t);
        }
        (plan, extra)
    }
}

/// A child process that holds `path` open for reading without allowing delete or rename.
pub struct Holder(Child);

impl Holder {
    pub fn new(path: &Path) -> Self {
        let s = path.to_string_lossy().into_owned();
        assert!(!s.contains('\''), "{s}");
        let script = format!(
            "$f=[System.IO.File]::Open('{s}','Open','Read','Read'); [Console]::Out.WriteLine('ready'); [Console]::Out.Flush(); Start-Sleep -Seconds 120"
        );
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let until = Instant::now() + Duration::from_secs(30);
        while Instant::now() < until {
            line.clear();
            if out.read_line(&mut line).unwrap() == 0 || line.trim() == "ready" {
                break;
            }
        }
        assert_eq!(line.trim(), "ready", "the child did not open the file");
        // Keep the pipe open so the child's output never fails.
        std::mem::forget(out);
        Self(child)
    }

    pub fn release(mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
