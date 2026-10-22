//! Revision expressions (SPEC §4.2): a parser for the git revspec subset and a resolver.
//!
//! Parsing is pure. Resolution reads only what the expression names: refs through the
//! [`Resolver`], then parents and generations through its [`GraphSource`].

use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use asc_core::id::{ChangesetId, HexPrefix, RefTarget, TagId, TreeId};
use asc_core::object::Changeset;
use asc_core::path::RepoPath;
use futures::future::BoxFuture;

use crate::graph::{self, Graph, GraphSource, Node};

/// Revision parse and resolution failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid revision `{spec}`: {reason}")]
    Parse { spec: String, reason: &'static str },
    #[error("unknown revision `{0}`")]
    NotFound(String),
    #[error("`{spec}` is ambiguous; candidates: {}", list(.candidates))]
    Ambiguous {
        spec: String,
        candidates: Vec<ChangesetId>,
    },
    #[error("{id} has no parent number {n}")]
    NoParent { id: ChangesetId, n: u32 },
    #[error("no commit message matches `{0}`")]
    NoMatch(String),
    #[error("`{0}` is not a commit")]
    NotACommit(String),
    #[error("invalid pattern: {0}")]
    Regex(#[from] regex::Error),
    #[error(transparent)]
    Graph(#[from] graph::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn list(ids: &[ChangesetId]) -> String {
    ids.iter().map(|i| i.short()).collect::<Vec<_>>().join(", ")
}

fn parse_err<T>(spec: &str, reason: &'static str) -> Result<T> {
    Err(Error::Parse {
        spec: spec.to_owned(),
        reason,
    })
}

/// What `^{...}` peels to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peel {
    /// `^{tree}`.
    Tree,
    /// `^{commit}` and `^{}`: tags are always peeled to their commit.
    Commit,
}

/// A parsed revision expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevExpr {
    /// An asc id, git SHA, abbreviation, `HEAD`, `@` or ref name.
    Name(String),
    /// `rev^N` (`N = 0` is the commit itself).
    Parent(Box<RevExpr>, u32),
    /// `rev~N`, the N-th first-parent ancestor.
    Ancestor(Box<RevExpr>, u32),
    /// `rev^{tree}`, `rev^{commit}`, `rev^{}`.
    Peel(Box<RevExpr>, Peel),
    /// `@{u}` (no branch) or `branch@{u}`.
    Upstream(Option<String>),
    /// `@{N}` (no ref, meaning HEAD) or `ref@{N}`.
    Reflog(Option<String>, u32),
    /// `:/regex`: the youngest commit reachable from any ref whose message matches.
    Message(String),
    /// `rev:path`.
    Path(Box<RevExpr>, RepoPath),
    /// `A..B`.
    DotDot(Box<RevExpr>, Box<RevExpr>),
    /// `A...B`.
    TriDot(Box<RevExpr>, Box<RevExpr>),
    /// `^A`.
    Not(Box<RevExpr>),
    /// `rev^@`: all parents.
    Parents(Box<RevExpr>),
    /// `rev^!`: the commit without its parents.
    Only(Box<RevExpr>),
    /// Several arguments, such as `^A B`.
    List(Vec<RevExpr>),
}

impl RevExpr {
    /// Whether this denotes a set of commits rather than one object.
    fn is_set(&self) -> bool {
        matches!(
            self,
            Self::DotDot(..)
                | Self::TriDot(..)
                | Self::Not(_)
                | Self::Parents(_)
                | Self::Only(_)
                | Self::List(_)
        )
    }
}

fn head() -> RevExpr {
    RevExpr::Name("HEAD".into())
}

/// Parses one revision argument.
pub fn parse(spec: &str) -> Result<RevExpr> {
    if spec.is_empty() {
        return parse_err(spec, "empty revision");
    }
    if let Some(re) = spec.strip_prefix(":/") {
        if re.is_empty() {
            return parse_err(spec, "empty pattern");
        }
        regex::bytes::Regex::new(re)?;
        return Ok(RevExpr::Message(re.to_owned()));
    }
    if let Some(rest) = spec.strip_prefix('^') {
        let inner = parse(rest)?;
        if inner.is_set() {
            return parse_err(spec, "cannot negate this");
        }
        return Ok(RevExpr::Not(Box::new(inner)));
    }
    if let Some(i) = top_level(spec, b':') {
        if i == 0 {
            return parse_err(spec, "index paths are not supported");
        }
        let Ok(path) = RepoPath::new(&spec.as_bytes()[i + 1..]) else {
            return parse_err(spec, "invalid path");
        };
        return Ok(RevExpr::Path(Box::new(parse_rev(&spec[..i])?), path));
    }
    if let Some(i) = spec.find("..") {
        let (tri, rest) = match spec[i..].strip_prefix("...") {
            Some(r) => (true, r),
            None => (false, &spec[i + 2..]),
        };
        let side = |s: &str| {
            if s.is_empty() {
                Ok(head())
            } else {
                parse_rev(s)
            }
        };
        let (a, b) = (Box::new(side(&spec[..i])?), Box::new(side(rest)?));
        return Ok(if tri {
            RevExpr::TriDot(a, b)
        } else {
            RevExpr::DotDot(a, b)
        });
    }
    parse_rev(spec)
}

/// Parses several arguments (`^A B`) into one expression.
pub fn parse_args(args: &[&str]) -> Result<RevExpr> {
    match args {
        [one] => parse(one),
        _ => args
            .iter()
            .map(|a| parse(a))
            .collect::<Result<_>>()
            .map(RevExpr::List),
    }
}

/// Index of the first `c` outside `{}`.
fn top_level(s: &str, c: u8) -> Option<usize> {
    let mut depth = 0u32;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            _ if b == c && depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Reads decimal digits at `*at`, advancing past them.
fn digits(s: &str, at: &mut usize) -> Option<u32> {
    let b = s.as_bytes();
    let start = *at;
    while b.get(*at).is_some_and(u8::is_ascii_digit) {
        *at += 1;
    }
    if start == *at {
        None
    } else {
        s[start..*at].parse().ok()
    }
}

/// A single revision: base, `@{..}` and suffix operators.
fn parse_rev(s: &str) -> Result<RevExpr> {
    let b = s.as_bytes();
    let end = (0..b.len())
        .find(|&i| b[i] == b'^' || b[i] == b'~' || (b[i] == b'@' && b.get(i + 1) == Some(&b'{')))
        .unwrap_or(b.len());
    let base = &s[..end];
    if base.contains(['{', '}', ':', ' ']) || base.contains("..") {
        return parse_err(s, "invalid name");
    }
    let mut at = end;
    let mut expr;
    if s[at..].starts_with("@{") {
        let Some(close) = s[at..].find('}').map(|i| at + i) else {
            return parse_err(s, "unterminated `@{`");
        };
        let inner = &s[at + 2..close];
        let name = (!base.is_empty()).then(|| base.to_owned());
        expr = if inner.eq_ignore_ascii_case("u") || inner.eq_ignore_ascii_case("upstream") {
            RevExpr::Upstream(name)
        } else if let Ok(n) = inner.parse::<u32>() {
            RevExpr::Reflog(name, n)
        } else {
            return parse_err(s, "unsupported `@{}` form");
        };
        at = close + 1;
    } else if base.is_empty() {
        return parse_err(s, "empty name");
    } else {
        expr = RevExpr::Name(base.to_owned());
    }
    while at < b.len() {
        let op = b[at];
        at += 1;
        let boxed = Box::new(expr);
        expr = match op {
            b'~' => RevExpr::Ancestor(boxed, digits(s, &mut at).unwrap_or(1)),
            b'^' if b.get(at) == Some(&b'{') => {
                let Some(close) = s[at..].find('}') else {
                    return parse_err(s, "unterminated `^{`");
                };
                let peel = match &s[at + 1..at + close] {
                    "tree" => Peel::Tree,
                    "commit" | "" => Peel::Commit,
                    _ => return parse_err(s, "unsupported `^{}` form"),
                };
                at += close + 1;
                RevExpr::Peel(boxed, peel)
            }
            b'^' if matches!(b.get(at), Some(b'@' | b'!')) => {
                at += 1;
                if at != b.len() {
                    return parse_err(s, "`^@` and `^!` must end the revision");
                }
                if b[at - 1] == b'@' {
                    RevExpr::Parents(boxed)
                } else {
                    RevExpr::Only(boxed)
                }
            }
            b'^' => RevExpr::Parent(boxed, digits(s, &mut at).unwrap_or(1)),
            _ => return parse_err(s, "unexpected character"),
        };
    }
    Ok(expr)
}

/// A resolved revision expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    Single(ChangesetId),
    /// `rev^{tree}`.
    Tree(TreeId),
    /// Commits reachable from `include` and not from `exclude`.
    Range {
        include: Vec<ChangesetId>,
        exclude: Vec<ChangesetId>,
    },
    /// `rev:path`; the path is not looked up.
    TreePath(ChangesetId, RepoPath),
}

/// Ref names git tries for a short name, in order (SPEC §4.2).
pub fn dwim_candidates(name: &str) -> [String; 6] {
    [
        name.to_owned(),
        format!("refs/{name}"),
        format!("refs/tags/{name}"),
        format!("refs/heads/{name}"),
        format!("refs/remotes/{name}"),
        format!("refs/remotes/{name}/HEAD"),
    ]
}

/// Everything resolution needs from a repository, beyond the commit graph.
#[async_trait]
pub trait Resolver: GraphSource {
    /// The ref `name` names, trying [`dwim_candidates`] in order. `HEAD` is a ref.
    async fn resolve_ref(&self, name: &str) -> Result<Option<RefTarget>>;
    /// The `n`-th previous value of `name` (`HEAD` for the current branch's log); 0 is current.
    async fn reflog(&self, name: &str, n: u32) -> Result<Option<ChangesetId>>;
    /// The upstream tip of `branch` (`HEAD` for the current branch).
    async fn upstream(&self, branch: &str) -> Result<Option<ChangesetId>>;
    /// Commits whose git SHA starts with the hex `prefix`.
    async fn git_to_asc(&self, prefix: &str) -> Result<Vec<ChangesetId>>;
    /// Changesets whose asc id starts with the hex `prefix`.
    async fn asc_prefix(&self, prefix: &str) -> Result<Vec<ChangesetId>>;
    /// The commit a tag points at.
    async fn peel_tag(&self, tag: TagId) -> Result<ChangesetId>;
    /// Every ref tip, for `:/regex`.
    async fn tips(&self) -> Result<Vec<ChangesetId>>;
}

/// Borrow of a resolver as a graph source, for [`Graph`].
struct Src<'a, R: ?Sized>(&'a R);

#[async_trait]
impl<R: Resolver + ?Sized> GraphSource for Src<'_, R> {
    async fn get(&self, id: ChangesetId) -> graph::Result<Node> {
        self.0.get(id).await
    }
    async fn changeset(&self, id: ChangesetId) -> graph::Result<Arc<Changeset>> {
        self.0.changeset(id).await
    }
    fn reads(&self) -> u64 {
        self.0.reads()
    }
}

/// Resolves `expr` against `r`.
pub async fn resolve<R: Resolver + ?Sized>(r: &R, expr: &RevExpr) -> Result<Resolved> {
    if expr.is_set() {
        let (include, exclude) = set(r, expr).await?;
        return Ok(Resolved::Range { include, exclude });
    }
    match expr {
        RevExpr::Peel(inner, Peel::Tree) => {
            let id = commit(r, inner).await?;
            Ok(Resolved::Tree(r.changeset(id).await?.root))
        }
        RevExpr::Path(inner, path) => Ok(Resolved::TreePath(commit(r, inner).await?, path.clone())),
        _ => Ok(Resolved::Single(commit(r, expr).await?)),
    }
}

/// Parses and resolves one argument.
pub async fn resolve_str<R: Resolver + ?Sized>(r: &R, spec: &str) -> Result<Resolved> {
    resolve(r, &parse(spec)?).await
}

type Set = (Vec<ChangesetId>, Vec<ChangesetId>);

fn set<'a, R: Resolver + ?Sized>(r: &'a R, e: &'a RevExpr) -> BoxFuture<'a, Result<Set>> {
    Box::pin(async move {
        Ok(match e {
            RevExpr::DotDot(a, b) => (vec![commit(r, b).await?], vec![commit(r, a).await?]),
            RevExpr::TriDot(a, b) => {
                let (a, b) = (commit(r, a).await?, commit(r, b).await?);
                let bases = Graph::new(Src(r)).merge_base(a, &[b], true).await?;
                (vec![a, b], bases)
            }
            RevExpr::Not(x) => (Vec::new(), vec![commit(r, x).await?]),
            RevExpr::Parents(x) => (r.get(commit(r, x).await?).await?.parents, Vec::new()),
            RevExpr::Only(x) => {
                let id = commit(r, x).await?;
                (vec![id], r.get(id).await?.parents)
            }
            RevExpr::List(items) => {
                let (mut inc, mut exc) = (Vec::new(), Vec::new());
                for item in items {
                    let (i, x) = set(r, item).await?;
                    inc.extend(i);
                    exc.extend(x);
                }
                (inc, exc)
            }
            _ => (vec![commit(r, e).await?], Vec::new()),
        })
    })
}

fn commit<'a, R: Resolver + ?Sized>(
    r: &'a R,
    e: &'a RevExpr,
) -> BoxFuture<'a, Result<ChangesetId>> {
    Box::pin(async move {
        match e {
            RevExpr::Name(n) => name(r, n).await,
            RevExpr::Parent(x, 0) => commit(r, x).await,
            RevExpr::Parent(x, n) => {
                let id = commit(r, x).await?;
                let parents = r.get(id).await?.parents;
                parents
                    .get(*n as usize - 1)
                    .copied()
                    .ok_or(Error::NoParent { id, n: *n })
            }
            RevExpr::Ancestor(x, n) => {
                let mut id = commit(r, x).await?;
                for _ in 0..*n {
                    id = *r
                        .get(id)
                        .await?
                        .parents
                        .first()
                        .ok_or(Error::NoParent { id, n: 1 })?;
                }
                Ok(id)
            }
            RevExpr::Peel(x, Peel::Commit) => commit(r, x).await,
            RevExpr::Upstream(b) => {
                let b = b.as_deref().unwrap_or("HEAD");
                r.upstream(b)
                    .await?
                    .ok_or_else(|| Error::NotFound(format!("{b}@{{u}}")))
            }
            RevExpr::Reflog(b, n) => {
                let b = b.as_deref().unwrap_or("HEAD");
                r.reflog(b, *n)
                    .await?
                    .ok_or_else(|| Error::NotFound(format!("{b}@{{{n}}}")))
            }
            RevExpr::Message(re) => message(r, re).await,
            other => Err(Error::NotACommit(format!("{other:?}"))),
        }
    })
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn peel<R: Resolver + ?Sized>(r: &R, t: RefTarget) -> Result<ChangesetId> {
    match t {
        RefTarget::Changeset(c) => Ok(c),
        RefTarget::Tag(t) => r.peel_tag(t).await,
    }
}

/// A full id, a ref (DWIM), or an abbreviated asc id or git SHA, in that order.
async fn name<R: Resolver + ?Sized>(r: &R, n: &str) -> Result<ChangesetId> {
    let n = if n == "@" { "HEAD" } else { n };
    if n.len() == 64 && is_hex(n) {
        let id: ChangesetId = n.parse().expect("64 hex digits");
        return match r.get(id).await {
            Ok(_) => Ok(id),
            Err(graph::Error::Missing(_)) => Err(Error::NotFound(n.to_owned())),
            Err(e) => Err(e.into()),
        };
    }
    if n.len() == 40 && is_hex(n) {
        let found = r.git_to_asc(n).await?;
        if let Some(id) = unique(n, found)? {
            return Ok(id);
        }
    }
    if let Some(t) = r.resolve_ref(n).await? {
        return peel(r, t).await;
    }
    if is_hex(n) && HexPrefix::parse(n).is_ok() {
        let mut found = r.asc_prefix(n).await?;
        found.extend(r.git_to_asc(n).await?);
        found.sort();
        found.dedup();
        if let Some(id) = unique(n, found)? {
            return Ok(id);
        }
    }
    Err(Error::NotFound(n.to_owned()))
}

fn unique(spec: &str, mut found: Vec<ChangesetId>) -> Result<Option<ChangesetId>> {
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(Error::Ambiguous {
            spec: spec.to_owned(),
            candidates: found,
        }),
    }
}

/// The youngest commit reachable from any tip whose message matches, by generation.
async fn message<R: Resolver + ?Sized>(r: &R, pattern: &str) -> Result<ChangesetId> {
    let re = regex::bytes::Regex::new(pattern)?;
    let mut heap = BinaryHeap::new();
    let mut seen = HashSet::new();
    for tip in r.tips().await? {
        if seen.insert(tip) {
            heap.push((r.get(tip).await?.generation, tip));
        }
    }
    while let Some((_, id)) = heap.pop() {
        if re.is_match(&r.changeset(id).await?.message) {
            return Ok(id);
        }
        for p in r.get(id).await?.parents {
            if seen.insert(p) {
                heap.push((r.get(p).await?.generation, p));
            }
        }
    }
    Err(Error::NoMatch(pattern.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use asc_core::object::SortedMap;
    use asc_core::sig::Signature;

    use super::*;

    /// In-memory repository: r <- a <- {b, c} <- m <- d <- f.
    struct Fx {
        nodes: HashMap<ChangesetId, Node>,
        sets: HashMap<ChangesetId, Arc<Changeset>>,
        ids: HashMap<&'static str, ChangesetId>,
        git: Vec<(String, ChangesetId)>,
        refs: HashMap<String, RefTarget>,
        tags: HashMap<TagId, ChangesetId>,
        reflog: HashMap<&'static str, Vec<ChangesetId>>,
        upstream: HashMap<&'static str, ChangesetId>,
    }

    impl Fx {
        fn add(&mut self, key: &'static str, msg: &str, parents: &[&str], sha: &str) {
            let parents: Vec<ChangesetId> = parents.iter().map(|p| self.ids[p]).collect();
            let sig = Signature::new(b"A".to_vec(), b"a@x".to_vec(), 1, 0).unwrap();
            let cs = Changeset {
                parents: parents.clone(),
                root: TreeId::of(msg.as_bytes()),
                author: sig.clone(),
                committer: sig,
                message: msg.as_bytes().to_vec(),
                extra: SortedMap::new(Vec::new()).unwrap(),
                changes: SortedMap::new(Vec::new()).unwrap(),
            };
            let id = cs.id();
            let generation = parents
                .iter()
                .map(|p| self.nodes[p].generation)
                .max()
                .unwrap_or(0)
                + 1;
            self.nodes.insert(
                id,
                Node {
                    generation,
                    parents,
                },
            );
            self.sets.insert(id, Arc::new(cs));
            self.ids.insert(key, id);
            self.git.push((sha.to_owned(), id));
        }

        fn new() -> Self {
            let mut f = Fx {
                nodes: HashMap::new(),
                sets: HashMap::new(),
                ids: HashMap::new(),
                git: Vec::new(),
                refs: HashMap::new(),
                tags: HashMap::new(),
                reflog: HashMap::new(),
                upstream: HashMap::new(),
            };
            let sha = |p: &str| format!("{p}{}", "0".repeat(40 - p.len()));
            f.add("r", "init\n", &[], &sha("1111"));
            f.add("a", "add parser\n", &["r"], &sha("2222"));
            f.add("b", "fix: typo in parser\n", &["a"], &sha("dead0001"));
            f.add("c", "side feature\n", &["a"], &sha("3333"));
            f.add("m", "merge side\n", &["b", "c"], &sha("dead0002"));
            f.add("d", "fix: crash\n", &["m"], &sha("4444"));
            f.add("f", "later work\n", &["d"], &sha("5555"));
            let cs = |k: &str| RefTarget::Changeset(f.ids[k]);
            let t1 = TagId::of(b"t1");
            f.tags.insert(t1, f.ids["a"]);
            let refs = [
                ("HEAD", cs("f")),
                ("refs/heads/master", cs("f")),
                ("refs/heads/feature", cs("c")),
                ("refs/heads/dup", cs("c")),
                ("refs/tags/dup", cs("b")),
                ("refs/tags/v1", RefTarget::Tag(t1)),
                ("refs/remotes/origin/master", cs("d")),
                ("refs/remotes/up/HEAD", cs("r")),
            ];
            f.refs = refs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
            f.reflog
                .insert("HEAD", vec![f.ids["f"], f.ids["d"], f.ids["m"]]);
            f.reflog.insert("master", vec![f.ids["f"], f.ids["d"]]);
            f.upstream.insert("HEAD", f.ids["d"]);
            f.upstream.insert("master", f.ids["d"]);
            f
        }
    }

    #[async_trait]
    impl GraphSource for Fx {
        async fn get(&self, id: ChangesetId) -> graph::Result<Node> {
            self.nodes
                .get(&id)
                .cloned()
                .ok_or(graph::Error::Missing(id))
        }
        async fn changeset(&self, id: ChangesetId) -> graph::Result<Arc<Changeset>> {
            self.sets.get(&id).cloned().ok_or(graph::Error::Missing(id))
        }
        fn reads(&self) -> u64 {
            0
        }
    }

    #[async_trait]
    impl Resolver for Fx {
        async fn resolve_ref(&self, name: &str) -> Result<Option<RefTarget>> {
            Ok(dwim_candidates(name)
                .iter()
                .find_map(|c| self.refs.get(c).copied()))
        }
        async fn reflog(&self, name: &str, n: u32) -> Result<Option<ChangesetId>> {
            Ok(self
                .reflog
                .get(name)
                .and_then(|l| l.get(n as usize))
                .copied())
        }
        async fn upstream(&self, branch: &str) -> Result<Option<ChangesetId>> {
            Ok(self.upstream.get(branch).copied())
        }
        async fn git_to_asc(&self, prefix: &str) -> Result<Vec<ChangesetId>> {
            let p = prefix.to_ascii_lowercase();
            Ok(self
                .git
                .iter()
                .filter(|(s, _)| s.starts_with(&p))
                .map(|(_, id)| *id)
                .collect())
        }
        async fn asc_prefix(&self, prefix: &str) -> Result<Vec<ChangesetId>> {
            let p = prefix.to_ascii_lowercase();
            Ok(self
                .nodes
                .keys()
                .filter(|id| id.to_string().starts_with(&p))
                .copied()
                .collect())
        }
        async fn peel_tag(&self, tag: TagId) -> Result<ChangesetId> {
            Ok(self.tags[&tag])
        }
        async fn tips(&self) -> Result<Vec<ChangesetId>> {
            let mut out = Vec::new();
            for t in self.refs.values() {
                out.push(peel(self, *t).await?);
            }
            Ok(out)
        }
    }

    enum Want {
        S(&'static str),
        Tree(&'static str),
        Range(&'static [&'static str], &'static [&'static str]),
        Path(&'static str, &'static str),
        Fail(fn(&Error) -> bool),
    }
    use Want::*;

    /// Substitutes `{x}` with the full asc id of fixture node `x`.
    fn expand(fx: &Fx, spec: &str) -> String {
        let mut out = spec.to_owned();
        for (k, id) in &fx.ids {
            out = out.replace(&format!("{{{k}}}"), &id.to_string());
        }
        out
    }

    fn ids(fx: &Fx, names: &[&str]) -> Vec<ChangesetId> {
        let mut v: Vec<_> = names.iter().map(|n| fx.ids[n]).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn expression_table() {
        let fx = Fx::new();
        let git_c = fx
            .git
            .iter()
            .find(|(_, id)| *id == fx.ids["c"])
            .unwrap()
            .0
            .clone();
        let git_c = Box::leak(git_c.into_boxed_str());
        let f_prefix = Box::leak(fx.ids["f"].to_string()[..12].to_owned().into_boxed_str());
        let b_prefix = Box::leak(fx.git[2].0[..8].to_owned().into_boxed_str());
        let table: Vec<(&str, Want)> = vec![
            ("HEAD", S("f")),
            ("@", S("f")),
            ("master", S("f")),
            ("heads/master", S("f")),
            ("refs/heads/feature", S("c")),
            ("feature", S("c")),
            ("v1", S("a")),
            ("dup", S("b")),
            ("origin/master", S("d")),
            ("up", S("r")),
            ("HEAD~", S("d")),
            ("HEAD~2", S("m")),
            ("HEAD~3", S("b")),
            ("HEAD^", S("d")),
            ("HEAD~2^2", S("c")),
            ("HEAD~2^1", S("b")),
            ("HEAD~2^0", S("m")),
            ("{a}", S("a")),
            ("{m}^2", S("c")),
            ("{m}~2", S("a")),
            (f_prefix, S("f")),
            (git_c, S("c")),
            (b_prefix, S("b")),
            ("HEAD^{commit}", S("f")),
            ("HEAD^{}", S("f")),
            ("v1^{}", S("a")),
            ("HEAD^{tree}", Tree("f")),
            ("@{u}", S("d")),
            ("@{upstream}", S("d")),
            ("master@{u}", S("d")),
            ("@{1}", S("d")),
            ("master@{1}", S("d")),
            (":/fix", S("d")),
            (":/typo", S("b")),
            (":/^side", S("c")),
            ("HEAD:d/g.txt", Path("f", "d/g.txt")),
            ("v1:README", Path("a", "README")),
            ("v1..HEAD", Range(&["f"], &["a"])),
            ("{b}...{c}", Range(&["b", "c"], &["a"])),
            ("feature...master", Range(&["c", "f"], &["c"])),
            ("HEAD^!", Range(&["f"], &["d"])),
            ("{m}^@", Range(&["b", "c"], &[])),
            ("{m}^!", Range(&["m"], &["b", "c"])),
            ("..feature", Range(&["c"], &["f"])),
            ("feature..", Range(&["f"], &["c"])),
            ("^v1", Range(&[], &["a"])),
            ("nosuch", Fail(|e| matches!(e, Error::NotFound(_)))),
            (
                "HEAD^2",
                Fail(|e| matches!(e, Error::NoParent { n: 2, .. })),
            ),
            ("HEAD~9", Fail(|e| matches!(e, Error::NoParent { .. }))),
            (
                "dead",
                Fail(|e| matches!(e, Error::Ambiguous { candidates, .. } if candidates.len() == 2)),
            ),
            (":/nonesuch", Fail(|e| matches!(e, Error::NoMatch(_)))),
            ("HEAD~x", Fail(|e| matches!(e, Error::Parse { .. }))),
            ("HEAD^{blob}", Fail(|e| matches!(e, Error::Parse { .. }))),
        ];
        assert!(table.len() >= 40);
        for (spec, want) in table {
            let got = resolve_str(&fx, &expand(&fx, spec)).await;
            match (want, got) {
                (S(n), Ok(Resolved::Single(id))) => assert_eq!(id, fx.ids[n], "{spec}"),
                (Tree(n), Ok(Resolved::Tree(t))) => {
                    assert_eq!(t, fx.sets[&fx.ids[n]].root, "{spec}")
                }
                (Path(n, p), Ok(Resolved::TreePath(id, path))) => {
                    assert_eq!((id, path.as_bytes()), (fx.ids[n], p.as_bytes()), "{spec}")
                }
                (
                    Range(i, x),
                    Ok(Resolved::Range {
                        mut include,
                        mut exclude,
                    }),
                ) => {
                    include.sort();
                    exclude.sort();
                    assert_eq!((include, exclude), (ids(&fx, i), ids(&fx, x)), "{spec}");
                }
                (Fail(check), Err(e)) => assert!(check(&e), "{spec}: {e}"),
                (_, other) => panic!("{spec}: unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn ambiguity_message_lists_candidates() {
        let fx = Fx::new();
        let msg = resolve_str(&fx, "dead").await.unwrap_err().to_string();
        assert!(
            msg.contains(&fx.ids["b"].short()) && msg.contains(&fx.ids["m"].short()),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn negated_arguments_form_a_range() {
        let fx = Fx::new();
        let e = parse_args(&["^v1", "HEAD"]).unwrap();
        let got = resolve(&fx, &e).await.unwrap();
        assert_eq!(
            got,
            Resolved::Range {
                include: vec![fx.ids["f"]],
                exclude: vec![fx.ids["a"]]
            }
        );
    }

    #[test]
    fn parser_structure() {
        let n = |s: &str| Box::new(RevExpr::Name(s.into()));
        assert_eq!(
            parse("HEAD~2^2").unwrap(),
            RevExpr::Parent(Box::new(RevExpr::Ancestor(n("HEAD"), 2)), 2)
        );
        assert_eq!(
            parse("a^{tree}").unwrap(),
            RevExpr::Peel(n("a"), Peel::Tree)
        );
        assert_eq!(parse("a...b").unwrap(), RevExpr::TriDot(n("a"), n("b")));
        assert_eq!(parse("a..b").unwrap(), RevExpr::DotDot(n("a"), n("b")));
        assert_eq!(parse("x^!").unwrap(), RevExpr::Only(n("x")));
        assert_eq!(parse("@{u}").unwrap(), RevExpr::Upstream(None));
        assert_eq!(
            parse("m@{3}").unwrap(),
            RevExpr::Reflog(Some("m".into()), 3)
        );
        assert_eq!(parse(":/a b").unwrap(), RevExpr::Message("a b".into()));
    }

    #[test]
    fn parser_rejects_malformed() {
        for bad in [
            "",
            "^^a",
            ":/",
            "a^!x",
            "a@{yesterday}",
            ":path",
            "a^{tree",
            "~1",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
