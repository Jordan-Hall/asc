//! The history walk behind `log`: git's own order ([`Ordered`]) for plain walks, or pages from a
//! [`HistoryApi`] (the local engine, or the server's when the default remote is one) when a
//! path filter, `--follow` or a server decides the walk. The filters git applies per commit
//! (author, committer, message, parents, `--until`) are applied here to both.
//!
//! A local `--follow` walks as git's does: the whole history in git's order, one path followed
//! from commit to commit and moved to the source when a commit adds it by a rename (recorded, or
//! found by content as git's `-M50%` finds it), merges shown only with `--first-parent`, `--skip`
//! counted over every commit walked and `-n` over the shown. With `--graph` each shown commit keeps its real parents and a hidden one still passes
//! through the graph, which prints git's `...` before the next shown commit. A server history's
//! graph is drawn from its own entries, sorted in graph order.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use asc_core::id::{ChangesetId, GitOid};
use asc_core::object::{Change, Changeset};
use asc_core::path::RepoPath;
use asc_engine::graph::LogQuery;
use asc_engine::history::{HistoryApi, LogEntry, PageToken};

use super::super::blame::compute::{content, entry_at, is_file, rename_source};
use super::opts::Opts;
use super::order::{Ordered, Sort};
use super::revs::Selection;
use crate::cli::common::{CliError, Result, WsCtx};
use crate::ext::{self, RemoteSpec};

/// The history service for this workspace: the server's when the default remote has one,
/// else the local engine.
pub struct Hist(Option<Arc<dyn HistoryApi>>);

impl Hist {
    pub fn select(ws: &WsCtx) -> Self {
        let spec = RemoteSpec {
            kind: ws.repo.repo.remote_kind,
            url: ws.repo.repo.remote_url.clone(),
            pack_cache: None,
        };
        Self(ext::history_selector().select(&spec).map(|(h, _)| h))
    }

    pub fn api<'a>(&'a self, ws: &'a WsCtx) -> &'a dyn HistoryApi {
        match &self.0 {
            Some(h) => h.as_ref(),
            None => &ws.engine,
        }
    }

    /// Whether a server answers history.
    pub fn is_remote(&self) -> bool {
        self.0.is_some()
    }
}

/// One commit of the walk, loaded.
#[derive(Clone)]
pub struct Entry {
    pub id: ChangesetId,
    pub git: GitOid,
    pub generation: u32,
    pub cs: Arc<Changeset>,
    /// The parents the graph draws (those inside the range, the first only with
    /// `--first-parent`).
    pub shown_parents: Vec<ChangesetId>,
    /// The `<` side of `A...B`.
    pub left: bool,
    /// Under `--follow`, the paths its diff shows: the followed path, and the rename's
    /// source when the commit moved it.
    pub followed: Vec<RepoPath>,
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// Seconds for `--since`/`--until` text, as git's approxidate reads it.
fn date_seconds(text: &str) -> Result<i64> {
    // git reads a bare date as that day at the current time of day.
    let b = text.as_bytes();
    let bare = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().filter(|c| c.is_ascii_digit()).count() == 8;
    let filled;
    let text = if bare {
        let now = gix_date::Zoned::now();
        filled = format!(
            "{text} {:02}:{:02}:{:02}",
            now.hour(),
            now.minute(),
            now.second()
        );
        filled.as_str()
    } else {
        text
    };
    gix_date::parse(text, Some(gix_date::Zoned::now()))
        .map(|t| t.seconds)
        .map_err(|_| CliError::Fatal(format!("invalid date format: {text}")))
}

/// Translates a POSIX basic regular expression to the `regex` crate's syntax.
pub fn bre_to_regex(bre: &str) -> String {
    let mut out = String::new();
    let mut chars = bre.chars().peekable();
    let mut at_start = true;
    while let Some(c) = chars.next() {
        let was_start = std::mem::replace(&mut at_start, false);
        match c {
            '\\' => match chars.next() {
                Some(n @ ('(' | '|')) => {
                    out.push(n);
                    at_start = true;
                }
                Some(n @ (')' | '{' | '}' | '+' | '?')) => out.push(n),
                Some(n) => {
                    out.push('\\');
                    out.push(n);
                }
                None => out.push_str("\\\\"),
            },
            '(' | ')' | '{' | '}' | '|' | '+' | '?' => {
                out.push('\\');
                out.push(c);
            }
            '*' if was_start => out.push_str("\\*"),
            '^' => {
                out.push(c);
                at_start = was_start;
            }
            '[' => {
                out.push(c);
                if chars.peek() == Some(&'^') {
                    out.extend(chars.next());
                }
                if chars.peek() == Some(&']') {
                    out.push_str("\\]");
                    chars.next();
                }
                while let Some(b) = chars.next() {
                    if b == '[' && matches!(chars.peek(), Some(':' | '.' | '=')) {
                        let kind = chars.next().unwrap_or(':');
                        out.push('[');
                        out.push(kind);
                        let mut prev = kind;
                        for k in chars.by_ref() {
                            out.push(k);
                            if k == ']' && prev == kind {
                                break;
                            }
                            prev = k;
                        }
                        continue;
                    }
                    if b == '\\' {
                        out.push_str("\\\\");
                        continue;
                    }
                    out.push(b);
                    if b == ']' {
                        break;
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The `--grep` patterns as one regular expression (alternation), or `None`.
pub fn grep_pattern(opts: &Opts) -> Option<String> {
    if opts.greps.is_empty() {
        return None;
    }
    let parts: Vec<String> = opts
        .greps
        .iter()
        .map(|g| {
            if opts.fixed {
                regex::escape(g)
            } else if opts.extended {
                g.clone()
            } else {
                bre_to_regex(g)
            }
        })
        .collect();
    let joined = if parts.len() == 1 {
        parts[0].clone()
    } else {
        parts
            .iter()
            .map(|p| format!("(?:{p})"))
            .collect::<Vec<_>>()
            .join("|")
    };
    Some(if opts.icase {
        format!("(?i){joined}")
    } else {
        joined
    })
}

/// The per-commit filters.
struct Filter {
    authors: Vec<Vec<u8>>,
    committers: Vec<Vec<u8>>,
    grep: Option<regex::bytes::Regex>,
    invert: bool,
    until: Option<i64>,
    min_parents: Option<usize>,
    max_parents: Option<usize>,
}

impl Filter {
    fn new(opts: &Opts) -> Result<Self> {
        let grep = grep_pattern(opts)
            .map(|g| {
                regex::bytes::Regex::new(&g)
                    .map_err(|e| fatal(format!("invalid regular expression: {e}")))
            })
            .transpose()?;
        Ok(Self {
            authors: opts.authors.iter().map(|a| a.as_bytes().to_vec()).collect(),
            committers: opts
                .committers
                .iter()
                .map(|c| c.as_bytes().to_vec())
                .collect(),
            grep,
            invert: opts.invert_grep,
            until: opts.until.as_deref().map(date_seconds).transpose()?,
            min_parents: opts.min_parents,
            max_parents: opts.max_parents,
        })
    }

    /// Whether anything is left to check after the walk (so skip and limit cannot be pushed
    /// into it).
    fn is_empty(&self) -> bool {
        self.authors.is_empty()
            && self.committers.is_empty()
            && self.grep.is_none()
            && self.until.is_none()
            && self.min_parents.is_none()
            && self.max_parents.is_none()
    }

    fn keeps(&self, cs: &Changeset) -> bool {
        let who = |s: &asc_core::sig::Signature| [s.name(), b" <", s.email(), b">"].concat();
        let has = |hay: &[u8], needle: &[u8]| {
            needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
        };
        if !self.authors.is_empty() {
            let a = who(&cs.author);
            if !self.authors.iter().any(|n| has(&a, n)) {
                return false;
            }
        }
        if !self.committers.is_empty() {
            let c = who(&cs.committer);
            if !self.committers.iter().any(|n| has(&c, n)) {
                return false;
            }
        }
        if self.until.is_some_and(|u| cs.committer.time() > u) {
            return false;
        }
        if self.min_parents.is_some_and(|m| cs.parents.len() < m)
            || self.max_parents.is_some_and(|m| cs.parents.len() > m)
        {
            return false;
        }
        match &self.grep {
            Some(re) => re.is_match(&cs.message) != self.invert,
            None => true,
        }
    }
}

/// Where commits come from.
enum Source<'a> {
    /// git's order; with a path, only the commits `--follow` shows under it.
    Ordered(Box<Ordered<'a>>, Option<RepoPath>),
    /// A whole server history, already in graph order.
    Sorted(VecDeque<LogEntry>),
    Paged {
        api: &'a dyn HistoryApi,
        query: Box<LogQuery>,
        page: Option<PageToken>,
        queue: VecDeque<LogEntry>,
        started: bool,
        exhausted: bool,
    },
}

/// One step of the walk.
pub enum Walked {
    Shown(Entry),
    /// A commit `--follow` walked but does not show, with the parents the graph draws.
    Hidden(ChangesetId, Vec<ChangesetId>),
}

/// A streaming walk over the selected history.
pub struct Walker<'a> {
    ws: &'a WsCtx,
    src: Source<'a>,
    filter: Filter,
    first_parent: bool,
    skip: usize,
    limit: Option<usize>,
    /// Hidden commits are returned (to the graph) rather than passed over.
    hidden: bool,
}

impl<'a> Walker<'a> {
    pub async fn new(ws: &'a WsCtx, hist: &'a Hist, sel: &Selection, opts: &Opts) -> Result<Self> {
        let filter = Filter::new(opts)?;
        let since = opts.since.as_deref().map(date_seconds).transpose()?;
        // The graph of a pathspec simplifies history in the local walk, as git's does.
        let simplify = opts.graph && !sel.paths.is_empty() && !opts.follow && !hist.is_remote();
        let paged = (!sel.paths.is_empty() || opts.follow || hist.is_remote()) && !simplify;
        if opts.follow && sel.paths.len() != 1 {
            return Err(CliError::Raw(
                "fatal: --follow requires exactly one pathspec".into(),
                128,
            ));
        }
        let local_follow = opts.follow && !hist.is_remote();
        if local_follow || (paged && opts.graph) {
            let src = if local_follow {
                let sort = match (opts.order, opts.graph) {
                    (Some(s), _) => s,
                    (None, true) => Sort::Topo,
                    (None, false) => Sort::Date,
                };
                let all = Ordered::new(ws, sel, opts.first_parent, sort, since, &[]).await?;
                Source::Ordered(Box::new(all), Some(sel.paths[0].clone()))
            } else {
                let query = LogQuery {
                    include: sel.include.clone(),
                    exclude: sel.exclude.clone(),
                    first_parent: opts.first_parent,
                    paths: sel.paths.clone(),
                    follow: opts.follow,
                    since,
                    ..LogQuery::default()
                };
                Source::Sorted(graph_sorted(drain(hist.api(ws), &query).await?))
            };
            return Ok(Self {
                ws,
                src,
                filter,
                first_parent: opts.first_parent,
                skip: opts.skip,
                limit: opts.max_count,
                hidden: opts.graph,
            });
        }
        let (mut skip, mut limit) = (opts.skip, opts.max_count);
        let src = if paged {
            let mut query = LogQuery {
                include: sel.include.clone(),
                exclude: sel.exclude.clone(),
                first_parent: opts.first_parent,
                paths: sel.paths.clone(),
                follow: opts.follow,
                since,
                ..LogQuery::default()
            };
            if filter.is_empty() {
                query.skip = skip;
                query.limit = limit;
                (skip, limit) = (0, None);
            }
            Source::Paged {
                api: hist.api(ws),
                query: Box::new(query),
                page: None,
                queue: VecDeque::new(),
                started: false,
                exhausted: false,
            }
        } else {
            let sort = match (opts.order, opts.graph) {
                (Some(s), _) => s,
                (None, true) => Sort::Topo,
                (None, false) => Sort::Date,
            };
            Source::Ordered(
                Box::new(Ordered::new(ws, sel, opts.first_parent, sort, since, &sel.paths).await?),
                None,
            )
        };
        Ok(Self {
            ws,
            src,
            filter,
            first_parent: opts.first_parent,
            skip,
            limit,
            hidden: false,
        })
    }

    async fn load(
        &self,
        id: ChangesetId,
        git: Option<GitOid>,
        generation: u32,
    ) -> Result<(Arc<Changeset>, GitOid, u32)> {
        let cs = self.ws.engine.get_changeset(id).await?;
        let git = match git {
            Some(g) => g,
            None => self.ws.engine.git_commit_of(id).await.map_err(fatal)?,
        };
        Ok((cs, git, generation))
    }

    /// The next commit shown, or `None` at the end.
    pub async fn next(&mut self) -> Result<Option<Entry>> {
        loop {
            match self.walk().await? {
                Some(Walked::Shown(e)) => return Ok(Some(e)),
                Some(Walked::Hidden(..)) => {}
                None => return Ok(None),
            }
        }
    }

    /// The next commit shown, or hidden when the graph wants it; `None` at the end.
    pub async fn walk(&mut self) -> Result<Option<Walked>> {
        loop {
            if self.limit == Some(0) {
                return Ok(None);
            }
            let (id, git, generation, shown, left) = match &mut self.src {
                Source::Ordered(o, _) => {
                    let Some(item) = o.next().await? else {
                        return Ok(None);
                    };
                    let shown = o.shown_parents(&item);
                    (item.id, None, 0, Some(shown), item.left)
                }
                Source::Sorted(queue) => {
                    let Some(e) = queue.pop_front() else {
                        return Ok(None);
                    };
                    (e.id, e.git, e.generation, None, false)
                }
                Source::Paged {
                    api,
                    query,
                    page,
                    queue,
                    started,
                    exhausted,
                } => {
                    let e = match queue.pop_front() {
                        Some(e) => e,
                        None => {
                            if *exhausted {
                                return Ok(None);
                            }
                            let got = api
                                .log(query, if *started { page.as_ref() } else { None })
                                .await
                                .map_err(fatal)?;
                            *started = true;
                            *exhausted = got.next.is_none();
                            *page = got.next;
                            queue.extend(got.entries);
                            continue;
                        }
                    };
                    (e.id, e.git, e.generation, None, false)
                }
            };
            let (cs, git, generation) = self.load(id, git, generation).await?;
            if !self.filter.keeps(&cs) {
                continue;
            }
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            let shown_parents = shown.unwrap_or_else(|| {
                let take = if self.first_parent {
                    1
                } else {
                    cs.parents.len()
                };
                cs.parents[..take.min(cs.parents.len())].to_vec()
            });
            let (ws, first_parent) = (self.ws, self.first_parent);
            let mut followed = Vec::new();
            let hide = match &mut self.src {
                Source::Ordered(_, Some(path)) => {
                    followed.push(path.clone());
                    let shown = follow(ws, id, &cs, path, first_parent).await?;
                    if *path != followed[0] {
                        followed.push(path.clone());
                    }
                    !shown
                }
                _ => false,
            };
            if hide {
                if self.hidden {
                    return Ok(Some(Walked::Hidden(id, shown_parents)));
                }
                continue;
            }
            if let Some(l) = &mut self.limit {
                *l -= 1;
            }
            return Ok(Some(Walked::Shown(Entry {
                id,
                git,
                generation,
                cs,
                shown_parents,
                left,
                followed,
            })));
        }
    }
}

/// One commit of git's `--follow` walk: whether `cs` shows under `path`, which moves to the
/// file `cs` renamed into it. A merge shows (diffed against its first parent) only with
/// `--first-parent`; git's log does not diff it otherwise.
async fn follow(
    ws: &WsCtx,
    id: ChangesetId,
    cs: &Changeset,
    path: &mut RepoPath,
    first_parent: bool,
) -> Result<bool> {
    if cs.parents.len() > 1 && !first_parent {
        return Ok(false);
    }
    let touched = cs
        .changes
        .as_slice()
        .iter()
        .any(|(k, _)| k == &*path || k.starts_with_dir(path) || path.starts_with_dir(k));
    if !touched {
        return Ok(false);
    }
    let Some(&parent) = cs.parents.first() else {
        return Ok(true);
    };
    let Some(Change::Write { copy_from, .. }) = cs.changes.get(path) else {
        return Ok(true);
    };
    if entry_at(ws, parent, path).await?.is_some() {
        return Ok(true);
    }
    // Added here: a rename moves the followed path to its source.
    let source = match copy_from {
        Some(src) if matches!(cs.changes.get(src), Some(Change::Delete)) => Some(src.clone()),
        _ => match ws.engine.lookup(cs.root, path).await?.filter(is_file) {
            Some(e) => {
                let data = content(ws, &e).await?;
                rename_source(ws, id, path, &data).await?.map(|(_, p, _)| p)
            }
            None => None,
        },
    };
    if let Some(src) = source {
        *path = src;
    }
    Ok(true)
}

/// The most entries a graph of a paged history holds.
const MAX_GRAPH: usize = 1 << 20;

/// Every entry of a paged log, in the service's order.
async fn drain(api: &dyn HistoryApi, query: &LogQuery) -> Result<Vec<LogEntry>> {
    let mut out = Vec::new();
    let mut page: Option<PageToken> = None;
    loop {
        let got = api.log(query, page.as_ref()).await.map_err(fatal)?;
        out.extend(got.entries);
        if out.len() > MAX_GRAPH {
            return Err(CliError::Fatal(
                "asc: the history is too long to draw as a graph".into(),
            ));
        }
        match got.next {
            Some(next) => page = Some(next),
            None => return Ok(out),
        }
    }
}

/// git's topological sort in graph order (`REV_SORT_IN_GRAPH_ORDER`): the tips in the given
/// order, a commit once every child among `items` came, the parent pushed last taken first.
/// Parents outside `items` are ignored.
pub(crate) fn graph_order<T>(
    items: Vec<T>,
    id: impl Fn(&T) -> ChangesetId,
    parents: impl Fn(&T) -> &[ChangesetId],
) -> Vec<T> {
    let index: HashMap<ChangesetId, usize> =
        items.iter().enumerate().map(|(n, t)| (id(t), n)).collect();
    let mut children = vec![0usize; items.len()];
    for t in &items {
        for p in parents(t) {
            if let Some(&n) = index.get(p) {
                children[n] += 1;
            }
        }
    }
    let mut stack: Vec<usize> = (0..items.len())
        .filter(|&n| children[n] == 0)
        .rev()
        .collect();
    let mut order: Vec<usize> = Vec::with_capacity(items.len());
    while let Some(n) = stack.pop() {
        order.push(n);
        for p in parents(&items[n]) {
            if let Some(&m) = index.get(p) {
                children[m] -= 1;
                if children[m] == 0 {
                    stack.push(m);
                }
            }
        }
    }
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    order.into_iter().filter_map(|n| slots[n].take()).collect()
}

fn graph_sorted(entries: Vec<LogEntry>) -> VecDeque<LogEntry> {
    graph_order(entries, |e| e.id, |e| &e.parents).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u8, parents: &[u8]) -> LogEntry {
        LogEntry {
            id: ChangesetId::from_bytes([id; 32]),
            generation: 0,
            parents: parents
                .iter()
                .map(|&p| ChangesetId::from_bytes([p; 32]))
                .collect(),
            git: None,
        }
    }

    fn ids(q: VecDeque<LogEntry>) -> Vec<u8> {
        q.iter().map(|e| e.id.as_bytes()[0]).collect()
    }

    #[test]
    fn graph_sorted_puts_children_first_and_takes_the_last_parent_first() {
        // 5 merges 4 (first parent) and 3; the server lists them by date.
        let log = vec![
            entry(5, &[4, 3]),
            entry(3, &[1]),
            entry(4, &[2]),
            entry(2, &[1]),
            entry(1, &[]),
        ];
        assert_eq!(ids(graph_sorted(log)), [5, 3, 4, 2, 1]);
    }

    #[test]
    fn graph_sorted_keeps_tip_order_and_ignores_missing_parents() {
        // 9 and 8 are tips; 7's parent 6 is not among the entries.
        let log = vec![entry(9, &[7]), entry(8, &[7]), entry(7, &[6])];
        assert_eq!(ids(graph_sorted(log)), [9, 8, 7]);
        assert!(graph_sorted(Vec::new()).is_empty());
    }

    #[test]
    fn basic_regex_translation() {
        assert_eq!(bre_to_regex("a\\(b\\|c\\)+"), "a(b|c)\\+");
        assert_eq!(bre_to_regex("*a"), "\\*a");
        assert_eq!(bre_to_regex("x{2}"), "x\\{2\\}");
        assert_eq!(bre_to_regex("x\\{2\\}"), "x{2}");
        assert_eq!(bre_to_regex("[a-c]+"), "[a-c]\\+");
        assert_eq!(bre_to_regex("^fix.*$"), "^fix.*$");
        assert!(regex::Regex::new(&bre_to_regex("[[:alpha:]]\\+")).is_ok());
    }

    #[test]
    fn grep_alternation_and_case() {
        let o = Opts {
            greps: vec!["a.c".into(), "x".into()],
            fixed: true,
            icase: true,
            ..Opts::default()
        };
        assert_eq!(grep_pattern(&o).unwrap(), "(?i)(?:a\\.c)|(?:x)");
    }
}
