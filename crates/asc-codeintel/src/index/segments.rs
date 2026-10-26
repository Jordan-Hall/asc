//! Segment writer, pack format, bucket I/O and the replica-side reader (SPEC §23.2.4).
//!
//! A segment is one finished single-segment tantivy index, packed into one immutable bucket
//! object `d/ciseg.v2/<scoped id>`, where the id hashes the index identity with the pack hash. A writer builds a segment from whole file jobs
//! and publishes nothing: only a manifest generation (see `manifest`) makes a pack live, so a
//! reader never sees a half-indexed file. Packs are untrusted on read: the hash is checked
//! against the manifest, sizes are capped, file names are checked against the segment id
//! and the schema must match.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::{self, Objects};
use bytes::Bytes;
use roaring::RoaringBitmap;
use tantivy::collector::DocSetCollector;
use tantivy::indexer::NoMergePolicy;
use tantivy::query::Query;
use tantivy::schema::Schema;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument};

use super::manifest::Manifest;
use super::schema::{ChunksSchema, FilesSchema, IndexKind, SCHEMA_VER, register_tokenizers};
use super::{IndexError, Result};
use crate::grams::Gram;
use crate::lang::Lang;

/// Blake3 of a pack (or of a serialized bitmap).
pub type Hash = [u8; 32];

pub const SEG_NAME: &str = "ciseg";
pub const IDX_NAME: &str = "ciidx";
/// Version of the object key scheme: v2 scopes every id to its `(ns, kind)` index.
pub const OBJ_VERSION: u32 = 2;
const KEY_CONTEXT: &str = "asc codeintel manifest object key v2";

/// Largest decoded pack, equal to the store's cap for derived values.
pub const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;
/// Largest serialized `indexed` bitmap.
pub const MAX_INDEXED_BYTES: u64 = 64 * 1024 * 1024;
/// Writer heap (SPEC §23.2.4: 32 MiB per writer).
pub const WRITER_HEAP: usize = 32 * 1024 * 1024;
/// Most documents in one segment.
pub const MAX_SEGMENT_DOCS: u32 = 1 << 22;
/// Most distinct trigrams of one file.
pub const MAX_GRAMS: usize = 1 << 22;
/// Most terms in one multi-valued text field of one document.
pub const MAX_TERMS: usize = 100_000;
/// Longest single raw term (symbol name), in bytes.
pub const MAX_TERM_LEN: usize = 256;
/// Longest free text field of one document, in bytes (cut at a char boundary).
pub const MAX_TEXT: usize = 1024 * 1024;
const MAX_PACK_FILES: usize = 64;

const MAGIC: &[u8; 4] = b"CISG";

/// Bucket id of a manifest-owned object: blake3 over the index identity `(ns, kind)`, the
/// object class and the content hash. Two indexes never share an object, even when their
/// bytes are identical, so one manifest deleting its dropped objects cannot corrupt another.
/// The reader recomputes this from its own `(ns, kind)`, then verifies the content hash.
fn scoped_id(name: &str, ns: &str, kind: IndexKind, content: Hash) -> Hash {
    let mut h = blake3::Hasher::new_derive_key(KEY_CONTEXT);
    h.update(&(ns.len() as u64).to_le_bytes());
    h.update(ns.as_bytes());
    h.update(&[kind.code()]);
    h.update(&(name.len() as u64).to_le_bytes());
    h.update(name.as_bytes());
    h.update(&content);
    *h.finalize().as_bytes()
}

/// Key of a segment pack of index `(ns, kind)` whose content hashes to `h`.
pub fn seg_key(ns: &str, kind: IndexKind, h: Hash) -> ObjKey {
    ObjKey::Derived {
        name: SEG_NAME,
        version: OBJ_VERSION,
        id: scoped_id(SEG_NAME, ns, kind, h),
    }
}

/// Key of an `indexed` bitmap of index `(ns, kind)` whose content hashes to `h`.
pub fn idx_key(ns: &str, kind: IndexKind, h: Hash) -> ObjKey {
    ObjKey::Derived {
        name: IDX_NAME,
        version: OBJ_VERSION,
        id: scoped_id(IDX_NAME, ns, kind, h),
    }
}

/// Everything the `files` index holds about one file.
pub struct FileDoc<'a> {
    pub ord: u32,
    /// Distinct folded trigrams (`grams::distinct_trigrams`).
    pub grams: &'a [Gram],
    pub sym_def: &'a [&'a str],
    pub sym_ref: &'a [&'a str],
    pub ident: &'a str,
    pub doc: &'a str,
    pub path: &'a str,
    pub langs: &'a [Lang],
}

/// One chunk of the `chunks` index.
pub struct ChunkDoc<'a> {
    pub ord: u32,
    pub chunk: u32,
    pub ident: &'a str,
    pub doc: &'a str,
    pub symbol: &'a str,
}

/// A finished segment, ready to upload.
pub struct SegmentPack {
    pub hash: Hash,
    /// `codec ‖ pack`, the bytes to `put`.
    pub stored: Bytes,
    pub kind: IndexKind,
    pub docs: u32,
    /// Raw pack size.
    pub bytes: u64,
    /// Ords of the documents in the segment.
    pub ords: RoaringBitmap,
}

fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

enum Fields {
    Files(FilesSchema),
    Chunks(ChunksSchema),
}

/// Builds one segment from whole documents. Nothing is visible until the pack is published.
pub struct SegmentWriter {
    kind: IndexKind,
    fields: Fields,
    dir: tempfile::TempDir,
    writer: IndexWriter<TantivyDocument>,
    ords: RoaringBitmap,
    docs: u32,
}

impl SegmentWriter {
    /// `work_dir` holds the scratch directory and must exist. `heap` is clamped to
    /// 16 MiB..=256 MiB.
    pub fn new(kind: IndexKind, work_dir: &Path, heap: usize) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("cisw")
            .tempdir_in(work_dir)?;
        let (schema, fields) = match kind {
            IndexKind::Files => {
                let s = FilesSchema::build();
                (s.schema.clone(), Fields::Files(s))
            }
            IndexKind::Chunks => {
                let s = ChunksSchema::build();
                (s.schema.clone(), Fields::Chunks(s))
            }
        };
        let index = Index::create_in_dir(dir.path(), schema)?;
        register_tokenizers(index.tokenizers());
        let writer = index.writer_with_num_threads(1, heap.clamp(16 << 20, 256 << 20))?;
        writer.set_merge_policy(Box::new(NoMergePolicy));
        Ok(SegmentWriter {
            kind,
            fields,
            dir,
            writer,
            ords: RoaringBitmap::new(),
            docs: 0,
        })
    }

    pub fn docs(&self) -> u32 {
        self.docs
    }

    fn check_room(&self) -> Result<()> {
        if self.docs >= MAX_SEGMENT_DOCS {
            return Err(IndexError::Limit("documents per segment"));
        }
        Ok(())
    }

    /// Adds one file. Fails on a second document for the same ord, or over a limit; a failed
    /// call adds nothing.
    pub fn add_file(&mut self, f: &FileDoc<'_>) -> Result<()> {
        let Fields::Files(s) = &self.fields else {
            return Err(IndexError::BadPack("files document in a chunks writer"));
        };
        self.check_room()?;
        if f.grams.len() > MAX_GRAMS {
            return Err(IndexError::Limit("trigrams per file"));
        }
        if f.sym_def.len() > MAX_TERMS || f.sym_ref.len() > MAX_TERMS {
            return Err(IndexError::Limit("symbols per file"));
        }
        if self.ords.contains(f.ord) {
            return Err(IndexError::DuplicateOrd(f.ord));
        }
        let mut d = TantivyDocument::default();
        d.add_u64(s.ord, u64::from(f.ord));
        for &g in f.grams {
            d.add_u64(s.tri, u64::from(g));
        }
        for t in f.sym_def {
            if !t.is_empty() {
                d.add_text(s.sym_def, cut(t, MAX_TERM_LEN));
            }
        }
        for t in f.sym_ref {
            if !t.is_empty() {
                d.add_text(s.sym_ref, cut(t, MAX_TERM_LEN));
            }
        }
        d.add_text(s.ident, cut(f.ident, MAX_TEXT));
        d.add_text(s.doc, cut(f.doc, MAX_TEXT));
        d.add_text(s.path_tok, cut(f.path, 4096));
        for l in f.langs.iter().take(crate::doc::MAX_LANGS) {
            d.add_text(s.lang, l.name());
        }
        self.writer.add_document(d)?;
        self.ords.insert(f.ord);
        self.docs += 1;
        Ok(())
    }

    /// Adds one chunk. A file's chunks share an ord, so the ord may repeat.
    pub fn add_chunk(&mut self, c: &ChunkDoc<'_>) -> Result<()> {
        let Fields::Chunks(s) = &self.fields else {
            return Err(IndexError::BadPack("chunk document in a files writer"));
        };
        self.check_room()?;
        let mut d = TantivyDocument::default();
        d.add_u64(s.ord, u64::from(c.ord));
        d.add_u64(s.chunk, u64::from(c.chunk));
        d.add_text(s.ident, cut(c.ident, MAX_TEXT));
        d.add_text(s.doc, cut(c.doc, MAX_TEXT));
        d.add_text(s.symbol, cut(c.symbol, MAX_TEXT));
        self.writer.add_document(d)?;
        self.ords.insert(c.ord);
        self.docs += 1;
        Ok(())
    }

    /// Commits and packs. `None` when no document was added.
    pub fn finish(mut self) -> Result<Option<SegmentPack>> {
        if self.docs == 0 {
            return Ok(None);
        }
        self.writer.commit()?;
        self.writer.wait_merging_threads()?;
        let raw = pack_dir(self.kind, self.dir.path(), None)?;
        Ok(Some(make_pack(self.kind, raw, self.docs, self.ords)))
    }
}

fn make_pack(kind: IndexKind, raw: Vec<u8>, docs: u32, ords: RoaringBitmap) -> SegmentPack {
    let hash = *blake3::hash(&raw).as_bytes();
    SegmentPack {
        hash,
        stored: encode_stored(&raw),
        kind,
        docs,
        bytes: raw.len() as u64,
        ords,
    }
}

/// A name tantivy writes for one index: `meta.json`, `.managed.json`, or
/// `<32 hex segment id>.<ext>`.
fn valid_name(n: &str) -> bool {
    if n == "meta.json" || n == ".managed.json" {
        return true;
    }
    match n.split_once('.') {
        Some((id, ext)) => {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && (1..=16).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_lowercase())
        }
        None => false,
    }
}

fn pack_dir(kind: IndexKind, dir: &Path, only_segment: Option<&str>) -> Result<Vec<u8>> {
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        let wanted = match only_segment {
            Some(id) => name == "meta.json" || name == ".managed.json" || name.starts_with(id),
            None => true,
        };
        if wanted && valid_name(&name) && e.file_type()?.is_file() {
            files.insert(name, e.path());
        }
    }
    if files.len() > MAX_PACK_FILES {
        return Err(IndexError::Limit("files per segment"));
    }
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(1);
    out.push(kind.code());
    out.push(SCHEMA_VER);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    for (name, path) in files {
        let data = std::fs::read(&path)?;
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(&data);
        if out.len() as u64 > MAX_PACK_BYTES {
            return Err(IndexError::Limit("segment pack size"));
        }
    }
    Ok(out)
}

/// Parses a pack into `(kind, [(name, bytes)])`. Every length is checked against the input.
type PackFiles<'a> = Vec<(&'a str, &'a [u8])>;

fn parse_pack(raw: &[u8]) -> Result<(IndexKind, PackFiles<'_>)> {
    let bad = IndexError::BadPack;
    if raw.len() as u64 > MAX_PACK_BYTES {
        return Err(bad("too large"));
    }
    if raw.len() < 9 || &raw[..4] != MAGIC || raw[4] != 1 {
        return Err(bad("header"));
    }
    let kind = IndexKind::from_code(raw[5]).ok_or(bad("kind"))?;
    if raw[6] != SCHEMA_VER {
        return Err(bad("schema version"));
    }
    let n = usize::from(u16::from_le_bytes([raw[7], raw[8]]));
    if n == 0 || n > MAX_PACK_FILES {
        return Err(bad("file count"));
    }
    let mut at = 9usize;
    let mut out: Vec<(&str, &[u8])> = Vec::with_capacity(n);
    for _ in 0..n {
        let nl = usize::from(*raw.get(at).ok_or(bad("truncated"))?);
        at += 1;
        let name = raw.get(at..at + nl).ok_or(bad("truncated"))?;
        let name = std::str::from_utf8(name).map_err(|_| bad("name"))?;
        if !valid_name(name) {
            return Err(bad("file name"));
        }
        at += nl;
        let lb: [u8; 8] = raw
            .get(at..at + 8)
            .ok_or(bad("truncated"))?
            .try_into()
            .map_err(|_| bad("truncated"))?;
        let len = usize::try_from(u64::from_le_bytes(lb)).map_err(|_| bad("length"))?;
        at += 8;
        let end = at.checked_add(len).ok_or(bad("length"))?;
        let data = raw.get(at..end).ok_or(bad("truncated"))?;
        at = end;
        if out.iter().any(|(n2, _)| *n2 == name) {
            return Err(bad("duplicate file"));
        }
        out.push((name, data));
    }
    if at != raw.len() {
        return Err(bad("trailing bytes"));
    }
    Ok((kind, out))
}

/// Refuses a symlink or non-directory, creating the directory when absent.
pub(crate) fn ensure_dir(p: &Path) -> Result<()> {
    let is_plain_dir = |p: &Path| {
        std::fs::symlink_metadata(p)
            .map(|m| m.is_dir() && !m.file_type().is_symlink())
            .unwrap_or(false)
    };
    match std::fs::symlink_metadata(p) {
        Ok(_) if is_plain_dir(p) => Ok(()),
        Ok(_) => Err(IndexError::Symlink(p.display().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Err(e) = std::fs::create_dir(p)
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(e.into());
            }
            if is_plain_dir(p) {
                Ok(())
            } else {
                Err(IndexError::Symlink(p.display().to_string()))
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn write_file(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(name))?;
    f.write_all(data)?;
    Ok(())
}

/// One segment opened from a local directory.
pub struct OpenSegment {
    pub hash: Hash,
    pub index: Index,
    pub reader: IndexReader,
    pub docs: u32,
}

fn open_dir(kind: IndexKind, hash: Hash, dir: &Path) -> Result<OpenSegment> {
    let index = Index::open_in_dir(dir)?;
    register_tokenizers(index.tokenizers());
    if index.schema() != kind.schema() {
        return Err(IndexError::BadPack("schema mismatch"));
    }
    let metas = index.searchable_segment_metas()?;
    let [meta] = metas.as_slice() else {
        return Err(IndexError::BadPack("a pack holds exactly one segment"));
    };
    if meta.has_deletes() {
        return Err(IndexError::BadPack("segment has deletes"));
    }
    let id = meta.id().uuid_string();
    for e in std::fs::read_dir(dir)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        let own = name == "meta.json"
            || name == ".managed.json"
            || name.ends_with(".lock")
            || name.starts_with(&id);
        if !own {
            return Err(IndexError::BadPack("file of another segment"));
        }
    }
    let docs = meta.num_docs();
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    Ok(OpenSegment {
        hash,
        index,
        reader,
        docs,
    })
}

/// Unpacks verified pack bytes into a new directory `dest` (which must not exist) via a
/// temporary sibling, then opens it.
pub fn unpack_and_open(
    kind: IndexKind,
    hash: Hash,
    raw: &[u8],
    dest: &Path,
) -> Result<OpenSegment> {
    let (pk, files) = parse_pack(raw)?;
    if pk != kind {
        return Err(IndexError::BadPack("kind mismatch"));
    }
    let parent = dest.parent().ok_or(IndexError::BadPack("destination"))?;
    let tmp = tempfile::Builder::new().prefix("ciun").tempdir_in(parent)?;
    for (name, data) in &files {
        write_file(tmp.path(), name, data)?;
    }
    // Validate before the directory becomes visible under its final name.
    drop(open_dir(kind, hash, tmp.path())?);
    let tmp_path = tmp.keep();
    if let Err(e) = std::fs::rename(&tmp_path, dest) {
        let _ = std::fs::remove_dir_all(&tmp_path);
        // A concurrent sync may have won; its copy was validated the same way.
        if !dest.is_dir() {
            return Err(e.into());
        }
    }
    open_dir(kind, hash, dest)
}

/// The ords of every document of an index (fast field `ord`).
fn segment_ords(index: &Index) -> Result<RoaringBitmap> {
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let mut out = RoaringBitmap::new();
    for sr in searcher.segment_readers() {
        let col = sr.fast_fields().u64("ord")?;
        for doc in 0..sr.max_doc() {
            if let Some(v) = col.first(doc) {
                out.insert(u32::try_from(v).map_err(|_| IndexError::BadPack("ord"))?);
            }
        }
    }
    Ok(out)
}

/// Merges whole packs into one (the leader's compaction). Inputs are decoded, hash-verified
/// pack bytes of one kind. Peak memory is the tantivy writer heap plus the merge's own buffers.
pub fn merge_packs(
    kind: IndexKind,
    work_dir: &Path,
    inputs: &[Bytes],
) -> Result<Option<SegmentPack>> {
    if inputs.is_empty() {
        return Ok(None);
    }
    let total: u64 = inputs.iter().map(|b| b.len() as u64).sum();
    if total > MAX_PACK_BYTES {
        return Err(IndexError::Limit("merge input size"));
    }
    let tmp = tempfile::Builder::new()
        .prefix("cimg")
        .tempdir_in(work_dir)?;
    let mut segments: Vec<serde_json::Value> = Vec::new();
    let mut base: Option<serde_json::Value> = None;
    for raw in inputs {
        let (pk, files) = parse_pack(raw)?;
        if pk != kind {
            return Err(IndexError::BadPack("kind mismatch"));
        }
        for (name, data) in &files {
            match *name {
                "meta.json" => {
                    let v: serde_json::Value = serde_json::from_slice(data)
                        .map_err(|_| IndexError::BadPack("meta.json"))?;
                    let segs = v
                        .get("segments")
                        .and_then(|s| s.as_array())
                        .ok_or(IndexError::BadPack("meta.json segments"))?;
                    segments.extend(segs.iter().cloned());
                    base.get_or_insert(v);
                }
                ".managed.json" => {}
                n => write_file(tmp.path(), n, data)?,
            }
        }
    }
    let mut meta = base.ok_or(IndexError::BadPack("meta.json missing"))?;
    meta["segments"] = serde_json::Value::Array(segments);
    let meta_bytes = serde_json::to_vec(&meta).map_err(|e| IndexError::Io(e.to_string()))?;
    write_file(tmp.path(), "meta.json", &meta_bytes)?;
    let index = Index::open_in_dir(tmp.path())?;
    register_tokenizers(index.tokenizers());
    if index.schema() != kind.schema() {
        return Err(IndexError::BadPack("schema mismatch"));
    }
    let ids = index.searchable_segment_ids()?;
    if ids.len() != inputs.len() {
        return Err(IndexError::BadPack("segment count"));
    }
    let mut writer: IndexWriter<TantivyDocument> = index.writer_with_num_threads(1, WRITER_HEAP)?;
    writer.set_merge_policy(Box::new(NoMergePolicy));
    let merged = writer.merge(&ids).wait()?;
    writer.garbage_collect_files().wait()?;
    writer.wait_merging_threads()?;
    let Some(meta) = merged else {
        return Ok(None);
    };
    let docs = meta.num_docs();
    let ords = segment_ords(&index)?;
    let id = meta.id().uuid_string();
    drop(index);
    let raw = pack_dir(kind, tmp.path(), Some(&id))?;
    Ok(Some(make_pack(kind, raw, docs, ords)))
}

/// Uploads a pack into the index `ns`. Idempotent: the key is scoped by the index and the
/// hash of the content.
pub async fn put_pack(objects: &dyn Objects, ns: &str, pack: &SegmentPack) -> Result<()> {
    objects
        .put(&seg_key(ns, pack.kind, pack.hash), pack.stored.clone())
        .await
        .map_err(|e| IndexError::Store(e.to_string()))
}

/// Reads a derived object by hash, decodes it within `max` and verifies its blake3.
pub async fn get_verified(
    objects: &dyn Objects,
    key: ObjKey,
    hash: Hash,
    max: u64,
) -> Result<Bytes> {
    let stored = match objects.get_stored(&key).await {
        Ok(Some(b)) => b,
        Ok(None) => return Err(IndexError::Missing(hex(&hash))),
        Err(objects::Error::Corrupt(_) | objects::Error::Codec(..)) => {
            return Err(IndexError::HashMismatch(hex(&hash)));
        }
        Err(e) => return Err(IndexError::Store(e.to_string())),
    };
    let data = asc_store::codec::decode_stored_capped(&stored, max)
        .map_err(|_| IndexError::HashMismatch(hex(&hash)))?;
    if *blake3::hash(&data).as_bytes() != hash {
        return Err(IndexError::HashMismatch(hex(&hash)));
    }
    Ok(data)
}

pub fn hex(h: &Hash) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in h {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Serializes and uploads an `indexed` bitmap; returns its hash.
pub async fn put_indexed(
    objects: &dyn Objects,
    ns: &str,
    kind: IndexKind,
    bm: &RoaringBitmap,
) -> Result<Hash> {
    let mut buf = Vec::with_capacity(bm.serialized_size());
    bm.serialize_into(&mut buf)?;
    if buf.len() as u64 > MAX_INDEXED_BYTES {
        return Err(IndexError::Limit("indexed bitmap size"));
    }
    let hash = *blake3::hash(&buf).as_bytes();
    objects
        .put(&idx_key(ns, kind, hash), encode_stored(&buf))
        .await
        .map_err(|e| IndexError::Store(e.to_string()))?;
    Ok(hash)
}

pub async fn get_indexed(
    objects: &dyn Objects,
    ns: &str,
    kind: IndexKind,
    hash: Hash,
) -> Result<RoaringBitmap> {
    let raw = get_verified(objects, idx_key(ns, kind, hash), hash, MAX_INDEXED_BYTES).await?;
    RoaringBitmap::deserialize_from(&raw[..]).map_err(|_| IndexError::BadPack("indexed bitmap"))
}

pub(crate) fn check_ns(ns: &str) -> Result<()> {
    if ns.is_empty()
        || ns.len() > 64
        || !ns
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(IndexError::BadPack("namespace name"));
    }
    Ok(())
}

/// A replica's local copy of the segments (`disk_dir/ci/`), synced from the bucket.
#[derive(Clone, Debug)]
pub struct Replica {
    root: PathBuf,
}

impl Replica {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Replica { root: root.into() }
    }

    fn seg_dir(&self, ns: &str, kind: IndexKind) -> Result<PathBuf> {
        check_ns(ns)?;
        ensure_dir(&self.root)?;
        let a = self.root.join(ns);
        ensure_dir(&a)?;
        let b = a.join(kind.name());
        ensure_dir(&b)?;
        Ok(b)
    }

    /// Opens the manifest's generation, downloading segments that are not local. Segments
    /// that fail verification are refused, never partially used.
    pub async fn snapshot(
        &self,
        objects: &dyn Objects,
        ns: &str,
        m: &Manifest,
    ) -> Result<Snapshot> {
        let base = self.seg_dir(ns, m.index)?;
        let mut segments = Vec::with_capacity(m.segments.len());
        for s in &m.segments {
            let dest = base.join(hex(&s.hash));
            let (kind, hash) = (m.index, s.hash);
            let d2 = dest.clone();
            let local = tokio::task::spawn_blocking(move || {
                if std::fs::symlink_metadata(&d2).is_ok() {
                    open_dir(kind, hash, &d2).ok()
                } else {
                    None
                }
            })
            .await
            .map_err(|e| IndexError::Io(e.to_string()))?;
            if let Some(o) = local {
                segments.push(o);
                continue;
            }
            let raw = get_verified(objects, seg_key(ns, kind, hash), hash, MAX_PACK_BYTES).await?;
            let open = tokio::task::spawn_blocking(move || {
                if std::fs::symlink_metadata(&dest).is_ok() {
                    // Present but damaged: replace it.
                    let _ = std::fs::remove_dir_all(&dest);
                }
                unpack_and_open(kind, hash, &raw, &dest)
            })
            .await
            .map_err(|e| IndexError::Io(e.to_string()))??;
            segments.push(open);
        }
        let indexed = match m.indexed {
            Some(h) => get_indexed(objects, ns, m.index, h).await?,
            None => RoaringBitmap::new(),
        };
        Ok(Snapshot {
            generation: m.generation,
            kind: m.index,
            indexed,
            segments,
        })
    }

    /// Removes local segment directories of `ns`/`kind` that are not in `keep`.
    pub fn prune(&self, ns: &str, kind: IndexKind, keep: &[Hash]) -> Result<()> {
        let base = self.seg_dir(ns, kind)?;
        let keep: Vec<String> = keep.iter().map(hex).collect();
        for e in std::fs::read_dir(&base)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if !keep.contains(&name) && e.file_type()?.is_dir() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        Ok(())
    }
}

/// A reader pinned to one manifest generation.
pub struct Snapshot {
    pub generation: u64,
    pub kind: IndexKind,
    /// Ords committed at this generation. A document whose ord is not here is invisible.
    pub indexed: RoaringBitmap,
    pub segments: Vec<OpenSegment>,
}

impl Snapshot {
    pub fn schema(&self) -> Schema {
        self.kind.schema()
    }

    /// Ords of the documents matching `q` in any segment, restricted to `indexed`.
    pub fn ords_for_query(&self, q: &dyn Query) -> Result<RoaringBitmap> {
        let mut out = RoaringBitmap::new();
        for seg in &self.segments {
            let searcher = seg.reader.searcher();
            let hits = searcher.search(q, &DocSetCollector)?;
            for addr in hits {
                let sr = searcher.segment_reader(addr.segment_ord);
                let col = sr.fast_fields().u64("ord")?;
                if let Some(v) = col.first(addr.doc_id)
                    && let Ok(o) = u32::try_from(v)
                    && self.indexed.contains(o)
                {
                    out.insert(o);
                }
            }
        }
        Ok(out)
    }

    /// Every visible ord.
    pub fn visible_ords(&self) -> Result<RoaringBitmap> {
        let mut out = RoaringBitmap::new();
        for seg in &self.segments {
            out |= segment_ords(&seg.index)?;
        }
        out &= &self.indexed;
        Ok(out)
    }

    pub fn doc_count(&self) -> u64 {
        self.segments.iter().map(|s| u64::from(s.docs)).sum()
    }
}
