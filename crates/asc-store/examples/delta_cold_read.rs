//! Cold-read cost of a delta chain against a store with a fixed per-request latency.
//!
//! Wraps an in-memory object store in a decorator that sleeps before every request and counts
//! requests and bytes returned. For each file size and chain depth it builds the chain (version
//! `k` stored as a delta on version `k - 1`), then reads the top with a fresh `BucketObjects`
//! handle, so no value is cached in process. Prints a Markdown table of the median of five runs.
//!
//! `cargo run -p asc-store --example delta_cold_read -- [--latency-ms N]`

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use asc_core::id::FileId;
use asc_store::bucket::BucketObjects;
use asc_store::codec::{MAX_DELTA_DEPTH, encode_stored};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

const RUNS: usize = 5;
const SIZES: [(usize, &str); 3] = [(4 << 10, "4 KiB"), (64 << 10, "64 KiB"), (1 << 20, "1 MiB")];

/// Sleeps `latency` before every request and counts requests and bytes returned.
struct Slow {
    inner: InMemory,
    latency_us: AtomicU64,
    requests: AtomicU64,
    bytes: AtomicU64,
}

impl Slow {
    async fn hit(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let us = self.latency_us.load(Ordering::Relaxed);
        if us > 0 {
            tokio::time::sleep(Duration::from_micros(us)).await;
        }
    }

    fn reset(&self) {
        self.requests.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }
}

impl fmt::Debug for Slow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Slow")
    }
}

impl fmt::Display for Slow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Slow({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for Slow {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.hit().await;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.hit().await;
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.hit().await;
        let head = options.head;
        let r = self.inner.get_opts(location, options).await?;
        if !head {
            self.bytes
                .fetch_add(r.range.end.saturating_sub(r.range.start), Ordering::Relaxed);
        }
        Ok(r)
    }

    async fn get_ranges(
        &self,
        location: &Path,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        self.hit().await;
        let r = self.inner.get_ranges(location, ranges).await?;
        let n: u64 = r.iter().map(|b| b.len() as u64).sum();
        self.bytes.fetch_add(n, Ordering::Relaxed);
        Ok(r)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.hit().await;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.hit().await;
        self.inner.copy_opts(from, to, options).await
    }
}

/// Deterministic text-like content of `size` bytes (lines of words from a small vocabulary).
fn base_text(size: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 16] = [
        "fn", "let", "value", "index", "buffer", "return", "match", "result", "self", "mut",
        "usize", "string", "state", "error", "next", "count",
    ];
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut out = Vec::with_capacity(size + 64);
    while out.len() < size {
        let words = 3 + next() % 6;
        for i in 0..words {
            if i > 0 {
                out.push(b' ');
            }
            out.extend_from_slice(WORDS[(next() % 16) as usize].as_bytes());
            if next() % 4 == 0 {
                out.extend_from_slice(format!("_{}", next() % 1000).as_bytes());
            }
        }
        out.push(b'\n');
    }
    out.truncate(size);
    out
}

/// A small edit of `prev`: one run of 48 bytes at a place that depends on `k` is rewritten.
fn edit(prev: &[u8], k: usize) -> Vec<u8> {
    let mut v = prev.to_vec();
    let at = (k * 7919 + 13) * 31 % (v.len() - 64);
    let patch = base_text(48, 0x9e37_79b9 ^ (k as u64 * 0x1000_0001));
    v[at..at + 48].copy_from_slice(&patch);
    v
}

fn median<T: Ord + Copy>(mut v: Vec<T>) -> T {
    v.sort();
    v[v.len() / 2]
}

#[tokio::main]
async fn main() {
    let mut latency_ms = 20u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match (a.as_str(), args.next().and_then(|v| v.parse().ok())) {
            ("--latency-ms", Some(n)) => latency_ms = n,
            _ => {
                eprintln!("usage: delta_cold_read [--latency-ms N]");
                std::process::exit(2);
            }
        }
    }
    let max_depth = MAX_DELTA_DEPTH as usize;

    println!("Latency per request: {latency_ms} ms (a real sleep). Median of {RUNS} cold reads.\n");
    println!("| size | depth | requests | bytes fetched | time at {latency_ms} ms |");
    println!("|---|---|---|---|---|");
    for (size, label) in SIZES {
        let slow = Arc::new(Slow {
            inner: InMemory::new(),
            latency_us: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        let store: Arc<dyn ObjectStore> = slow.clone();
        let setup = BucketObjects::new(store.clone(), Path::from("ns"));
        // Version 0 is stored whole; version k is a delta on version k - 1.
        let mut data = base_text(size, 42 + size as u64);
        let mut keys = Vec::new();
        let mut prev: Option<ObjKey> = None;
        for k in 0..max_depth {
            if k > 0 {
                data = edit(&data, k);
            }
            let key = ObjKey::File(FileId::of(&data));
            setup.put(&key, encode_stored(&data)).await.unwrap();
            if let Some(base) = prev {
                let plan = setup.plan_delta(key, base).await.unwrap();
                let plan = plan.unwrap_or_else(|s| panic!("{label} depth {k}: skipped {s:?}"));
                setup.store_delta(&plan).await.unwrap();
            }
            keys.push((key, data.clone()));
            prev = Some(key);
        }
        slow.latency_us.store(latency_ms * 1000, Ordering::Relaxed);
        for (depth, (key, want)) in keys.iter().enumerate() {
            let (mut reqs, mut bytes, mut times) = (Vec::new(), Vec::new(), Vec::new());
            for _ in 0..RUNS {
                let fresh = BucketObjects::new(store.clone(), Path::from("ns"));
                slow.reset();
                let t = Instant::now();
                let got = fresh.get(key).await.unwrap().unwrap();
                times.push(t.elapsed());
                assert_eq!(&got[..], &want[..], "{label} depth {depth} reads back");
                reqs.push(slow.requests.load(Ordering::Relaxed));
                bytes.push(slow.bytes.load(Ordering::Relaxed));
            }
            println!(
                "| {label} | {depth} | {} | {} | {} ms |",
                median(reqs),
                median(bytes),
                median(times).as_millis()
            );
        }
    }
}
