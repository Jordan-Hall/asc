//! Rename detection (SPEC §4.3): exact matches by `FileId` first, then similarity among added and
//! deleted regular files, capped by `diff.renameLimit`. Similarity follows git's `diffcore-delta`
//! span hashing, so scores agree with git's `similarity index`.

use std::collections::HashMap;

use asc_core::object::Kind;

use super::{ContentSource, Result, Status, TreeChange, is_gitlink_or_link};
use crate::diff::line::is_binary;

/// Score of an identical file (git's `MAX_SCORE`).
const MAX_SCORE: u64 = 60_000;

/// Span hash table size (git's `HASHBASE`).
const HASHBASE: u32 = 107_927;

/// Rename detection settings.
#[derive(Clone, Copy, Debug)]
pub struct RenameOptions {
    /// Minimum similarity in percent (`-M<n>`, default 50).
    pub threshold: u8,
    /// `diff.renameLimit`: inexact matching is skipped when both the added and the deleted set
    /// exceed it (or their product exceeds its square).
    pub limit: usize,
    /// Files above this size are matched only exactly.
    pub max_file_size: u64,
}

impl Default for RenameOptions {
    fn default() -> Self {
        Self {
            threshold: 50,
            limit: 1000,
            max_file_size: 512 * 1024 * 1024,
        }
    }
}

/// Pairs deleted and added files in `changes` as renames. The result is sorted by path; a rename
/// sits at its new path.
pub async fn detect(
    changes: Vec<TreeChange>,
    src: &dyn ContentSource,
    opts: &RenameOptions,
) -> Result<Vec<TreeChange>> {
    let mut deleted: Vec<usize> = Vec::new();
    let mut added: Vec<usize> = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        match c.status {
            Status::Deleted => deleted.push(i),
            Status::Added => added.push(i),
            _ => {}
        }
    }
    if deleted.is_empty() || added.is_empty() {
        return Ok(changes);
    }

    let mut pairs: Vec<(usize, usize, u64)> = Vec::new(); // (deleted idx, added idx, score)
    let mut used_src = vec![false; changes.len()];
    let mut used_dst = vec![false; changes.len()];

    // Exact: equal file id and the same kind of object.
    let mut by_id: HashMap<([u8; 32], bool), Vec<usize>> = HashMap::new();
    for &d in &deleted {
        let e = changes[d]
            .old
            .as_ref()
            .expect("a deletion has an old entry");
        by_id
            .entry((e.id, is_gitlink_or_link(e.kind)))
            .or_default()
            .push(d);
    }
    for &a in &added {
        let e = changes[a]
            .new
            .as_ref()
            .expect("an addition has a new entry");
        let Some(cands) = by_id.get_mut(&(e.id, is_gitlink_or_link(e.kind))) else {
            continue;
        };
        let want = changes[a].path.file_name();
        let pick = cands
            .iter()
            .position(|&d| changes[d].path.file_name() == want)
            .or_else(|| (!cands.is_empty()).then_some(0));
        if let Some(p) = pick {
            let d = cands.remove(p);
            used_src[d] = true;
            used_dst[a] = true;
            pairs.push((d, a, MAX_SCORE));
        }
    }

    // Inexact: regular files only.
    let srcs: Vec<usize> = deleted
        .iter()
        .copied()
        .filter(|&d| !used_src[d] && inexact_candidate(&changes[d], opts))
        .collect();
    let dsts: Vec<usize> = added
        .iter()
        .copied()
        .filter(|&a| !used_dst[a] && inexact_candidate(&changes[a], opts))
        .collect();
    if !srcs.is_empty() && !dsts.is_empty() && within_limit(srcs.len(), dsts.len(), opts.limit) {
        let min_score = MAX_SCORE * u64::from(opts.threshold) / 100;
        let mut src_spans = Vec::with_capacity(srcs.len());
        for &d in &srcs {
            src_spans.push(spans(src, &changes[d]).await?);
        }
        let mut cands: Vec<(u64, bool, usize, usize)> = Vec::new(); // score, same name, dst, src
        for &a in &dsts {
            let dst = spans(src, &changes[a]).await?;
            for (k, &d) in srcs.iter().enumerate() {
                let score = similarity(&src_spans[k], &dst, min_score);
                if score >= min_score && score > 0 {
                    let same = changes[d].path.file_name() == changes[a].path.file_name();
                    cands.push((score, same, a, d));
                }
            }
        }
        cands.sort_by(|x, y| {
            y.0.cmp(&x.0)
                .then(y.1.cmp(&x.1))
                .then(x.2.cmp(&y.2))
                .then(x.3.cmp(&y.3))
        });
        for (score, _, a, d) in cands {
            if !used_src[d] && !used_dst[a] {
                used_src[d] = true;
                used_dst[a] = true;
                pairs.push((d, a, score));
            }
        }
    }

    Ok(assemble(changes, &pairs))
}

fn inexact_candidate(c: &TreeChange, opts: &RenameOptions) -> bool {
    let e = c.old.as_ref().or(c.new.as_ref());
    e.is_some_and(|e| matches!(e.kind, Kind::File | Kind::Exec) && e.size <= opts.max_file_size)
}

/// Git's `too_many_rename_candidates` rule.
fn within_limit(srcs: usize, dsts: usize, limit: usize) -> bool {
    let limit = if limit == 0 || limit > 32767 {
        32767
    } else {
        limit
    };
    (dsts <= limit || srcs <= limit) && srcs.saturating_mul(dsts) <= limit * limit
}

/// Replaces matched deletion/addition pairs by renames and restores path order.
fn assemble(changes: Vec<TreeChange>, pairs: &[(usize, usize, u64)]) -> Vec<TreeChange> {
    let mut slots: Vec<Option<TreeChange>> = changes.into_iter().map(Some).collect();
    let mut renames: Vec<TreeChange> = Vec::with_capacity(pairs.len());
    for &(d, a, score) in pairs {
        let old = slots[d].take().expect("each deletion pairs once");
        let new = slots[a].take().expect("each addition pairs once");
        renames.push(TreeChange {
            path: new.path,
            old_path: old.path,
            old: old.old,
            new: new.new,
            status: Status::Renamed((score * 100 / MAX_SCORE) as u8),
        });
    }
    let mut out: Vec<TreeChange> = slots.into_iter().flatten().chain(renames).collect();
    out.sort_by(|x, y| x.path.cmp(&y.path));
    out
}

/// Chunk-hash table of one file: hash to byte count, sorted by hash.
struct Spans {
    size: u64,
    table: Vec<(u32, u64)>,
}

async fn spans(src: &dyn ContentSource, c: &TreeChange) -> Result<Spans> {
    let e = c.old.as_ref().or(c.new.as_ref()).expect("a side exists");
    let data = src.read(e).await?;
    Ok(Spans {
        size: data.len() as u64,
        table: span_hash(&data),
    })
}

/// Hashes `data` in chunks ending at a newline or after 64 bytes, as git's `hash_chars`. CR before
/// LF is ignored in text files.
fn span_hash(data: &[u8]) -> Vec<(u32, u64)> {
    let is_text = !is_binary(data);
    let mut map: HashMap<u32, u64> = HashMap::new();
    let (mut a1, mut a2, mut n) = (0u32, 0u32, 0u64);
    for (i, &c) in data.iter().enumerate() {
        if is_text && c == b'\r' && data.get(i + 1) == Some(&b'\n') {
            continue;
        }
        let old1 = a1;
        a1 = (a1 << 7) ^ (a2 >> 25);
        a2 = (a2 << 7) ^ (old1 >> 25);
        a1 = a1.wrapping_add(u32::from(c));
        n += 1;
        if n < 64 && c != b'\n' {
            continue;
        }
        *map.entry(hash_of(a1, a2)).or_default() += n;
        (a1, a2, n) = (0, 0, 0);
    }
    if n > 0 {
        *map.entry(hash_of(a1, a2)).or_default() += n;
    }
    let mut table: Vec<_> = map.into_iter().collect();
    table.sort_unstable();
    table
}

fn hash_of(a1: u32, a2: u32) -> u32 {
    a1.wrapping_add(a2.wrapping_mul(0x61)) % HASHBASE
}

/// Similarity of `dst` to `src` on git's 0..=60000 scale; 0 when below `min_score`'s size gate.
fn similarity(src: &Spans, dst: &Spans, min_score: u64) -> u64 {
    let max = src.size.max(dst.size);
    let min = src.size.min(dst.size);
    if max == 0 {
        return MAX_SCORE;
    }
    if max * (MAX_SCORE - min_score) < (max - min) * MAX_SCORE {
        return 0;
    }
    let (mut i, mut j, mut copied) = (0, 0, 0u64);
    while i < src.table.len() && j < dst.table.len() {
        match src.table[i].0.cmp(&dst.table[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                copied += src.table[i].1.min(dst.table[j].1);
                i += 1;
                j += 1;
            }
        }
    }
    copied * MAX_SCORE / max
}
