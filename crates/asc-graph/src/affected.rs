//! The affected algebra (SPEC §12A.7 steps 3 to 5): a pure function of two
//! graphs, their manifest sets and the changed paths.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;

use asc_core::path::{Name, RepoPath};

use crate::config::{ProjectEntry, ProjectTarget, ResolveError};
use crate::detect::{Claim, Detector};
use crate::model::{Ecosystem, Fingerprint, Issue, ManifestSet, Project, ProjectGraph, ProjectKey};
use crate::own::Ownership;
use crate::pattern::{IgnoreSet, PatternError, PatternSet};
use crate::select::{SelectError, Selector, select};

/// How a path changed between the merge base and the head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Added or modified. A rename is a `Delete` of the old path plus a `Write` of the new one.
    Write,
    Delete,
    /// A gitlink was added, moved or removed.
    Gitlink,
}

/// One entry of the changed-path set `C` (step 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    pub path: RepoPath,
    pub kind: ChangeKind,
}

/// A graph with the manifest set it was linked from.
#[derive(Clone, Copy)]
pub struct Side<'a> {
    pub graph: &'a ProjectGraph,
    pub set: &'a ManifestSet,
}

/// The optional inputs of a query; the default is "everything, no target".
#[derive(Clone, Copy, Debug, Default)]
pub struct Query<'a> {
    pub target: Option<&'a str>,
    /// `--type`: report only these ecosystems (empty = all).
    pub ecosystems: &'a [Ecosystem],
    /// `--select`: report only the matching projects (empty = all).
    pub selectors: &'a [Selector],
}

/// Why a project is affected (SPEC §12A.7).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    Global(RepoPath),
    EcoGlobal(RepoPath),
    New,
    Config,
    Lockfile,
    Manifest,
    File(RepoPath),
    Gitlink(RepoPath),
    /// An affected dependency, by display id.
    Dep(String),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global(p) => write!(f, "global:{p}"),
            Self::EcoGlobal(p) => write!(f, "eco-global:{p}"),
            Self::New => f.write_str("new"),
            Self::Config => f.write_str("config"),
            Self::Lockfile => f.write_str("lockfile"),
            Self::Manifest => f.write_str("manifest"),
            Self::File(p) => write!(f, "file:{p}"),
            Self::Gitlink(p) => write!(f, "gitlink:{p}"),
            Self::Dep(id) => write!(f, "dep:{id}"),
        }
    }
}

/// A project named by key and display id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRef {
    pub key: ProjectKey,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffectedProject {
    pub project: ProjectRef,
    /// In the direct set `D`, not only reached through the closure.
    pub direct: bool,
    /// Sorted, unique.
    pub reasons: Vec<Reason>,
    /// Filled in by the engine (SPEC §12A.6) when asked for.
    pub input_hash: Option<Fingerprint>,
}

/// The result of step 5. The engine adds base, head, merge base and changed paths.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Affected {
    /// Every project is affected (step 3a or an `unowned = "all"` path).
    pub all: bool,
    /// Dependencies first, cycles as components, ties by display id.
    pub projects: Vec<AffectedProject>,
    /// In the base graph only: reported, never counted as affected.
    pub removed: Vec<ProjectRef>,
    /// Changed non-claimed paths no project owns.
    pub unowned: Vec<RepoPath>,
    pub issues: Vec<Issue>,
}

/// Why the algebra could not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AffectedError {
    #[error(transparent)]
    Pattern(#[from] PatternError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Select(#[from] SelectError),
}

/// Runs steps 3 to 5 of SPEC §12A.7.
///
/// `changed` is `C` of step 1: both sides of a rename, working-copy paths for
/// `--uncommitted`. `head` is the graph to report on, `base` the merge base's.
pub fn affected(
    detectors: &[&dyn Detector],
    base: Side<'_>,
    head: Side<'_>,
    changed: &[ChangedPath],
    query: &Query<'_>,
) -> Result<Affected, AffectedError> {
    let mut run = Run::new(detectors, base, head, query.target)?;
    let mut all = run.global_hit(changed)?;
    if all.is_none() {
        run.eco_globals(changed)?;
        run.new_and_changed();
        all = run.file_changes(changed)?;
    }
    match &all {
        Some(path) => run.mark_all(path),
        None => run.close()?,
    }
    run.finish(all.is_some(), query)
}

/// Where a pattern set came from: the base or the head graph.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Which {
    Base,
    Head,
}

/// A project's compiled inputs for the query's target.
struct Inputs {
    set: PatternSet,
    /// Its input list has a `^` item, so edges to its dependencies carry the closure.
    follows_deps: bool,
}

struct Run<'a> {
    detectors: &'a [&'a dyn Detector],
    base: Side<'a>,
    head: Side<'a>,
    target: Option<&'a str>,
    own_base: Ownership<'a>,
    own_head: Ownership<'a>,
    ids: Vec<String>,
    base_has: HashMap<&'a ProjectKey, usize>,
    inputs: HashMap<(Which, usize), Inputs>,
    extra: Vec<(usize, PatternSet)>,
    direct: Vec<bool>,
    reasons: Vec<Vec<Reason>>,
    unowned: BTreeSet<RepoPath>,
}

impl<'a> Run<'a> {
    fn new(
        detectors: &'a [&'a dyn Detector],
        base: Side<'a>,
        head: Side<'a>,
        target: Option<&'a str>,
    ) -> Result<Self, AffectedError> {
        let n = head.graph.projects.len();
        let mut extra = Vec::new();
        for (i, p) in head.graph.projects.iter().enumerate() {
            if !p.extra_inputs.is_empty() {
                let globs: Vec<String> = p
                    .extra_inputs
                    .iter()
                    .filter_map(|g| String::from_utf8(g.clone()).ok())
                    .collect();
                extra.push((
                    i,
                    PatternSet::new(globs.iter().map(String::as_str), [], None)?,
                ));
            }
        }
        Ok(Self {
            detectors,
            base,
            head,
            target,
            own_base: Ownership::new(base.graph, detectors)?,
            own_head: Ownership::new(head.graph, detectors)?,
            ids: head.graph.display_ids(),
            base_has: base
                .graph
                .projects
                .iter()
                .enumerate()
                .map(|(i, p)| (&p.key, i))
                .collect(),
            inputs: HashMap::new(),
            extra,
            direct: vec![false; n],
            reasons: vec![Vec::new(); n],
            unowned: BTreeSet::new(),
        })
    }

    fn projects(&self) -> &'a [Project] {
        &self.head.graph.projects
    }

    fn head_index(&self, key: &ProjectKey) -> Option<usize> {
        self.projects().binary_search_by(|p| p.key.cmp(key)).ok()
    }

    fn add(&mut self, i: usize, reason: Reason) {
        self.direct[i] = true;
        self.reasons[i].push(reason);
    }

    /// Compiled inputs of project `i` of one side, for the query's target.
    fn inputs(&mut self, which: Which, i: usize) -> Result<&Inputs, AffectedError> {
        if !self.inputs.contains_key(&(which, i)) {
            let graph = match which {
                Which::Base => self.base.graph,
                Which::Head => self.head.graph,
            };
            let p = &graph.projects[i];
            let entry = graph.config.projects.iter().find(|e| entry_applies(e, p));
            let r = graph.config.inputs_for(entry, self.target)?;
            let set = PatternSet::new(
                r.includes.iter().map(String::as_str),
                r.excludes.iter().map(String::as_str),
                p.root.as_ref(),
            )?;
            let follows_deps = !r.deps.is_empty();
            self.inputs.insert((which, i), Inputs { set, follows_deps });
        }
        Ok(&self.inputs[&(which, i)])
    }

    /// Step 3a: the first changed path that makes everything affected.
    fn global_hit(&self, changed: &[ChangedPath]) -> Result<Option<RepoPath>, AffectedError> {
        let globs = IgnoreSet::new(
            self.head
                .graph
                .config
                .global_inputs
                .iter()
                .map(String::as_str),
        )?;
        Ok(changed
            .iter()
            .map(|c| &c.path)
            .filter(|p| {
                matches!(p.as_bytes(), b".asc/projects.toml" | b".asc/ci.toml") || globs.is_ignored(p)
            })
            .min()
            .cloned())
    }

    fn mark_all(&mut self, path: &RepoPath) {
        for i in 0..self.projects().len() {
            self.add(i, Reason::Global(path.clone()));
        }
    }

    /// Step 3b: eco-global files and `[ecosystem.<e>] inputs`.
    fn eco_globals(&mut self, changed: &[ChangedPath]) -> Result<(), AffectedError> {
        let config = &self.head.graph.config;
        let mut by_config = Vec::new();
        for (eco, c) in &config.ecosystems {
            if c.enabled && !c.inputs.is_empty() {
                by_config.push((*eco, IgnoreSet::new(c.inputs.iter().map(String::as_str))?));
            }
        }
        for c in changed {
            let mut hits: Vec<(Ecosystem, Option<RepoPath>)> = Vec::new();
            if config.builtin_eco_globals {
                for d in self.detectors {
                    let eco = d.ecosystem();
                    if config.eco(eco).enabled && d.claim(&c.path) == Some(Claim::EcoGlobal) {
                        hits.push((eco, c.path.parent()));
                    }
                }
            }
            for (eco, globs) in &by_config {
                if globs.is_ignored(&c.path) {
                    hits.push((*eco, None));
                }
            }
            for (eco, dir) in hits {
                for i in 0..self.projects().len() {
                    let p = &self.projects()[i];
                    if p.key.eco == eco && under(dir.as_ref(), p.root.as_ref()) {
                        self.add(i, Reason::EcoGlobal(c.path.clone()));
                    }
                }
            }
        }
        Ok(())
    }

    /// Steps 3c and 3d: new projects and changed fingerprints or manifests.
    fn new_and_changed(&mut self) {
        for i in 0..self.projects().len() {
            let p = &self.projects()[i];
            let Some(&b) = self.base_has.get(&p.key) else {
                self.add(i, Reason::New);
                continue;
            };
            let old = &self.base.graph.projects[b];
            if old.config_fp != p.config_fp {
                self.add(i, Reason::Config);
            }
            if old.external_fp != p.external_fp {
                self.add(i, Reason::Lockfile);
            }
            let manifest = |s: &Side<'_>| set_id(s.set, &p.key.manifest);
            if manifest(&self.base) != manifest(&self.head)
                || old.deps != p.deps
                || old.extra_inputs != p.extra_inputs
            {
                self.add(i, Reason::Manifest);
            }
        }
    }

    /// Steps 3e and 3f. Returns a path that makes everything affected (`unowned = "all"`).
    fn file_changes(&mut self, changed: &[ChangedPath]) -> Result<Option<RepoPath>, AffectedError> {
        let mut by_gitlink: HashMap<&RepoPath, Vec<usize>> = HashMap::new();
        for (i, p) in self.projects().iter().enumerate() {
            if let Some(g) = &p.gitlink {
                by_gitlink.entry(g).or_default().push(i);
            }
        }
        let mut unowned_all = None;
        for c in changed {
            let mut extra_hit = false;
            for k in 0..self.extra.len() {
                if self.extra[k].1.matches(&c.path) {
                    extra_hit = true;
                    let i = self.extra[k].0;
                    self.add(i, Reason::File(c.path.clone()));
                }
            }
            if self.own_head.is_excluded(&c.path) {
                continue;
            }
            let owners = match c.kind {
                ChangeKind::Gitlink => {
                    for &i in by_gitlink.get(&c.path).into_iter().flatten() {
                        self.add(i, Reason::Gitlink(c.path.clone()));
                    }
                    self.owners_both(c, true)
                }
                _ if self.own_head.is_claimed(&c.path) => continue,
                ChangeKind::Write | ChangeKind::Delete => self.owners_both(c, false),
            };
            if owners.is_empty() {
                if c.kind != ChangeKind::Gitlink && !extra_hit {
                    self.unowned.insert(c.path.clone());
                    if self.head.graph.config.unowned == crate::config::Unowned::All {
                        unowned_all = Some(match unowned_all {
                            Some(first) if first <= c.path => first,
                            _ => c.path.clone(),
                        });
                    }
                }
                continue;
            }
            let probe = match c.kind {
                ChangeKind::Gitlink => {
                    Some(c.path.join(&Name::new("gitlink").expect("static name")))
                }
                _ => None,
            };
            for (which, i) in owners {
                let key = match which {
                    Which::Base => self.base.graph.projects[i].key.clone(),
                    Which::Head => self.projects()[i].key.clone(),
                };
                let Some(h) = self.head_index(&key) else {
                    continue;
                };
                let inputs = self.inputs(which, i)?;
                if inputs.set.matches(&c.path)
                    || probe.as_ref().is_some_and(|p| inputs.set.matches(p))
                {
                    let reason = if c.kind == ChangeKind::Gitlink {
                        Reason::Gitlink(c.path.clone())
                    } else {
                        Reason::File(c.path.clone())
                    };
                    self.add(h, reason);
                }
            }
        }
        Ok(unowned_all)
    }

    /// Owners of a changed path: in the head, plus in the base when the path
    /// was removed (a gitlink may have moved, so it checks both).
    fn owners_both(&self, c: &ChangedPath, gitlink: bool) -> Vec<(Which, usize)> {
        let find = |o: &Ownership<'_>| {
            if gitlink {
                o.owners_of_gitlink(&c.path)
            } else {
                o.owners(&c.path)
            }
        };
        let mut out: Vec<_> = find(&self.own_head)
            .into_iter()
            .map(|i| (Which::Head, i))
            .collect();
        if c.kind != ChangeKind::Write {
            out.extend(find(&self.own_base).into_iter().map(|i| (Which::Base, i)));
        }
        out
    }

    /// Step 4: every project that reaches a direct one along head edges.
    fn close(&mut self) -> Result<(), AffectedError> {
        let rev = reverse_adjacency(self.head.graph);
        let mut seen = self.direct.clone();
        let mut queue: VecDeque<usize> = (0..seen.len()).filter(|&i| seen[i]).collect();
        while let Some(q) = queue.pop_front() {
            for &p in &rev[q] {
                if self.target.is_some() && !self.inputs(Which::Head, p)?.follows_deps {
                    continue;
                }
                if !self.direct[p] {
                    let id = self.ids[q].clone();
                    self.reasons[p].push(Reason::Dep(id));
                }
                if !seen[p] {
                    seen[p] = true;
                    queue.push_back(p);
                }
            }
        }
        Ok(())
    }

    /// Step 5: removed projects, filters and order.
    fn finish(mut self, all: bool, query: &Query<'_>) -> Result<Affected, AffectedError> {
        let head = self.head.graph;
        let keep: Option<Vec<usize>> = if query.selectors.is_empty() {
            None
        } else {
            Some(select(&head.projects, query.selectors)?)
        };
        let mut projects = Vec::new();
        for i in topo_order(head, &self.ids) {
            let p = &head.projects[i];
            let wanted = (query.ecosystems.is_empty() || query.ecosystems.contains(&p.key.eco))
                && keep.as_ref().is_none_or(|k| k.binary_search(&i).is_ok());
            if self.reasons[i].is_empty() || !wanted {
                continue;
            }
            let mut reasons = std::mem::take(&mut self.reasons[i]);
            reasons.sort();
            reasons.dedup();
            projects.push(AffectedProject {
                project: ProjectRef {
                    key: p.key.clone(),
                    id: self.ids[i].clone(),
                },
                direct: self.direct[i],
                reasons,
                input_hash: None,
            });
        }
        let base_ids = self.base.graph.display_ids();
        let removed = self
            .base
            .graph
            .projects
            .iter()
            .zip(base_ids)
            .filter(|(p, _)| self.head_index(&p.key).is_none())
            .map(|(p, id)| ProjectRef {
                key: p.key.clone(),
                id,
            })
            .collect();
        Ok(Affected {
            all,
            projects,
            removed,
            unowned: self.unowned.into_iter().collect(),
            issues: head.issues.clone(),
        })
    }
}

fn entry_applies(e: &ProjectEntry, p: &Project) -> bool {
    match &e.target {
        ProjectTarget::Select(sel) => sel.matches(p),
        ProjectTarget::Declared { root, .. } => {
            p.key.eco == Ecosystem::Declared && p.key.manifest == *root
        }
    }
}

/// True when a project root lies in `dir` (`None` is the repository root, which holds everything).
fn under(dir: Option<&RepoPath>, root: Option<&RepoPath>) -> bool {
    match (dir, root) {
        (None, _) => true,
        (Some(d), Some(r)) => r == d || r.starts_with_dir(d),
        (Some(_), None) => false,
    }
}

fn set_id(set: &ManifestSet, path: &RepoPath) -> Option<[u8; 32]> {
    let e = set.entries();
    e.binary_search_by(|x| x.0.cmp(path)).ok().map(|i| e[i].2)
}

/// `adj[p]` lists the projects `p` depends on, as indexes into `graph.projects`.
pub fn adjacency(graph: &ProjectGraph) -> Vec<Vec<usize>> {
    graph
        .projects
        .iter()
        .map(|p| {
            p.deps
                .iter()
                .filter_map(|(k, _)| graph.projects.binary_search_by(|q| q.key.cmp(k)).ok())
                .collect()
        })
        .collect()
}

/// `rev[q]` lists the projects that depend on `q`.
pub fn reverse_adjacency(graph: &ProjectGraph) -> Vec<Vec<usize>> {
    let mut rev = vec![Vec::new(); graph.projects.len()];
    for (p, deps) in adjacency(graph).into_iter().enumerate() {
        for q in deps {
            rev[q].push(p);
        }
    }
    rev
}

/// Strongly connected components of the dependency graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sccs {
    /// Component of each project.
    pub comp: Vec<usize>,
    /// Members of each component, ascending. A component comes after every
    /// component it depends on.
    pub members: Vec<Vec<usize>>,
}

/// Tarjan's algorithm, iterative so deep chains cannot overflow the stack.
pub fn sccs(adj: &[Vec<usize>]) -> Sccs {
    const NONE: usize = usize::MAX;
    let n = adj.len();
    let mut index = vec![NONE; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut comp = vec![NONE; n];
    let mut members: Vec<Vec<usize>> = Vec::new();
    let (mut stack, mut counter) = (Vec::new(), 0);
    for root in 0..n {
        if index[root] != NONE {
            continue;
        }
        let mut calls = vec![(root, 0usize)];
        index[root] = counter;
        low[root] = counter;
        counter += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&(v, next)) = calls.last() {
            if let Some(&w) = adj[v].get(next) {
                calls.last_mut().expect("non-empty").1 += 1;
                if index[w] == NONE {
                    index[w] = counter;
                    low[w] = counter;
                    counter += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    calls.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let c = members.len();
                let mut group = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    comp[w] = c;
                    group.push(w);
                    if w == v {
                        break;
                    }
                }
                group.sort_unstable();
                members.push(group);
            }
        }
    }
    Sccs { comp, members }
}

/// Topological order over dependency edges: dependencies first, a cycle as
/// one run of its members, ties by display id.
pub fn topo_order(graph: &ProjectGraph, ids: &[String]) -> Vec<usize> {
    let adj = adjacency(graph);
    let s = sccs(&adj);
    let k = s.members.len();
    let mut waiting = vec![0usize; k];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); k];
    for (c, group) in s.members.iter().enumerate() {
        let mut deps: Vec<usize> = group
            .iter()
            .flat_map(|&v| adj[v].iter().map(|&w| s.comp[w]))
            .filter(|&d| d != c)
            .collect();
        deps.sort_unstable();
        deps.dedup();
        waiting[c] = deps.len();
        for d in deps {
            dependents[d].push(c);
        }
    }
    let first_id = |c: usize| {
        s.members[c]
            .iter()
            .map(|&v| ids[v].as_str())
            .min()
            .unwrap_or_default()
    };
    let mut ready: BTreeSet<(&str, usize)> = (0..k)
        .filter(|&c| waiting[c] == 0)
        .map(|c| (first_id(c), c))
        .collect();
    let mut out = Vec::with_capacity(graph.projects.len());
    while let Some((_, c)) = ready.pop_first() {
        let mut group = s.members[c].clone();
        group.sort_by(|&a, &b| (&ids[a], a).cmp(&(&ids[b], b)));
        out.extend(group);
        for &d in &dependents[c] {
            waiting[d] -= 1;
            if waiting[d] == 0 {
                ready.insert((first_id(d), d));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::config::{self, ResolvedConfig};
    use crate::detect::testkit::{Tree, build, detectors, rp};
    use crate::model::DepKind;

    fn diff(base: &Tree, head: &Tree) -> Vec<ChangedPath> {
        let mut out = Vec::new();
        for (p, c) in &head.0 {
            if base.0.get(p) != Some(c) {
                out.push(ChangedPath {
                    path: rp(p),
                    kind: ChangeKind::Write,
                });
            }
        }
        for p in base.0.keys().filter(|p| !head.0.contains_key(*p)) {
            out.push(ChangedPath {
                path: rp(p),
                kind: ChangeKind::Delete,
            });
        }
        out
    }

    fn cfg(text: &str) -> ResolvedConfig {
        if text.is_empty() {
            ResolvedConfig::default()
        } else {
            config::parse(text).unwrap()
        }
    }

    fn run_with(base: &Tree, head: &Tree, config: &str, query: &Query<'_>) -> Affected {
        let (gb, sb) = build(base, &[], cfg(config));
        let (gh, sh) = build(head, &[], cfg(config));
        let dets = detectors();
        affected(
            &dets,
            Side {
                graph: &gb,
                set: &sb,
            },
            Side {
                graph: &gh,
                set: &sh,
            },
            &diff(base, head),
            query,
        )
        .unwrap()
    }

    fn run(base: &Tree, head: &Tree) -> Affected {
        run_with(base, head, "", &Query::default())
    }

    fn ids(a: &Affected) -> Vec<&str> {
        a.projects.iter().map(|p| p.project.id.as_str()).collect()
    }

    fn reasons(a: &Affected, id: &str) -> Vec<String> {
        a.projects
            .iter()
            .find(|p| p.project.id == id)
            .unwrap()
            .reasons
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn abc() -> Tree {
        Tree::new(&[
            ("a/rust.fake", "name=a"),
            ("b/rust.fake", "name=b\ndep=a"),
            ("c/rust.fake", "name=c"),
        ])
    }

    fn edited(t: &Tree, path: &str, content: &str) -> Tree {
        let mut h = t.clone();
        h.set(path, content);
        h
    }

    fn asc() -> Tree {
        Tree::new(&[
            ("node.fake", "name=@asc/private"),
            (
                "packages/dsl/compiler/node.fake",
                "name=@asc/dsl-compiler",
            ),
            (
                "packages/dsl/compiler/rust/core/rust.fake",
                "name=dsl-core\ndep=dsl-ir",
            ),
            ("packages/dsl/compiler/rust/ir/rust.fake", "name=dsl-ir"),
            (
                "apps/analysis-pipeline/pipeline-api/rust.fake",
                "name=pipeline-api",
            ),
            (
                "apps/analysis-pipeline/pipeline-api/node.fake",
                "name=analysis-pipeline-api\ndep=@asc/dsl-compiler",
            ),
        ])
    }

    #[test]
    fn a_file_edit_hits_its_owner_then_dependents() {
        let t = abc();
        let a = run(&t, &edited(&t, "a/src/lib.rs", "x"));
        assert_eq!(ids(&a), ["rust:a", "rust:b"]);
        assert!(a.projects[0].direct && !a.projects[1].direct);
        assert_eq!(reasons(&a, "rust:a"), ["file:a/src/lib.rs"]);
        assert_eq!(reasons(&a, "rust:b"), ["dep:rust:a"]);
        assert!(!a.all && a.removed.is_empty() && a.unowned.is_empty());
    }

    #[test]
    fn a_shared_root_is_owned_by_both_packages() {
        let t = asc();
        let h = edited(&t, "apps/analysis-pipeline/pipeline-api/src/main.rs", "x");
        let a = run(&t, &h);
        assert_eq!(ids(&a), ["rust:pipeline-api", "node:analysis-pipeline-api"]);
        assert!(a.projects.iter().all(|p| p.direct));
    }

    #[test]
    fn contains_edges_carry_native_changes_to_wrappers_and_their_dependents() {
        let t = asc();
        let a = run(
            &t,
            &edited(&t, "packages/dsl/compiler/rust/core/lib.rs", "x"),
        );
        assert_eq!(
            ids(&a),
            [
                "rust:dsl-core",
                "node:@asc/dsl-compiler",
                "node:analysis-pipeline-api"
            ]
        );
        assert_eq!(
            reasons(&a, "node:@asc/dsl-compiler"),
            ["dep:rust:dsl-core"]
        );
        // The wrapper's own sources do not reach the native crates.
        let a = run(&t, &edited(&t, "packages/dsl/compiler/index.ts", "x"));
        assert_eq!(
            ids(&a),
            ["node:@asc/dsl-compiler", "node:analysis-pipeline-api"]
        );
    }

    #[test]
    fn target_follows_only_caret_edges() {
        let t = abc();
        let h = edited(&t, "a/x.rs", "x");
        let conf = "version = 1\n[targets.lint]\ninputs = [\"default\"]\n";
        let q = |target| Query {
            target: Some(target),
            ..Query::default()
        };
        assert_eq!(ids(&run_with(&t, &h, conf, &q("lint"))), ["rust:a"]);
        // The built-in target list has ^default, so the closure is not narrowed.
        assert_eq!(
            ids(&run_with(&t, &h, conf, &q("build"))),
            ["rust:a", "rust:b"]
        );
    }

    #[test]
    fn removed_projects_are_reported_not_affected() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a"),
            ("a/x.rs", "x"),
            ("b/rust.fake", "name=b\ndep=a"),
        ]);
        let mut h = t.clone();
        h.remove("a/rust.fake");
        h.remove("a/x.rs");
        let a = run(&t, &h);
        let removed: Vec<_> = a.removed.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(removed, ["rust:a"]);
        assert_eq!(ids(&a), ["rust:b"]);
        assert_eq!(reasons(&a, "rust:b"), ["manifest"]);
        assert!(a.unowned.is_empty());
    }

    #[test]
    fn unowned_paths_are_listed_and_can_affect_everything() {
        let t = abc();
        let h = edited(&t, "docs/x.md", "x");
        let a = run(&t, &h);
        assert!(a.projects.is_empty());
        assert_eq!(a.unowned, [rp("docs/x.md")]);
        let all = "version = 1\n[global]\nunowned = \"all\"\n";
        let a = run_with(&t, &h, all, &Query::default());
        assert!(a.all);
        assert_eq!(ids(&a), ["rust:a", "rust:b", "rust:c"]);
        assert_eq!(reasons(&a, "rust:c"), ["global:docs/x.md"]);
    }

    #[test]
    fn config_files_and_global_inputs_affect_everything() {
        let t = abc();
        let a = run(&t, &edited(&t, ".asc/projects.toml", "version = 1\n"));
        assert!(a.all && a.projects.len() == 3);
        let conf = "version = 1\n[global]\ninputs = [\".ci/ci.yml\"]\n";
        let a = run_with(&t, &edited(&t, ".ci/ci.yml", "x"), conf, &Query::default());
        assert!(a.all);
        assert_eq!(reasons(&a, "rust:a"), ["global:.ci/ci.yml"]);
    }

    #[test]
    fn eco_global_files_hit_their_ecosystem_under_their_directory() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=ra"),
            ("a/sub/rust.fake", "name=rs"),
            ("a/node.fake", "name=na"),
            ("b/rust.fake", "name=rb"),
        ]);
        let a = run(&t, &edited(&t, "a/rust.global", "x"));
        assert_eq!(ids(&a), ["rust:ra", "rust:rs", "node:na"]);
        assert_eq!(reasons(&a, "rust:rs"), ["eco-global:a/rust.global"]);
        let a = run(&t, &edited(&t, "rust.global", "x"));
        assert_eq!(ids(&a), ["rust:ra", "rust:rb", "rust:rs", "node:na"]);
        let off = "version = 1\n[global]\nbuiltin_eco_globals = false\n";
        let a = run_with(
            &t,
            &edited(&t, "a/rust.global", "x"),
            off,
            &Query::default(),
        );
        assert!(a.projects.is_empty());
        let eco = "version = 1\n[ecosystem.node]\ninputs = [\".env.example\"]\n";
        let a = run_with(&t, &edited(&t, ".env.example", "x"), eco, &Query::default());
        assert_eq!(ids(&a), ["node:na"]);
    }

    #[test]
    fn fingerprint_and_manifest_changes_have_their_own_reasons() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a\next=1"),
            ("b/rust.fake", "name=b\ndep=a"),
            ("c/rust.fake", "name=c"),
        ]);
        let a = run(&t, &edited(&t, "a/rust.fake", "name=a\next=2"));
        assert_eq!(ids(&a), ["rust:a", "rust:b"]);
        assert!(reasons(&a, "rust:a").contains(&"lockfile".to_owned()));
        let a = run(&t, &edited(&t, "c/rust.fake", "name=c\ndep=a"));
        assert_eq!(ids(&a), ["rust:c"]);
        assert_eq!(reasons(&a, "rust:c"), ["manifest"]);
    }

    #[test]
    fn new_projects_are_affected() {
        let t = abc();
        let a = run(&t, &edited(&t, "d/rust.fake", "name=d\ndep=c"));
        assert_eq!(ids(&a), ["rust:d"]);
        assert_eq!(reasons(&a, "rust:d"), ["new"]);
    }

    #[test]
    fn named_input_exclusions_and_ignore_silence_paths() {
        let t = abc();
        let md = "version = 1\n[named_inputs]\ndefault = [\"{projectRoot}/**\", \"!{projectRoot}/**/*.md\"]\n";
        let a = run_with(&t, &edited(&t, "a/README.md", "x"), md, &Query::default());
        assert!(a.projects.is_empty() && a.unowned.is_empty());
        let a = run_with(&t, &edited(&t, "a/lib.rs", "x"), md, &Query::default());
        assert_eq!(ids(&a), ["rust:a", "rust:b"]);
        let ig = "version = 1\nignore = [\"**/gen/**\"]\n";
        let a = run_with(&t, &edited(&t, "a/gen/x.rs", "x"), ig, &Query::default());
        assert!(a.projects.is_empty() && a.unowned.is_empty());
    }

    #[test]
    fn extra_inputs_trigger_projects_outside_their_root() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a\nextra=shared/**"),
            ("b/rust.fake", "name=b"),
        ]);
        let a = run(&t, &edited(&t, "shared/x.txt", "x"));
        assert_eq!(ids(&a), ["rust:a"]);
        assert_eq!(reasons(&a, "rust:a"), ["file:shared/x.txt"]);
        assert!(a.unowned.is_empty());
    }

    #[test]
    fn gitlink_changes_reach_members_declared_roots_and_dependents() {
        use crate::model::ProjectKind;
        let g = rp("vendor/soma");
        let mk = |eco, manifest: &str, name: &str| {
            let key = ProjectKey {
                eco,
                manifest: rp(manifest),
            };
            Project::new(key, name, ProjectKind::Package)
        };
        let mut member = mk(
            Ecosystem::Node,
            "vendor/soma/sdk/package.json",
            "@soma/budgets",
        );
        member.gitlink = Some(g.clone());
        let mut app = mk(Ecosystem::Node, "apps/web/package.json", "web");
        app.deps.push((member.key.clone(), DepKind::Normal));
        let mut pin = mk(Ecosystem::Declared, "vendor/soma", "pin");
        pin.root = Some(g.clone());
        let other = mk(Ecosystem::Node, "apps/other/package.json", "other");
        let mut projects = vec![member, app, pin, other];
        projects.sort_by(|a, b| a.key.cmp(&b.key));
        let graph = ProjectGraph {
            projects,
            config: ResolvedConfig::default(),
            issues: Vec::new(),
        };
        let set = ManifestSet::default();
        let side = Side {
            graph: &graph,
            set: &set,
        };
        let changed = [ChangedPath {
            path: g,
            kind: ChangeKind::Gitlink,
        }];
        let a = affected(&detectors(), side, side, &changed, &Query::default()).unwrap();
        assert_eq!(ids(&a), ["declared:pin", "node:@soma/budgets", "node:web"]);
        assert_eq!(reasons(&a, "declared:pin"), ["gitlink:vendor/soma"]);
        assert_eq!(reasons(&a, "node:web"), ["dep:node:@soma/budgets"]);
        assert!(a.unowned.is_empty());
    }

    #[test]
    fn filters_apply_after_the_closure() {
        let t = abc();
        let h = edited(&t, "a/x.rs", "x");
        let sel = [Selector::parse("rust:b").unwrap()];
        let q = Query {
            selectors: &sel,
            ..Query::default()
        };
        assert_eq!(ids(&run_with(&t, &h, "", &q)), ["rust:b"]);
        let eco = [Ecosystem::Node];
        let q = Query {
            ecosystems: &eco,
            ..Query::default()
        };
        assert!(run_with(&t, &h, "", &q).projects.is_empty());
    }

    #[test]
    fn order_puts_dependencies_first_with_cycles_as_components() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a\ndep=b"),
            ("b/rust.fake", "name=b\ndep=a"),
            ("c/rust.fake", "name=c\ndep=a"),
            ("z/rust.fake", "name=z"),
        ]);
        let (g, _) = build(&t, &[], ResolvedConfig::default());
        let all = g.display_ids();
        let order: Vec<_> = topo_order(&g, &all)
            .into_iter()
            .map(|i| all[i].clone())
            .collect();
        assert_eq!(order, ["rust:a", "rust:b", "rust:c", "rust:z"]);
        let s = sccs(&adjacency(&g));
        assert_eq!(s.comp[0], s.comp[1]);
        assert_ne!(s.comp[0], s.comp[2]);
    }

    #[test]
    fn long_chains_do_not_overflow() {
        let n = 20_000;
        let adj: Vec<Vec<usize>> = (0..n)
            .map(|i| if i + 1 < n { vec![i + 1] } else { vec![] })
            .collect();
        assert_eq!(sccs(&adj).members.len(), n);
    }

    // The naive oracle: link both full trees, mark the owners of every changed
    // path with a quadratic scan, close over reverse edges to a fixpoint.
    fn oracle(base: &Tree, head: &Tree) -> BTreeSet<ProjectKey> {
        let (gb, _) = build(base, &[], ResolvedConfig::default());
        let (gh, _) = build(head, &[], ResolvedConfig::default());
        let mut hit: BTreeSet<ProjectKey> = BTreeSet::new();
        let content = |t: &Tree, p: &RepoPath| t.0.get(&p.to_string()).cloned();
        for p in &gh.projects {
            let same = gb
                .projects
                .iter()
                .find(|q| q.key == p.key)
                .is_some_and(|q| {
                    q.deps == p.deps
                        && q.extra_inputs == p.extra_inputs
                        && q.external_fp == p.external_fp
                        && q.config_fp == p.config_fp
                        && content(base, &q.key.manifest) == content(head, &p.key.manifest)
                });
            if !same {
                hit.insert(p.key.clone());
            }
        }
        let is_fake = |path: &str| {
            let n = path.rsplit('/').next().unwrap();
            n.ends_with(".fake") || n.ends_with(".lock") || n.ends_with(".global")
        };
        let mut paths: BTreeSet<&String> = base.0.keys().collect();
        paths.extend(head.0.keys());
        for path in paths
            .into_iter()
            .filter(|p| base.0.get(*p) != head.0.get(*p))
        {
            let file = rp(path);
            if path.ends_with(".global") {
                let eco = if path.ends_with("rust.global") {
                    Ecosystem::Rust
                } else {
                    Ecosystem::Node
                };
                let dir = file.parent();
                for p in gh.projects.iter().filter(|p| p.key.eco == eco) {
                    if under(dir.as_ref(), p.root.as_ref()) {
                        hit.insert(p.key.clone());
                    }
                }
            }
            if is_fake(path) {
                continue;
            }
            let own = |g: &ProjectGraph| -> Vec<ProjectKey> {
                let depth = |p: &Project| p.root.as_ref().map_or(0, |r| r.to_string().len() + 1);
                let cands: Vec<&Project> = g
                    .projects
                    .iter()
                    .filter(|p| p.root.as_ref().is_none_or(|r| file.starts_with_dir(r)))
                    .collect();
                let best = cands.iter().map(|p| depth(p)).max();
                cands
                    .into_iter()
                    .filter(|p| Some(depth(p)) == best)
                    .map(|p| p.key.clone())
                    .collect()
            };
            hit.extend(own(&gh));
            if !head.0.contains_key(path) {
                let alive = |k: &ProjectKey| gh.projects.iter().any(|p| p.key == *k);
                hit.extend(own(&gb).into_iter().filter(alive));
            }
        }
        loop {
            let more: Vec<ProjectKey> = gh
                .projects
                .iter()
                .filter(|p| !hit.contains(&p.key) && p.deps.iter().any(|(k, _)| hit.contains(k)))
                .map(|p| p.key.clone())
                .collect();
            if more.is_empty() {
                return hit;
            }
            hit.extend(more);
        }
    }

    const MANIFESTS: [&str; 7] = [
        "rust.fake",
        "a/rust.fake",
        "a/b/rust.fake",
        "c/rust.fake",
        "a/node.fake",
        "c/d/node.fake",
        "e/node.fake",
    ];
    const NAMES: [&str; 7] = ["r_root", "r_a", "r_ab", "r_c", "n_a", "n_cd", "n_e"];
    const PLAIN: [&str; 7] = [
        "a/f.txt",
        "a/b/g.txt",
        "c/h.txt",
        "c/d/i.txt",
        "z/j.txt",
        "k.txt",
        "a/rust.global",
    ];

    /// A manifest slot: absent, or present with a dependency mask and an `ext` value.
    type Slot = Option<(u8, u8)>;

    fn render(slots: &[Slot], plain: &[u8]) -> Tree {
        let mut t = Tree::default();
        for (i, s) in slots.iter().enumerate() {
            if let Some((mask, ext)) = s {
                let mut c = format!("name={}\next={ext}\n", NAMES[i]);
                for (j, n) in NAMES.iter().enumerate() {
                    if (mask >> j) & 1 == 1 {
                        c.push_str(&format!("dep={n}\n"));
                    }
                }
                t.set(MANIFESTS[i], &c);
            }
        }
        for (i, v) in plain.iter().enumerate().filter(|(_, v)| **v > 0) {
            t.set(PLAIN[i], &v.to_string());
        }
        t
    }

    fn slots() -> impl Strategy<Value = Vec<Slot>> {
        proptest::collection::vec(proptest::option::of((0u8..128, 0u8..2)), 7)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn matches_the_naive_oracle(
            bs in slots(),
            hs in slots(),
            bp in proptest::collection::vec(0u8..2, 7),
            hp in proptest::collection::vec(0u8..2, 7),
            keep in proptest::collection::vec(any::<bool>(), 7),
        ) {
            // Keep most slots equal so that small edits dominate.
            let hs: Vec<Slot> = (0..7).map(|i| if keep[i] { bs[i] } else { hs[i] }).collect();
            let hp: Vec<u8> = (0..7).map(|i| if keep[i] { bp[i] } else { hp[i] }).collect();
            let (base, head) = (render(&bs, &bp), render(&hs, &hp));
            let got: BTreeSet<ProjectKey> =
                run(&base, &head).projects.into_iter().map(|p| p.project.key).collect();
            prop_assert_eq!(got, oracle(&base, &head));
        }
    }
}
