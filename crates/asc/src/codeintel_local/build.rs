//! Building the local index of one revision (SPEC §23.2.5).
//!
//! The same `asc-codeintel` code as the server: skip rules and secret scan (`DocRow`), folded
//! trigrams, grammar tags, and a tantivy `files` segment. What differs is the budget. Files are
//! read from `store/` and `cache/` only (the caller passes the local object stack), in groups of
//! at most [`GROUP_BYTES`], analysed on [`THREADS`](super::THREADS) threads, and written through a
//! [`WRITER_HEAP`](super::WRITER_HEAP) tantivy writer into one segment per batch. A file that is
//! not available locally is counted in `missing`, which lowers `coverage`; it is never fetched.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use asc_codeintel::doc::{DocRow, Versions};
use asc_codeintel::grams::{Gram, distinct_trigrams};
use asc_codeintel::index::schema::IndexKind;
use asc_codeintel::index::segments::{FileDoc, SegmentWriter};
use asc_codeintel::lang::{self, Lang};
use asc_codeintel::secrets;
use asc_codeintel::skip::{self, Flags};
use asc_codeintel::tags::{self, Role};
use asc_core::id::FileId;
use asc_store::key::ObjKey;
use asc_store::objects::Objects;

use super::store::{
    CiRoot, FileRec, Lock, MAX_REV_FILES, MAX_TRACKED, Protect, RepoStore, RevRec, SegRec,
    enforce_cap,
};
use super::{BATCH_BYTES, BATCH_FILES, LocalError, Result, WRITER_HEAP, hex};

/// Most bytes read before a group is analysed.
pub const GROUP_BYTES: u64 = 8 << 20;
/// Most files in a group.
pub const GROUP_FILES: usize = 64;
/// Time one file's tags may take.
const TAGS_BUDGET: Duration = Duration::from_millis(500);
/// Most distinct symbol names indexed per file and role.
const MAX_NAMES: usize = 20_000;

/// One file of the revision's tree.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub id: FileId,
    pub size: u64,
}

/// What the analysis of one file found.
pub struct Analyzed {
    pub flags: Flags,
    pub lang: Option<Lang>,
    pub grams: Vec<Gram>,
    pub defs: Vec<String>,
    pub refs: Vec<String>,
    pub ident: String,
}

/// Applies the skip rules, the secret rules, and extracts grams and tags.
pub fn analyze(path: &str, bytes: &[u8]) -> Analyzed {
    let mut flags = DocRow::from_bytes(0, bytes, Versions::default()).flags;
    if secrets::path_is_secret(path) {
        flags.insert(Flags::SECRET);
    }
    let lang = lang::detect(path, &bytes[..bytes.len().min(256)]);
    let mut a = Analyzed {
        flags,
        lang,
        grams: Vec::new(),
        defs: Vec::new(),
        refs: Vec::new(),
        ident: String::new(),
    };
    if !flags.indexable() {
        return a;
    }
    match distinct_trigrams(bytes, skip::MAX_GRAMS) {
        Some(g) => a.grams = g,
        None => {
            a.flags.insert(Flags::TOO_MANY_GRAMS);
            return a;
        }
    }
    if let Some(l) = lang
        && let Ok(tags) = tags::extract_within(l, bytes, TAGS_BUDGET)
    {
        let mut defs = BTreeSet::new();
        let mut refs = BTreeSet::new();
        for t in &tags {
            match t.role {
                Role::Def if defs.len() < MAX_NAMES => {
                    defs.insert(t.name.clone());
                }
                Role::Ref if refs.len() < MAX_NAMES => {
                    refs.insert(t.name.clone());
                }
                _ => {}
            }
        }
        a.ident = defs
            .iter()
            .chain(refs.iter())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        a.defs = defs.into_iter().collect();
        a.refs = refs.into_iter().collect();
    }
    a
}

/// Analyses a group on up to `threads` threads, in order.
fn analyze_group(group: &[(String, Vec<u8>)], threads: usize) -> Vec<Analyzed> {
    let threads = threads.clamp(1, group.len().max(1));
    if threads == 1 {
        return group.iter().map(|(p, b)| analyze(p, b)).collect();
    }
    let per = group.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = group
            .chunks(per)
            .map(|part| {
                s.spawn(move || part.iter().map(|(p, b)| analyze(p, b)).collect::<Vec<_>>())
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// What one build did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Built {
    pub rev: RevRec,
    /// Files analysed by this build (the others were already in the table).
    pub analyzed: u64,
    pub segments: u32,
}

/// A build request.
pub struct Job<'a> {
    pub ci: &'a CiRoot,
    pub store: &'a RepoStore,
    pub lock: Option<&'a Lock>,
    /// The size cap (`codeintel.local.maxSize`).
    pub cap: u64,
    pub changeset: [u8; 32],
    /// The git commit of the changeset (hex), if known.
    pub git: Option<String>,
    pub entries: Vec<Entry>,
    /// Files of the tree past what the index records.
    pub extra_missing: u64,
    /// Local objects only: `store/` and `cache/`.
    pub objects: &'a dyn Objects,
    pub threads: usize,
}

fn over_cap() -> LocalError {
    LocalError::Other("the local index would exceed codeintel.local.maxSize".into())
}

/// Indexes the files of `job` that the table does not know yet, writes the revision record,
/// and keeps the index inside the cap.
pub async fn index_rev(job: Job<'_>) -> Result<Built> {
    let Job {
        ci,
        store,
        lock,
        cap,
        changeset,
        git,
        mut entries,
        extra_missing,
        objects,
        threads,
    } = job;
    let cs_hex = hex(&changeset);
    let mut st = store.state()?;
    let mut missing = extra_missing;
    if entries.len() > MAX_REV_FILES {
        missing += (entries.len() - MAX_REV_FILES) as u64;
        entries.truncate(MAX_REV_FILES);
    }
    let total = entries.len() as u64 + extra_missing;
    let indexed = st.indexed();

    // The distinct contents that need a look: unknown, or known but lost from every segment.
    let mut pending: Vec<usize> = Vec::new();
    let mut first: HashMap<FileId, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let known = st.files.get(&e.id.to_string()).is_some_and(|r| {
            !skip::Flags::from_bits(r.flags).is_some_and(Flags::indexable)
                || indexed.contains(r.ord)
        });
        if known || first.contains_key(&e.id) {
            continue;
        }
        first.insert(e.id, i);
        pending.push(i);
    }

    let mut protect = Protect {
        rev: Some((store.id().to_owned(), cs_hex.clone())),
        mine: BTreeSet::from([store.id().to_owned()]),
        ..Protect::default()
    };
    let mut lost: BTreeSet<FileId> = BTreeSet::new();
    let mut analyzed = 0u64;
    let mut segments = 0u32;
    let mut at = 0usize;
    while at < pending.len() {
        if st.files.len() >= MAX_TRACKED {
            // The table is full: the rest of the revision is not covered.
            for &i in &pending[at..] {
                lost.insert(entries[i].id);
            }
            break;
        }
        let mut writer = SegmentWriter::new(IndexKind::Files, &store.work(), WRITER_HEAP)?;
        let mut added: Vec<(String, FileRec)> = Vec::new();
        let mut batch_bytes = 0u64;
        let mut batch_files = 0usize;
        while at < pending.len() && batch_files < BATCH_FILES && batch_bytes < BATCH_BYTES {
            // One group: read, then analyse.
            let mut group: Vec<(String, Vec<u8>)> = Vec::new();
            let mut ids: Vec<usize> = Vec::new();
            let mut group_bytes = 0u64;
            while at < pending.len()
                && group.len() < GROUP_FILES
                && group_bytes < GROUP_BYTES
                && batch_files + ids.len() < BATCH_FILES
            {
                let i = pending[at];
                at += 1;
                let e = &entries[i];
                if e.size > skip::MAX_BYTES as u64 {
                    // Too large to read: the size alone decides, like the server's skip rule.
                    let ord = st.next_ord;
                    st.next_ord = ord.checked_add(1).ok_or(LocalError::Corrupt("ords"))?;
                    let mut flags = Flags::TOO_LARGE;
                    if secrets::path_is_secret(&e.path) {
                        flags.insert(Flags::SECRET);
                    }
                    added.push((
                        e.id.to_string(),
                        FileRec {
                            ord,
                            flags: flags.bits(),
                            size: e.size,
                            lang: None,
                        },
                    ));
                    continue;
                }
                let bytes = match objects.get(&ObjKey::File(e.id)).await {
                    Ok(Some(b)) if FileId::of(&b) == e.id && b.len() as u64 == e.size => b,
                    // Not on this machine, damaged, or not the content the tree names: the file
                    // is not covered, and nothing is fetched.
                    _ => {
                        lost.insert(e.id);
                        continue;
                    }
                };
                group_bytes += bytes.len() as u64;
                group.push((e.path.clone(), bytes.to_vec()));
                ids.push(i);
            }
            batch_bytes += group_bytes;
            batch_files += group.len();
            let results = analyze_group(&group, threads);
            for ((i, (_, bytes)), a) in ids.iter().zip(&group).zip(results) {
                let e = &entries[*i];
                let ord = st.next_ord;
                st.next_ord = ord.checked_add(1).ok_or(LocalError::Corrupt("ords"))?;
                if a.flags.indexable() {
                    let defs: Vec<&str> = a.defs.iter().map(String::as_str).collect();
                    let refs: Vec<&str> = a.refs.iter().map(String::as_str).collect();
                    let langs: Vec<Lang> = a.lang.into_iter().collect();
                    writer.add_file(&FileDoc {
                        ord,
                        grams: &a.grams,
                        sym_def: &defs,
                        sym_ref: &refs,
                        ident: &a.ident,
                        doc: "",
                        path: "",
                        langs: &langs,
                    })?;
                }
                analyzed += 1;
                added.push((
                    e.id.to_string(),
                    FileRec {
                        ord,
                        flags: a.flags.bits(),
                        size: bytes.len() as u64,
                        lang: a.lang.map(|l| l.name().to_owned()),
                    },
                ));
            }
        }
        let pack = writer.finish()?;
        if let Some(p) = &pack {
            store.write_seg(&p.hash, &p.stored)?;
            let h = hex(&p.hash);
            let mut rec = SegRec::from_bitmap(&p.ords);
            rec.bytes = p.stored.len() as u64;
            st.segs.insert(h.clone(), rec);
            protect.segs.insert(h);
            segments += 1;
        }
        st.files.extend(added);
        store.save_state(&st)?;
        if let Some(l) = lock {
            l.refresh();
        }
        // The size is checked after every segment, so the overshoot is one segment at most.
        let r = enforce_cap(ci, cap, &protect);
        if r.size > cap {
            enforce_cap(
                ci,
                cap,
                &Protect {
                    mine: BTreeSet::from([store.id().to_owned()]),
                    ..Protect::default()
                },
            );
            return Err(over_cap());
        }
    }

    // The revision record. A file is covered when the table holds it; a file the table does
    // not hold is missing.
    let st = store.state()?;
    let indexed = st.indexed();
    let mut rev = RevRec {
        changeset: cs_hex.clone(),
        total,
        missing,
        ..RevRec::default()
    };
    let mut ords = roaring::RoaringBitmap::new();
    let mut skipped_ids: BTreeSet<FileId> = BTreeSet::new();
    for e in &entries {
        let Some(r) = st.files.get(&e.id.to_string()) else {
            rev.missing += 1;
            continue;
        };
        let covered =
            Flags::from_bits(r.flags).is_some_and(|f| !f.indexable()) || indexed.contains(r.ord);
        if !covered {
            rev.missing += 1;
            continue;
        }
        if Flags::from_bits(r.flags).is_some_and(|f| !f.indexable()) {
            skipped_ids.insert(e.id);
            rev.skipped += 1;
        } else {
            ords.insert(r.ord);
        }
        rev.files.push((e.path.clone(), r.ord));
    }
    for (h, s) in &st.segs {
        if !(s.bitmap() & &ords).is_empty() {
            rev.segs.push(h.clone());
        }
    }
    debug_assert!(lost.is_empty() || rev.missing >= lost.len() as u64);
    store.save_rev(&rev)?;
    if let Some(g) = &git {
        store.save_alias(g, &cs_hex)?;
    }
    let r = enforce_cap(
        ci,
        cap,
        &Protect {
            rev: Some((store.id().to_owned(), cs_hex)),
            mine: BTreeSet::from([store.id().to_owned()]),
            ..Protect::default()
        },
    );
    if r.dropped_protected || r.size > cap {
        return Err(over_cap());
    }
    Ok(Built {
        rev,
        analyzed,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::super::THREADS;
    use super::*;

    #[test]
    fn analysis_extracts_grams_symbols_and_flags() {
        let a = analyze(
            "src/lib.rs",
            b"pub fn parse_widget() {}\nfn run() { parse_widget(); }\n",
        );
        assert!(a.flags.indexable());
        assert_eq!(a.lang, Some(Lang::Rust));
        assert!(!a.grams.is_empty());
        assert!(a.defs.contains(&"parse_widget".to_owned()));
        assert!(a.refs.contains(&"parse_widget".to_owned()));
    }

    #[test]
    fn binary_and_secret_files_are_flagged() {
        let a = analyze("a.bin", &[0u8, 1, 2, 3, 0, 0, 9]);
        assert!(!a.flags.indexable());
        assert!(a.grams.is_empty());
        let s = analyze(".env", b"TOKEN=abc\n");
        assert!(s.flags.contains(Flags::SECRET));
        assert!(
            s.flags.indexable(),
            "a secret still answers exact search, redacted"
        );
    }

    #[test]
    fn group_analysis_keeps_order_across_threads() {
        let group: Vec<(String, Vec<u8>)> = (0..9)
            .map(|i| {
                (
                    format!("f{i}.rs"),
                    format!("fn name_{i}() {{}}\n").into_bytes(),
                )
            })
            .collect();
        let out = analyze_group(&group, THREADS);
        assert_eq!(out.len(), 9);
        for (i, a) in out.iter().enumerate() {
            assert!(a.defs.contains(&format!("name_{i}")), "{i}");
        }
    }
}
