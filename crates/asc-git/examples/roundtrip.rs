//! Round-trips a real git repository through asc (U35, SPEC §14.1, §14.2, §14.4).
//!
//! Imports every ref's history into a temp store, exports it to a temp bare repository and
//! compares every reachable commit, tree, tag and blob oid. The source repository is only read.
//! Usage: `cargo run -p asc-git --release --example roundtrip -- <repo>...`
//! (default: `D:\dev\monorepo`). Exits non-zero when anything mismatches.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asc_core::git::ObjectKind;
use asc_core::id::GitOid;
use asc_core::path::RefName;
use asc_engine::Engine;
use asc_engine::derive::{self, GitMapRow, GitMapSink};
use asc_git::export::{ExportRoot, Exporter};
use asc_git::import::{ImportMap, ImportPolicy, Importer, SkippedRef, Tip};
use asc_git::odb::Odb;
use asc_git::realgit::RealGit;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::{self, Objects, decode_verified, slice_range};
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt as _};

/// Largest blob that can be an LFS pointer file (the spec text is about 130 bytes).
const POINTER_MAX: u64 = 1024;
const POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1";

/// Why a round trip could not run to the end.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: std::io::Error,
    },
    #[error("git {0} failed: {1}")]
    Git(String, String),
    #[error("{0}")]
    Other(String),
}

fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Io { what, source }
}

fn other(e: impl std::fmt::Display) -> Error {
    Error::Other(e.to_string())
}

/// What one repository's round trip found.
#[derive(Debug, Default)]
pub struct Report {
    pub commits: usize,
    pub blobs: usize,
    pub trees: usize,
    pub tags: usize,
    pub overrides: usize,
    pub lfs_pointers: usize,
    /// Reachable source objects (of the non-skipped refs).
    pub source_objects: usize,
    /// Objects missing from the export plus objects the export has beyond the source.
    pub mismatches: usize,
    /// A few mismatching oids, for diagnosis.
    pub examples: Vec<String>,
    pub skipped_refs: Vec<SkippedRef>,
    pub rejected_tags: Vec<(Vec<u8>, GitOid, String)>,
    pub import_time: Duration,
    pub elapsed: Duration,
}

impl Report {
    /// The report as the lines the example prints.
    pub fn render(&self, path: &Path) -> String {
        let mut out = format!(
            "{}\n  commits {}  trees {}  blobs {}  tags {}\n  overrides needed {}  LFS pointers found {}\n  source objects {}  mismatches {}\n  import {:.1}s  total {:.1}s\n",
            path.display(),
            self.commits,
            self.trees,
            self.blobs,
            self.tags,
            self.overrides,
            self.lfs_pointers,
            self.source_objects,
            self.mismatches,
            self.import_time.as_secs_f64(),
            self.elapsed.as_secs_f64(),
        );
        for s in &self.skipped_refs {
            out.push_str(&format!("  {}\n", s.warning()));
        }
        for (name, oid, why) in &self.rejected_tags {
            out.push_str(&format!(
                "  rejected tag '{}' {oid}: {why}\n",
                String::from_utf8_lossy(name)
            ));
        }
        for e in &self.examples {
            out.push_str(&format!("  mismatch {e}\n"));
        }
        out
    }
}

// ------------------------------------------------------------ in-memory stand-ins

/// A ref store with no refs; the round trip never touches asc refs.
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

/// The git map in memory.
#[derive(Default)]
struct MemMap {
    rows: Mutex<HashMap<(GitOid, u8), [u8; 32]>>,
}

fn kind_byte(k: ObjectKind) -> u8 {
    match k {
        ObjectKind::Blob => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Commit => 3,
        ObjectKind::Tag => 4,
    }
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

// ------------------------------------------------------------------------ git helpers

/// Runs real git with `--git-dir`, feeding `stdin` from a thread so large pipes cannot deadlock.
fn git_out(git: &RealGit, git_dir: &Path, args: &[&str], stdin: Vec<u8>) -> Result<Vec<u8>, Error> {
    let mut cmd = git.command().map_err(other)?;
    let mut child = cmd
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io("spawn git"))?;
    let mut pipe = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || {
        let _ = pipe.write_all(&stdin);
    });
    let out = child.wait_with_output().map_err(io("wait for git"))?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(Error::Git(
            args.first().copied().unwrap_or("command").to_owned(),
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    Ok(out.stdout)
}

/// The git directory of a work tree or bare repository.
fn git_dir_of(path: &Path) -> PathBuf {
    let dot = path.join(".git");
    if dot.is_dir() { dot } else { path.to_owned() }
}

/// Every ref as a [`Tip`].
fn list_tips(git: &RealGit, git_dir: &Path) -> Result<Vec<Tip>, Error> {
    let out = git_out(
        git,
        git_dir,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
        Vec::new(),
    )?;
    let mut tips = Vec::new();
    for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let sp = line
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| Error::Other("malformed for-each-ref line".into()))?;
        let oid = std::str::from_utf8(&line[..sp])
            .map_err(other)?
            .parse::<GitOid>()
            .map_err(other)?;
        tips.push(Tip {
            name: line[sp + 1..].to_vec(),
            oid,
        });
    }
    Ok(tips)
}

/// The first 40 hex digits of each line, as oids.
fn oids_of(out: &[u8]) -> Result<BTreeSet<GitOid>, Error> {
    let mut set = BTreeSet::new();
    for line in out.split(|&b| b == b'\n').filter(|l| l.len() >= 40) {
        let hex = std::str::from_utf8(&line[..40]).map_err(other)?;
        set.insert(hex.parse::<GitOid>().map_err(other)?);
    }
    Ok(set)
}

/// Counts reachable small blobs that are LFS pointer files (they are plain blobs to asc).
fn count_pointers(
    git: &RealGit,
    git_dir: &Path,
    odb: &Odb,
    oids: &BTreeSet<GitOid>,
) -> Result<usize, Error> {
    let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
    let out = git_out(
        git,
        git_dir,
        &[
            "cat-file",
            "--batch-check=%(objecttype) %(objectname) %(objectsize)",
        ],
        input.into_bytes(),
    )?;
    let mut found = 0;
    for line in String::from_utf8_lossy(&out).lines() {
        let mut parts = line.split(' ');
        let (Some("blob"), Some(oid), Some(size)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if size.parse::<u64>().map_or(true, |s| s > POINTER_MAX) {
            continue;
        }
        let oid: GitOid = oid.parse().map_err(other)?;
        if odb
            .read_blob_verified(oid)
            .map_err(other)?
            .starts_with(POINTER_PREFIX)
        {
            found += 1;
        }
    }
    Ok(found)
}

// ----------------------------------------------------------------------- the temp store

/// Where a stored value sits in the log.
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

/// An append-only log file with an in-memory index: bounded RSS, and one file instead of one
/// file per object (which is slow on Windows). Used as the temp store for the round trip.
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

// ---------------------------------------------------------------------------- the run

/// Imports `repo` (read-only), exports to a temp bare repository and compares every oid.
pub async fn run(repo: &Path) -> Result<Report, Error> {
    let started = Instant::now();
    let git = RealGit::resolve(None, None).map_err(other)?;
    let src = git_dir_of(repo);
    let scratch = tempfile::tempdir().map_err(io("temp dir"))?;

    let objects =
        Arc::new(LogStore::create(&scratch.path().join("store.log")).map_err(io("create store"))?);
    let engine = Engine::new(objects.clone(), Arc::new(NoRefs), RepoKey(1));
    let map = MemMap::default();
    let odb = Odb::open(&src).map_err(other)?;
    let tips = list_tips(&git, &src)?;
    eprintln!("roundtrip: {} refs, importing", tips.len());
    stage("before import");

    let imported = Instant::now();
    let imp = Importer::new(
        &engine,
        objects.clone(),
        &odb,
        &map,
        ImportPolicy::default(),
    )
    .import_commits(&tips)
    .await
    .map_err(other)?;
    let import_time = imported.elapsed();
    stage("after import");

    // Export every ref's changeset and every tag into a fresh bare repository.
    let dst = scratch.path().join("export.git");
    let status = git
        .command()
        .map_err(other)?
        .args(["init", "-q", "--bare"])
        .arg(&dst)
        .status()
        .map_err(io("git init"))?;
    if !status.success() {
        return Err(Error::Git("init".into(), status.to_string()));
    }
    eprintln!(
        "roundtrip: imported {} commits in {:.1}s, exporting",
        imp.commits,
        import_time.as_secs_f64()
    );
    let target = Odb::open(&dst).map_err(other)?;
    let roots: Vec<ExportRoot> = imp
        .refs
        .iter()
        .map(|(_, id)| ExportRoot::Changeset(*id))
        .chain(imp.tags.iter().map(|(_, id)| ExportRoot::Tag(*id)))
        .collect();
    Exporter::new(&engine, &*objects, &git, &dst)
        .export(&roots, &target)
        .await
        .map_err(other)?;

    // Compare the reachable source objects of the representable refs with the export.
    let skipped: BTreeSet<&[u8]> = imp.skipped_refs.iter().map(|s| s.name.as_slice()).collect();
    let tip_oids: BTreeSet<GitOid> = tips
        .iter()
        .filter(|t| !skipped.contains(t.name.as_slice()))
        .map(|t| t.oid)
        .collect();
    let input: String = tip_oids.iter().map(|o| format!("{o}\n")).collect();
    stage("after export");
    eprintln!("roundtrip: exported, comparing");
    let want = oids_of(&git_out(
        &git,
        &src,
        &["rev-list", "--objects", "--stdin"],
        input.into_bytes(),
    )?)?;
    let have = oids_of(&git_out(
        &git,
        &dst,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
        Vec::new(),
    )?)?;
    let missing: Vec<&GitOid> = want.difference(&have).collect();
    let extra: Vec<&GitOid> = have.difference(&want).collect();
    let examples = missing
        .iter()
        .map(|o| format!("missing {o}"))
        .chain(extra.iter().map(|o| format!("extra {o}")))
        .take(10)
        .collect();

    Ok(Report {
        commits: imp.commits,
        blobs: imp.blobs,
        trees: imp.trees,
        tags: imp.tags.len(),
        overrides: imp.overrides,
        lfs_pointers: count_pointers(&git, &src, &odb, &want)?,
        source_objects: want.len(),
        mismatches: missing.len() + extra.len(),
        examples,
        skipped_refs: imp.skipped_refs,
        rejected_tags: imp.rejected_tags,
        import_time,
        elapsed: started.elapsed(),
    })
}

/// Logs peak RSS at a stage boundary (stderr), to find which stage owns the peak.
fn stage(label: &str) {
    if let Some(rss) = peak_rss_bytes() {
        eprintln!(
            "roundtrip: peak RSS {:>5} MiB {label} (private peak {} MiB)",
            rss >> 20,
            peak_private_mib().unwrap_or(0)
        );
    }
}

fn peak_private_mib() -> Option<u64> {
    #[cfg(windows)]
    {
        let pid = std::process::id();
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).PeakPagedMemorySize64"),
            ])
            .output()
            .ok()?;
        let b: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(b >> 20)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Peak resident memory of this process in bytes, where the platform tells us.
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        let pid = std::process::id();
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).PeakWorkingSet64"),
            ])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(not(windows))]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let kb: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))?
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        paths.push(PathBuf::from(r"D:\dev\monorepo"));
    }
    let mut failed = false;
    for path in paths {
        match run(&path).await {
            Ok(report) => {
                print!("{}", report.render(&path));
                failed |= report.mismatches != 0;
            }
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                failed = true;
            }
        }
    }
    if let Some(rss) = peak_rss_bytes() {
        println!("peak RSS {:.0} MiB", rss as f64 / (1024.0 * 1024.0));
    }
    if failed {
        std::process::exit(1);
    }
}
