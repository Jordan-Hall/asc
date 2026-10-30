//! Imports a git repository into a store on disk and reports what it cost: wall time, files and
//! bytes on disk, and a read-back of a sample. For `docs/BENCHMARKS.md` (packed objects).
//!
//! Usage: `cargo run -p asc-git --release --example import_bench -- <git-dir> <out-dir> <mode>`
//! where mode is `loose` (a write after every commit, no packs: the shape before batching),
//! `batched` (500 commits per batch, loose objects) or `packed` (batched, small objects in packs).
//! The source is only read; `<out-dir>` must not exist.
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
        "batched" => (base, ImportPolicy::default()),
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
    let (files, bytes, blocks) = du(&out);
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
