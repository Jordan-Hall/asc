//! The blame result for the CLI: the engine's `blame.v1` for committed content, plus the cases
//! the engine does not serve: a dirty working file, `-w` and `--reverse`.
//!
//! Every walk is bounded: `-w` refinement and the `--reverse` chain have step budgets, files are
//! read through the engine (hash verified, size bounded), and the working file through
//! `SafeRoot` with a size limit.

use std::collections::HashMap;
use std::io::Read;

use bytes::Bytes;
use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::RepoPath;
use asc_engine::blame::{Blame, BlameApi};
use asc_engine::diff::line::{self, Algorithm};
use asc_wc::SafeRoot;

use crate::cli::common::{CliError, Result, WsCtx};

/// Most pieces `-w` refinement splits and moves; past it the rest keep their origin.
const MAX_REFINE_STEPS: usize = 200_000;
/// Longest first-parent chain `--reverse` walks.
const MAX_CHAIN: usize = 1_000_000;
/// Largest working file blamed.
const MAX_WORK_FILE: u64 = 256 << 20;

pub fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// Where the lines of a piece come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    Commit(ChangesetId),
    /// The line is in the working file only.
    Dirty,
}

/// A run of consecutive final lines with one origin. Lines are zero-based.
#[derive(Clone, Debug)]
pub struct Piece {
    pub start: u32,
    pub len: u32,
    pub origin: Origin,
    /// Line number of `start` in the origin's version of the file.
    pub orig_line: u32,
    pub orig_path: RepoPath,
}

impl Piece {
    pub fn end(&self) -> u32 {
        self.start + self.len
    }

    /// The part inside `start..end`, if any.
    pub fn clip(&self, start: u32, end: u32) -> Option<Piece> {
        let (s, e) = (self.start.max(start), self.end().min(end));
        (s < e).then(|| Piece {
            start: s,
            len: e - s,
            orig_line: self.orig_line + (s - self.start),
            ..self.clone()
        })
    }
}

/// Facts about a working file blamed with uncommitted lines.
pub struct Dirty {
    /// Modification time of the file, seconds since the epoch.
    pub mtime: i64,
    /// The commit the committed lines were blamed at.
    pub head: ChangesetId,
}

pub struct Blamed {
    pub text: Vec<u8>,
    pub pieces: Vec<Piece>,
    /// `--reverse`: the starting commit, which is printed as a boundary.
    pub reverse_start: Option<ChangesetId>,
    pub dirty: Option<Dirty>,
}

impl Blamed {
    pub fn lines(&self) -> u32 {
        self.pieces.last().map_or(0, Piece::end)
    }
}

/// Builds pieces line by line, joining contiguous lines of one origin.
#[derive(Default)]
struct Pieces(Vec<Piece>);

impl Pieces {
    fn line(&mut self, line: u32, origin: Origin, orig_line: u32, path: &RepoPath) {
        if let Some(l) = self.0.last_mut()
            && l.end() == line
            && l.origin == origin
            && l.orig_path == *path
            && l.orig_line + l.len == orig_line
        {
            l.len += 1;
            return;
        }
        self.0.push(Piece { start: line, len: 1, origin, orig_line, orig_path: path.clone() });
    }

    fn piece(&mut self, p: Piece) {
        if let Some(l) = self.0.last_mut()
            && l.end() == p.start
            && l.origin == p.origin
            && l.orig_path == p.orig_path
            && l.orig_line + l.len == p.orig_line
        {
            l.len += p.len;
            return;
        }
        self.0.push(p);
    }
}

fn from_blame(b: &Blame) -> Vec<Piece> {
    b.runs()
        .iter()
        .map(|r| Piece {
            start: r.start,
            len: r.len,
            origin: Origin::Commit(r.origin),
            orig_line: r.orig_line,
            orig_path: r.orig_path.clone(),
        })
        .collect()
}

fn is_file(e: &Entry) -> bool {
    !matches!(e.kind, Kind::Dir | Kind::Gitlink)
}

pub async fn entry_at(ws: &WsCtx, cs: ChangesetId, path: &RepoPath) -> Result<Option<Entry>> {
    let c = ws.engine.get_changeset(cs).await?;
    Ok(ws.engine.lookup(c.root, path).await?.filter(is_file))
}

async fn content(ws: &WsCtx, e: &Entry) -> Result<Bytes> {
    ws.engine.read_file(FileId::from_bytes(e.id), e.size).await.map_err(fatal)
}

/// git quotes the path when no revision was typed (`'f' in HEAD`) and not when one was.
pub fn no_such_path(path: &RepoPath, rev: Option<&str>) -> CliError {
    let name = String::from_utf8_lossy(path.as_bytes());
    CliError::Fatal(match rev {
        Some(r) => format!("no such path {name} in {r}"),
        None => format!("no such path '{name}' in HEAD"),
    })
}

/// Every line with its whitespace removed, one per line of `data`.
fn squeeze(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 1);
    for l in line::split_lines(data) {
        out.extend(l.iter().copied().filter(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)));
        out.push(b'\n');
    }
    out
}

/// For each line of `new`, the line of `old` it is unchanged from. With `ignore_ws`, lines that
/// differ only in whitespace count as unchanged (git's `-w`).
fn matching(old: &[u8], new: &[u8], ignore_ws: bool) -> Vec<Option<u32>> {
    let (o, n);
    let (old, new) = if ignore_ws {
        o = squeeze(old);
        n = squeeze(new);
        (&o[..], &n[..])
    } else {
        (old, new)
    };
    let d = line::diff(old, new, Algorithm::Myers, false);
    let mut map = vec![None; d.new.len()];
    let (mut i, mut j) = (0u32, 0u32);
    for h in &d.hunks {
        while i < h.before.start && (j as usize) < map.len() {
            map[j as usize] = Some(i);
            i += 1;
            j += 1;
        }
        i = h.before.end;
        j = h.after.end;
    }
    while (j as usize) < map.len() && (i as usize) < d.old.len() {
        map[j as usize] = Some(i);
        i += 1;
        j += 1;
    }
    map
}

/// `path` at `cs`, blamed by the engine.
pub async fn committed(
    ws: &WsCtx,
    cs: ChangesetId,
    path: &RepoPath,
    rev: Option<&str>,
    ignore_ws: bool,
) -> Result<Blamed> {
    let entry = entry_at(ws, cs, path).await?.ok_or_else(|| no_such_path(path, rev))?;
    let text = content(ws, &entry).await?.to_vec();
    let b = ws.engine.blame(cs, path, None).await.map_err(fatal)?;
    let pieces = refine(ws, from_blame(&b), ignore_ws).await?;
    Ok(Blamed { text, pieces, reverse_start: None, dirty: None })
}

/// The working file at `abs`, if it is a regular file below `gate`'s root: its bytes and mtime.
pub fn read_work_file(gate: &SafeRoot, path: &RepoPath) -> Result<Option<(Vec<u8>, i64)>> {
    let Some(abs) = gate.resolve(path) else { return Ok(None) };
    let Ok(meta) = std::fs::symlink_metadata(&abs) else { return Ok(None) };
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > MAX_WORK_FILE {
        return Err(CliError::Fatal(format!(
            "{}: the file is larger than {} MiB",
            String::from_utf8_lossy(path.as_bytes()),
            MAX_WORK_FILE >> 20
        )));
    }
    let mut data = Vec::new();
    std::fs::File::open(&abs)?.take(MAX_WORK_FILE + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_WORK_FILE {
        return Err(CliError::Fatal("the file grew while it was read".into()));
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    Ok(Some((data, mtime)))
}

/// The working file `work` blamed against `head`: lines unchanged from HEAD keep HEAD's origin,
/// the others are `Not Committed Yet`. A file HEAD lacks (staged as new) is all uncommitted.
pub async fn working(
    ws: &WsCtx,
    head: ChangesetId,
    path: &RepoPath,
    work: (Vec<u8>, i64),
    ignore_ws: bool,
) -> Result<Blamed> {
    let (text, mtime) = work;
    let dirty = Some(Dirty { mtime, head });
    let Some(entry) = entry_at(ws, head, path).await? else {
        let n = line::split_lines(&text).len() as u32;
        let pieces = if n == 0 {
            Vec::new()
        } else {
            vec![Piece { start: 0, len: n, origin: Origin::Dirty, orig_line: 0, orig_path: path.clone() }]
        };
        return Ok(Blamed { text, pieces, reverse_start: None, dirty });
    };
    let old = content(ws, &entry).await?;
    if old[..] == text[..] {
        return committed(ws, head, path, None, ignore_ws).await;
    }
    let base = ws.engine.blame(head, path, None).await.map_err(fatal)?;
    let map = matching(&old, &text, ignore_ws);
    let mut pieces = Pieces::default();
    for (j, m) in map.iter().enumerate() {
        let j = j as u32;
        match m.and_then(|i| base.run_at(i).map(|r| (i, r))) {
            Some((i, r)) => pieces.line(j, Origin::Commit(r.origin), r.orig_line + (i - r.start), &r.orig_path),
            None => pieces.line(j, Origin::Dirty, j, path),
        }
    }
    let pieces = refine(ws, pieces.0, ignore_ws).await?;
    Ok(Blamed { text, pieces, reverse_start: None, dirty })
}

/// How the lines of one origin's file map onto its parent's.
struct Step {
    parent: ChangesetId,
    parent_path: RepoPath,
    /// For each line of the origin's file, the parent's line it is unchanged from (ignoring
    /// whitespace).
    map: Vec<Option<u32>>,
}

/// The first parent holding the file (at `path`, or at the source of a rename).
pub async fn parent_file(
    ws: &WsCtx,
    cs: ChangesetId,
    path: &RepoPath,
) -> Result<Option<(ChangesetId, RepoPath, Entry)>> {
    let c = ws.engine.get_changeset(cs).await?;
    let renamed = match c.changes.get(path) {
        Some(Change::Write { copy_from: Some(src), .. }) if matches!(c.changes.get(src), Some(Change::Delete)) => {
            Some(src.clone())
        }
        _ => None,
    };
    for (i, p) in c.parents.iter().enumerate() {
        if let Some(e) = entry_at(ws, *p, path).await? {
            return Ok(Some((*p, path.clone(), e)));
        }
        if i == 0
            && let Some(src) = &renamed
            && let Some(e) = entry_at(ws, *p, src).await?
        {
            return Ok(Some((*p, src.clone(), e)));
        }
    }
    Ok(None)
}

/// Most deleted (or added) files compared when looking for a rename.
const MAX_RENAME_CANDIDATES: usize = 256;
/// Largest file compared for a rename.
const MAX_RENAME_FILE: u64 = 8 << 20;
/// Percentage of equal bytes that makes two files one renamed file (git's default `-M50%`).
const RENAME_SCORE: u64 = 50;

/// Percentage of `new`'s lines (by bytes) that are unchanged from `old`.
fn similarity(old: &[u8], new: &[u8]) -> u64 {
    if old == new {
        return 100;
    }
    let lines = line::split_lines(new);
    let kept: usize = matching(old, new, false)
        .iter()
        .zip(&lines)
        .filter(|(m, _)| m.is_some())
        .map(|(_, l)| l.len())
        .sum();
    (kept as u64 * 100) / (old.len().max(new.len()) as u64).max(1)
}

/// Among `candidates` (paths with their entries) the file most like `data`, at least
/// [`RENAME_SCORE`] percent alike; an identical file wins at once.
async fn most_alike(ws: &WsCtx, data: &[u8], candidates: Vec<(RepoPath, Entry)>) -> Result<Option<(RepoPath, Entry)>> {
    let mut best: Option<(u64, RepoPath, Entry)> = None;
    for (path, entry) in candidates.into_iter().take(MAX_RENAME_CANDIDATES) {
        if entry.size > MAX_RENAME_FILE || !is_file(&entry) {
            continue;
        }
        let other = content(ws, &entry).await?;
        let score = similarity(&other, data);
        if score == 100 {
            return Ok(Some((path, entry)));
        }
        if score >= RENAME_SCORE && best.as_ref().is_none_or(|(b, ..)| score > *b) {
            best = Some((score, path, entry));
        }
    }
    Ok(best.map(|(_, p, e)| (p, e)))
}

/// The file `cs` deleted that `path` (added in `cs`) most likely came from, with its entry in
/// the first parent.
async fn rename_source(ws: &WsCtx, cs: ChangesetId, path: &RepoPath, data: &[u8]) -> Result<Option<(ChangesetId, RepoPath, Entry)>> {
    let c = ws.engine.get_changeset(cs).await?;
    let Some(parent) = c.parents.first().copied() else { return Ok(None) };
    let mut candidates = Vec::new();
    for (p, ch) in c.changes.as_slice() {
        if candidates.len() >= MAX_RENAME_CANDIDATES {
            break;
        }
        if matches!(ch, Change::Delete)
            && p != path
            && let Some(e) = entry_at(ws, parent, p).await?
        {
            candidates.push((p.clone(), e));
        }
    }
    Ok(most_alike(ws, data, candidates).await?.map(|(p, e)| (parent, p, e)))
}

/// The file that `cs` (the next commit) holds in place of `path`, which it deleted.
async fn rename_dest(ws: &WsCtx, cs: ChangesetId, path: &RepoPath, data: &[u8]) -> Result<Option<(RepoPath, Entry)>> {
    let c = ws.engine.get_changeset(cs).await?;
    if !matches!(c.changes.get(path), Some(Change::Delete)) {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    for (p, ch) in c.changes.as_slice() {
        if candidates.len() >= MAX_RENAME_CANDIDATES {
            break;
        }
        if let Change::Write { copy_from, .. } = ch {
            if copy_from.as_ref() == Some(path) {
                if let Some(e) = ws.engine.lookup(c.root, p).await?.filter(is_file) {
                    return Ok(Some((p.clone(), e)));
                }
                continue;
            }
            if let Some(e) = ws.engine.lookup(c.root, p).await?.filter(is_file) {
                candidates.push((p.clone(), e));
            }
        }
    }
    most_alike(ws, data, candidates).await
}

/// The parent and path `path` at `cs` was derived from (`previous` in porcelain output),
/// including a rename the parent does not record.
pub async fn previous_of(ws: &WsCtx, cs: ChangesetId, path: &RepoPath) -> Result<Option<(ChangesetId, RepoPath)>> {
    if let Some((parent, parent_path, _)) = parent_file(ws, cs, path).await? {
        return Ok(Some((parent, parent_path)));
    }
    let Some(me) = entry_at(ws, cs, path).await? else { return Ok(None) };
    if me.size > MAX_RENAME_FILE {
        return Ok(None);
    }
    let new = content(ws, &me).await?;
    Ok(rename_source(ws, cs, path, &new).await?.map(|(parent, p, _)| (parent, p)))
}

/// How `path` at `cs` maps onto its parent: through the file the parent holds (only with `-w`;
/// the engine has already placed the other lines), or through a rename the parent does not
/// record.
async fn step_of(ws: &WsCtx, cs: ChangesetId, path: &RepoPath, ignore_ws: bool) -> Result<Option<Step>> {
    let Some(me) = entry_at(ws, cs, path).await? else { return Ok(None) };
    match parent_file(ws, cs, path).await? {
        Some(_) if !ignore_ws => Ok(None),
        Some((parent, parent_path, pe)) => {
            let (old, new) = (content(ws, &pe).await?, content(ws, &me).await?);
            Ok(Some(Step { parent, parent_path, map: matching(&old, &new, true) }))
        }
        None => {
            if me.size > MAX_RENAME_FILE {
                return Ok(None);
            }
            let new = content(ws, &me).await?;
            let Some((parent, parent_path, pe)) = rename_source(ws, cs, path, &new).await? else {
                return Ok(None);
            };
            let old = content(ws, &pe).await?;
            Ok(Some(Step { parent, parent_path, map: matching(&old, &new, ignore_ws) }))
        }
    }
}

/// Moves lines back to the parent's origin where the engine could not: across a rename git
/// finds by content, and with `-w` across whitespace-only changes.
async fn refine(ws: &WsCtx, pieces: Vec<Piece>, ignore_ws: bool) -> Result<Vec<Piece>> {
    let mut cache: HashMap<(ChangesetId, RepoPath), Option<Step>> = HashMap::new();
    let mut done: Vec<Piece> = Vec::new();
    let mut work = pieces;
    let mut budget = MAX_REFINE_STEPS;
    while let Some(p) = work.pop() {
        let Origin::Commit(cs) = p.origin else {
            done.push(p);
            continue;
        };
        if budget == 0 {
            done.push(p);
            continue;
        }
        budget -= 1;
        let key = (cs, p.orig_path.clone());
        if !cache.contains_key(&key) {
            let s = step_of(ws, cs, &p.orig_path, ignore_ws).await?;
            cache.insert(key.clone(), s);
        }
        let Some(step) = cache[&key].as_ref() else {
            done.push(p);
            continue;
        };
        let at = |k: u32| step.map.get((p.orig_line + k) as usize).copied().flatten();
        let mut k = 0;
        while k < p.len {
            let mut e = k + 1;
            match at(k) {
                Some(pl) => {
                    while e < p.len && at(e) == Some(pl + (e - k)) {
                        e += 1;
                    }
                    let b = ws
                        .engine
                        .blame(step.parent, &step.parent_path, Some(pl..pl + (e - k)))
                        .await
                        .map_err(fatal)?;
                    for r in b.runs() {
                        work.push(Piece {
                            start: p.start + k + (r.start - pl),
                            len: r.len,
                            origin: Origin::Commit(r.origin),
                            orig_line: r.orig_line,
                            orig_path: r.orig_path.clone(),
                        });
                    }
                }
                None => {
                    while e < p.len && at(e).is_none() {
                        e += 1;
                    }
                    done.push(Piece {
                        start: p.start + k,
                        len: e - k,
                        orig_line: p.orig_line + k,
                        ..p.clone()
                    });
                }
            }
            k = e;
        }
    }
    done.sort_by_key(|p| p.start);
    let mut out = Pieces::default();
    for p in done {
        out.piece(p);
    }
    Ok(out.0)
}

/// `--reverse start..end`: for each line of the file at `start`, the last commit of the
/// first-parent chain up to `end` that still has it.
pub async fn reverse(
    ws: &WsCtx,
    start: ChangesetId,
    end: ChangesetId,
    path: &RepoPath,
    start_text: &str,
    ignore_ws: bool,
) -> Result<Blamed> {
    let mut chain = vec![end];
    let mut cur = end;
    while cur != start {
        if chain.len() > MAX_CHAIN {
            return Err(CliError::Fatal("asc: --reverse: the history between the revisions is too long".into()));
        }
        let c = ws.engine.get_changeset(cur).await?;
        let Some(p) = c.parents.first() else {
            return Err(CliError::Fatal(format!(
                "asc: --reverse needs {start_text} on the first-parent history of the end revision"
            )));
        };
        chain.push(*p);
        cur = *p;
    }
    chain.reverse();
    let mut entry = entry_at(ws, start, path).await?.ok_or_else(|| no_such_path(path, Some(start_text)))?;
    let text = content(ws, &entry).await?;
    let n = line::split_lines(&text).len();
    let mut data = text.clone();
    let mut cur_path = path.clone();
    let mut alive: Vec<Option<u32>> = (0..n as u32).map(Some).collect();
    let mut last: Vec<Option<(usize, u32, RepoPath)>> = vec![None; n];
    let mut alive_count = n;
    for k in 0..chain.len() - 1 {
        if alive_count == 0 {
            break;
        }
        let c = ws.engine.get_changeset(chain[k + 1]).await?;
        let mut next_path = cur_path.clone();
        let mut next = ws.engine.lookup(c.root, &next_path).await?.filter(is_file);
        if next.is_none()
            && let Some((dst, e)) = rename_dest(ws, chain[k + 1], &cur_path, &data).await?
        {
            next_path = dst;
            next = Some(e);
        }
        let Some(next) = next else {
            for (s, a) in alive.iter_mut().enumerate() {
                if let Some(l) = a.take() {
                    last[s] = Some((k, l, cur_path.clone()));
                }
            }
            break;
        };
        if next.id == entry.id && next_path == cur_path {
            entry = next;
            continue;
        }
        let new_data = content(ws, &next).await?;
        let map = matching(&data, &new_data, ignore_ws);
        let mut old_to_new: Vec<Option<u32>> = vec![None; line::split_lines(&data).len()];
        for (j, m) in map.iter().enumerate() {
            if let Some(i) = m {
                old_to_new[*i as usize] = Some(j as u32);
            }
        }
        for (s, a) in alive.iter_mut().enumerate() {
            let Some(l) = *a else { continue };
            match old_to_new.get(l as usize).copied().flatten() {
                Some(nl) => *a = Some(nl),
                None => {
                    last[s] = Some((k, l, cur_path.clone()));
                    *a = None;
                    alive_count -= 1;
                }
            }
        }
        data = new_data;
        entry = next;
        cur_path = next_path;
    }
    let end_index = chain.len() - 1;
    let mut pieces = Pieces::default();
    for s in 0..n {
        let (k, l, p) = match (alive[s], last[s].take()) {
            (Some(l), _) => (end_index, l, cur_path.clone()),
            (None, Some(x)) => x,
            (None, None) => continue,
        };
        pieces.line(s as u32, Origin::Commit(chain[k]), l, &p);
    }
    Ok(Blamed { text: text.to_vec(), pieces: pieces.0, reverse_start: Some(start), dirty: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_pairs_unchanged_lines() {
        let m = matching(b"a\nb\nc\n", b"a\nX\nb\nc\nd\n", false);
        assert_eq!(m, [Some(0), None, Some(1), Some(2), None]);
    }

    #[test]
    fn whitespace_changes_match_only_with_w() {
        let (old, new) = (&b"a\n b \nc\n"[..], &b"a\nb\nc\n"[..]);
        assert_eq!(matching(old, new, false), [Some(0), None, Some(2)]);
        assert_eq!(matching(old, new, true), [Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn a_missing_final_newline_is_a_change_unless_w() {
        assert_eq!(matching(b"a\nb", b"a\nb\n", false), [Some(0), None]);
        assert_eq!(matching(b"a\nb", b"a\nb\n", true), [Some(0), Some(1)]);
    }

    #[test]
    fn pieces_join_contiguous_lines() {
        let p = RepoPath::new("f").unwrap();
        let o = Origin::Dirty;
        let mut b = Pieces::default();
        b.line(0, o, 0, &p);
        b.line(1, o, 1, &p);
        b.line(2, o, 5, &p);
        assert_eq!(b.0.iter().map(|x| (x.start, x.len)).collect::<Vec<_>>(), [(0, 2), (2, 1)]);
        let c = b.0[0].clip(1, 5).unwrap();
        assert_eq!((c.start, c.len, c.orig_line), (1, 1, 1));
        assert!(b.0[0].clip(2, 4).is_none());
    }
}
