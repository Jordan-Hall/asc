//! Commit graph and history walks (SPEC §4.3, §5.1).
//!
//! Every walk is ordered by descending generation, so ancestry queries prune by generation and
//! `log` costs O(result) node reads. Nodes load lazily through a [`GraphSource`]; nothing here
//! materialises a whole history.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use asc_core::id::ChangesetId;
use asc_core::object::{Change, Changeset};
use asc_core::path::RepoPath;
use asc_store::objects::Objects;
use asc_store::refs::Refs;
use futures::stream::{self, Stream};

use crate::Engine;
use crate::commit;

/// Graph walk failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("changeset {0} is missing")]
    Missing(ChangesetId),
    #[error(transparent)]
    Engine(#[from] commit::Error),
    #[error("invalid log query: {0}")]
    Query(&'static str),
    #[error("invalid grep pattern: {0}")]
    Grep(#[from] regex::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A changeset's place in the graph: `gen = 1 + max(gen(parents))`; roots have `gen = 1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub generation: u32,
    pub parents: Vec<ChangesetId>,
}

/// Where graph nodes and changeset headers come from.
#[async_trait]
pub trait GraphSource: Send + Sync {
    /// The node for `id`.
    async fn get(&self, id: ChangesetId) -> Result<Node>;

    /// Nodes for `ids`, in order. Sources with a batch path override this.
    async fn get_many(&self, ids: &[ChangesetId]) -> Result<Vec<Node>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            out.push(self.get(id).await?);
        }
        Ok(out)
    }

    /// The decoded changeset, for header and `changes` filters.
    async fn changeset(&self, id: ChangesetId) -> Result<Arc<Changeset>>;

    /// Changesets read so far (SPEC §5.1 read bound).
    fn reads(&self) -> u64;
}

/// Skip pointers over first-parent chains (`skip.v1`, SPEC §5). Purely an accelerator.
pub trait JumpIndex: Send + Sync {
    /// The deepest first-parent ancestor of `from` whose generation is still `>= min_gen`.
    fn jump(&self, from: ChangesetId, min_gen: u32) -> Option<ChangesetId>;
}

/// A [`JumpIndex`] with no pointers.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoJump;

impl JumpIndex for NoJump {
    fn jump(&self, _: ChangesetId, _: u32) -> Option<ChangesetId> {
        None
    }
}

/// A [`GraphSource`] over an [`Engine`].
///
/// Until derived `gen.v1` (U14) supplies generations, they are computed by walking parents and
/// memoised in memory, which reads the ancestry of any first-seen node.
pub struct EngineSource<'a, O: Objects, R: Refs> {
    engine: &'a Engine<O, R>,
    known: Mutex<HashMap<ChangesetId, Node>>,
    reads: AtomicU64,
}

impl<'a, O: Objects, R: Refs> EngineSource<'a, O, R> {
    pub fn new(engine: &'a Engine<O, R>) -> Self {
        Self {
            engine,
            known: Mutex::new(HashMap::new()),
            reads: AtomicU64::new(0),
        }
    }

    fn lookup(&self, id: &ChangesetId) -> Option<Node> {
        self.known.lock().unwrap().get(id).cloned()
    }
}

#[async_trait]
impl<O: Objects, R: Refs> GraphSource for EngineSource<'_, O, R> {
    async fn get(&self, id: ChangesetId) -> Result<Node> {
        // Iterative post-order so deep histories cannot overflow the stack.
        let mut stack = vec![id];
        while let Some(&top) = stack.last() {
            if self.lookup(&top).is_some() {
                stack.pop();
                continue;
            }
            let cs = self.changeset(top).await?;
            let mut top_gen = 0;
            let mut missing = false;
            for p in &cs.parents {
                match self.lookup(p) {
                    Some(n) => top_gen = top_gen.max(n.generation),
                    None => {
                        stack.push(*p);
                        missing = true;
                    }
                }
            }
            if !missing {
                let node = Node {
                    generation: top_gen + 1,
                    parents: cs.parents.clone(),
                };
                self.known.lock().unwrap().insert(top, node);
                stack.pop();
            }
        }
        Ok(self.lookup(&id).expect("computed above"))
    }

    async fn changeset(&self, id: ChangesetId) -> Result<Arc<Changeset>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.engine.get_changeset(id).await?)
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

/// Filters and paging for [`Graph::log`].
#[derive(Clone, Debug, Default)]
pub struct LogQuery {
    /// Start points. Empty means an empty log.
    pub include: Vec<ChangesetId>,
    /// Changesets and everything they reach are hidden; wins over `include`.
    pub exclude: Vec<ChangesetId>,
    /// Follow only the first parent (for `exclude` too).
    pub first_parent: bool,
    /// Keep changesets whose `changes` touch any of these paths or directories.
    pub paths: Vec<RepoPath>,
    /// With exactly one path, track it back through `copy_from` renames.
    pub follow: bool,
    /// Substring of `name <email>` of the author.
    pub author: Option<Vec<u8>>,
    /// Regular expression over the message.
    pub grep: Option<String>,
    /// Committer time lower bound (unix seconds, inclusive).
    pub since: Option<i64>,
    /// Committer time upper bound (unix seconds, inclusive).
    pub until: Option<i64>,
    /// Matches to drop first (git's `--skip`).
    pub skip: usize,
    /// Maximum matches to return.
    pub limit: Option<usize>,
    /// Resume point from a previous page; replaces `include`, `exclude` and `skip`.
    pub cursor: Option<LogCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pending {
    id: ChangesetId,
    excluded: bool,
    /// Followed path, when `follow` is set.
    path: Option<RepoPath>,
}

/// An opaque position in a log walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogCursor {
    frontier: Vec<Pending>,
    skip_left: usize,
}

/// One page of a log.
#[derive(Clone, Debug)]
pub struct LogPage {
    pub ids: Vec<ChangesetId>,
    /// `None` once the walk is exhausted; a `Some` page may still turn out empty.
    pub next: Option<LogCursor>,
}

const P1: u8 = 1;
const P2: u8 = 2;
const STALE: u8 = 4;

/// The commit graph over a [`GraphSource`], with an in-memory node cache.
pub struct Graph<S, J = NoJump> {
    source: S,
    jump: J,
    nodes: Mutex<HashMap<ChangesetId, Arc<Node>>>,
}

impl<S: GraphSource> Graph<S> {
    pub fn new(source: S) -> Self {
        Self::with_jump(source, NoJump)
    }
}

impl<S: GraphSource, J: JumpIndex> Graph<S, J> {
    pub fn with_jump(source: S, jump: J) -> Self {
        Self {
            source,
            jump,
            nodes: Mutex::new(HashMap::new()),
        }
    }

    /// Changesets the source has read so far.
    pub fn reads(&self) -> u64 {
        self.source.reads()
    }

    /// The node for `id`, loading it once.
    pub async fn node(&self, id: ChangesetId) -> Result<Arc<Node>> {
        if let Some(n) = self.nodes.lock().unwrap().get(&id) {
            return Ok(n.clone());
        }
        let n = Arc::new(self.source.get(id).await?);
        self.nodes.lock().unwrap().insert(id, n.clone());
        Ok(n)
    }

    /// Nodes for `ids` in order, fetching the unknown ones as one batch.
    pub async fn nodes(&self, ids: &[ChangesetId]) -> Result<Vec<Arc<Node>>> {
        let mut missing = Vec::new();
        {
            let cache = self.nodes.lock().unwrap();
            for id in ids {
                if !cache.contains_key(id) && !missing.contains(id) {
                    missing.push(*id);
                }
            }
        }
        if !missing.is_empty() {
            let got = self.source.get_many(&missing).await?;
            let mut cache = self.nodes.lock().unwrap();
            for (id, n) in missing.into_iter().zip(got) {
                cache.insert(id, Arc::new(n));
            }
        }
        let cache = self.nodes.lock().unwrap();
        Ok(ids.iter().map(|id| cache[id].clone()).collect())
    }

    /// True when `a` is `b` or an ancestor of `b`.
    pub async fn is_ancestor(&self, a: ChangesetId, b: ChangesetId) -> Result<bool> {
        if a == b {
            return Ok(true);
        }
        let ga = self.node(a).await?.generation;
        if self.node(b).await?.generation <= ga {
            return Ok(false);
        }
        if self.jump.jump(b, ga) == Some(a) {
            return Ok(true);
        }
        let mut seen = HashSet::from([b]);
        let mut stack = vec![b];
        while let Some(id) = stack.pop() {
            let node = self.node(id).await?;
            let parents = self.nodes(&node.parents).await?;
            for (&p, pn) in node.parents.iter().zip(parents) {
                if p == a {
                    return Ok(true);
                }
                if pn.generation > ga && seen.insert(p) {
                    stack.push(p);
                }
            }
        }
        Ok(false)
    }

    /// Best common ancestors of `a` and the merge of `others` (git's `merge-base`).
    ///
    /// Returns every best base when `all`, else at most one. Empty when unrelated.
    pub async fn merge_base(
        &self,
        a: ChangesetId,
        others: &[ChangesetId],
        all: bool,
    ) -> Result<Vec<ChangesetId>> {
        let mut flags: HashMap<ChangesetId, u8> = HashMap::new();
        let mut heap = BinaryHeap::new();
        let mut live = 0usize; // queued entries without STALE
        *flags.entry(a).or_default() |= P1;
        for &o in others {
            *flags.entry(o).or_default() |= P2;
        }
        let starts: Vec<ChangesetId> = flags.keys().copied().collect();
        for (&id, n) in starts.iter().zip(self.nodes(&starts).await?) {
            heap.push((n.generation, id));
            live += 1;
        }
        let mut found = Vec::new();
        while live > 0 {
            let (_, id) = heap.pop().expect("live entries are queued");
            let mut f = flags[&id];
            if f & STALE == 0 {
                live -= 1;
            }
            if f & (P1 | P2) == (P1 | P2) && f & STALE == 0 {
                found.push(id);
                f |= STALE;
                flags.insert(id, f);
            }
            let node = self.node(id).await?;
            let parents = self.nodes(&node.parents).await?;
            for (&p, pn) in node.parents.iter().zip(parents) {
                let old = flags.get(&p).copied();
                if old.is_some_and(|o| o & f == f) {
                    continue;
                }
                let new = old.unwrap_or(0) | f;
                flags.insert(p, new);
                match old {
                    None => {
                        heap.push((pn.generation, p));
                        if new & STALE == 0 {
                            live += 1;
                        }
                    }
                    Some(o) if o & STALE == 0 && new & STALE != 0 => live -= 1,
                    Some(_) => {}
                }
            }
        }
        if found.len() > 1 {
            let mut keep = Vec::new();
            for &r in &found {
                let mut redundant = false;
                for &s in &found {
                    if s != r && self.is_ancestor(r, s).await? {
                        redundant = true;
                        break;
                    }
                }
                if !redundant {
                    keep.push(r);
                }
            }
            found = keep;
        }
        if !all {
            found.truncate(1);
        }
        Ok(found)
    }

    /// Streams matching changesets, highest generation first (ties by id).
    pub fn log(&self, q: LogQuery) -> impl Stream<Item = Result<ChangesetId>> + '_ {
        enum Step<'a, S, J> {
            Start(Box<LogQuery>),
            Run(Box<Walk<'a, S, J>>),
        }
        stream::try_unfold(Step::Start(Box::new(q)), move |st| async move {
            let mut walk = match st {
                Step::Start(q) => Box::new(Walk::new(self, *q).await?),
                Step::Run(w) => w,
            };
            Ok(walk.next().await?.map(|id| (id, Step::Run(walk))))
        })
    }

    /// One page (at most `q.limit`) plus the cursor for the next, resumed without rescanning.
    pub async fn log_page(&self, q: LogQuery) -> Result<LogPage> {
        let mut walk = Walk::new(self, q).await?;
        let mut ids = Vec::new();
        while let Some(id) = walk.next().await? {
            ids.push(id);
        }
        let next = (walk.live > 0).then(|| walk.cursor());
        Ok(LogPage { ids, next })
    }

    /// Number of matches, ignoring `skip`, `limit` and `cursor`.
    pub async fn count(&self, mut q: LogQuery) -> Result<usize> {
        q.skip = 0;
        q.limit = None;
        q.cursor = None;
        let mut walk = Walk::new(self, q).await?;
        let mut n = 0;
        while walk.next().await?.is_some() {
            n += 1;
        }
        Ok(n)
    }
}

/// True when a change at `key` affects `path` (equal, inside it, or a file replacing a parent).
fn touches(key: &RepoPath, path: &RepoPath) -> bool {
    key == path || key.starts_with_dir(path) || path.starts_with_dir(key)
}

/// A resumable generation-ordered log walk.
struct Walk<'g, S, J> {
    graph: &'g Graph<S, J>,
    q: LogQuery,
    grep: Option<regex::bytes::Regex>,
    heap: BinaryHeap<(u32, Reverse<ChangesetId>)>,
    queued: HashMap<ChangesetId, Pending>,
    /// Queued entries that are not excluded; the walk ends when it reaches zero.
    live: usize,
    skip_left: usize,
    limit_left: Option<usize>,
}

impl<'g, S: GraphSource, J: JumpIndex> Walk<'g, S, J> {
    async fn new(graph: &'g Graph<S, J>, q: LogQuery) -> Result<Self> {
        if q.follow && q.paths.len() != 1 {
            return Err(Error::Query("follow needs exactly one path"));
        }
        let grep = q
            .grep
            .as_deref()
            .map(regex::bytes::Regex::new)
            .transpose()?;
        let (frontier, skip_left) = match &q.cursor {
            Some(c) => (c.frontier.clone(), c.skip_left),
            None => {
                let path = q.follow.then(|| q.paths[0].clone());
                let mut f: Vec<Pending> = Vec::new();
                for (ids, excluded) in [(&q.exclude, true), (&q.include, false)] {
                    for &id in ids {
                        match f.iter_mut().find(|e| e.id == id) {
                            Some(e) => e.excluded |= excluded,
                            None => f.push(Pending {
                                id,
                                excluded,
                                path: path.clone(),
                            }),
                        }
                    }
                }
                (f, q.skip)
            }
        };
        let ids: Vec<ChangesetId> = frontier.iter().map(|p| p.id).collect();
        let nodes = graph.nodes(&ids).await?;
        let mut w = Walk {
            graph,
            limit_left: q.limit,
            q,
            grep,
            heap: BinaryHeap::new(),
            queued: HashMap::new(),
            live: 0,
            skip_left,
        };
        for (p, n) in frontier.into_iter().zip(nodes) {
            w.heap.push((n.generation, Reverse(p.id)));
            w.live += usize::from(!p.excluded);
            w.queued.insert(p.id, p);
        }
        Ok(w)
    }

    fn cursor(&self) -> LogCursor {
        let mut frontier: Vec<Pending> = self.queued.values().cloned().collect();
        frontier.sort_by_key(|p| p.id);
        LogCursor {
            frontier,
            skip_left: self.skip_left,
        }
    }

    /// Pops by descending generation, so every child is seen before its parents and exclusion
    /// flags are final when a node is popped.
    async fn next(&mut self) -> Result<Option<ChangesetId>> {
        if self.limit_left == Some(0) {
            return Ok(None);
        }
        while self.live > 0 {
            let (_, Reverse(id)) = self.heap.pop().expect("live entries are queued");
            let entry = self.queued.remove(&id).expect("queued with heap entry");
            if !entry.excluded {
                self.live -= 1;
            }
            let node = self.graph.node(id).await?;
            let parents: &[ChangesetId] = if self.q.first_parent {
                &node.parents[..node.parents.len().min(1)]
            } else {
                &node.parents
            };
            let header = if entry.excluded || !self.needs_header() {
                None
            } else {
                Some(self.graph.source.changeset(id).await?)
            };
            let (matched, parent_path) = match &header {
                Some(cs) => self.filter(cs, entry.path.as_ref()),
                None => (!entry.excluded, entry.path.clone()),
            };
            let pnodes = self.graph.nodes(parents).await?;
            for (&p, pn) in parents.iter().zip(pnodes) {
                match self.queued.get_mut(&p) {
                    Some(e) => {
                        if entry.excluded && !e.excluded {
                            e.excluded = true;
                            self.live -= 1;
                        }
                    }
                    None => {
                        self.heap.push((pn.generation, Reverse(p)));
                        self.live += usize::from(!entry.excluded);
                        self.queued.insert(
                            p,
                            Pending {
                                id: p,
                                excluded: entry.excluded,
                                path: parent_path.clone(),
                            },
                        );
                    }
                }
            }
            if !matched {
                continue;
            }
            if self.skip_left > 0 {
                self.skip_left -= 1;
                continue;
            }
            if let Some(n) = &mut self.limit_left {
                *n -= 1;
            }
            return Ok(Some(id));
        }
        Ok(None)
    }

    fn needs_header(&self) -> bool {
        let q = &self.q;
        !q.paths.is_empty()
            || q.author.is_some()
            || self.grep.is_some()
            || q.since.is_some()
            || q.until.is_some()
    }

    /// Whether `cs` matches, and the path to follow into its parents.
    fn filter(&self, cs: &Changeset, followed: Option<&RepoPath>) -> (bool, Option<RepoPath>) {
        let q = &self.q;
        let mut next_path = followed.cloned();
        let mut ok = true;
        let changes = cs.changes.as_slice();
        if let Some(cur) = followed {
            ok = changes.iter().any(|(k, _)| touches(k, cur));
            if let Some(Change::Write {
                copy_from: Some(src),
                ..
            }) = cs.changes.get(cur)
            {
                next_path = Some(src.clone());
            }
        } else if !q.paths.is_empty() {
            ok = changes
                .iter()
                .any(|(k, _)| q.paths.iter().any(|p| touches(k, p)));
        }
        let t = cs.committer.time();
        ok = ok && q.since.is_none_or(|s| t >= s) && q.until.is_none_or(|u| t <= u);
        if let Some(a) = q.author.as_deref().filter(|a| !a.is_empty()) {
            let sig = [cs.author.name(), b" <", cs.author.email(), b">"].concat();
            ok = ok && sig.windows(a.len()).any(|w| w == a);
        }
        if let Some(re) = &self.grep {
            ok = ok && re.is_match(&cs.message);
        }
        (ok, next_path)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use asc_core::codec::Enveloped;
    use asc_core::git::blob_oid;
    use asc_core::id::{FileId, TreeId};
    use asc_core::object::{Kind, SortedMap};
    use asc_core::sig::Signature;
    use asc_store::bucket::BucketObjects;
    use asc_store::codec::encode_stored;
    use asc_store::key::ObjKey;
    use asc_store::refs::{RefEntry, RefError, RefLogEntry, RefTxn, RepoKey};
    use futures::TryStreamExt;

    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    /// Description of one test changeset.
    #[derive(Clone)]
    struct Spec {
        parents: Vec<usize>,
        time: i64,
        author: &'static str,
        msg: String,
        changes: Vec<(&'static str, Option<&'static str>)>, // (path, copy_from)
    }

    fn spec(parents: &[usize], time: i64) -> Spec {
        Spec {
            parents: parents.to_vec(),
            time,
            author: "A",
            msg: format!("n{time}"),
            changes: Vec::new(),
        }
    }

    /// Builds changesets for `specs` (parents refer to earlier indexes).
    fn build(specs: &[Spec]) -> (Vec<ChangesetId>, HashMap<ChangesetId, Arc<Changeset>>) {
        let mut ids: Vec<ChangesetId> = Vec::new();
        let mut map = HashMap::new();
        for (i, s) in specs.iter().enumerate() {
            let sig = |name: &str| Signature::new(name.into(), b"x@y".to_vec(), s.time, 0).unwrap();
            let changes = s
                .changes
                .iter()
                .map(|&(path, from)| {
                    let c = Change::write(
                        Kind::File,
                        FileId::of(&[i as u8]),
                        1,
                        blob_oid(&[i as u8]),
                        from.map(p),
                    )
                    .unwrap();
                    (p(path), c)
                })
                .collect();
            let cs = Changeset {
                parents: s.parents.iter().map(|&j| ids[j]).collect(),
                root: TreeId::of(b"t"),
                author: sig(s.author),
                committer: sig(s.author),
                message: s.msg.clone().into_bytes(),
                extra: SortedMap::default(),
                changes: SortedMap::new(changes).unwrap(),
            };
            let id = ChangesetId::of(&cs.to_envelope());
            ids.push(id);
            map.insert(id, Arc::new(cs));
        }
        (ids, map)
    }

    /// In-memory source with generations computed up front and a read counter.
    struct MemSource {
        map: HashMap<ChangesetId, Arc<Changeset>>,
        gens: HashMap<ChangesetId, u32>,
        reads: AtomicU64,
    }

    impl MemSource {
        fn new(ids: &[ChangesetId], map: HashMap<ChangesetId, Arc<Changeset>>) -> Self {
            let mut gens = HashMap::new();
            for id in ids {
                let g = map[id].parents.iter().map(|q| gens[q]).max().unwrap_or(0) + 1;
                gens.insert(*id, g);
            }
            Self {
                map,
                gens,
                reads: AtomicU64::new(0),
            }
        }
    }

    #[async_trait]
    impl GraphSource for MemSource {
        async fn get(&self, id: ChangesetId) -> Result<Node> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let cs = self.map.get(&id).ok_or(Error::Missing(id))?;
            Ok(Node {
                generation: self.gens[&id],
                parents: cs.parents.clone(),
            })
        }
        async fn changeset(&self, id: ChangesetId) -> Result<Arc<Changeset>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.map.get(&id).cloned().ok_or(Error::Missing(id))
        }
        fn reads(&self) -> u64 {
            self.reads.load(Ordering::Relaxed)
        }
    }

    fn graph(specs: &[Spec]) -> (Vec<ChangesetId>, Graph<MemSource>) {
        let (ids, map) = build(specs);
        let src = MemSource::new(&ids, map);
        (ids, Graph::new(src))
    }

    async fn all(g: &Graph<MemSource>, q: LogQuery) -> Vec<ChangesetId> {
        g.log(q).try_collect().await.unwrap()
    }

    fn inc(ids: &[ChangesetId]) -> LogQuery {
        LogQuery {
            include: ids.to_vec(),
            ..LogQuery::default()
        }
    }

    // ---- git oracle -------------------------------------------------------------------

    fn git(dir: &std::path::Path, args: &[&str], date: Option<i64>) -> String {
        let mut c = Command::new("git");
        c.current_dir(dir).args(args);
        if let Some(d) = date {
            let d = format!("{} +0000", 1_000_000_000 + d);
            c.env("GIT_AUTHOR_DATE", &d).env("GIT_COMMITTER_DATE", &d);
        }
        c.env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "x@y")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "x@y")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL");
        let out = c.output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Mirrors `specs` into a git repo with `git commit-tree`; returns git ids by index.
    fn git_dag(dir: &std::path::Path, specs: &[Spec]) -> Vec<String> {
        git(dir, &["init", "-q"], None);
        let tree = git(dir, &["hash-object", "-t", "tree", "-w", "--stdin"], None);
        let mut out: Vec<String> = Vec::new();
        for s in specs {
            let mut args = vec!["commit-tree".to_string(), tree.clone(), "-m".into()];
            args.push(s.msg.clone());
            for &q in &s.parents {
                args.push("-p".into());
                args.push(out[q].clone());
            }
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            out.push(git(dir, &a, Some(s.time)));
        }
        out
    }

    /// xorshift DAG: node i has 0..=3 distinct parents among earlier nodes, biased to recent.
    fn random_dag(seed: u64, n: usize) -> Vec<Spec> {
        let mut x = seed;
        let mut rnd = move |m: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % m as u64) as usize
        };
        let mut specs = Vec::new();
        for i in 0..n {
            let mut parents = Vec::new();
            if i > 0 {
                let k = match rnd(10) {
                    0 => 0,
                    1..=6 => 1,
                    7..=8 => 2,
                    _ => 3,
                };
                for _ in 0..k {
                    let q = i - 1 - rnd(i.min(6));
                    if !parents.contains(&q) {
                        parents.push(q);
                    }
                }
            }
            specs.push(spec(&parents, i as i64 + 1));
        }
        specs
    }

    fn set<T: Ord>(mut v: Vec<T>) -> Vec<T> {
        v.sort();
        v
    }

    /// True when every parent appears after its child.
    fn topological(g: &Graph<MemSource>, order: &[ChangesetId]) -> bool {
        let pos: HashMap<_, _> = order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        order.iter().all(|id| {
            g.source.map[id]
                .parents
                .iter()
                .all(|q| pos.get(q).is_none_or(|&pq| pq > pos[id]))
        })
    }

    #[tokio::test]
    async fn matches_git_on_generated_dags() {
        let dir = tempfile::tempdir().unwrap();
        for (seed, n) in [(0x9e37_79b9u64, 28usize), (0xdead_beef, 36), (7, 20)] {
            let sub = dir.path().join(format!("r{seed}"));
            std::fs::create_dir(&sub).unwrap();
            let specs = random_dag(seed, n);
            let gids = git_dag(&sub, &specs);
            let (ids, g) = graph(&specs);
            let to_asc: HashMap<&str, ChangesetId> = gids
                .iter()
                .map(String::as_str)
                .zip(ids.iter().copied())
                .collect();
            let asc_set =
                |out: String| -> Vec<ChangesetId> { set(out.lines().map(|l| to_asc[l]).collect()) };
            let mut state = seed | 1;
            let mut pick = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % n as u64) as usize
            };
            for _ in 0..6 {
                let (i, j, k) = (pick(), pick(), pick());
                // merge-base --all, two and three arguments
                for (a, rest) in [(i, vec![j]), (i, vec![j, k])] {
                    let mut args = vec!["merge-base", "--all", &gids[a]];
                    args.extend(rest.iter().map(|&r| gids[r].as_str()));
                    let want = git_try(&sub, &args);
                    let others: Vec<_> = rest.iter().map(|&r| ids[r]).collect();
                    let got = g.merge_base(ids[a], &others, true).await.unwrap();
                    assert_eq!(
                        set(got.clone()),
                        asc_set(want.clone()),
                        "merge-base {args:?}"
                    );
                    let one = g.merge_base(ids[a], &others, false).await.unwrap();
                    assert!(one.len() == got.len().min(1) && got.contains_or_empty(&one));
                }
                // is_ancestor
                let want = Command::new("git")
                    .current_dir(&sub)
                    .args(["merge-base", "--is-ancestor", &gids[i], &gids[j]])
                    .status()
                    .unwrap()
                    .success();
                assert_eq!(g.is_ancestor(ids[i], ids[j]).await.unwrap(), want);
                // rev-list sets, ranges, first-parent
                let want = asc_set(git(&sub, &["rev-list", &gids[i]], None));
                let got = all(&g, inc(&[ids[i]])).await;
                assert!(topological(&g, &got));
                assert_eq!(set(got.clone()), want);
                assert_eq!(g.count(inc(&[ids[i]])).await.unwrap(), want.len());
                let range = LogQuery {
                    include: vec![ids[i]],
                    exclude: vec![ids[j]],
                    ..LogQuery::default()
                };
                let want = asc_set(git(
                    &sub,
                    &["rev-list", &gids[i], &format!("^{}", gids[j])],
                    None,
                ));
                assert_eq!(set(all(&g, range).await), want);
                let multi = LogQuery {
                    include: vec![ids[i], ids[j]],
                    exclude: vec![ids[k]],
                    ..LogQuery::default()
                };
                let want = asc_set(git(
                    &sub,
                    &["rev-list", &gids[i], &gids[j], &format!("^{}", gids[k])],
                    None,
                ));
                assert_eq!(set(all(&g, multi).await), want);
                let fp = LogQuery {
                    first_parent: true,
                    ..inc(&[ids[i]])
                };
                let want = asc_set(git(&sub, &["rev-list", "--first-parent", &gids[i]], None));
                assert_eq!(set(all(&g, fp).await), want);
            }
        }
    }

    trait ContainsOrEmpty {
        fn contains_or_empty(&self, one: &[ChangesetId]) -> bool;
    }
    impl ContainsOrEmpty for Vec<ChangesetId> {
        fn contains_or_empty(&self, one: &[ChangesetId]) -> bool {
            one.iter().all(|x| self.contains(x))
        }
    }

    /// `git` that may exit 1 with no output (unrelated histories).
    fn git_try(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    // ---- behaviour --------------------------------------------------------------------

    /// 0 - 1 - 2 (merge of 1 and 3) - 4, with 0 - 3 side branch.
    fn diamond() -> Vec<Spec> {
        vec![
            spec(&[], 1),
            spec(&[0], 2),
            spec(&[0], 3),
            spec(&[1, 2], 4),
            spec(&[3], 5),
        ]
    }

    #[tokio::test]
    async fn criss_cross_has_two_merge_bases_and_best_one_is_returned() {
        // 0; 1,2 children of 0; 3 = merge(1,2); 4 = merge(2,1); both bases 1 and 2.
        let specs = vec![
            spec(&[], 1),
            spec(&[0], 2),
            spec(&[0], 3),
            spec(&[1, 2], 4),
            spec(&[2, 1], 5),
        ];
        let (ids, g) = graph(&specs);
        let both = g.merge_base(ids[3], &[ids[4]], true).await.unwrap();
        assert_eq!(set(both), set(vec![ids[1], ids[2]]));
        assert_eq!(
            g.merge_base(ids[3], &[ids[4]], false).await.unwrap().len(),
            1
        );
        assert!(g.merge_base(ids[3], &[ids[0]], true).await.unwrap() == vec![ids[0]]);
        assert_eq!(
            g.merge_base(ids[1], &[ids[1]], true).await.unwrap(),
            vec![ids[1]]
        );
    }

    #[tokio::test]
    async fn unrelated_histories_have_no_merge_base() {
        let specs = vec![spec(&[], 1), spec(&[], 2)];
        let (ids, g) = graph(&specs);
        assert!(
            g.merge_base(ids[0], &[ids[1]], true)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!g.is_ancestor(ids[0], ids[1]).await.unwrap());
    }

    #[tokio::test]
    async fn first_parent_skips_merged_branch() {
        let (ids, g) = graph(&diamond());
        let q = LogQuery {
            first_parent: true,
            ..inc(&[ids[3]])
        };
        assert_eq!(all(&g, q).await, vec![ids[3], ids[1], ids[0]]);
    }

    #[tokio::test]
    async fn exclude_wins_and_hides_ancestors() {
        let (ids, g) = graph(&diamond());
        let q = LogQuery {
            include: vec![ids[3], ids[4]],
            exclude: vec![ids[2]],
            ..LogQuery::default()
        };
        assert_eq!(set(all(&g, q).await), set(vec![ids[1], ids[3], ids[4]]));
        let both = LogQuery {
            include: vec![ids[3]],
            exclude: vec![ids[3]],
            ..LogQuery::default()
        };
        assert!(all(&g, both).await.is_empty());
    }

    fn history_with_paths() -> Vec<Spec> {
        let mut s: Vec<Spec> = (0..6)
            .map(|i| spec(&if i == 0 { vec![] } else { vec![i - 1] }, i as i64 + 1))
            .collect();
        s[0].changes = vec![("old.txt", None), ("lib/a.rs", None)];
        s[1].changes = vec![("lib/b.rs", None)];
        s[2].changes = vec![("new.txt", Some("old.txt"))]; // rename old -> new
        s[3].changes = vec![("lib/a.rs", None)];
        s[4].changes = vec![("new.txt", None)];
        s[4].author = "Bob";
        s[4].msg = "fix: the bug".into();
        s
    }

    #[tokio::test]
    async fn path_filter_uses_prefixes() {
        let (ids, g) = graph(&history_with_paths());
        let q = LogQuery {
            paths: vec![p("lib")],
            ..inc(&[ids[5]])
        };
        assert_eq!(all(&g, q).await, vec![ids[3], ids[1], ids[0]]);
        let q = LogQuery {
            paths: vec![p("lib/a.rs"), p("new.txt")],
            ..inc(&[ids[5]])
        };
        assert_eq!(all(&g, q).await, vec![ids[4], ids[3], ids[2], ids[0]]);
        // "lib/a" is not a directory prefix of "lib/a.rs".
        let q = LogQuery {
            paths: vec![p("lib/a")],
            ..inc(&[ids[5]])
        };
        assert!(all(&g, q).await.is_empty());
    }

    #[tokio::test]
    async fn follow_crosses_copy_from() {
        let (ids, g) = graph(&history_with_paths());
        let plain = LogQuery {
            paths: vec![p("new.txt")],
            ..inc(&[ids[5]])
        };
        assert_eq!(all(&g, plain.clone()).await, vec![ids[4], ids[2]]);
        let follow = LogQuery {
            follow: true,
            ..plain
        };
        assert_eq!(all(&g, follow).await, vec![ids[4], ids[2], ids[0]]);
        let bad = LogQuery {
            follow: true,
            ..inc(&[ids[5]])
        };
        assert!(matches!(g.log_page(bad).await, Err(Error::Query(_))));
    }

    #[tokio::test]
    async fn header_filters() {
        let (ids, g) = graph(&history_with_paths());
        let q = LogQuery {
            author: Some(b"Bob".to_vec()),
            ..inc(&[ids[5]])
        };
        assert_eq!(all(&g, q).await, vec![ids[4]]);
        let q = LogQuery {
            grep: Some("^fix: .*bug$".into()),
            ..inc(&[ids[5]])
        };
        assert_eq!(all(&g, q).await, vec![ids[4]]);
        let q = LogQuery {
            since: Some(2),
            until: Some(4),
            ..inc(&[ids[5]])
        };
        assert_eq!(all(&g, q).await, vec![ids[3], ids[2], ids[1]]);
        let bad = LogQuery {
            grep: Some("(".into()),
            ..inc(&[ids[5]])
        };
        assert!(matches!(g.log_page(bad).await, Err(Error::Grep(_))));
    }

    #[tokio::test]
    async fn skip_limit_and_count() {
        let (ids, g) = graph(&history_with_paths());
        let q = LogQuery {
            skip: 1,
            limit: Some(2),
            ..inc(&[ids[5]])
        };
        assert_eq!(all(&g, q.clone()).await, vec![ids[4], ids[3]]);
        assert_eq!(g.count(q).await.unwrap(), 6);
    }

    fn chain(n: usize) -> Vec<Spec> {
        (0..n)
            .map(|i| spec(&if i == 0 { vec![] } else { vec![i - 1] }, i as i64 + 1))
            .collect()
    }

    #[tokio::test]
    async fn pages_concatenate_to_the_full_log_without_rescanning() {
        let (ids, g) = graph(&chain(2000));
        let tip = ids[1999];
        let mut seen = Vec::new();
        let mut cursor = None;
        let mut per_page_reads = Vec::new();
        loop {
            let before = g.reads();
            let page = g
                .log_page(LogQuery {
                    limit: Some(500),
                    cursor: cursor.clone(),
                    ..if cursor.is_none() {
                        inc(&[tip])
                    } else {
                        LogQuery::default()
                    }
                })
                .await
                .unwrap();
            per_page_reads.push(g.reads() - before);
            seen.extend(page.ids);
            match page.next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        let want: Vec<_> = ids.iter().rev().copied().collect();
        assert_eq!(seen, want);
        // Each page reads about its own size, not the skipped prefix.
        assert!(
            per_page_reads.iter().all(|&r| r <= 502),
            "{per_page_reads:?}"
        );
    }

    #[tokio::test]
    async fn skip_survives_a_page_boundary() {
        let (ids, g) = graph(&chain(10));
        let page = g
            .log_page(LogQuery {
                skip: 5,
                limit: Some(2),
                ..inc(&[ids[9]])
            })
            .await
            .unwrap();
        assert_eq!(page.ids, vec![ids[4], ids[3]]);
        let rest = g
            .log_page(LogQuery {
                cursor: page.next,
                ..LogQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(rest.ids.len(), 3);
        assert!(rest.next.is_none());
    }

    #[tokio::test]
    async fn bounded_log_reads_are_proportional_to_the_result() {
        let (ids, g) = graph(&chain(5000));
        let page = g
            .log_page(LogQuery {
                limit: Some(10),
                ..inc(&[ids[4999]])
            })
            .await
            .unwrap();
        assert_eq!(page.ids.len(), 10);
        assert!(g.reads() <= 15, "reads {}", g.reads());
        // A range that excludes almost everything also stops early.
        let (ids, g) = graph(&chain(5000));
        let q = LogQuery {
            include: vec![ids[4999]],
            exclude: vec![ids[4990]],
            ..LogQuery::default()
        };
        assert_eq!(all(&g, q).await.len(), 9);
        assert!(g.reads() <= 25, "reads {}", g.reads());
    }

    struct ChainJump(Vec<ChangesetId>);

    impl JumpIndex for ChainJump {
        fn jump(&self, from: ChangesetId, min_gen: u32) -> Option<ChangesetId> {
            let i = self.0.iter().position(|x| *x == from)?;
            self.0
                .get(min_gen as usize - 1)
                .filter(|_| i + 1 > min_gen as usize)
                .copied()
        }
    }

    #[tokio::test]
    async fn jump_index_answers_first_parent_ancestry_without_walking() {
        let (ids, map) = build(&chain(3000));
        let g = Graph::with_jump(MemSource::new(&ids, map), ChainJump(ids.clone()));
        assert!(g.is_ancestor(ids[10], ids[2999]).await.unwrap());
        assert!(g.reads() <= 2, "reads {}", g.reads());
        assert!(!g.is_ancestor(ids[2999], ids[10]).await.unwrap());
    }

    #[tokio::test]
    async fn is_ancestor_prunes_below_the_target_generation() {
        // A 3000-long chain with a side branch off node 2985: the negative answer must stop
        // at the side branch's generation instead of walking to the root.
        let mut specs = chain(3000);
        specs.push(spec(&[2985], 5000));
        let (ids, g) = graph(&specs);
        assert!(!g.is_ancestor(ids[3000], ids[2999]).await.unwrap());
        assert!(g.reads() <= 40, "reads {}", g.reads());
        assert!(g.is_ancestor(ids[2985], ids[3000]).await.unwrap());
        assert!(g.is_ancestor(ids[2990], ids[2999]).await.unwrap());
    }

    // ---- engine source ----------------------------------------------------------------

    struct NoRefs;

    #[async_trait]
    impl Refs for NoRefs {
        async fn get(
            &self,
            _: RepoKey,
            _: &asc_core::path::RefName,
        ) -> asc_store::refs::Result<Option<RefEntry>> {
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
            _: &asc_core::path::RefName,
            _: u32,
        ) -> asc_store::refs::Result<Vec<RefLogEntry>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn engine_source_computes_generations_from_stored_changesets() {
        let engine = Engine::new(
            Arc::new(BucketObjects::memory()),
            Arc::new(NoRefs),
            RepoKey(1),
        );
        let (ids, map) = build(&diamond());
        for id in &ids {
            engine
                .objects
                .put(
                    &ObjKey::Changeset(*id),
                    encode_stored(&map[id].to_envelope()),
                )
                .await
                .unwrap();
        }
        let g = Graph::new(EngineSource::new(&engine));
        let gens: Vec<u32> = vec![1, 2, 2, 3, 4];
        for (id, want) in ids.iter().zip(gens) {
            assert_eq!(g.node(*id).await.unwrap().generation, want);
        }
        assert_eq!(
            g.merge_base(ids[1], &[ids[2]], true).await.unwrap(),
            vec![ids[0]]
        );
        let missing = ChangesetId::of(b"absent");
        assert!(g.node(missing).await.is_err());
    }
}
