//! Imports a git repository into a store on disk and reports what it cost: wall time, files and
//! bytes on disk, and a read-back of a sample. For `docs/BENCHMARKS.md` (packed objects).
//!
//! Usage: `cargo run -p asc-git --release --example import_bench -- <git-dir> <out-dir> <mode>`
//! where mode is `loose` (a write after every commit, no packs: the shape before batching),
//! `batched` (500 commits per batch, loose objects), `packed` (batched, small objects in packs) or
//! `delta` (batched, then every modified file stored as a delta against its previous version at the
//! same path, as the server's delta pass does; the pairs come from `git log --raw`). The source is only
//! read; `<out-dir>` must not exist. Bytes are also reported per object kind (the store's top folder).
#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asc_core::git::ObjectKind;
use asc_core::id::GitOid;
use asc_core::path::RefName;
use asc_engine::Engine;
use asc_engine::derive::{self, GitMapRow, GitMapSink};
use asc_git::import::{ImportMap, ImportPolicy, Importer, Tip};
use asc_git::odb::Odb;
use asc_store::bucket::BucketObjects;
use asc_store::key::{KeyPrefix, ObjKey};
use asc_store::objects::Objects;
use asc_store::pack::PackConfig;
use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, Refs, RepoKey};
use async_trait::async_trait;
use futures::TryStreamExt as _;

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

/// The git map in memory, counting the calls the importer makes.
#[derive(Default)]
struct Map {
    rows: Mutex<HashMap<(GitOid, u8), [u8; 32]>>,
    calls: Mutex<usize>,
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
impl GitMapSink for Map {
    async fn put(&self, rows: &[GitMapRow]) -> derive::Result<()> {
        *self.calls.lock().unwrap() += 1;
        let mut m = self.rows.lock().unwrap();
        for r in rows {
            m.insert((r.git, kind_byte(r.kind)), r.asc);
        }
        Ok(())
    }
}

#[async_trait]
impl ImportMap for Map {
    async fn get(&self, git: GitOid, kind: ObjectKind) -> derive::Result<Option<[u8; 32]>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(&(git, kind_byte(kind)))
            .copied())
    }
}

fn tips(git_dir: &Path) -> Vec<Tip> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .output()
        .expect("run git");
    assert!(out.status.success(), "git for-each-ref failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (oid, name) = l.split_once(' ')?;
            Some(Tip {
                name: name.as_bytes().to_vec(),
                oid: oid.parse().ok()?,
            })
        })
        .collect()
}

/// `(old blob, new blob)` for every modified file in the history, oldest commit first, against the
/// first parent (the server's pass takes the same path in the first parent's tree).
fn modified_pairs(git_dir: &Path) -> Vec<(GitOid, GitOid)> {
    pairs_with(git_dir, &["--first-parent", "--no-renames"])
}

/// `(old, new)` blob pairs from `git log --raw` with `extra` options: `M` entries, and `R` entries
/// when rename detection is on.
fn pairs_with(git_dir: &Path, extra: &[&str]) -> Vec<(GitOid, GitOid)> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args([
            "log",
            "--all",
            "--reverse",
            "--raw",
            "--no-abbrev",
            "--format=",
        ])
        .args(extra)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git log failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            // :100644 100644 <old> <new> M	<path>
            let mut f = l.strip_prefix(':')?.split_whitespace();
            let (_, mode, old, new, st) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
            ((st == "M" || st.starts_with('R'))
                && mode != "120000"
                && mode != "160000"
                && old != new)
                .then(|| Some((old.parse().ok()?, new.parse().ok()?)))?
        })
        .collect()
}

/// Sizes of the candidate deltas for several pair sets, zstd levels and gain thresholds; writes
/// nothing. Also the loose file bytes recompressed at a high level.
async fn explore(objects: &BucketObjects, map: &Map, git_dir: &Path) {
    use asc_core::id::FileId;
    let rows = map.rows.lock().unwrap().clone();
    let file = |g: &GitOid| rows.get(&(*g, 1)).map(|b| FileId::from_bytes(*b));
    // Every commit, same path: the widest same-path set. Each new version once.
    let pairs = pairs_with(git_dir, &["--no-renames"]);
    let mut seen = std::collections::HashSet::new();
    let mut work = Vec::new();
    for (old, new) in &pairs {
        let (Some(b), Some(f)) = (file(old), file(new)) else {
            continue;
        };
        if !seen.insert(f) {
            continue;
        }
        let base = objects.get(&ObjKey::File(b)).await.ok().flatten();
        let data = objects.get(&ObjKey::File(f)).await.ok().flatten();
        let (Some(base), Some(data)) = (base, data) else {
            continue;
        };
        if base.is_empty() || data.len() > 4 << 20 || base.len() > 4 << 20 {
            continue;
        }
        let full = zstd::bulk::compress(&data, 3)
            .unwrap()
            .len()
            .min(data.len()) as u64
            + 1;
        work.push((base, data, full));
    }
    let was: u64 = work.iter().map(|w| w.2).sum();
    println!(
        "explore pairs={} files={} full_z3={was}",
        pairs.len(),
        work.len()
    );
    for lvl in [3, 6, 9, 12, 15, 17, 19] {
        let t = Instant::now();
        let mut out = Vec::with_capacity(work.len());
        for (base, data, _) in &work {
            let mut c = zstd::bulk::Compressor::with_dictionary(lvl, base).unwrap();
            c.include_contentsize(true).unwrap();
            out.push(c.compress(data).unwrap());
        }
        let enc = t.elapsed();
        let t = Instant::now();
        for ((base, data, _), z) in work.iter().zip(&out) {
            let mut d = zstd::bulk::Decompressor::with_dictionary(base).unwrap();
            let got = d.decompress(z, data.len()).unwrap();
            assert_eq!(got.len(), data.len());
        }
        let dec = t.elapsed();
        let half: u64 = work
            .iter()
            .zip(&out)
            .map(|((_, _, full), z)| {
                let d = z.len() as u64 + 33;
                if d * 2 < *full { d } else { *full }
            })
            .sum();
        println!(
            "explore level={lvl} stored_half={half} encode={:.1}s decode={:.2}s",
            enc.as_secs_f64(),
            dec.as_secs_f64()
        );
    }
}

/// Stores each modified file as a delta against its previous version when that saves half.
/// `(deltas written, pairs tried, stored bytes saved)`.
async fn delta_pass(objects: &BucketObjects, map: &Map, git_dir: &Path) -> (u64, u64, u64) {
    use asc_core::id::FileId;
    let pairs = modified_pairs(git_dir);
    let rows = map.rows.lock().unwrap().clone();
    let file = |g: &GitOid| {
        rows.get(&(*g, 1))
            .map(|b| asc_store::key::ObjKey::File(FileId::from_bytes(*b)))
    };
    let (mut written, mut tried, mut saved) = (0, 0, 0);
    for (old, new) in &pairs {
        let (Some(base), Some(f)) = (file(old), file(new)) else {
            continue;
        };
        tried += 1;
        if let Ok(p) = objects.plan_delta(f, base).await.expect("plan") {
            objects.store_delta(&p).await.expect("store");
            written += 1;
            saved += p.was.saturating_sub(p.stored.len() as u64);
        }
    }
    (written, tried, saved)
}

/// [`du`] per top folder of the store (one folder per object kind).
fn by_kind(dir: &Path) -> std::collections::BTreeMap<String, (u64, u64, u64)> {
    std::fs::read_dir(dir)
        .expect("read dir")
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), du(&e.path())))
        .collect()
}

/// Files, apparent bytes, and bytes if every file were rounded up to a 4 KiB block.
fn du(dir: &Path) -> (u64, u64, u64) {
    let (mut files, mut bytes, mut blocks) = (0, 0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("read dir").flatten() {
            let m = e.metadata().expect("metadata");
            if m.is_dir() {
                stack.push(e.path());
            } else {
                files += 1;
                bytes += m.len();
                blocks += m.len().div_ceil(4096).max(1) * 4096;
            }
        }
    }
    (files, bytes, blocks)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, src, out, mode] = &args[..] else {
        eprintln!("usage: import_bench <git-dir> <out-dir> <loose|batched|packed>");
        std::process::exit(2);
    };
    let out: PathBuf = out.into();
    assert!(!out.exists(), "{} exists", out.display());
    let base = BucketObjects::local(&out).expect("open store");
    let (objects, policy) = match mode.as_str() {
        "loose" => (
            base,
            ImportPolicy {
                batch_commits: 1,
                ..ImportPolicy::default()
            },
        ),
        "batched" | "delta" | "explore" => (base, ImportPolicy::default()),
        "packed" => (
            base.with_bucket_packs(PackConfig {
                enabled: true,
                negative_ttl: Duration::ZERO,
                ..PackConfig::default()
            }),
            ImportPolicy::default(),
        ),
        other => panic!("unknown mode {other}"),
    };
    let objects = Arc::new(objects);
    let engine = Engine::new(objects.clone(), Arc::new(NoRefs), RepoKey(1));
    let map = Map::default();
    let src = PathBuf::from(src);
    let odb = Odb::open(&src).expect("open odb");
    let tips = tips(&src);

    let started = Instant::now();
    let report = Importer::new(&engine, objects.clone(), &odb, &map, policy)
        .import_commits(&tips)
        .await
        .expect("import");
    let wall = started.elapsed();
    if mode == "explore" {
        explore(&objects, &map, &src).await;
        return;
    }
    if mode == "delta" {
        let t = Instant::now();
        let (written, tried, saved) = delta_pass(&objects, &map, &src).await;
        println!(
            "delta pairs={tried} written={written} saved_bytes={saved} wall={:.1}s",
            t.elapsed().as_secs_f64()
        );
    }
    let (files, bytes, blocks) = du(&out);
    for (kind, (f, b, _)) in by_kind(&out) {
        println!("kind={kind} files={f} bytes={b}");
    }
    println!(
        "mode={mode} commits={} blobs={} trees={} wall={:.1}s map_calls={} files={files} bytes={bytes} bytes_4k={blocks}",
        report.commits,
        report.blobs,
        report.trees,
        wall.as_secs_f64(),
        map.calls.lock().unwrap()
    );

    // Read a sample back: every 50th key, so a packed store is shown to serve what it stored.
    let keys: Vec<ObjKey> = objects
        .list(KeyPrefix::all())
        .map_ok(|(k, _)| k)
        .try_collect()
        .await
        .expect("list");
    let sample: Vec<&ObjKey> = keys.iter().step_by(50).collect();
    let t = Instant::now();
    for k in &sample {
        assert!(objects.get_stored(k).await.expect("read").is_some(), "{k}");
    }
    println!(
        "objects={} sample={} read_back={:.2}s",
        keys.len(),
        sample.len(),
        t.elapsed().as_secs_f64()
    );
    if mode == "packed" {
        let s = objects.pack_status().await.expect("status");
        println!("{s:?}");
    }
}
