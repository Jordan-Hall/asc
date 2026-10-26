//! Linking (SPEC §12A.1, §12A.3): detectors' projects become one graph with
//! Contains and opaque Implicit edges, declared projects and the config applied.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use asc_core::path::RepoPath;

use crate::config::ProjectTarget;
use crate::detect::{Claim, Detector, LinkCx, LinkFile, ParseFailure, claim_of};
use crate::model::{
    DepKind, Ecosystem, Issue, MAX_PROJECTS, Parsed, Project, ProjectGraph, ProjectKey, ProjectKind,
};
use crate::own::in_node_modules;
use crate::pattern::{IgnoreSet, PatternError};
use crate::select::{SelectError, Selector, Term, resolve_one, select};

/// Most edges that opaque projects and configured `implicit_dependencies`
/// together (additions and removals counted per entry, after deduplication)
/// may add to one graph.
pub const MAX_IMPLICIT_EDGES: usize = 10_000_000;

/// Why a graph could not be linked; the server reports these as `PROJECTS_CONFIG`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error(transparent)]
    Pattern(#[from] PatternError),
    #[error("line {line}: {source}")]
    Select { line: u32, source: SelectError },
    #[error("line {line}: declared root {root} does not exist")]
    MissingRoot { line: u32, root: RepoPath },
    #[error("line {line}: declared root {root} is already used by another declared project")]
    DuplicateRoot { line: u32, root: RepoPath },
    #[error("more than {MAX_PROJECTS} projects")]
    TooManyProjects,
    #[error(
        "opaque projects and implicit dependencies would add more than {MAX_IMPLICIT_EDGES} edges"
    )]
    TooManyEdges,
}

/// One claimed file of the manifest set with its parse result.
#[derive(Clone, Debug)]
pub struct LinkInput {
    pub path: RepoPath,
    /// Content id of the file.
    pub id: [u8; 32],
    pub parsed: Result<Parsed, ParseFailure>,
}

/// Builds the graph of one manifest set.
///
/// `dir_exists` answers whether a directory (or gitlink) is in the tree, for
/// declared roots. Files that are ignored, in `node_modules`, or claimed by a
/// disabled detector are skipped; a manifest that fails to parse becomes an
/// opaque project and an issue.
pub fn link_graph(
    detectors: &[&dyn Detector],
    files: &[LinkInput],
    gitlinks: &[RepoPath],
    config: crate::config::ResolvedConfig,
    dir_exists: &dyn Fn(&RepoPath) -> bool,
) -> Result<ProjectGraph, LinkError> {
    let ignore = IgnoreSet::new(config.ignore.iter().map(String::as_str))?;
    let mut issues = Vec::new();
    let mut linked = Vec::new();
    let mut failed: Vec<(Ecosystem, &RepoPath)> = Vec::new();
    for f in files {
        if in_node_modules(&f.path) || ignore.is_ignored(&f.path) {
            continue;
        }
        let Some((eco, claim)) = claim_of(detectors, &config, &f.path) else {
            continue;
        };
        match &f.parsed {
            Ok(parsed) => linked.push(LinkFile {
                path: &f.path,
                eco,
                claim,
                id: f.id,
                parsed,
            }),
            Err(e) => {
                issues.push(Issue {
                    path: f.path.clone(),
                    message: e.message.clone().into_bytes(),
                });
                if claim == Claim::Manifest {
                    failed.push((eco, &f.path));
                }
            }
        }
    }

    let mut projects = Vec::new();
    for d in detectors {
        let eco = d.ecosystem();
        if !config.eco(eco).enabled {
            continue;
        }
        let cx = LinkCx::new(eco, linked.clone(), gitlinks, &config).with_failed(
            failed
                .iter()
                .filter(|(e, _)| *e == eco)
                .map(|(_, p)| *p)
                .collect(),
        );
        projects.extend(d.link(&cx).into_iter().filter(|p| p.key.eco == eco));
        issues.extend(cx.take_issues());
    }
    for (eco, path) in failed {
        projects.push(opaque_placeholder(eco, path));
    }
    declare_projects(&config, dir_exists, &mut projects)?;

    projects.sort_by(|a, b| a.key.cmp(&b.key));
    projects.dedup_by(|dup, first| dup.key == first.key);
    if projects.len() > MAX_PROJECTS {
        return Err(LinkError::TooManyProjects);
    }
    prune_edges(&mut projects);
    add_contains(&mut projects);
    let used = add_opaque_edges(&mut projects)?;
    apply_config(
        &config,
        &mut projects,
        &mut issues,
        used,
        MAX_IMPLICIT_EDGES,
    )?;
    for p in &mut projects {
        p.deps.sort();
        p.deps.dedup();
        p.tags.sort();
        p.tags.dedup();
    }
    for p in projects.iter().filter(|p| p.opaque) {
        if !issues.iter().any(|i| i.path == p.key.manifest) {
            issues.push(Issue {
                path: p.key.manifest.clone(),
                message: b"opaque project".to_vec(),
            });
        }
    }
    issues.sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
    issues.dedup();
    Ok(ProjectGraph {
        projects,
        config,
        issues,
    })
}

/// A Package standing in for a manifest that failed to parse.
fn opaque_placeholder(eco: Ecosystem, manifest: &RepoPath) -> Project {
    let name = manifest
        .parent()
        .map_or(manifest.file_name(), |d| d.file_name());
    let key = ProjectKey {
        eco,
        manifest: manifest.clone(),
    };
    let mut p = Project::new(key, name.as_bytes(), ProjectKind::Package);
    p.opaque = true;
    p
}

/// Adds the `[[project]]` entries that declare projects (SPEC §12A.2.5).
fn declare_projects(
    config: &crate::config::ResolvedConfig,
    dir_exists: &dyn Fn(&RepoPath) -> bool,
    projects: &mut Vec<Project>,
) -> Result<(), LinkError> {
    for e in &config.projects {
        let ProjectTarget::Declared {
            name,
            root,
            root_line,
        } = &e.target
        else {
            continue;
        };
        if !dir_exists(root) {
            return Err(LinkError::MissingRoot {
                line: *root_line,
                root: root.clone(),
            });
        }
        let key = ProjectKey {
            eco: Ecosystem::Declared,
            manifest: root.clone(),
        };
        if projects.iter().any(|p| p.key == key) {
            return Err(LinkError::DuplicateRoot {
                line: *root_line,
                root: root.clone(),
            });
        }
        let mut p = Project::new(key, name.as_bytes(), ProjectKind::Package);
        p.root = Some(root.clone());
        projects.push(p);
    }
    Ok(())
}

/// Drops self edges and edges to projects that do not exist.
fn prune_edges(projects: &mut [Project]) {
    let keys: Vec<ProjectKey> = projects.iter().map(|p| p.key.clone()).collect();
    for p in projects {
        let own = p.key.clone();
        p.deps
            .retain(|(k, _)| *k != own && keys.binary_search(k).is_ok());
    }
}

/// A Node Package P not at the repository root gets Contains edges to every
/// non-Node Package whose root is P's root or below it.
fn add_contains(projects: &mut [Project]) {
    let mut below: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
    for (i, p) in projects.iter().enumerate() {
        if p.kind == ProjectKind::Package
            && p.key.eco != Ecosystem::Node
            && let Some(r) = &p.root
        {
            below.entry(r.as_bytes().to_vec()).or_default().push(i);
        }
    }
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (i, p) in projects.iter().enumerate() {
        let (Ecosystem::Node, ProjectKind::Package, Some(root)) = (p.key.eco, p.kind, &p.root)
        else {
            continue;
        };
        let lo = [root.as_bytes(), b"/"].concat();
        let hi = [root.as_bytes(), b"0"].concat();
        let nested = below.range(lo..hi).flat_map(|(_, v)| v);
        let same = below.get(root.as_bytes()).into_iter().flatten();
        edges.extend(same.chain(nested).map(|&q| (i, q)));
    }
    for (i, q) in edges {
        let key = projects[q].key.clone();
        projects[i].deps.push((key, DepKind::Contains));
    }
}

/// An opaque project depends on every other project of its ecosystem.
/// Returns the number of edges added, for the shared budget.
fn add_opaque_edges(projects: &mut [Project]) -> Result<usize, LinkError> {
    let mut per_eco: BTreeMap<Ecosystem, Vec<usize>> = BTreeMap::new();
    for (i, p) in projects.iter().enumerate() {
        per_eco.entry(p.key.eco).or_default().push(i);
    }
    let total: usize = projects
        .iter()
        .filter(|p| p.opaque)
        .map(|p| per_eco[&p.key.eco].len() - 1)
        .sum();
    if total > MAX_IMPLICIT_EDGES {
        return Err(LinkError::TooManyEdges);
    }
    let mut edges = Vec::with_capacity(total);
    for (i, p) in projects.iter().enumerate().filter(|(_, p)| p.opaque) {
        edges.extend(
            per_eco[&p.key.eco]
                .iter()
                .filter(|&&q| q != i)
                .map(|&q| (i, q)),
        );
    }
    for (i, q) in edges {
        let key = projects[q].key.clone();
        projects[i].deps.push((key, DepKind::Implicit));
    }
    Ok(total)
}

fn index_of(projects: &[Project], key: &ProjectKey) -> Option<usize> {
    projects.binary_search_by(|p| p.key.cmp(key)).ok()
}

/// Applies tags and `implicit_dependencies` of the `[[project]]` entries.
///
/// Every `select` is resolved against the detected projects first, so one
/// entry's tags never change what another selects. Additions run before
/// removals, as exclusions do everywhere else. Matches count against
/// [`MAX_IMPLICIT_EDGES`] together with the `used` opaque edges.
fn apply_config(
    config: &crate::config::ResolvedConfig,
    projects: &mut [Project],
    issues: &mut Vec<Issue>,
    used: usize,
    max_edges: usize,
) -> Result<(), LinkError> {
    let mut targets = Vec::with_capacity(config.projects.len());
    for e in &config.projects {
        let key = match &e.target {
            ProjectTarget::Select(sel) => resolve_one(projects, sel)
                .map_err(|source| LinkError::Select {
                    line: e.line,
                    source,
                })?
                .clone(),
            ProjectTarget::Declared { root, .. } => ProjectKey {
                eco: Ecosystem::Declared,
                manifest: root.clone(),
            },
        };
        targets.push(index_of(projects, &key).expect("resolved project exists"));
    }
    let mut budget = used;
    let mut adds: Vec<(usize, usize)> = Vec::new();
    let mut removes: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (e, &i) in config.projects.iter().zip(&targets) {
        // Hits are deduplicated per entry, so repeated selectors cost once.
        let mut add_set = BTreeSet::new();
        let mut remove_set = BTreeSet::new();
        for hits_for in &e.implicit_dependencies {
            let positive = Selector {
                exclude: false,
                term: hits_for.term.clone(),
            };
            // `select` builds every display id per call; only a bare name can
            // be ambiguous and need them, so other terms match directly.
            let hits = if matches!(hits_for.term, Term::Name(_)) {
                select(projects, &[positive]).map_err(|source| LinkError::Select {
                    line: e.line,
                    source,
                })?
            } else {
                (0..projects.len())
                    .filter(|&q| positive.matches(&projects[q]))
                    .collect()
            };
            let hits = || hits.iter().copied().filter(|&q| q != i);
            if hits().next().is_none() && !matches!(hits_for.term, Term::Tag(_)) {
                issues.push(Issue {
                    path: RepoPath::new(".asc/projects.toml").expect("static path"),
                    message: format!("line {}: implicit dependency matches no project", e.line)
                        .into_bytes(),
                });
            }
            let set = if hits_for.exclude {
                &mut remove_set
            } else {
                &mut add_set
            };
            set.extend(hits());
            if budget.saturating_add(add_set.len() + remove_set.len()) > max_edges {
                return Err(LinkError::TooManyEdges);
            }
        }
        budget += add_set.len() + remove_set.len();
        adds.extend(add_set.into_iter().map(|q| (i, q)));
        removes.entry(i).or_default().extend(remove_set);
        projects[i]
            .tags
            .extend(e.tags.iter().map(|t| t.as_bytes().to_vec()));
    }
    for (i, q) in adds {
        let key = projects[q].key.clone();
        projects[i].deps.push((key, DepKind::Implicit));
    }
    if !removes.is_empty() {
        let keys: Vec<ProjectKey> = projects.iter().map(|p| p.key.clone()).collect();
        for (i, gone) in removes {
            projects[i]
                .deps
                .retain(|(k, _)| keys.binary_search(k).map_or(true, |q| !gone.contains(&q)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, ResolvedConfig};
    use crate::detect::testkit::{Tree, build, rp, try_build};

    fn graph(t: &Tree, cfg: &str) -> ProjectGraph {
        let cfg = if cfg.is_empty() {
            ResolvedConfig::default()
        } else {
            config::parse(cfg).unwrap()
        };
        build(t, &[], cfg).0
    }

    fn edges(g: &ProjectGraph, id: &str) -> Vec<(String, DepKind)> {
        let ids = g.display_ids();
        let at = ids.iter().position(|x| x == id).unwrap();
        g.projects[at]
            .deps
            .iter()
            .map(|(k, d)| {
                let q = g.projects.iter().position(|p| p.key == *k).unwrap();
                (ids[q].clone(), *d)
            })
            .collect()
    }

    fn asc() -> Tree {
        Tree::new(&[
            ("packages/dsl/compiler/node.fake", "name=@asc/dsl-compiler"),
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
            ("node.fake", "name=@asc/private"),
        ])
    }

    #[test]
    fn contains_edges_wrap_native_code() {
        let g = graph(&asc(), "");
        assert_eq!(
            edges(&g, "node:@asc/dsl-compiler"),
            [
                ("rust:dsl-core".to_owned(), DepKind::Contains),
                ("rust:dsl-ir".to_owned(), DepKind::Contains),
            ]
        );
        // Same root counts, and Normal and Contains edges coexist.
        assert_eq!(
            edges(&g, "node:analysis-pipeline-api"),
            [
                ("rust:pipeline-api".to_owned(), DepKind::Contains),
                ("node:@asc/dsl-compiler".to_owned(), DepKind::Normal),
            ]
        );
        // A root-level Node package contains nothing.
        assert!(edges(&g, "node:@asc/private").is_empty());
        // Rust crates keep only their manifest edges.
        assert_eq!(
            edges(&g, "rust:dsl-core"),
            [("rust:dsl-ir".to_owned(), DepKind::Normal)]
        );
    }

    #[test]
    fn sibling_directories_with_a_shared_prefix_are_not_contained() {
        let t = Tree::new(&[
            ("a/node.fake", "name=n"),
            ("a-b/rust.fake", "name=near"),
            ("a/b/rust.fake", "name=inside"),
        ]);
        let g = graph(&t, "");
        assert_eq!(
            edges(&g, "node:n"),
            [("rust:inside".to_owned(), DepKind::Contains)]
        );
    }

    #[test]
    fn opaque_projects_depend_on_their_ecosystem_and_are_reported() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a\nopaque"),
            ("b/rust.fake", "name=b"),
            ("c/rust.fake", "bad line"),
            ("d/node.fake", "name=d"),
        ]);
        let g = graph(&t, "");
        assert_eq!(
            edges(&g, "rust:a"),
            [
                ("rust:b".to_owned(), DepKind::Implicit),
                ("rust:c".to_owned(), DepKind::Implicit),
            ]
        );
        assert!(edges(&g, "rust:b").is_empty());
        // The unparsable manifest keeps a root and an owner-less opaque project.
        let c = g.projects.iter().find(|p| p.name == b"c").unwrap();
        assert!(c.opaque);
        assert_eq!(c.root, Some(rp("c")));
        let paths: Vec<String> = g.issues.iter().map(|i| i.path.to_string()).collect();
        assert_eq!(paths, ["a/rust.fake", "c/rust.fake"]);
    }

    #[test]
    fn config_applies_tags_edges_and_declared_projects() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a"),
            ("b/rust.fake", "name=b\ndep=a"),
            ("c/node.fake", "name=c"),
            ("tools/gen/x", "x"),
        ]);
        let g = graph(
            &t,
            r#"
version = 1
[[project]]
select = "rust:b"
implicit_dependencies = ["node:c", "!rust:a"]
tags = ["deploy", "image"]
[[project]]
name = "gen"
root = "tools/gen"
tags = ["tool"]
implicit_dependencies = ["rust:a"]
"#,
        );
        assert_eq!(
            edges(&g, "rust:b"),
            [("node:c".to_owned(), DepKind::Implicit)]
        );
        let b = g.projects.iter().find(|p| p.name == b"b").unwrap();
        assert_eq!(b.tags, [b"deploy".to_vec(), b"image".to_vec()]);
        let gen_ = g.projects.iter().find(|p| p.name == b"gen").unwrap();
        assert_eq!(gen_.key.eco, Ecosystem::Declared);
        assert_eq!(gen_.root, Some(rp("tools/gen")));
        assert_eq!(
            edges(&g, "declared:gen"),
            [("rust:a".to_owned(), DepKind::Implicit)]
        );
    }

    #[test]
    fn bad_selects_and_roots_are_errors_naming_the_line() {
        let t = Tree::new(&[("a/rust.fake", "name=a"), ("b/rust.fake", "name=a")]);
        let e = try_build(
            &t,
            &[],
            config::parse("version = 1\n[[project]]\nselect = \"rust:a\"\n").unwrap(),
        )
        .unwrap_err();
        assert!(matches!(e, LinkError::Select { line: 3, .. }), "{e:?}");
        let e = try_build(
            &t,
            &[],
            config::parse("version = 1\n[[project]]\nname = \"x\"\nroot = \"nope\"\n").unwrap(),
        )
        .unwrap_err();
        assert!(matches!(e, LinkError::MissingRoot { line: 4, .. }), "{e:?}");
    }

    #[test]
    fn ignored_files_define_no_projects_and_disabled_ecosystems_are_skipped() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a"),
            ("ex/rust.fake", "name=ex"),
            ("n/node.fake", "name=n"),
        ]);
        let g = graph(
            &t,
            "version = 1\nignore = [\"ex/**\"]\n[ecosystem.node]\nenabled = false\n",
        );
        assert_eq!(g.display_ids(), ["rust:a"]);
    }

    #[test]
    fn dangling_and_self_edges_are_dropped() {
        let t = Tree::new(&[("a/rust.fake", "name=a\ndep=a\ndep=zzz")]);
        assert!(graph(&t, "").projects[0].deps.is_empty());
    }

    fn many(n: usize) -> Tree {
        let files: Vec<(String, String)> = (0..n)
            .map(|i| (format!("p{i}/rust.fake"), format!("name=p{i}")))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        Tree::new(&refs)
    }

    #[test]
    fn implicit_edges_share_the_edge_budget() {
        // Two entries add 49 edges each. The test lowers the cap instead of building the
        // millions of edges the real one needs; what it checks is that opaque edges already
        // `used` count against the same cap.
        let n = 50;
        let cfg = "version = 1
[[project]]
select = \"rust:p0\"
implicit_dependencies = [\"eco:rust\"]
[[project]]
select = \"rust:p1\"
implicit_dependencies = [\"eco:rust\"]
";
        let g = graph(&many(n), "version = 1\n");
        let config = config::parse(cfg).unwrap();
        let run = |used: usize, max: usize| {
            apply_config(&config, &mut g.projects.clone(), &mut Vec::new(), used, max)
        };
        assert_eq!(run(0, 98), Ok(()));
        assert_eq!(run(0, 97), Err(LinkError::TooManyEdges));
        assert_eq!(run(1, 98), Err(LinkError::TooManyEdges));
        assert_eq!(run(60, 150), Err(LinkError::TooManyEdges));
        assert_eq!(run(52, 150), Ok(()));
        // The real cap is far above anything a small input reaches.
        assert_eq!(run(0, MAX_IMPLICIT_EDGES), Ok(()));
    }

    #[test]
    fn repeated_selectors_are_deduplicated_and_removals_apply() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a"),
            ("b/rust.fake", "name=b"),
            ("c/rust.fake", "name=c"),
        ]);
        let g = graph(
            &t,
            "version = 1
[[project]]
select = \"rust:a\"
implicit_dependencies = [\"eco:rust\", \"rust:b\", \"!rust:c\"]
",
        );
        assert_eq!(
            edges(&g, "rust:a"),
            [("rust:b".to_owned(), DepKind::Implicit)]
        );
    }

    #[test]
    fn removing_many_keys_is_linear() {
        let n = 2_000;
        let mut cfg = String::from(
            "version = 1
[[project]]
select = \"rust:p0\"
implicit_dependencies = [\"eco:rust\", \"!eco:rust\"",
        );
        for i in 1..n {
            cfg.push_str(&format!(", \"!rust:p{i}\""));
        }
        cfg.push_str(
            "]
",
        );
        let start = std::time::Instant::now();
        let g = graph(&many(n), &cfg);
        assert!(edges(&g, "rust:p0").is_empty());
        assert!(start.elapsed().as_secs() < 30, "{:?}", start.elapsed());
    }
}
