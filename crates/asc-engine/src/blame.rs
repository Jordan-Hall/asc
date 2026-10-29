//! Blame: the origin of every line of a file at a revision (SPEC §4.6).
//!
//! The result is the keyed derived type `blame.v1` ([`Blame`], `DerivedKeyed<Key = RepoPath>`,
//! SPEC §5). Deriving it for `(changeset, path)` recurses to the previous changeset that
//! touched the path, following renames recorded as `copy_from`. The recursion is an explicit
//! stack, so history depth never grows the call stack, and a value is released as soon as its
//! last dependent has used it.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use asc_core::id::{ChangesetId, FileId};
use asc_core::object::{Change, Entry, Kind};
use asc_core::path::{PathError, RepoPath};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use async_trait::async_trait;

use crate::derive::{self, DerivedKeyed, read_memo, write_memo};
use crate::diff::line::{self, Algorithm};
use crate::{Engine, commit, diff, tree};

/// Blame failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Derive(#[from] derive::Error),
    #[error(transparent)]
    Commit(#[from] commit::Error),
    #[error(transparent)]
    Tree(#[from] tree::Error),
    #[error(transparent)]
    Diff(#[from] diff::Error),
    #[error("{} is not a file at that revision", String::from_utf8_lossy(.0.as_bytes()))]
    NotFound(RepoPath),
    #[error("line range {}..{} is outside a file of {lines} lines", .range.start, .range.end)]
    BadRange { range: Range<u32>, lines: u32 },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A maximal run of consecutive lines that share one origin. Lines are zero-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// First line of the run in the blamed file.
    pub start: u32,
    pub len: u32,
    /// The changeset that introduced these lines.
    pub origin: ChangesetId,
    /// Line number of `start` in the origin's version of the file.
    pub orig_line: u32,
    /// The file's path in the origin changeset (differs from the blamed path after renames).
    pub orig_path: RepoPath,
}

impl Run {
    fn end(&self) -> u32 {
        self.start + self.len
    }

    /// The part of this run inside `range`, if any.
    fn clip(&self, range: &Range<u32>) -> Option<Run> {
        let (s, e) = (self.start.max(range.start), self.end().min(range.end));
        (s < e).then(|| Run {
            start: s,
            len: e - s,
            orig_line: self.orig_line + (s - self.start),
            ..self.clone()
        })
    }
}

/// Line origins of one file, as consecutive [`Run`]s covering every line (`blame.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blame {
    runs: Vec<Run>,
}

impl Blame {
    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// Number of lines covered.
    pub fn lines(&self) -> u32 {
        self.runs.last().map_or(0, Run::end)
    }

    /// The run holding zero-based `line`.
    pub fn run_at(&self, line: u32) -> Option<&Run> {
        let i = self.runs.partition_point(|r| r.end() <= line);
        self.runs.get(i).filter(|r| r.start <= line)
    }

    /// The runs overlapping `range`, clipped to it (the `-L` view).
    pub fn slice(&self, range: Range<u32>) -> Result<Blame> {
        if range.start > range.end || range.end > self.lines() {
            return Err(Error::BadRange {
                range,
                lines: self.lines(),
            });
        }
        let first = self.runs.partition_point(|r| r.end() <= range.start);
        let runs = self.runs[first..]
            .iter()
            .take_while(|r| r.start < range.end)
            .filter_map(|r| r.clip(&range))
            .collect();
        Ok(Blame { runs })
    }

    /// Appends `run`, joining it to the last run when they are contiguous in both files.
    fn push(&mut self, run: Run) {
        if let Some(last) = self.runs.last_mut()
            && last.end() == run.start
            && last.origin == run.origin
            && last.orig_path == run.orig_path
            && last.orig_line + last.len == run.orig_line
        {
            last.len += run.len;
        } else {
            self.runs.push(run);
        }
    }
}

const NAME: &str = "blame";
const VERSION: u32 = 1;

fn bad() -> derive::Error {
    derive::Error::BadValue { name: NAME }
}

#[async_trait]
impl DerivedKeyed for Blame {
    const NAME: &'static str = NAME;
    const VERSION: u32 = VERSION;
    type Key = RepoPath;

    fn encode_key(key: &RepoPath) -> Vec<u8> {
        key.as_bytes().to_vec()
    }

    async fn derive<O: Objects, R: Refs>(
        e: &Engine<O, R>,
        cs: ChangesetId,
        key: &RepoPath,
    ) -> derive::Result<Self> {
        let b = blame_of(e, &NoIndex, cs, key)
            .await
            .map_err(|err| match err {
                Error::Derive(d) => d,
                Error::Commit(c) => derive::Error::Commit(c),
                Error::Tree(t) => derive::Error::Tree(t),
                Error::Diff(diff::Error::Store(s)) => derive::Error::Store(s),
                _ => bad(),
            })?;
        Ok(Arc::unwrap_or_clone(b))
    }

    /// `u32` run count, then per run `start`, `len`, `orig_line` (`u32` LE), the origin id,
    /// and the original path as a `u32` length plus bytes.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend((self.runs.len() as u32).to_le_bytes());
        for r in &self.runs {
            out.extend(r.start.to_le_bytes());
            out.extend(r.len.to_le_bytes());
            out.extend(r.orig_line.to_le_bytes());
            out.extend(r.origin.as_bytes());
            out.extend((r.orig_path.as_bytes().len() as u32).to_le_bytes());
            out.extend(r.orig_path.as_bytes());
        }
        out
    }

    fn decode(b: &[u8]) -> derive::Result<Self> {
        let mut c = Cursor(b);
        let n = c.u32()?;
        let mut blame = Blame::default();
        for _ in 0..n {
            let (start, len, orig_line) = (c.u32()?, c.u32()?, c.u32()?);
            let origin = ChangesetId::from_bytes(c.take(32)?.try_into().map_err(|_| bad())?);
            let plen = c.u32()? as usize;
            let orig_path = RepoPath::new(c.take(plen)?).map_err(|_: PathError| bad())?;
            // Runs must tile the file from line 0 without gaps.
            if len == 0 || start != blame.lines() {
                return Err(bad());
            }
            blame.runs.push(Run {
                start,
                len,
                origin,
                orig_line,
                orig_path,
            });
        }
        if c.0.is_empty() {
            Ok(blame)
        } else {
            Err(bad())
        }
    }
}

/// Reads a value off the front of a byte slice.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> derive::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(bad());
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u32(&mut self) -> derive::Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| bad())?,
        ))
    }
}

/// The server's path index (SPEC §5.1, U65): lets blame jump over changesets that did not
/// touch the path instead of reading each one.
#[async_trait]
pub trait PathIndex: Send + Sync {
    /// The nearest first-parent ancestor-or-self of `from` that changed `path`, or `None`
    /// when the index cannot say (blame then walks parents itself).
    async fn last_touching(
        &self,
        from: ChangesetId,
        path: &RepoPath,
    ) -> Result<Option<ChangesetId>>;
}

/// No index: blame walks parents.
pub struct NoIndex;

#[async_trait]
impl PathIndex for NoIndex {
    async fn last_touching(&self, _: ChangesetId, _: &RepoPath) -> Result<Option<ChangesetId>> {
        Ok(None)
    }
}

/// Blame of a file at a revision (SPEC §4.3). `Engine` implements it here; the RPC client
/// implements it remotely.
#[async_trait]
pub trait BlameApi: Send + Sync {
    /// Origins of the lines of `path` at `rev`, optionally restricted to a zero-based,
    /// half-open line range.
    async fn blame(
        &self,
        rev: ChangesetId,
        path: &RepoPath,
        lines: Option<Range<u32>>,
    ) -> Result<Blame>;
}

#[async_trait]
impl<O: Objects, R: Refs> BlameApi for Engine<O, R> {
    async fn blame(
        &self,
        rev: ChangesetId,
        path: &RepoPath,
        lines: Option<Range<u32>>,
    ) -> Result<Blame> {
        self.blame_indexed(rev, path, lines, &NoIndex).await
    }
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// [`BlameApi::blame`] with a path index supplying the candidate changesets.
    pub async fn blame_indexed(
        &self,
        rev: ChangesetId,
        path: &RepoPath,
        lines: Option<Range<u32>>,
        index: &dyn PathIndex,
    ) -> Result<Blame> {
        let whole = blame_of(self, index, rev, path).await?;
        match lines {
            Some(range) => whole.slice(range),
            None => Ok(Arc::unwrap_or_clone(whole)),
        }
    }
}

/// A changeset and the path of the file in it.
type Key = (ChangesetId, RepoPath);

/// Where the lines of a node come from.
enum Plan {
    /// The file equals the first parent's (or later parent's) version: same blame.
    Pass(Key),
    /// Lines are matched against these parent versions in order; the rest are new.
    Diff(Vec<Source>),
}

struct Source {
    key: Key,
    entry: Entry,
}

impl Plan {
    fn deps(&self) -> Vec<Key> {
        match self {
            Plan::Pass(k) => vec![k.clone()],
            Plan::Diff(s) => s.iter().map(|s| s.key.clone()).collect(),
        }
    }
}

fn is_file(entry: &Entry) -> bool {
    !matches!(entry.kind, Kind::Dir | Kind::Gitlink)
}

/// Memo key; must equal the one `Engine::get_keyed` uses for `Blame`.
fn memo_key(cs: ChangesetId, path: &RepoPath) -> ObjKey {
    let mut h = blake3::Hasher::new();
    h.update(cs.as_bytes());
    h.update(&Blame::encode_key(path));
    ObjKey::Derived {
        name: NAME,
        version: VERSION,
        id: *h.finalize().as_bytes(),
    }
}

/// Moves back over changesets that did not touch `path` (a lone parent with no change at
/// the path holds the identical file), asking the index first.
async fn resolve<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    index: &dyn PathIndex,
    mut cs: ChangesetId,
    path: &RepoPath,
) -> Result<Key> {
    if let Some(t) = index.last_touching(cs, path).await? {
        return Ok((t, path.clone()));
    }
    loop {
        let c = e.get_changeset(cs).await?;
        match c.parents.as_slice() {
            [p] if c.changes.get(path).is_none() => cs = *p,
            _ => return Ok((cs, path.clone())),
        }
    }
}

/// The version of the file at `path` in `cs`, if it is a file there.
async fn file_in<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    cs: ChangesetId,
    path: &RepoPath,
) -> Result<Option<Entry>> {
    let c = e.get_changeset(cs).await?;
    Ok(e.lookup(c.root, path).await?.filter(is_file))
}

async fn plan<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    index: &dyn PathIndex,
    (cs, path): &Key,
) -> Result<(Entry, Plan)> {
    let c = e.get_changeset(*cs).await?;
    let entry = file_in(e, *cs, path)
        .await?
        .ok_or_else(|| Error::NotFound(path.clone()))?;
    // A rename: the destination records its source, which the same changeset deletes.
    let renamed_from = match c.changes.get(path) {
        Some(Change::Write {
            copy_from: Some(src),
            ..
        }) if matches!(c.changes.get(src), Some(Change::Delete)) => Some(src),
        _ => None,
    };
    let mut sources = Vec::new();
    for (i, p) in c.parents.iter().enumerate() {
        let mut at = path;
        let mut found = file_in(e, *p, at).await?;
        if found.is_none()
            && i == 0
            && let Some(src) = renamed_from
        {
            at = src;
            found = file_in(e, *p, at).await?;
        }
        let Some(pe) = found else { continue };
        let key = resolve(e, index, *p, at).await?;
        if pe.id == entry.id {
            return Ok((entry, Plan::Pass(key)));
        }
        sources.push(Source { key, entry: pe });
    }
    Ok((entry, Plan::Diff(sources)))
}

/// Blame of `path` at `cs`, computed iteratively over the touching history.
async fn blame_of<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    index: &dyn PathIndex,
    cs: ChangesetId,
    path: &RepoPath,
) -> Result<Arc<Blame>> {
    let target = resolve(e, index, cs, path).await?;
    let mut done: HashMap<Key, Arc<Blame>> = HashMap::new();
    let mut plans: HashMap<Key, (Entry, Plan)> = HashMap::new();
    // Dependents still to consume each value; a value is dropped at zero.
    let mut uses: HashMap<Key, usize> = HashMap::new();
    let mut stack = vec![target.clone()];
    while let Some(top) = stack.last().cloned() {
        if done.contains_key(&top) {
            stack.pop();
            continue;
        }
        if !plans.contains_key(&top) {
            if let Some(b) = read_memo(&*e.objects, &memo_key(top.0, &top.1)).await?
                && let Ok(v) = Blame::decode(&b)
            {
                done.insert(top, Arc::new(v));
                stack.pop();
                continue;
            }
            let p = plan(e, index, &top).await?;
            for d in p.1.deps() {
                *uses.entry(d).or_default() += 1;
            }
            plans.insert(top.clone(), p);
        }
        let pending: Vec<Key> = plans[&top]
            .1
            .deps()
            .into_iter()
            .filter(|d| !done.contains_key(d))
            .collect();
        if !pending.is_empty() {
            stack.extend(pending);
            continue;
        }
        let (entry, plan) = plans.remove(&top).expect("planned above");
        let value = match &plan {
            Plan::Pass(k) => done[k].clone(),
            Plan::Diff(sources) => {
                let v = compute(e, &top, &entry, sources, &done).await?;
                write_memo(&*e.objects, &memo_key(top.0, &top.1), &v.encode()).await?;
                Arc::new(v)
            }
        };
        for d in plan.deps() {
            if let Some(n) = uses.get_mut(&d) {
                *n -= 1;
                if *n == 0 {
                    uses.remove(&d);
                    done.remove(&d);
                }
            }
        }
        done.insert(top, value);
        stack.pop();
    }
    done.remove(&target)
        .ok_or_else(|| Error::NotFound(target.1))
}

/// Matches the new version against each parent version in turn; lines the parent also has
/// inherit its origin, and lines no parent has belong to this changeset.
async fn compute<O: Objects, R: Refs>(
    e: &Engine<O, R>,
    (cs, path): &Key,
    entry: &Entry,
    sources: &[Source],
    done: &HashMap<Key, Arc<Blame>>,
) -> Result<Blame> {
    let new = e
        .read_file(FileId::from_bytes(entry.id), entry.size)
        .await?;
    let total = line::split_lines(&new).len() as u32;
    let mut taken = vec![false; total as usize];
    let mut left = total;
    let mut found: Vec<Run> = Vec::new();
    for s in sources {
        if left == 0 {
            break;
        }
        let old = e
            .read_file(FileId::from_bytes(s.entry.id), s.entry.size)
            .await?;
        let d = line::diff(&old, &new, Algorithm::Myers, false);
        let parent = &done[&s.key];
        let end = (d.old.len() as u32, d.new.len() as u32);
        let (mut i, mut j) = (0, 0);
        let ends = d
            .hunks
            .iter()
            .map(|h| ((h.before.start, h.after.start), (h.before.end, h.after.end)))
            .chain([(end, end)]);
        for ((bs, as_), (be, ae)) in ends {
            // Lines `i..bs` of the parent equal lines `j..as_` of the new file.
            for r in parent
                .runs
                .iter()
                .skip(parent.runs.partition_point(|r| r.end() <= i))
            {
                if r.start >= bs {
                    break;
                }
                let Some(c) = r.clip(&(i..bs)) else { continue };
                // Place the part of the clip not already claimed by an earlier parent.
                let at = j + (c.start - i);
                let mut k = 0;
                while k < c.len {
                    if taken[(at + k) as usize] {
                        k += 1;
                        continue;
                    }
                    let from = k;
                    while k < c.len && !taken[(at + k) as usize] {
                        taken[(at + k) as usize] = true;
                        k += 1;
                    }
                    found.push(Run {
                        start: at + from,
                        len: k - from,
                        orig_line: c.orig_line + from,
                        ..c.clone()
                    });
                    left -= k - from;
                }
            }
            debug_assert_eq!(bs - i, as_ - j);
            (i, j) = (be, ae);
        }
    }
    found.sort_by_key(|r| r.start);
    let own = |range: Range<u32>| Run {
        start: range.start,
        len: range.end - range.start,
        origin: *cs,
        orig_line: range.start,
        orig_path: path.clone(),
    };
    let mut out = Blame::default();
    let mut next = 0;
    for r in found {
        if r.start > next {
            out.push(own(next..r.start));
        }
        next = r.end();
        out.push(r);
    }
    if next < total {
        out.push(own(next..total));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor as IoCursor;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use asc_core::content::{Body, ingest};
    use asc_core::sig::Signature;
    use asc_store::codec::encode_stored;

    use super::*;
    use crate::commit::{NewChangeset, PathPolicy};
    use crate::derive::testutil::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    /// One commit of a fixture: parents by index, the full file set, and renames.
    struct Step {
        parents: Vec<usize>,
        files: Vec<(&'static str, String)>,
        copies: Vec<(&'static str, &'static str)>,
    }

    fn step(parents: &[usize], content: &str) -> Step {
        Step {
            parents: parents.to_vec(),
            files: vec![("f.txt", content.to_owned())],
            copies: Vec::new(),
        }
    }

    async fn write_blob(e: &TestEngine, content: &[u8]) -> Change {
        let got = ingest(IoCursor::new(content)).unwrap();
        let Body::Inline(b) = got.body else {
            unreachable!("test files are small")
        };
        e.objects
            .put(&ObjKey::File(got.file), encode_stored(&b))
            .await
            .unwrap();
        Change::write(Kind::File, got.file, got.size, got.git, None).unwrap()
    }

    async fn build(e: &TestEngine, steps: &[Step]) -> Vec<ChangesetId> {
        let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1_700_000_000, 0).unwrap();
        let mut ids: Vec<ChangesetId> = Vec::new();
        for s in steps {
            let mut writes = Vec::new();
            for (path, content) in &s.files {
                writes.push((p(path), write_blob(e, content.as_bytes()).await));
            }
            let root = e.apply(TestEngine::empty_tree_id(), &writes).await.unwrap();
            let id = e
                .create_changeset(
                    NewChangeset {
                        parents: s.parents.iter().map(|&i| ids[i]).collect(),
                        author: sig.clone(),
                        committer: sig.clone(),
                        message: b"msg\n".to_vec(),
                        extra: Vec::new(),
                        root,
                        copies: s.copies.iter().map(|(d, s)| (p(d), p(s))).collect(),
                    },
                    PathPolicy::Native { windows_safe: true },
                )
                .await
                .unwrap();
            ids.push(id);
        }
        ids
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let mut child = Command::new(crate::merge::testutil::git_program())
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@x")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@x")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
        assert!(child.wait().unwrap().success(), "git {args:?} failed");
        out
    }

    /// Replays the fixture in a git repo; returns each commit's SHA.
    fn replay(dir: &Path, steps: &[Step]) -> Vec<String> {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "core.autocrlf", "false"]);
        let mut shas: Vec<String> = Vec::new();
        for s in steps {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.file_name().unwrap() != ".git" {
                    std::fs::remove_file(path).unwrap();
                }
            }
            for (path, content) in &s.files {
                std::fs::write(dir.join(path), content).unwrap();
            }
            git(dir, &["add", "-A"]);
            let tree = git(dir, &["write-tree"]).trim().to_owned();
            let mut args = vec!["commit-tree".to_owned(), tree];
            for &i in &s.parents {
                args.extend(["-p".to_owned(), shas[i].clone()]);
            }
            args.extend(["-m".to_owned(), "msg".to_owned()]);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            shas.push(git(dir, &refs).trim().to_owned());
        }
        shas
    }

    /// `final line (1-based) -> (sha, original line)` from `git blame --porcelain`.
    fn porcelain(out: &str) -> BTreeMap<u32, (String, u32)> {
        let mut lines = BTreeMap::new();
        for l in out.lines().filter(|l| !l.starts_with('\t')) {
            let t: Vec<&str> = l.split(' ').collect();
            if t.len() >= 3
                && t[0].len() == 40
                && t[0].bytes().all(|b| b.is_ascii_hexdigit())
                && let (Ok(orig), Ok(fin)) = (t[1].parse::<u32>(), t[2].parse::<u32>())
            {
                lines.insert(fin, (t[0].to_owned(), orig));
            }
        }
        lines
    }

    fn have_git() -> bool {
        Command::new(crate::merge::testutil::git_program())
            .arg("--version")
            .output()
            .is_ok() // lint:allow-raw-git
    }

    /// Asserts our blame of `path` at the last step equals `git blame --porcelain`.
    async fn matches_git(steps: &[Step], path: &str) -> Blame {
        let e = engine();
        let ids = build(&e, steps).await;
        let tip = *ids.last().unwrap();
        let ours = e.blame(tip, &p(path), None).await.unwrap();
        if !have_git() {
            return ours;
        }
        let dir = tempfile::tempdir().unwrap();
        let shas = replay(dir.path(), steps);
        // Copy records add an `asc` header, so only copy-free fixtures share git's SHAs.
        let same_shas = steps.iter().all(|s| s.copies.is_empty());
        for (id, sha) in ids.iter().zip(&shas).filter(|_| same_shas) {
            assert_eq!(&e.git_commit_of(*id).await.unwrap().to_string(), sha);
        }
        let theirs = porcelain(&git(
            dir.path(),
            &["blame", "--porcelain", shas.last().unwrap(), "--", path],
        ));
        assert_eq!(theirs.len() as u32, ours.lines());
        for line in 0..ours.lines() {
            let run = ours.run_at(line).unwrap();
            let (sha, orig) = &theirs[&(line + 1)];
            let want = ids[shas.iter().position(|s| s == sha).unwrap()];
            assert_eq!(run.origin, want, "origin of line {}", line + 1);
            assert_eq!(
                run.orig_line + (line - run.start) + 1,
                *orig,
                "line {}",
                line + 1
            );
        }
        ours
    }

    #[tokio::test]
    async fn linear_history_matches_git() {
        let steps = [
            step(&[], "a1\na2\na3\n"),
            step(&[0], "a1\nb1\na2\na3\n"),
            step(&[1], "a1\nb1\na3\nc1\n"),
            step(&[2], "a1\nb1\nB2\na3\nc1\n"),
            step(&[3], "a1\nX\nB2\na3\nc1\nlast"),
            step(&[4], "a1\nX\nB2\na3\nc1\nlast\n"),
        ];
        let b = matches_git(&steps, "f.txt").await;
        assert_eq!(b.lines(), 6);
    }

    #[tokio::test]
    async fn merge_history_matches_git() {
        let steps = [
            step(&[], "l1\nl2\nl3\nl4\n"),
            step(&[0], "l1\nm2\nl3\nl4\n"),
            step(&[0], "l1\nl2\nl3\nn4\n"),
            step(&[1, 2], "l1\nm2\nl3\nn4\n"),
            // Equal to the second parent: blame passes to it unchanged.
            step(&[3, 2], "l1\nl2\nl3\nn4\n"),
            step(&[4], "l1\nl2\nnew\nl3\nn4\n"),
        ];
        matches_git(&steps, "f.txt").await;
    }

    #[tokio::test]
    async fn untouched_commits_are_skipped() {
        let mut other = step(&[0], "a\nb\n");
        other.files.push(("g.txt", "g1\n".into()));
        let mut other2 = step(&[1], "a\nb\n");
        other2.files.push(("g.txt", "g2\n".into()));
        let steps = [step(&[], "a\nb\n"), other, other2, step(&[2], "a\nb\nc\n")];
        let b = matches_git(&steps, "f.txt").await;
        assert_eq!(b.runs().len(), 2);
    }

    #[tokio::test]
    async fn renames_are_followed() {
        let rename = Step {
            parents: vec![0],
            files: vec![("new.txt", "r1\nr2\nr3\n".into())],
            copies: vec![("new.txt", "old.txt")],
        };
        let edit = Step {
            parents: vec![1],
            files: vec![("new.txt", "r1\nr2\nr2b\nr3\n".into())],
            copies: Vec::new(),
        };
        let steps = [
            Step {
                parents: vec![],
                files: vec![("old.txt", "r1\nr2\nr3\n".into())],
                copies: Vec::new(),
            },
            rename,
            edit,
        ];
        let b = matches_git(&steps, "new.txt").await;
        assert_eq!(b.run_at(0).unwrap().orig_path, p("old.txt"));
        assert_eq!(b.run_at(2).unwrap().orig_path, p("new.txt"));
    }

    #[tokio::test]
    async fn long_history_is_iterative() {
        let mut text = String::new();
        let mut steps = Vec::new();
        for i in 0..300 {
            text.push_str(&format!("line {i}\n"));
            let parents: Vec<usize> = (i > 0).then(|| i - 1).into_iter().collect();
            steps.push(step(&parents, &text));
        }
        let e = engine();
        let ids = build(&e, &steps).await;
        let b = e.blame(ids[299], &p("f.txt"), None).await.unwrap();
        assert_eq!(b.runs().len(), 300);
        for (i, r) in b.runs().iter().enumerate() {
            assert_eq!(
                (r.origin, r.start, r.orig_line),
                (ids[i], i as u32, i as u32)
            );
        }
    }

    #[tokio::test]
    async fn missing_path_and_directories_are_not_found() {
        let e = engine();
        let ids = build(&e, &[step(&[], "a\n")]).await;
        for path in ["nope.txt", "f.txt/x"] {
            let err = e.blame(ids[0], &p(path), None).await.unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err}");
        }
        let mut nested = step(&[], "a\n");
        nested.files.push(("d/g.txt", "g\n".into()));
        let ids = build(&e, &[nested]).await;
        assert!(matches!(
            e.blame(ids[0], &p("d"), None).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn empty_file_has_no_runs() {
        let e = engine();
        let ids = build(&e, &[step(&[], "")]).await;
        let b = e.blame(ids[0], &p("f.txt"), None).await.unwrap();
        assert_eq!((b.lines(), b.runs().len()), (0, 0));
    }

    #[tokio::test]
    async fn line_ranges_slice_runs() {
        let e = engine();
        let ids = build(&e, &[step(&[], "a\nb\nc\n"), step(&[0], "a\nb\nX\nc\n")]).await;
        let b = e.blame(ids[1], &p("f.txt"), Some(1..3)).await.unwrap();
        let got: Vec<_> = b
            .runs()
            .iter()
            .map(|r| (r.start, r.len, r.origin, r.orig_line))
            .collect();
        assert_eq!(got, [(1, 1, ids[0], 1), (2, 1, ids[1], 2)]);
        let err = e.blame(ids[1], &p("f.txt"), Some(2..9)).await.unwrap_err();
        assert!(matches!(err, Error::BadRange { lines: 4, .. }));
    }

    #[tokio::test]
    async fn keyed_memo_is_where_blame_reads_it() {
        let e = engine();
        let ids = build(&e, &[step(&[], "a\nb\n"), step(&[0], "a\nc\n")]).await;
        let v = e.get_keyed::<Blame>(ids[1], &p("f.txt")).await.unwrap();
        let raw = read_memo(&*e.objects, &memo_key(ids[1], &p("f.txt")))
            .await
            .unwrap()
            .expect("memo stored under the shared key");
        assert_eq!(Blame::decode(&raw).unwrap(), v);
        // The parent's value was memoised while deriving.
        assert!(
            read_memo(&*e.objects, &memo_key(ids[0], &p("f.txt")))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(e.blame(ids[1], &p("f.txt"), None).await.unwrap(), v);
    }

    #[test]
    fn encoding_round_trips_and_rejects_damage() {
        let o = ChangesetId::from_bytes([7; 32]);
        let run = |start, len, orig_line| Run {
            start,
            len,
            origin: o,
            orig_line,
            orig_path: p("a/b.txt"),
        };
        let b = Blame {
            runs: vec![run(0, 2, 0), run(2, 3, 9)],
        };
        let bytes = b.encode();
        assert_eq!(Blame::decode(&bytes).unwrap(), b);
        assert!(Blame::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(Blame::decode(&extra).is_err());
        let gap = Blame {
            runs: vec![run(0, 2, 0), run(3, 1, 0)],
        };
        assert!(Blame::decode(&gap.encode()).is_err());
        assert_eq!(
            Blame::decode(&Blame::default().encode()).unwrap(),
            Blame::default()
        );
    }

    /// An index answering from the fixture's own history; counts questions.
    struct Counting {
        calls: AtomicUsize,
        answer: Vec<(ChangesetId, ChangesetId)>,
    }

    #[async_trait]
    impl PathIndex for Counting {
        async fn last_touching(&self, f: ChangesetId, _: &RepoPath) -> Result<Option<ChangesetId>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .answer
                .iter()
                .find(|(from, _)| *from == f)
                .map(|(_, t)| *t))
        }
    }

    #[tokio::test]
    async fn path_index_supplies_candidates() {
        let e = engine();
        let ids = build(
            &e,
            &[
                step(&[], "a\nb\n"),
                step(&[0], "a\nb\nc\n"),
                step(&[1], "a\nb\nc\n"),
            ],
        )
        .await;
        // The index says changeset 1 is the last to touch the path, as it was.
        let index = Counting {
            calls: AtomicUsize::new(0),
            answer: vec![(ids[2], ids[1]), (ids[1], ids[1]), (ids[0], ids[0])],
        };
        let b = e
            .blame_indexed(ids[2], &p("f.txt"), None, &index)
            .await
            .unwrap();
        assert!(index.calls.load(Ordering::SeqCst) > 0);
        assert_eq!(b, e.blame(ids[2], &p("f.txt"), None).await.unwrap());
        assert_eq!(b.runs().len(), 2);
    }
}
