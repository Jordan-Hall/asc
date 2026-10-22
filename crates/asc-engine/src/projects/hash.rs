//! Per-project input hashes (SPEC §12A.6): trees only, never file content.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use asc_core::id::{ChangesetId, TreeId};
use asc_core::object::Kind;
use asc_core::path::{MAX_TREE_DEPTH, RepoPath};
use asc_graph::affected::{adjacency, sccs};
use asc_graph::config::{ProjectEntry, ProjectTarget, ResolvedInputs};
use asc_graph::detect::Claim;
use asc_graph::model::{
    Derived as Envelope, Ecosystem, Fingerprint, InputHashes, Project, ProjectKey, ProjectKind,
    phash_key,
};
use asc_graph::own::Ownership;
use asc_graph::pattern::{Decision, IgnoreSet, InputItem, PatternSet};
use asc_store::key::ObjKey;
use asc_store::objects::Objects;
use asc_store::refs::Refs;

use super::derive::pad_oid;
use super::{Loaded, Result};
use crate::Engine;
use crate::derive::{read_memo, write_memo};
use crate::tree;

const PHASH: &str = "phash";

/// One selected path: `(path, kind, content or tree id)`; the root directory has an empty path.
type Contribution = (Vec<u8>, u8, [u8; 32]);

/// Which files under a matched directory count.
#[derive(Clone, Copy)]
enum Filter<'a> {
    /// Files the project owns (SPEC §12A.1).
    Owned(&'a Ownership<'a>, usize),
    /// Files not ignored and not in `node_modules`.
    NotExcluded(&'a Ownership<'a>),
}

impl Filter<'_> {
    fn allows(&self, path: &RepoPath) -> bool {
        match self {
            Self::Owned(o, i) => o.owners(path).contains(i),
            Self::NotExcluded(o) => !o.is_excluded(path),
        }
    }
}

/// Everything the hash phase reads, computed once per graph.
struct Plan<'a> {
    own: Ownership<'a>,
    ignore: IgnoreSet,
}

fn entry_applies(e: &ProjectEntry, p: &Project) -> bool {
    match &e.target {
        ProjectTarget::Select(sel) => sel.matches(p),
        ProjectTarget::Declared { root, .. } => {
            p.key.eco == Ecosystem::Declared && p.key.manifest == *root
        }
    }
}

/// True when `dir` is the repository root or holds `root`.
fn under(dir: Option<&RepoPath>, root: Option<&RepoPath>) -> bool {
    match (dir, root) {
        (None, _) => true,
        (Some(d), Some(r)) => r == d || r.starts_with_dir(d),
        (Some(_), None) => false,
    }
}

/// True when `root` is strictly inside `parent`.
fn below(root: &RepoPath, parent: Option<&RepoPath>) -> bool {
    parent.is_none_or(|d| root.starts_with_dir(d))
}

fn digest(mut items: Vec<Contribution>) -> [u8; 32] {
    items.sort();
    items.dedup();
    let mut h = blake3::Hasher::new();
    for (path, kind, id) in &items {
        h.update(&(path.len() as u64).to_le_bytes());
        h.update(path);
        h.update(&[*kind]);
        h.update(id);
    }
    *h.finalize().as_bytes()
}

/// The hash of one project and label over its already-computed parts.
fn input_hash(
    p: &Project,
    label: &str,
    files: [u8; 32],
    eco_globals: [u8; 32],
    deps: &[(String, ProjectKey, [u8; 32])],
) -> [u8; 32] {
    let enc = postcard::to_stdvec(&(
        &p.key,
        label,
        files,
        p.config_fp,
        p.external_fp,
        eco_globals,
        deps,
    ))
    .expect("in-memory encode cannot fail");
    blake3::derive_key("asc v1 project input", &enc)
}

impl<O: Objects, R: Refs> Engine<O, R> {
    /// The input hash of every project for `target` at `cs` (SPEC §12A.6); an empty target
    /// hashes the `default` named input. Memoised as `phash.v1`.
    pub async fn input_hashes(&self, cs: ChangesetId, target: &str) -> Result<InputHashes> {
        let root = self.get_changeset(cs).await?.root;
        let l = self.loaded(cs).await?;
        let key = ObjKey::Derived {
            name: PHASH,
            version: 1,
            id: phash_key(&l.id, &root, target),
        };
        if let Some(b) = read_memo(&*self.objects, &key).await?
            && let Ok(h) = InputHashes::from_envelope(&b)
        {
            return Ok(h);
        }
        let h = self.compute_hashes(&l, root, target).await?;
        write_memo(&*self.objects, &key, &h.to_envelope()).await?;
        Ok(h)
    }

    async fn compute_hashes(
        &self,
        l: &Arc<Loaded>,
        root: TreeId,
        target: &str,
    ) -> Result<InputHashes> {
        let graph = &*l.graph;
        let cfg = &graph.config;
        let projects = &graph.projects;
        let t = (!target.is_empty()).then_some(target);
        let plan = Plan {
            own: Ownership::new(graph, self.projects.detectors)?,
            ignore: IgnoreSet::new(cfg.ignore.iter().map(String::as_str))?,
        };

        let mut target_inputs = Vec::with_capacity(projects.len());
        for p in projects {
            let entry = cfg.projects.iter().find(|e| entry_applies(e, p));
            target_inputs.push(cfg.inputs_for(entry, t)?);
        }
        // Every named input some `^` item reaches, with its own `^` items.
        let mut names: BTreeSet<String> = target_inputs
            .iter()
            .flat_map(|r| r.deps.iter().cloned())
            .collect();
        let mut named: BTreeMap<String, ResolvedInputs> = BTreeMap::new();
        let mut queue: Vec<String> = names.iter().cloned().collect();
        while let Some(n) = queue.pop() {
            if named.contains_key(&n) {
                continue;
            }
            let r = cfg.resolve(&[InputItem::Named(n.clone())])?;
            for d in &r.deps {
                if names.insert(d.clone()) {
                    queue.push(d.clone());
                }
            }
            named.insert(n, r);
        }

        // Roots that own files inside each project's root.
        let mut roots: Vec<&RepoPath> = projects
            .iter()
            .filter(|p| p.kind == ProjectKind::Package && p.gitlink.is_none())
            .filter_map(|p| p.root.as_ref())
            .collect();
        roots.sort();
        roots.dedup();

        // Files digests, shared by projects and labels with identical patterns.
        let mut memo: HashMap<(usize, Vec<String>, Vec<String>), [u8; 32]> = HashMap::new();
        let mut target_fd = Vec::with_capacity(projects.len());
        let mut named_fd: HashMap<(usize, &str), [u8; 32]> = HashMap::new();
        for (i, p) in projects.iter().enumerate() {
            let nested: Vec<RepoPath> = roots
                .iter()
                .filter(|r| below(r, p.root.as_ref()))
                .map(|r| (*r).clone())
                .collect();
            let mut want: Vec<(Option<&str>, &ResolvedInputs)> = vec![(None, &target_inputs[i])];
            want.extend(named.iter().map(|(n, r)| (Some(n.as_str()), r)));
            for (label, r) in want {
                let k = (i, r.includes.clone(), r.excludes.clone());
                let fd = match memo.get(&k) {
                    Some(d) => *d,
                    None => {
                        let d = self.files_digest(root, &plan, i, p, r, &nested).await?;
                        memo.insert(k, d);
                        d
                    }
                };
                match label {
                    None => target_fd.push(fd),
                    Some(n) => {
                        named_fd.insert((i, n), fd);
                    }
                }
            }
        }

        let eco = self.eco_globals(l, root, &plan).await?;

        // Components come after everything they depend on, so a `^` edge always finds its hash.
        let adj = adjacency(graph);
        let scc = sccs(&adj);
        let deps_of = |i: usize,
                       dep_names: &[String],
                       full: &HashMap<(usize, &str), [u8; 32]>|
         -> Vec<(String, ProjectKey, [u8; 32])> {
            let mut out = Vec::new();
            for n in dep_names {
                for (k, _) in &projects[i].deps {
                    let Ok(j) = projects.binary_search_by(|q| q.key.cmp(k)) else {
                        continue;
                    };
                    let v = if scc.comp[j] == scc.comp[i] {
                        named_fd[&(j, n.as_str())]
                    } else {
                        full[&(j, n.as_str())]
                    };
                    out.push((n.clone(), k.clone(), v));
                }
            }
            out.sort();
            out.dedup();
            out
        };
        let mut full: HashMap<(usize, &str), [u8; 32]> = HashMap::new();
        let mut out = vec![[0; 32]; projects.len()];
        for members in &scc.members {
            for &i in members {
                for (n, r) in &named {
                    let deps = deps_of(i, &r.deps, &full);
                    let h = input_hash(&projects[i], n, named_fd[&(i, n.as_str())], eco[i], &deps);
                    full.insert((i, n.as_str()), h);
                }
                let deps = deps_of(i, &target_inputs[i].deps, &full);
                out[i] = input_hash(&projects[i], target, target_fd[i], eco[i], &deps);
            }
        }
        Ok(InputHashes(
            projects.iter().map(|p| p.key.clone()).zip(out).collect(),
        ))
    }

    /// `files_digest` of project `i` for one pattern list (SPEC §12A.6).
    async fn files_digest(
        &self,
        root: TreeId,
        plan: &Plan<'_>,
        i: usize,
        p: &Project,
        r: &ResolvedInputs,
        nested: &[RepoPath],
    ) -> Result<[u8; 32]> {
        let set = PatternSet::new(
            r.includes.iter().map(String::as_str),
            r.excludes.iter().map(String::as_str),
            p.root.as_ref(),
        )?;
        let mut items = self
            .select(
                root,
                &set,
                nested,
                &plan.ignore,
                Filter::Owned(&plan.own, i),
            )
            .await?;
        let extra: Vec<&str> = p
            .extra_inputs
            .iter()
            .filter_map(|g| std::str::from_utf8(g).ok())
            .collect();
        if !extra.is_empty() {
            let x = PatternSet::new(extra, [], None)?;
            items.extend(
                self.select(root, &x, &[], &plan.ignore, Filter::NotExcluded(&plan.own))
                    .await?,
            );
        }
        Ok(digest(items))
    }

    /// `eco_globals_digest` of every project: eco-global files that apply, plus the files
    /// `[global] inputs` and `[ecosystem.<e>] inputs` match.
    async fn eco_globals(
        &self,
        l: &Loaded,
        root: TreeId,
        plan: &Plan<'_>,
    ) -> Result<Vec<[u8; 32]>> {
        let cfg = &l.graph.config;
        let filter = Filter::NotExcluded(&plan.own);
        let mut shared: Vec<Contribution> = Vec::new();
        if !cfg.global_inputs.is_empty() {
            let set = PatternSet::new(cfg.global_inputs.iter().map(String::as_str), [], None)?;
            shared = self.select(root, &set, &[], &plan.ignore, filter).await?;
        }
        let mut by_eco: BTreeMap<Ecosystem, Vec<Contribution>> = BTreeMap::new();
        for (eco, c) in &cfg.ecosystems {
            if c.enabled && !c.inputs.is_empty() {
                let set = PatternSet::new(c.inputs.iter().map(String::as_str), [], None)?;
                by_eco.insert(
                    *eco,
                    self.select(root, &set, &[], &plan.ignore, filter).await?,
                );
            }
        }
        let mut files: Vec<(Ecosystem, Option<RepoPath>, Contribution)> = Vec::new();
        if cfg.builtin_eco_globals {
            for (path, kind, id) in l.set.entries() {
                if *kind == Kind::Gitlink || plan.own.is_excluded(path) {
                    continue;
                }
                for d in self.projects.detectors {
                    let eco = d.ecosystem();
                    if cfg.eco(eco).enabled && d.claim(path) == Some(Claim::EcoGlobal) {
                        files.push((
                            eco,
                            path.parent(),
                            (path.as_bytes().to_vec(), *kind as u8, *id),
                        ));
                    }
                }
            }
        }
        Ok(l.graph
            .projects
            .iter()
            .map(|p| {
                let mut items = shared.clone();
                items.extend(by_eco.get(&p.key.eco).into_iter().flatten().cloned());
                items.extend(
                    files
                        .iter()
                        .filter(|(eco, dir, _)| {
                            *eco == p.key.eco && under(dir.as_ref(), p.root.as_ref())
                        })
                        .map(|f| f.2.clone()),
                );
                digest(items)
            })
            .collect())
    }

    /// The selected paths under `root`: whole directories where `decide` says `All`, else files.
    async fn select(
        &self,
        root: TreeId,
        pats: &PatternSet,
        nested: &[RepoPath],
        ignore: &IgnoreSet,
        filter: Filter<'_>,
    ) -> Result<Vec<Contribution>> {
        let mut out = Vec::new();
        match pats.decide(None, nested, ignore) {
            Decision::None => return Ok(out),
            Decision::All => {
                out.push((Vec::new(), Kind::Dir as u8, *root.as_bytes()));
                return Ok(out);
            }
            Decision::Partial => {}
        }
        let mut stack = vec![(None::<RepoPath>, self.get_tree(root).await?, 0usize)];
        while let Some(top) = stack.last_mut() {
            let Some(e) = top.1.entries().get(top.2).cloned() else {
                stack.pop();
                continue;
            };
            top.2 += 1;
            let path = match &top.0 {
                Some(dir) => dir.join(&e.name),
                None => RepoPath::from(e.name.clone()),
            };
            if e.kind != Kind::Dir {
                if pats.matches(&path) && filter.allows(&path) {
                    let id = if e.kind == Kind::Gitlink {
                        pad_oid(&e.git)
                    } else {
                        e.id
                    };
                    out.push((path.as_bytes().to_vec(), e.kind as u8, id));
                }
                continue;
            }
            match pats.decide(Some(&path), nested, ignore) {
                Decision::None => {}
                Decision::All => out.push((path.as_bytes().to_vec(), Kind::Dir as u8, e.id)),
                Decision::Partial => {
                    if stack.len() > MAX_TREE_DEPTH {
                        return Err(tree::Error::TooDeep.into());
                    }
                    let t = self.get_tree(TreeId::from_bytes(e.id)).await?;
                    stack.push((Some(path), t, 0));
                }
            }
        }
        Ok(out)
    }
}

/// The hash for display: the first bytes as a fingerprint.
pub(crate) fn fingerprint(h: [u8; 32]) -> Fingerprint {
    Fingerprint::from(h)
}
