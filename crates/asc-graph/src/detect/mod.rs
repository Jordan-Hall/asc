//! The `Detector` trait and its link-time context (SPEC §12A.2).
//!
//! A detector is one module that implements [`Detector`] plus one line in
//! [`crate::registry::DETECTORS`]; there is no dynamic loading.

pub mod dotnet;
pub mod node;
pub mod python;
pub mod rust;

use std::cell::RefCell;
use std::collections::BTreeMap;

use asc_core::path::RepoPath;

use crate::config::ResolvedConfig;
use crate::model::{Ecosystem, Issue, Parsed, Project};

/// What a claimed path is to its ecosystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Claim {
    Manifest,
    Lockfile,
    /// A change affects every project of the ecosystem under the file's directory.
    EcoGlobal,
}

/// Why `parse` rejected a file. The caller attaches the path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ParseFailure {
    pub message: String,
}

impl ParseFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// One ecosystem's detector.
pub trait Detector: Send + Sync {
    fn ecosystem(&self) -> Ecosystem;
    /// Claims a path by basename or suffix; this is the only place paths are classified.
    fn claim(&self, path: &RepoPath) -> Option<Claim>;
    /// Pure; the result is shared by content, so the path is not an input.
    fn parse(&self, claim: Claim, bytes: &[u8]) -> Result<Parsed, ParseFailure>;
    /// Turns every parsed file of this ecosystem into projects.
    ///
    /// Edges must name keys of projects this call returns or of other
    /// ecosystems' projects; unknown targets are dropped by the caller.
    fn link(&self, cx: &LinkCx<'_>) -> Vec<Project>;
}

/// The detector that claims `path` and how, unless its ecosystem is disabled.
pub fn claim_of(
    detectors: &[&dyn Detector],
    config: &ResolvedConfig,
    path: &RepoPath,
) -> Option<(Ecosystem, Claim)> {
    detectors.iter().find_map(|d| {
        let eco = d.ecosystem();
        if !config.eco(eco).enabled {
            return None;
        }
        d.claim(path).map(|c| (eco, c))
    })
}

/// True for paths a detector claims, `.asc/projects.toml` and `.asc/ci.toml`.
pub fn is_claimed(detectors: &[&dyn Detector], config: &ResolvedConfig, path: &RepoPath) -> bool {
    matches!(path.as_bytes(), b".asc/projects.toml" | b".asc/ci.toml")
        || claim_of(detectors, config, path).is_some()
}

/// One successfully parsed, claimed file.
#[derive(Clone, Copy, Debug)]
pub struct LinkFile<'a> {
    pub path: &'a RepoPath,
    pub eco: Ecosystem,
    pub claim: Claim,
    /// Content id of the file.
    pub id: [u8; 32],
    pub parsed: &'a Parsed,
}

/// What `link` sees: the manifest set and nothing else. No tree walk happens
/// and no file outside the set is read.
pub struct LinkCx<'a> {
    eco: Ecosystem,
    files: Vec<LinkFile<'a>>,
    by_dir: BTreeMap<Option<RepoPath>, Vec<usize>>,
    gitlinks: &'a [RepoPath],
    config: &'a ResolvedConfig,
    issues: RefCell<Vec<Issue>>,
}

impl<'a> LinkCx<'a> {
    /// A context for ecosystem `eco` over the claimed files of every ecosystem.
    pub fn new(
        eco: Ecosystem,
        mut files: Vec<LinkFile<'a>>,
        gitlinks: &'a [RepoPath],
        config: &'a ResolvedConfig,
    ) -> Self {
        files.sort_by(|a, b| a.path.cmp(b.path));
        let mut by_dir: BTreeMap<Option<RepoPath>, Vec<usize>> = BTreeMap::new();
        for (i, f) in files.iter().enumerate() {
            by_dir.entry(f.path.parent()).or_default().push(i);
        }
        Self {
            eco,
            files,
            by_dir,
            gitlinks,
            config,
            issues: RefCell::new(Vec::new()),
        }
    }

    pub fn ecosystem(&self) -> Ecosystem {
        self.eco
    }

    /// The files claimed by this context's ecosystem, sorted by path.
    pub fn files(&self) -> impl Iterator<Item = &LinkFile<'a>> {
        self.files.iter().filter(|f| f.eco == self.eco)
    }

    /// This ecosystem's files of one claim.
    pub fn files_of(&self, claim: Claim) -> impl Iterator<Item = &LinkFile<'a>> {
        self.files().filter(move |f| f.claim == claim)
    }

    /// The file at `path`, whichever ecosystem claimed it.
    pub fn file(&self, path: &RepoPath) -> Option<&LinkFile<'a>> {
        self.files
            .binary_search_by(|f| f.path.cmp(path))
            .ok()
            .map(|i| &self.files[i])
    }

    /// Claimed files of every ecosystem directly in `dir` (`None` is the repository root).
    pub fn files_in_dir(&self, dir: Option<&RepoPath>) -> impl Iterator<Item = &LinkFile<'a>> {
        self.by_dir
            .get(&dir.cloned())
            .into_iter()
            .flatten()
            .map(|&i| &self.files[i])
    }

    /// Gitlink paths of the tree, sorted.
    pub fn gitlinks(&self) -> &'a [RepoPath] {
        self.gitlinks
    }

    /// The configuration of the revision being linked.
    pub fn config(&self) -> &'a ResolvedConfig {
        self.config
    }

    /// Records a non-fatal problem.
    pub fn report(&self, path: RepoPath, message: impl Into<Vec<u8>>) {
        self.issues.borrow_mut().push(Issue {
            path,
            message: message.into(),
        });
    }

    /// The issues reported so far.
    pub fn take_issues(&self) -> Vec<Issue> {
        std::mem::take(&mut self.issues.borrow_mut())
    }
}

/// A fake two-ecosystem detector and tree builder shared by the unit tests.
///
/// Files are `<eco>.fake` manifests, `<eco>.lock` lockfiles and `<eco>.global`
/// eco-globals (`rust` and `node`). A manifest has one `key=value` per line:
/// `name`, `dep` (repeatable), `ext` (becomes `external_fp`), `extra`
/// (repeatable) and the flag `opaque`.
#[cfg(test)]
pub(crate) mod testkit {
    use std::collections::BTreeMap;

    use asc_core::object::Kind;

    use super::*;
    use crate::canon::Value;
    use crate::link::{LinkError, LinkInput, link_graph};
    use crate::model::{Fingerprint, ManifestSet, ProjectGraph, ProjectKey, ProjectKind};

    pub struct Fake {
        eco: Ecosystem,
        prefix: &'static str,
    }

    pub static RUST: Fake = Fake {
        eco: Ecosystem::Rust,
        prefix: "rust",
    };
    pub static NODE: Fake = Fake {
        eco: Ecosystem::Node,
        prefix: "node",
    };

    pub fn detectors() -> Vec<&'static dyn Detector> {
        vec![&RUST, &NODE]
    }

    pub fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn field<'v>(v: &'v Value, key: &str) -> Option<&'v Value> {
        match v {
            Value::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn strings(v: Option<&Value>) -> Vec<String> {
        match v {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|x| match x {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    impl Detector for Fake {
        fn ecosystem(&self) -> Ecosystem {
            self.eco
        }

        fn claim(&self, path: &RepoPath) -> Option<Claim> {
            let n = path.file_name();
            let n = std::str::from_utf8(n.as_bytes()).ok()?;
            let ext = n.strip_prefix(self.prefix)?.strip_prefix('.')?;
            match ext {
                "fake" => Some(Claim::Manifest),
                "lock" => Some(Claim::Lockfile),
                "global" => Some(Claim::EcoGlobal),
                _ => None,
            }
        }

        fn parse(&self, claim: Claim, bytes: &[u8]) -> Result<Parsed, ParseFailure> {
            if claim != Claim::Manifest {
                return Ok(Parsed(Value::Null));
            }
            let text = std::str::from_utf8(bytes).map_err(|_| ParseFailure::new("not utf-8"))?;
            let (mut deps, mut extra) = (Vec::new(), Vec::new());
            let mut pairs = Vec::new();
            for line in text.lines().filter(|l| !l.is_empty()) {
                match line.split_once('=') {
                    Some(("dep", v)) => deps.push(Value::Str(v.into())),
                    Some(("extra", v)) => extra.push(Value::Str(v.into())),
                    Some((k @ ("name" | "ext"), v)) => {
                        pairs.push((k.to_owned(), Value::Str(v.into())))
                    }
                    None if line == "opaque" => pairs.push(("opaque".into(), Value::Bool(true))),
                    _ => return Err(ParseFailure::new(format!("bad line {line:?}"))),
                }
            }
            pairs.push(("deps".into(), Value::Array(deps)));
            pairs.push(("extra".into(), Value::Array(extra)));
            Value::map(pairs)
                .map(Parsed)
                .map_err(|e| ParseFailure::new(e.to_string()))
        }

        fn link(&self, cx: &LinkCx<'_>) -> Vec<Project> {
            let mut out: Vec<(Project, Vec<String>)> = Vec::new();
            for f in cx.files_of(Claim::Manifest) {
                let v = &f.parsed.0;
                let name = match field(v, "name") {
                    Some(Value::Str(s)) => s.clone(),
                    _ => f
                        .path
                        .parent()
                        .map(|d| d.file_name().to_string())
                        .unwrap_or_default(),
                };
                let key = ProjectKey {
                    eco: self.eco,
                    manifest: f.path.clone(),
                };
                let mut p = Project::new(key, name.as_bytes(), ProjectKind::Package);
                if let Some(Value::Str(e)) = field(v, "ext") {
                    p.external_fp = Fingerprint::from(*blake3::hash(e.as_bytes()).as_bytes());
                }
                p.extra_inputs = strings(field(v, "extra"))
                    .into_iter()
                    .map(String::into_bytes)
                    .collect();
                p.opaque = matches!(field(v, "opaque"), Some(Value::Bool(true)));
                out.push((p, strings(field(v, "deps"))));
            }
            let by_name: BTreeMap<Vec<u8>, ProjectKey> = out
                .iter()
                .map(|(p, _)| (p.name.clone(), p.key.clone()))
                .collect();
            out.into_iter()
                .map(|(mut p, deps)| {
                    for d in deps {
                        if let Some(k) = by_name.get(d.as_bytes()) {
                            p.deps.push((k.clone(), crate::model::DepKind::Normal));
                        }
                    }
                    p
                })
                .collect()
        }
    }

    /// An in-memory tree of path -> content.
    #[derive(Clone, Debug, Default)]
    pub struct Tree(pub BTreeMap<String, String>);

    impl Tree {
        pub fn new(files: &[(&str, &str)]) -> Self {
            Self(
                files
                    .iter()
                    .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
                    .collect(),
            )
        }

        pub fn set(&mut self, path: &str, content: &str) {
            self.0.insert(path.to_owned(), content.to_owned());
        }

        pub fn remove(&mut self, path: &str) {
            self.0.remove(path);
        }
    }

    /// Links a tree; panics on a link error.
    pub fn build(
        tree: &Tree,
        gitlinks: &[RepoPath],
        config: ResolvedConfig,
    ) -> (ProjectGraph, ManifestSet) {
        try_build(tree, gitlinks, config).unwrap()
    }

    pub fn try_build(
        tree: &Tree,
        gitlinks: &[RepoPath],
        config: ResolvedConfig,
    ) -> Result<(ProjectGraph, ManifestSet), LinkError> {
        let dets = detectors();
        let mut inputs = Vec::new();
        let mut set = Vec::new();
        for (p, c) in &tree.0 {
            let path = rp(p);
            let Some((eco, claim)) = claim_of(&dets, &config, &path) else {
                continue;
            };
            let d = dets.iter().find(|d| d.ecosystem() == eco).unwrap();
            let id = *blake3::hash(c.as_bytes()).as_bytes();
            set.push((path.clone(), Kind::File, id));
            inputs.push(LinkInput {
                path,
                id,
                parsed: d.parse(claim, c.as_bytes()),
            });
        }
        set.extend(gitlinks.iter().map(|g| (g.clone(), Kind::Gitlink, [0; 32])));
        let exists = |d: &RepoPath| {
            gitlinks.contains(d)
                || tree.0.keys().any(|k| {
                    k.as_bytes().starts_with(d.as_bytes())
                        && k.as_bytes().get(d.as_bytes().len()) == Some(&b'/')
                })
        };
        let graph = link_graph(&dets, &inputs, gitlinks, config, &exists)?;
        Ok((graph, ManifestSet::new(set).unwrap()))
    }
}
