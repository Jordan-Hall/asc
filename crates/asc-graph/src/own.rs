//! Path ownership (SPEC §12A.1): the deepest `Package` root wins, ties share.

use std::collections::{BTreeMap, HashMap};

use asc_core::path::RepoPath;

use crate::detect::{Detector, is_claimed};
use crate::model::{ProjectGraph, ProjectKind};
use crate::pattern::IgnoreSet;

/// True when a path component is `node_modules`.
pub fn in_node_modules(path: &RepoPath) -> bool {
    path.components().any(|c| c == b"node_modules")
}

/// Ownership lookup over one graph.
pub struct Ownership<'a> {
    detectors: &'a [&'a dyn Detector],
    graph: &'a ProjectGraph,
    ignore: IgnoreSet,
    /// Package roots; `None` is the repository root. Values index `graph.projects`.
    roots: HashMap<Option<RepoPath>, Vec<usize>>,
    /// Projects by their own manifest.
    manifests: BTreeMap<&'a RepoPath, Vec<usize>>,
}

impl<'a> Ownership<'a> {
    /// Indexes `graph`; `detectors` decide which paths are claimed.
    pub fn new(
        graph: &'a ProjectGraph,
        detectors: &'a [&'a dyn Detector],
    ) -> Result<Self, crate::pattern::PatternError> {
        let ignore = IgnoreSet::new(graph.config.ignore.iter().map(String::as_str))?;
        let mut roots: HashMap<Option<RepoPath>, Vec<usize>> = HashMap::new();
        let mut manifests: BTreeMap<&RepoPath, Vec<usize>> = BTreeMap::new();
        for (i, p) in graph.projects.iter().enumerate() {
            manifests.entry(&p.key.manifest).or_default().push(i);
            // Gitlink members own nothing.
            if p.kind == ProjectKind::Package && p.gitlink.is_none() {
                roots.entry(p.root.clone()).or_default().push(i);
            }
        }
        Ok(Self {
            detectors,
            graph,
            ignore,
            roots,
            manifests,
        })
    }

    /// True for paths matched by `ignore` or inside `node_modules`: no owner, no trigger.
    pub fn is_excluded(&self, path: &RepoPath) -> bool {
        in_node_modules(path) || self.ignore.is_ignored(path)
    }

    /// True when a detector (or the config files) claims the path.
    pub fn is_claimed(&self, path: &RepoPath) -> bool {
        is_claimed(self.detectors, &self.graph.config, path)
    }

    /// The owners of a file path, as sorted indexes into `graph.projects`.
    ///
    /// A claimed path is owned only by the projects whose manifest it is.
    pub fn owners(&self, path: &RepoPath) -> Vec<usize> {
        self.owners_from(path, path.parent())
    }

    /// Like [`Self::owners`], but for a gitlink: a project rooted at the
    /// gitlink itself counts, so a declared project can follow a pin bump.
    pub fn owners_of_gitlink(&self, path: &RepoPath) -> Vec<usize> {
        self.owners_from(path, Some(path.clone()))
    }

    fn owners_from(&self, path: &RepoPath, start: Option<RepoPath>) -> Vec<usize> {
        if self.is_excluded(path) {
            return Vec::new();
        }
        if self.is_claimed(path) {
            return self.manifests.get(path).cloned().unwrap_or_default();
        }
        let mut dir = start;
        loop {
            if let Some(v) = self.roots.get(&dir) {
                return v.clone();
            }
            match dir {
                None => return Vec::new(),
                Some(d) => dir = d.parent(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResolvedConfig;
    use crate::detect::testkit::{Tree, build, rp};

    fn asc() -> Tree {
        Tree::new(&[
            ("node.fake", "name=@asc/private"),
            (
                "apps/analysis-pipeline/pipeline-api/rust.fake",
                "name=pipeline-api",
            ),
            (
                "apps/analysis-pipeline/pipeline-api/node.fake",
                "name=analysis-pipeline-api",
            ),
            ("apps/analysis-pipeline/pipeline-api/src/main.rs", "x"),
            ("packages/dsl/compiler/node.fake", "name=@asc/dsl-compiler"),
            ("packages/dsl/compiler/native/rust.fake", "name=dsl-core"),
        ])
    }

    fn names(g: &ProjectGraph, idx: Vec<usize>) -> Vec<String> {
        idx.into_iter()
            .map(|i| g.display_ids()[i].clone())
            .collect()
    }

    #[test]
    fn ties_share_and_deepest_wins() {
        let (g, _) = build(&asc(), &[], ResolvedConfig::default());
        let dets = crate::detect::testkit::detectors();
        let o = Ownership::new(&g, &dets).unwrap();
        let tie = names(
            &g,
            o.owners(&rp("apps/analysis-pipeline/pipeline-api/src/main.rs")),
        );
        assert_eq!(tie, ["rust:pipeline-api", "node:analysis-pipeline-api"]);
        let deep = names(&g, o.owners(&rp("packages/dsl/compiler/native/lib.rs")));
        assert_eq!(deep, ["rust:dsl-core"]);
        let up = names(&g, o.owners(&rp("packages/dsl/compiler/index.ts")));
        assert_eq!(up, ["node:@asc/dsl-compiler"]);
    }

    #[test]
    fn root_package_owns_the_rest_and_unowned_otherwise() {
        let (g, _) = build(&asc(), &[], ResolvedConfig::default());
        let dets = crate::detect::testkit::detectors();
        let o = Ownership::new(&g, &dets).unwrap();
        // Root package.json has a name, so it owns everything not owned deeper.
        assert_eq!(names(&g, o.owners(&rp("docs/a.md"))), ["node:@asc/private"]);
        let t = Tree::new(&[("a/rust.fake", "name=a")]);
        let (g, _) = build(&t, &[], ResolvedConfig::default());
        let o = Ownership::new(&g, &dets).unwrap();
        assert!(o.owners(&rp("docs/a.md")).is_empty());
    }

    #[test]
    fn claimed_paths_are_owned_by_their_project_only() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a"),
            ("a/rust.lock", "x"),
            ("a/b/rust.fake", "name=b"),
        ]);
        let (g, _) = build(&t, &[], ResolvedConfig::default());
        let dets = crate::detect::testkit::detectors();
        let o = Ownership::new(&g, &dets).unwrap();
        assert!(o.is_claimed(&rp("a/rust.lock")));
        assert!(o.owners(&rp("a/rust.lock")).is_empty());
        assert_eq!(names(&g, o.owners(&rp("a/rust.fake"))), ["rust:a"]);
        assert_eq!(names(&g, o.owners(&rp("a/b/rust.fake"))), ["rust:b"]);
        assert_eq!(names(&g, o.owners(&rp("a/x.rs"))), ["rust:a"]);
    }

    #[test]
    fn ignored_and_node_modules_have_no_owner() {
        let t = Tree::new(&[
            ("a/rust.fake", "name=a"),
            ("a/fixtures/rust.fake", "name=f"),
        ]);
        let cfg = crate::config::parse("version = 1\nignore = [\"**/fixtures/**\"]\n").unwrap();
        let (g, _) = build(&t, &[], cfg);
        let dets = crate::detect::testkit::detectors();
        let o = Ownership::new(&g, &dets).unwrap();
        assert_eq!(g.projects.len(), 1);
        assert!(o.is_excluded(&rp("a/fixtures/x.rs")));
        assert!(o.owners(&rp("a/fixtures/x.rs")).is_empty());
        assert!(o.owners(&rp("a/node_modules/x/index.js")).is_empty());
    }

    #[test]
    fn gitlink_members_own_nothing_and_declared_root_follows_gitlink() {
        let t = Tree::new(&[("a/rust.fake", "name=a")]);
        let cfg =
            crate::config::parse("version = 1\n[[project]]\nname = \"pin\"\nroot = \"a/vendor\"\n")
                .unwrap();
        let (g, _) = build(&t, &[rp("a/vendor")], cfg);
        let dets = crate::detect::testkit::detectors();
        let o = Ownership::new(&g, &dets).unwrap();
        assert_eq!(
            names(&g, o.owners_of_gitlink(&rp("a/vendor"))),
            ["declared:pin"]
        );
        assert_eq!(names(&g, o.owners(&rp("a/vendor"))), ["rust:a"]);
    }
}
