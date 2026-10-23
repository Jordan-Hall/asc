//! The on-disk layout of the local index, `$ASC_HOME/ci/` (SPEC §23.2.5):
//!
//! ```text
//! ci/<repo-id>/repo.json        name and id of the repository
//!              state.json       FileId -> ord table and the segment list
//!              rev/<changeset>.json   one revision: its paths, its segments, its coverage
//!              seg/<hash>.pack  segment packs (`asc-codeintel` pack format, hash-addressed)
//!              open/<hash>/     unpacked packs a reader mmaps (a cache; rebuilt on demand)
//!              work/            scratch space of the helper's writer
//!              alias/<git-oid>  the changeset (hex) whose git commit that is
//!              lock             the helper's lock
//! ```
//!
//! Nothing here trusts the disk: directories must be real directories (no link, no junction),
//! JSON is read through a hard size cap and validated, and an id is only ever used as a path
//! component after it was parsed as lower-case hex or a UUID.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{LocalError, Result, hex, unhex32};

/// Largest JSON file read.
pub const MAX_JSON: u64 = 128 << 20;
/// Most files one revision records.
pub const MAX_REV_FILES: usize = 400_000;
/// Most `FileId`s the ord table tracks.
pub const MAX_TRACKED: usize = 1_500_000;
/// Longest path recorded.
pub const MAX_PATH: usize = 4096;
/// Largest segment pack file read.
pub const MAX_PACK: u64 = 256 << 20;
/// A lock older than this is stale.
pub const LOCK_STALE: Duration = Duration::from_secs(600);

const STATE_VERSION: u32 = 1;

fn corrupt(what: &'static str) -> LocalError {
    LocalError::Corrupt(what)
}

/// True for a real directory: not a symlink, not a junction.
pub fn plain_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.is_dir() && !asc_wc::safe_path::is_link(&m))
        .unwrap_or(false)
}

/// Creates `p` when absent; refuses a link or a file.
pub fn ensure_dir(p: &Path) -> Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(_) if plain_dir(p) => Ok(()),
        Ok(_) => Err(LocalError::Other(format!(
            "{} is a link or not a directory",
            p.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            if plain_dir(p) {
                Ok(())
            } else {
                Err(LocalError::Other(format!(
                    "{} is not a directory",
                    p.display()
                )))
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Reads a regular file (never a link) of at most `max` bytes.
pub fn read_capped(p: &Path, max: u64) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(p)?;
    if !meta.is_file() || asc_wc::safe_path::is_link(&meta) || meta.len() > max {
        return Err(corrupt("not a regular file within its size limit"));
    }
    let mut out = Vec::with_capacity(meta.len() as usize);
    std::fs::File::open(p)?
        .take(max + 1)
        .read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(corrupt("file grew past its size limit"));
    }
    Ok(out)
}

/// Writes `data` to `p` through a sibling temporary file and a rename.
pub fn write_atomic(p: &Path, data: &[u8]) -> Result<()> {
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| corrupt("path"))?;
    let tmp = p.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(p: &Path) -> Result<T> {
    serde_json::from_slice(&read_capped(p, MAX_JSON)?).map_err(|_| corrupt("json"))
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let data = serde_json::to_vec(v).map_err(|e| LocalError::Other(e.to_string()))?;
    if data.len() as u64 > MAX_JSON {
        return Err(LocalError::Other("the index table is too large".into()));
    }
    write_atomic(p, &data)
}

/// Lists a directory of the index. The index lives under `$ASC_HOME`; it is not a work tree.
fn list_dir(p: impl AsRef<Path>) -> std::io::Result<std::fs::ReadDir> {
    std::fs::read_dir(p.as_ref()) // lint:allow-worktree-fs: the index directory under $ASC_HOME
}

/// Bytes under `dir`, links not followed.
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = list_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if asc_wc::safe_path::is_link(&m) {
                continue;
            }
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total = total.saturating_add(m.len());
            }
        }
    }
    total
}

/// A repository id is a UUID: hex digits and dashes only, so it is one safe path component.
pub fn valid_repo_id(s: &str) -> bool {
    s.len() == 36
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

// ------------------------------------------------------------------------------- records

/// What the table knows about one `FileId`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRec {
    pub ord: u32,
    /// `skip::Flags` bits.
    pub flags: u8,
    pub size: u64,
    /// Language name, if the file has a grammar.
    pub lang: Option<String>,
}

/// A segment pack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegRec {
    /// Size of the stored pack.
    pub bytes: u64,
    /// Inclusive ranges of the ords the segment holds.
    pub ords: Vec<(u32, u32)>,
}

impl SegRec {
    pub fn from_bitmap(b: &RoaringBitmap) -> Self {
        let mut ords: Vec<(u32, u32)> = Vec::new();
        for o in b {
            match ords.last_mut() {
                Some(last) if last.1.checked_add(1) == Some(o) => last.1 = o,
                _ => ords.push((o, o)),
            }
        }
        Self { bytes: 0, ords }
    }

    pub fn bitmap(&self) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for &(lo, hi) in &self.ords {
            b.insert_range(lo..=hi);
        }
        b
    }
}

/// `state.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub next_ord: u32,
    /// By `FileId` hex.
    pub files: BTreeMap<String, FileRec>,
    /// By pack hash hex.
    pub segs: BTreeMap<String, SegRec>,
}

impl State {
    pub fn new() -> Self {
        Self {
            version: STATE_VERSION,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != STATE_VERSION {
            return Err(corrupt("state version"));
        }
        if self.files.len() > MAX_TRACKED {
            return Err(corrupt("too many tracked files"));
        }
        let mut seen = BTreeSet::new();
        for (id, r) in &self.files {
            if unhex32(id).is_none()
                || r.ord >= self.next_ord
                || !seen.insert(r.ord)
                || skip_flags(r.flags).is_none()
            {
                return Err(corrupt("file record"));
            }
        }
        for (h, s) in &self.segs {
            if unhex32(h).is_none() || s.ords.len() > MAX_TRACKED {
                return Err(corrupt("segment record"));
            }
            for &(lo, hi) in &s.ords {
                if lo > hi || hi >= self.next_ord {
                    return Err(corrupt("segment ords"));
                }
            }
        }
        Ok(())
    }

    /// Ords held by some segment.
    pub fn indexed(&self) -> RoaringBitmap {
        let mut b = RoaringBitmap::new();
        for s in self.segs.values() {
            b |= s.bitmap();
        }
        b
    }

    /// The `FileId` hex of every ord.
    pub fn by_ord(&self) -> BTreeMap<u32, &str> {
        self.files
            .iter()
            .map(|(id, r)| (r.ord, id.as_str()))
            .collect()
    }
}

pub fn skip_flags(bits: u8) -> Option<asc_codeintel::skip::Flags> {
    asc_codeintel::skip::Flags::from_bits(bits)
}

/// One revision of the index.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevRec {
    /// Changeset id hex.
    pub changeset: String,
    /// Files of the revision's tree (symlinks and gitlinks are not files).
    pub total: u64,
    /// Files whose bytes were not available (or past a limit): the coverage gap.
    pub missing: u64,
    /// Files the skip rules keep out of the index (binary, large, minified, ...).
    pub skipped: u64,
    /// `(path, ord)` of every file that has a record.
    pub files: Vec<(String, u32)>,
    /// Segment hashes the revision reads.
    pub segs: Vec<String>,
}

impl RevRec {
    /// Share of the revision's files that the index covers, in `0.0..=1.0`.
    pub fn coverage(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        let have = self.total.saturating_sub(self.missing);
        (have as f64 / self.total as f64) as f32
    }

    fn validate(&self, st: &State) -> Result<()> {
        if unhex32(&self.changeset).is_none()
            || self.files.len() > MAX_REV_FILES
            || self.missing > self.total
            || self.skipped > self.total
        {
            return Err(corrupt("revision record"));
        }
        for (p, o) in &self.files {
            if p.is_empty() || p.len() > MAX_PATH || p.contains('\0') || *o >= st.next_ord {
                return Err(corrupt("revision path"));
            }
        }
        if self.segs.iter().any(|s| !st.segs.contains_key(s)) {
            return Err(corrupt("revision segment"));
        }
        Ok(())
    }
}

/// `repo.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMeta {
    pub id: String,
    pub name: String,
}

// -------------------------------------------------------------------------------- stores

/// `$ASC_HOME/ci`.
#[derive(Clone, Debug)]
pub struct CiRoot {
    dir: PathBuf,
}

impl CiRoot {
    pub fn new(home: &Path) -> Self {
        Self {
            dir: home.join("ci"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The store of `id`, created when absent.
    pub fn repo(&self, id: &str) -> Result<RepoStore> {
        if !valid_repo_id(id) {
            return Err(corrupt("repository id"));
        }
        ensure_dir(&self.dir)?;
        let store = RepoStore {
            dir: self.dir.join(id),
            id: id.to_owned(),
        };
        ensure_dir(&store.dir)?;
        for sub in ["rev", "seg", "open", "work", "alias"] {
            ensure_dir(&store.dir.join(sub))?;
        }
        Ok(store)
    }

    /// Every repository store that exists (nothing is created).
    pub fn repos(&self) -> Vec<RepoStore> {
        if !plain_dir(&self.dir) {
            return Vec::new();
        }
        let Ok(rd) = list_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<RepoStore> = rd
            .flatten()
            .filter_map(|e| {
                let id = e.file_name().into_string().ok()?;
                (valid_repo_id(&id) && plain_dir(&e.path()))
                    .then(|| RepoStore { dir: e.path(), id })
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Total size of the local index.
    pub fn size(&self) -> u64 {
        dir_size(&self.dir)
    }
}

/// One repository's directory.
#[derive(Clone, Debug)]
pub struct RepoStore {
    dir: PathBuf,
    id: String,
}

/// A revision file on disk.
#[derive(Clone, Debug)]
pub struct RevFile {
    pub changeset: String,
    pub used: SystemTime,
    pub bytes: u64,
}

impl RepoStore {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Scratch space of the helper's tantivy writer.
    pub fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    pub fn meta(&self) -> Option<RepoMeta> {
        let m: RepoMeta = read_json(&self.dir.join("repo.json")).ok()?;
        (m.id == self.id && m.name.len() <= 4096 && !m.name.contains('\0')).then_some(m)
    }

    pub fn save_meta(&self, name: &str) -> Result<()> {
        write_json(
            &self.dir.join("repo.json"),
            &RepoMeta {
                id: self.id.clone(),
                name: name.to_owned(),
            },
        )
    }

    /// The state; a missing file is an empty state, a damaged one an error.
    pub fn state(&self) -> Result<State> {
        let p = self.dir.join("state.json");
        if std::fs::symlink_metadata(&p).is_err() {
            return Ok(State::new());
        }
        let st: State = read_json(&p)?;
        st.validate()?;
        Ok(st)
    }

    pub fn save_state(&self, st: &State) -> Result<()> {
        write_json(&self.dir.join("state.json"), st)
    }

    fn rev_path(&self, changeset: &str) -> Result<PathBuf> {
        if unhex32(changeset).is_none() {
            return Err(corrupt("changeset id"));
        }
        Ok(self.dir.join("rev").join(format!("{changeset}.json")))
    }

    pub fn has_rev(&self, changeset: &str) -> bool {
        self.rev_path(changeset)
            .is_ok_and(|p| std::fs::symlink_metadata(p).is_ok())
    }

    /// The record of a revision, validated against `st`. `None` when absent or damaged.
    pub fn rev(&self, changeset: &str, st: &State) -> Option<RevRec> {
        let r: RevRec = read_json(&self.rev_path(changeset).ok()?).ok()?;
        (r.changeset == changeset && r.validate(st).is_ok()).then_some(r)
    }

    pub fn save_rev(&self, r: &RevRec) -> Result<()> {
        write_json(&self.rev_path(&r.changeset)?, r)
    }

    /// Marks the revision as used now (the LRU order of eviction).
    pub fn touch_rev(&self, changeset: &str) {
        if let Ok(p) = self.rev_path(changeset)
            && let Ok(f) = std::fs::OpenOptions::new().write(true).open(p)
        {
            let _ = f.set_modified(SystemTime::now());
        }
    }

    /// Revision files with their last use.
    pub fn revs(&self) -> Vec<RevFile> {
        let Ok(rd) = list_dir(self.dir.join("rev")) else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let changeset = name.strip_suffix(".json")?.to_owned();
                unhex32(&changeset)?;
                let m = std::fs::symlink_metadata(e.path()).ok()?;
                (m.is_file() && !asc_wc::safe_path::is_link(&m)).then(|| RevFile {
                    changeset,
                    used: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    bytes: m.len(),
                })
            })
            .collect()
    }

    pub fn remove_rev(&self, changeset: &str) {
        if let Ok(p) = self.rev_path(changeset) {
            let _ = std::fs::remove_file(p);
        }
        if let Ok(rd) = list_dir(self.dir.join("alias")) {
            for e in rd.flatten() {
                if read_capped(&e.path(), 128).is_ok_and(|b| b == changeset.as_bytes()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    fn alias_path(&self, git: &str) -> Result<PathBuf> {
        if git.len() != 40
            || !git
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(corrupt("git commit id"));
        }
        Ok(self.dir.join("alias").join(git))
    }

    /// Records that the git commit `git` (hex) is the changeset `changeset` (hex), so a query
    /// that names `HEAD` by its git id finds the revision.
    pub fn save_alias(&self, git: &str, changeset: &str) -> Result<()> {
        if unhex32(changeset).is_none() {
            return Err(corrupt("changeset id"));
        }
        write_atomic(&self.alias_path(git)?, changeset.as_bytes())
    }

    /// The changeset (hex) of the git commit `git`, when the index holds that revision.
    pub fn resolve_alias(&self, git: &str) -> Option<String> {
        let body = read_capped(&self.alias_path(git).ok()?, 128).ok()?;
        let cs = String::from_utf8(body).ok()?;
        (unhex32(&cs).is_some() && self.has_rev(&cs)).then_some(cs)
    }

    fn seg_path(&self, hash: &str) -> Result<PathBuf> {
        if unhex32(hash).is_none() {
            return Err(corrupt("segment hash"));
        }
        Ok(self.dir.join("seg").join(format!("{hash}.pack")))
    }

    pub fn write_seg(&self, hash: &[u8; 32], stored: &[u8]) -> Result<()> {
        write_atomic(&self.seg_path(&hex(hash))?, stored)
    }

    pub fn read_seg(&self, hash: &str) -> Result<Vec<u8>> {
        read_capped(&self.seg_path(hash)?, MAX_PACK)
    }

    pub fn open_path(&self, hash: &str) -> Result<PathBuf> {
        if unhex32(hash).is_none() {
            return Err(corrupt("segment hash"));
        }
        Ok(self.dir.join("open").join(hash))
    }

    /// Deletes the packs and unpacked copies that `keep` does not name.
    pub fn gc_segments(&self, keep: &BTreeSet<String>) {
        for sub in ["seg", "open"] {
            let Ok(rd) = list_dir(self.dir.join(sub)) else {
                continue;
            };
            for e in rd.flatten() {
                let Ok(name) = e.file_name().into_string() else {
                    continue;
                };
                let stem = name.strip_suffix(".pack").unwrap_or(&name);
                if keep.contains(stem) {
                    continue;
                }
                let p = e.path();
                match std::fs::symlink_metadata(&p) {
                    Ok(m) if m.is_dir() && !asc_wc::safe_path::is_link(&m) => {
                        let _ = std::fs::remove_dir_all(&p);
                    }
                    Ok(_) => {
                        let _ = std::fs::remove_file(&p);
                    }
                    Err(_) => {}
                }
            }
        }
    }

    /// Takes the helper lock; `None` when another live helper holds it.
    pub fn lock(&self) -> Option<Lock> {
        let p = self.dir.join("lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
            {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    return Some(Lock { path: p });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::symlink_metadata(&p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_none_or(|age| age > LOCK_STALE);
                    if !stale {
                        return None;
                    }
                    let _ = std::fs::remove_file(&p);
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// Whether a helper holds a fresh lock.
    pub fn locked(&self) -> bool {
        std::fs::symlink_metadata(self.dir.join("lock"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age <= LOCK_STALE)
    }
}

/// The helper lock; released on drop.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Refreshes the lock so a long run is not taken for a dead one.
    pub fn refresh(&self) {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&self.path) {
            let _ = f.set_modified(SystemTime::now());
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------- the size cap

/// What an enforcement pass must not remove.
#[derive(Default)]
pub struct Protect {
    /// `(repo id, changeset)` being built or just built.
    pub rev: Option<(String, String)>,
    /// Segment hashes written for it that no revision names yet.
    pub segs: BTreeSet<String>,
    /// Repositories whose lock this process holds.
    pub mine: BTreeSet<String>,
}

/// What an enforcement pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Enforced {
    pub evicted: Vec<(String, String)>,
    /// The protected revision alone did not fit and was dropped.
    pub dropped_protected: bool,
    pub size: u64,
}

/// Brings `ci/` under `cap` bytes: the least recently used revision goes first, then the
/// segments no revision names, then the file records no revision or segment names. The
/// protected revision goes last, and only when nothing else is left to remove.
pub fn enforce_cap(ci: &CiRoot, cap: u64, protect: &Protect) -> Enforced {
    let mut out = Enforced::default();
    let repos = ci.repos();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let is_protected = |repo: &str, cs: &str| {
        protect
            .rev
            .as_ref()
            .is_some_and(|(r, c)| r == repo && c == cs)
    };
    // Unreferenced segments may be what is over the cap.
    for r in &repos {
        touched.insert(r.id().to_owned());
    }
    collect(&repos, &touched, protect);
    touched.clear();
    loop {
        out.size = ci.size();
        if out.size <= cap {
            break;
        }
        let mut all: Vec<(SystemTime, usize, String)> = Vec::new();
        for (i, r) in repos.iter().enumerate() {
            for f in r.revs() {
                if !is_protected(r.id(), &f.changeset) {
                    all.push((f.used, i, f.changeset));
                }
            }
        }
        all.sort();
        if let Some((_, i, cs)) = all.into_iter().next() {
            repos[i].remove_rev(&cs);
            out.evicted.push((repos[i].id().to_owned(), cs));
            touched.insert(repos[i].id().to_owned());
        } else if let Some((rid, cs)) = &protect.rev
            && let Some(r) = repos.iter().find(|r| r.id() == rid)
            && r.has_rev(cs)
        {
            r.remove_rev(cs);
            out.dropped_protected = true;
            touched.insert(rid.clone());
        } else {
            break;
        }
        collect(&repos, &touched, protect);
    }
    out.size = ci.size();
    out
}

/// Drops the segments and file records that no remaining revision of the touched repositories
/// names.
fn collect(repos: &[RepoStore], touched: &BTreeSet<String>, protect: &Protect) {
    for r in repos.iter().filter(|r| touched.contains(r.id())) {
        // A repository that another live helper is building holds segments no revision names
        // yet; they are not garbage.
        let ours = protect.rev.as_ref().is_some_and(|(id, _)| id == r.id());
        if r.locked() && !ours && !protect.mine.contains(r.id()) {
            continue;
        }
        // While the protected revision is still being built, the records of the files it will
        // name are not yet referenced by anything.
        let building = protect
            .rev
            .as_ref()
            .is_some_and(|(id, cs)| id == r.id() && !r.has_rev(cs));
        let Ok(mut st) = r.state() else { continue };
        let mut segs: BTreeSet<String> = protect.segs.clone();
        let mut ords: BTreeSet<u32> = BTreeSet::new();
        for f in r.revs() {
            if let Some(rev) = r.rev(&f.changeset, &st) {
                segs.extend(rev.segs.iter().cloned());
                ords.extend(rev.files.iter().map(|(_, o)| *o));
            }
        }
        st.segs.retain(|h, _| segs.contains(h));
        for s in st.segs.values() {
            ords.extend(s.bitmap());
        }
        if !building {
            st.files.retain(|_, rec| ords.contains(&rec.ord));
        }
        let _ = r.save_state(&st);
        r.gc_segments(&segs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "00000000-0000-4000-8000-000000000001";

    fn rev(cs: u8, segs: &[&str], ords: &[u32]) -> RevRec {
        RevRec {
            changeset: hex(&[cs; 32]),
            total: ords.len() as u64,
            files: ords.iter().map(|o| (format!("f{o}.rs"), *o)).collect(),
            segs: segs.iter().map(|s| (*s).to_owned()).collect(),
            ..RevRec::default()
        }
    }

    fn state(n: u32, segs: &[(&str, u64, (u32, u32))]) -> State {
        let mut st = State::new();
        st.next_ord = n;
        for o in 0..n {
            st.files.insert(
                hex(&[o as u8 + 1; 32]),
                FileRec {
                    ord: o,
                    flags: 0,
                    size: 1,
                    lang: None,
                },
            );
        }
        for (h, bytes, r) in segs {
            st.segs.insert(
                (*h).to_owned(),
                SegRec {
                    bytes: *bytes,
                    ords: vec![*r],
                },
            );
        }
        st
    }

    #[test]
    fn repo_ids_are_one_safe_component() {
        let d = tempfile::tempdir().unwrap();
        let ci = CiRoot::new(d.path());
        assert!(ci.repo(ID).is_ok());
        for bad in [
            "",
            "..",
            "../x",
            "a/b",
            "00000000-0000-4000-8000-00000000000A",
            "not-a-uuid",
        ] {
            assert!(ci.repo(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn state_round_trips_and_a_corrupt_state_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let r = CiRoot::new(d.path()).repo(ID).unwrap();
        assert_eq!(r.state().unwrap(), State::new());
        let st = state(3, &[(&hex(&[9; 32]), 10, (0, 2))]);
        r.save_state(&st).unwrap();
        assert_eq!(r.state().unwrap(), st);
        let mut bad = st.clone();
        bad.next_ord = 1;
        r.save_state(&bad).unwrap();
        assert!(r.state().is_err(), "ords past next_ord");
        std::fs::write(r.path().join("state.json"), b"{not json").unwrap();
        assert!(r.state().is_err());
    }

    #[test]
    fn an_oversized_json_file_is_not_read() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big.json");
        std::fs::write(&p, vec![b' '; 2048]).unwrap();
        assert!(read_capped(&p, 1024).is_err());
        assert!(read_capped(&p, 4096).is_ok());
    }

    #[test]
    fn a_revision_naming_an_unknown_segment_or_ord_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let r = CiRoot::new(d.path()).repo(ID).unwrap();
        let st = state(2, &[]);
        r.save_state(&st).unwrap();
        let bad = rev(1, &[&hex(&[5; 32])], &[0]);
        r.save_rev(&bad).unwrap();
        assert!(r.rev(&bad.changeset, &st).is_none());
        let bad = rev(2, &[], &[7]);
        r.save_rev(&bad).unwrap();
        assert!(r.rev(&bad.changeset, &st).is_none());
        let ok = rev(3, &[], &[0, 1]);
        r.save_rev(&ok).unwrap();
        assert_eq!(r.rev(&ok.changeset, &st), Some(ok));
    }

    #[test]
    fn coverage_counts_missing_files() {
        let mut r = rev(1, &[], &[0, 1, 2, 3]);
        assert_eq!(r.coverage(), 1.0);
        r.missing = 1;
        assert!((r.coverage() - 0.75).abs() < 1e-6);
        r.total = 0;
        r.missing = 0;
        assert_eq!(r.coverage(), 1.0);
    }

    #[test]
    fn cap_evicts_least_recently_used_revision_first() {
        let d = tempfile::tempdir().unwrap();
        let ci = CiRoot::new(d.path());
        let r = ci.repo(ID).unwrap();
        let (h1, h2) = (hex(&[1; 32]), hex(&[2; 32]));
        r.write_seg(&[1; 32], &vec![0u8; 4000]).unwrap();
        r.write_seg(&[2; 32], &vec![0u8; 4000]).unwrap();
        r.save_state(&state(2, &[(&h1, 4000, (0, 0)), (&h2, 4000, (1, 1))]))
            .unwrap();
        let (old, new) = (rev(1, &[&h1], &[0]), rev(2, &[&h2], &[1]));
        r.save_rev(&old).unwrap();
        r.save_rev(&new).unwrap();
        let past = SystemTime::now() - Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(r.path().join("rev").join(format!("{}.json", old.changeset)))
            .unwrap()
            .set_modified(past)
            .unwrap();
        let before = ci.size();
        let out = enforce_cap(&ci, before - 3000, &Protect::default());
        assert_eq!(out.evicted, [(ID.to_owned(), old.changeset.clone())]);
        assert!(!r.has_rev(&old.changeset));
        assert!(r.has_rev(&new.changeset));
        let st = r.state().unwrap();
        assert!(!st.segs.contains_key(&h1), "the unreferenced segment goes");
        assert!(st.segs.contains_key(&h2));
        assert!(ci.size() <= before - 3000);
    }

    #[test]
    fn the_protected_revision_goes_last() {
        let d = tempfile::tempdir().unwrap();
        let ci = CiRoot::new(d.path());
        let r = ci.repo(ID).unwrap();
        let h = hex(&[1; 32]);
        r.write_seg(&[1; 32], &vec![0u8; 5000]).unwrap();
        r.save_state(&state(1, &[(&h, 5000, (0, 0))])).unwrap();
        let only = rev(1, &[&h], &[0]);
        r.save_rev(&only).unwrap();
        let protect = Protect {
            rev: Some((ID.to_owned(), only.changeset.clone())),
            ..Protect::default()
        };
        let out = enforce_cap(&ci, 1000, &protect);
        assert!(out.dropped_protected);
        assert!(!r.has_rev(&only.changeset));
        assert!(ci.size() < 5000, "the pack went with it");
    }

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let d = tempfile::tempdir().unwrap();
        let r = CiRoot::new(d.path()).repo(ID).unwrap();
        let l = r.lock().expect("first lock");
        assert!(r.lock().is_none());
        assert!(r.locked());
        drop(l);
        assert!(!r.locked());
        assert!(r.lock().is_some());
    }

    #[test]
    fn seg_records_round_trip_ords() {
        let mut b = RoaringBitmap::new();
        b.extend([1, 2, 3, 7, 9, 10]);
        let s = SegRec::from_bitmap(&b);
        assert_eq!(s.ords, [(1, 3), (7, 7), (9, 10)]);
        assert_eq!(s.bitmap(), b);
    }
}
