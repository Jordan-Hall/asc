//! Imports a git repository into a store on disk and reports what it cost: wall time, files and
//! bytes on disk, and a read-back of a sample. For `docs/BENCHMARKS.md` (packed objects).
//!
//! Usage: `cargo run -p asc-git --release --example import_bench -- <git-dir> <out-dir> <mode>`
//! where mode is `loose` (a write after every commit, no packs: the shape before batching),
//! `batched` (500 commits per batch, loose objects), `packed` (batched, small objects in packs) or
//! `delta` (batched, then every modified file, large-file chunk and changed tree stored as a delta
//! against its previous version at the same place, as the server's delta pass does; the pairs come
//! from `git log --raw -t`). The source is only
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

/// Stores what the server's delta pass would: each modified file as a delta against its previous
/// version at the same path, each new chunk of a large file against the old version's chunk at the
/// same place, and each changed tree (roots and subdirectories) against the same directory's old
/// tree, when that saves half. Prints one line per kind.
async fn delta_pass(objects: &BucketObjects, map: &Map, git_dir: &Path) {
    use asc_core::codec::decode_envelope;
    use asc_core::id::{FileId, TreeId};
    use asc_core::object::ChunkList;
    use asc_store::delta::pair_chunks;
    let rows = map.rows.lock().unwrap().clone();
    let id = |g: &GitOid, k: u8| rows.get(&(*g, k)).copied();
    let chunk_list = async |f: [u8; 32]| -> Option<ChunkList> {
        let b = objects
            .get(&ObjKey::ChunkList(FileId::from_bytes(f)))
            .await
            .ok()??;
        decode_envelope(&b).ok()
    };
    let mut files = Vec::new();
    let mut chunks = Vec::new();
    for (old, new) in &modified_pairs(git_dir) {
        let (Some(b), Some(f)) = (id(old, 1), id(new, 1)) else {
            continue;
        };
        let (fk, bk) = (
            ObjKey::File(FileId::from_bytes(f)),
            ObjKey::File(FileId::from_bytes(b)),
        );
        if objects.has_many(&[fk]).await.expect("has")[0] {
            files.push((fk, vec![bk]));
        } else if let (Some(n), Some(o)) = (chunk_list(f).await, chunk_list(b).await) {
            chunks.extend(
                pair_chunks(&n, &o)
                    .into_iter()
                    .map(|(c, o)| (ObjKey::Chunk(c), vec![ObjKey::Chunk(o)])),
            );
        }
    }
    let file = |g: &GitOid| id(g, 1).map(|f| ObjKey::File(FileId::from_bytes(f)));
    for (new, near) in &new_file_candidates(git_dir) {
        let Some(fk) = file(new) else { continue };
        let bases: Vec<ObjKey> = near.iter().filter_map(file).collect();
        if !bases.is_empty() && objects.has_many(&[fk]).await.expect("has")[0] {
            files.push((fk, bases));
        }
    }
    let trees: Vec<(ObjKey, Vec<ObjKey>)> = tree_pairs(git_dir)
        .iter()
        .filter_map(|(old, new)| {
            let (b, t) = (id(old, 2)?, id(new, 2)?);
            Some((
                ObjKey::Tree(TreeId::from_bytes(t)),
                vec![ObjKey::Tree(TreeId::from_bytes(b))],
            ))
        })
        .collect();
    for (kind, pairs) in [("file", files), ("chunk", chunks), ("tree", trees)] {
        let t = Instant::now();
        let (mut written, mut saved) = (0u64, 0u64);
        let mut cache = asc_store::delta::PlanCache::default();
        let (mut plan_t, mut store_t) = (Duration::ZERO, Duration::ZERO);
        for (key, bases) in &pairs {
            let at = Instant::now();
            let planned = objects.plan_best(*key, bases, &mut cache).await;
            plan_t += at.elapsed();
            if let Ok(p) = planned.expect("plan") {
                let at = Instant::now();
                objects.store_delta(&p).await.expect("store");
                store_t += at.elapsed();
                written += 1;
                saved += p.was.saturating_sub(p.stored.len() as u64);
            }
        }
        println!(
            "delta kind={kind} pairs={} written={written} saved_bytes={saved} wall={:.1}s plan={:.1}s store={:.1}s",
            pairs.len(),
            t.elapsed().as_secs_f64(),
            plan_t.as_secs_f64(),
            store_t.as_secs_f64()
        );
    }
}

/// Each file a first-parent commit adds, with the bases the server's pass tries for it: files of
/// the parent's same directory with the same extension and a size within half to double, nearest
/// sizes first, at most four.
fn new_file_candidates(git_dir: &Path) -> Vec<(GitOid, Vec<GitOid>)> {
    use std::collections::HashMap;
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let ext = |name: &str| -> String {
        let name = name.rsplit('/').next().unwrap_or(name);
        match name.rfind('.') {
            Some(0) | None => String::new(),
            Some(i) => name[i + 1..].to_owned(),
        }
    };
    let log = git(&[
        "log",
        "--all",
        "--reverse",
        "--first-parent",
        "--no-renames",
        "--raw",
        "--no-abbrev",
        "--format=C %P",
    ]);
    let sizes: HashMap<GitOid, u64> = git(&[
        "cat-file",
        "--batch-all-objects",
        "--batch-check=%(objectname) %(objecttype) %(objectsize)",
    ])
    .lines()
    .filter_map(|l| {
        let mut f = l.split(' ');
        let (oid, kind, size) = (f.next()?, f.next()?, f.next()?);
        (kind == "blob").then(|| Some((oid.parse().ok()?, size.parse().ok()?)))?
    })
    .collect();
    // (parent commit, directory) -> its files as (blob, size, extension).
    type Listing = Vec<(GitOid, u64, String)>;
    let mut listings: HashMap<(String, String), Listing> = HashMap::new();
    let mut out = Vec::new();
    let mut parent: Option<String> = None;
    for line in log.lines() {
        if let Some(ps) = line.strip_prefix("C ") {
            parent = ps.split_whitespace().next().map(str::to_owned);
            continue;
        }
        // :000000 100644 <zero> <new> A	<path>
        let Some((meta, path)) = line.strip_prefix(':').and_then(|l| l.split_once('\t')) else {
            continue;
        };
        let f: Vec<&str> = meta.split_whitespace().collect();
        let (Some(p), [_, mode, _, new, "A"]) = (&parent, f.as_slice()) else {
            continue;
        };
        if *mode == "120000" || *mode == "160000" {
            continue;
        }
        let Ok(new) = new.parse::<GitOid>() else {
            continue;
        };
        let dir = path
            .rsplit_once('/')
            .map(|(d, _)| d.to_owned())
            .unwrap_or_default();
        let entries = listings.entry((p.clone(), dir.clone())).or_insert_with(|| {
            let spec = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            let mut args = vec!["ls-tree", "-l", p.as_str()];
            if !spec.is_empty() {
                args.push(spec.as_str());
            }
            git(&args)
                .lines()
                .filter_map(|l| {
                    // <mode> blob <oid> <size>	<name>
                    let (meta, name) = l.split_once('\t')?;
                    let f: Vec<&str> = meta.split_whitespace().collect();
                    let [m, "blob", oid, size] = f.as_slice() else {
                        return None;
                    };
                    (*m == "100644" || *m == "100755")
                        .then(|| Some((oid.parse().ok()?, size.parse().ok()?, ext(name))))?
                })
                .collect()
        });
        let Some(&size) = sizes.get(&new) else {
            continue;
        };
        let e = ext(path);
        let mut near: Vec<(u64, GitOid)> = entries
            .iter()
            .filter(|(_, s, x)| *x == e && *s <= 4 << 20 && s * 2 >= size && *s <= size * 2)
            .map(|(o, s, _)| (s.abs_diff(size), *o))
            .collect();
        near.sort_by_key(|(d, _)| *d);
        let near: Vec<GitOid> = near.into_iter().take(4).map(|(_, o)| o).collect();
        if !near.is_empty() {
            out.push((new, near));
        }
    }
    out
}

/// `(old, new)` tree pairs in history order: each commit's root against its first parent's root,
/// then every modified subdirectory against the same directory before (`git log --raw -t`).
fn tree_pairs(git_dir: &Path) -> Vec<(GitOid, GitOid)> {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let log = git(&["log", "--all", "--reverse", "--format=%H %T %P"]);
    let commits: Vec<Vec<&str>> = log.lines().map(|l| l.split(' ').collect()).collect();
    let tree_of: HashMap<&str, &str> = commits
        .iter()
        .filter(|c| c.len() >= 2)
        .map(|c| (c[0], c[1]))
        .collect();
    let mut out: Vec<(GitOid, GitOid)> = commits
        .iter()
        .filter_map(|c| {
            let (t, p) = (*c.get(1)?, *c.get(2)?);
            let pt = *tree_of.get(p)?;
            (pt != t).then(|| Some((pt.parse().ok()?, t.parse().ok()?)))?
        })
        .collect();
    let raw = git(&[
        "log",
        "--all",
        "--reverse",
        "--raw",
        "-t",
        "--no-abbrev",
        "--no-renames",
        "--format=",
    ]);
    out.extend(raw.lines().filter_map(|l| {
        // :040000 040000 <old> <new> M	<dir>
        let mut f = l.strip_prefix(':')?.split_whitespace();
        let (a, b, old, new, st) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
        (a == "040000" && b == "040000" && st == "M" && old != new)
            .then(|| Some((old.parse().ok()?, new.parse().ok()?)))?
    }));
    out
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
        delta_pass(&objects, &map, &src).await;
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
