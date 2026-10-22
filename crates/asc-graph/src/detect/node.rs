//! Node detector: `package.json` workspaces and `bun.lock` (SPEC §12A.2.4,
//! §12A.2.6, node rows of §12A.2 and §12A.4).

use std::collections::BTreeMap;

use asc_core::path::RepoPath;
use globset::{GlobBuilder, GlobMatcher};

use crate::canon::{self, Value};
use crate::detect::{Claim, Detector, LinkCx, LinkFile, ParseFailure};
use crate::jsonc;
use crate::lock::bun::{self, DepRef, Lock, get, get_str};
use crate::model::{
    DepKind, Ecosystem, Fingerprint, MAX_MANIFEST_BYTES, Parsed, Project, ProjectKey, ProjectKind,
};

/// Fields of a `package.json` that the detector keeps.
const KEPT: [&str; 9] = [
    "name",
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "patchedDependencies",
    "packageManager",
    "engines",
    "trustedDependencies",
];

/// Root fields that make up `config_fp` (SPEC §12A.4).
const CONFIG_FIELDS: [&str; 3] = ["packageManager", "engines", "trustedDependencies"];

/// Files that change every Node project under their directory.
const ECO_GLOBALS: [&[u8]; 8] = [
    b"bunfig.toml",
    b".npmrc",
    b".nvmrc",
    b".node-version",
    b"bun.lockb",
    b"package-lock.json",
    b"pnpm-lock.yaml",
    b"yarn.lock",
];

/// The Node (npm, Bun) detector.
pub struct Node;

impl Detector for Node {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Node
    }

    fn claim(&self, path: &RepoPath) -> Option<Claim> {
        let name = path.file_name();
        let name = name.as_bytes();
        match name {
            b"package.json" => Some(Claim::Manifest),
            b"bun.lock" => Some(Claim::Lockfile),
            b"turbo.json" if path.parent().is_none() => Some(Claim::EcoGlobal),
            n if ECO_GLOBALS.contains(&n) => Some(Claim::EcoGlobal),
            // Patch files named by `patchedDependencies`; bun writes them to `patches/`.
            n if n.ends_with(b".patch")
                && path.components().rev().skip(1).any(|c| c == b"patches") =>
            {
                Some(Claim::Lockfile)
            }
            _ => None,
        }
    }

    fn parse(&self, claim: Claim, bytes: &[u8]) -> Result<Parsed, ParseFailure> {
        match claim {
            Claim::Manifest => parse_manifest(bytes),
            // A patch file shares the Lockfile claim; its content id is all that matters.
            Claim::Lockfile if bun::looks_like_lock(bytes) => Ok(bun::parse(bytes)),
            Claim::Lockfile | Claim::EcoGlobal => Ok(Parsed(Value::Null)),
        }
    }

    fn link(&self, cx: &LinkCx<'_>) -> Vec<Project> {
        Linker::new(cx).projects(cx)
    }
}

fn fail(msg: impl Into<String>) -> ParseFailure {
    ParseFailure::new(msg)
}

fn all_strings(v: &serde_json::Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.values().all(serde_json::Value::is_string))
}

/// Keeps the fields the detector reads; `workspaces` becomes an array of globs.
fn parse_manifest(bytes: &[u8]) -> Result<Parsed, ParseFailure> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(fail(format!(
            "manifest larger than {MAX_MANIFEST_BYTES} bytes"
        )));
    }
    let doc = jsonc::parse(bytes).map_err(|e| fail(e.to_string()))?;
    let serde_json::Value::Object(obj) = doc else {
        return Err(fail("package.json is not an object"));
    };
    let mut kept = serde_json::Map::new();
    for key in KEPT {
        if let Some(v) = obj.get(key) {
            kept.insert(key.to_owned(), v.clone());
        }
    }
    if kept.get("name").is_some_and(|n| !n.is_string()) {
        return Err(fail("name is not text"));
    }
    for (table, _) in bun::TABLES
        .iter()
        .chain(&[("patchedDependencies", DepKind::Normal)])
    {
        if kept.get(*table).is_some_and(|t| !all_strings(t)) {
            return Err(fail(format!("{table} is not a table of text")));
        }
    }
    if let Some(w) = obj.get("workspaces") {
        let globs = match w {
            serde_json::Value::Array(a) => Some(a),
            serde_json::Value::Object(o) => match o.get("packages") {
                Some(serde_json::Value::Array(a)) => Some(a),
                None => None,
                Some(_) => return Err(fail("unsupported workspaces")),
            },
            _ => return Err(fail("unsupported workspaces")),
        };
        let globs: Vec<_> = globs.map(Vec::as_slice).unwrap_or_default().to_vec();
        if !globs.iter().all(serde_json::Value::is_string) {
            return Err(fail("unsupported workspaces"));
        }
        kept.insert("workspaces".into(), serde_json::Value::Array(globs));
    }
    canon::from_json(&serde_json::Value::Object(kept))
        .map(Parsed)
        .map_err(|e| fail(e.to_string()))
}

/// The directory of a path as text; `""` is the repository root.
fn dir_of(p: &RepoPath) -> String {
    p.parent()
        .map(|d| String::from_utf8_lossy(d.as_bytes()).into_owned())
        .unwrap_or_default()
}

/// `dir` relative to `root`, when it is `root` or below it.
fn under<'d>(dir: &'d str, root: &str) -> Option<&'d str> {
    if root.is_empty() {
        return Some(dir);
    }
    let rest = dir.strip_prefix(root)?;
    if rest.is_empty() {
        Some(rest)
    } else {
        rest.strip_prefix('/')
    }
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{a}/{b}"),
    }
}

/// `rel` resolved against `base`; `None` when it leaves the repository.
fn resolve_rel(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = base.split('/').filter(|c| !c.is_empty()).collect();
    for c in rel.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join("/"))
}

fn last_segment(dir: &str) -> &str {
    dir.rsplit('/').next().unwrap_or(dir)
}

fn glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
}

/// The `workspaces` globs of a root, split into inclusions and exclusions.
#[derive(Default)]
struct Patterns {
    include: Vec<(String, GlobMatcher)>,
    exclude: Vec<GlobMatcher>,
}

impl Patterns {
    fn new(cx: &LinkCx<'_>, root: &LinkFile<'_>) -> Self {
        let mut out = Self::default();
        let Some(Value::Array(globs)) = get(&root.parsed.0, "workspaces") else {
            return out;
        };
        for g in globs {
            let Value::Str(g) = g else { continue };
            let g = g.trim().trim_start_matches("./").trim_end_matches('/');
            let (negated, g) = match g.strip_prefix('!') {
                Some(rest) => (true, rest.trim_end_matches('/')),
                None => (false, g),
            };
            // A pattern may name the manifest instead of its directory.
            let g = g.strip_suffix("/package.json").unwrap_or(g);
            match glob(g) {
                Ok(m) if negated => out.exclude.push(m),
                Ok(m) => out.include.push((g.to_owned(), m)),
                Err(e) => cx.report(root.path.clone(), format!("workspace pattern {g:?}: {e}")),
            }
        }
        out
    }

    fn matches(&self, rel: &str) -> bool {
        self.include.iter().any(|(_, m)| m.is_match(rel))
            && !self.exclude.iter().any(|m| m.is_match(rel))
    }

    /// True when an inclusion matches `rel` or something below it.
    fn reaches_under(&self, rel: &str) -> bool {
        let gs: Vec<&str> = rel.split('/').collect();
        self.include.iter().any(|(p, _)| {
            for (i, seg) in p.split('/').enumerate() {
                if seg == "**" {
                    return true;
                }
                let Some(g) = gs.get(i) else { return true };
                if !glob(seg).is_ok_and(|m| m.is_match(g)) {
                    return false;
                }
            }
            p.split('/').count() >= gs.len()
        })
    }
}

/// What a workspace knows about its lockfile.
enum LockState<'a> {
    Absent,
    /// Over a limit or unreadable: every member takes the file's id.
    Unusable(Fingerprint),
    Usable(Lock<'a>),
}

struct Member<'a> {
    key: ProjectKey,
    name: String,
    /// Directory from the repository root; `""` is the root.
    dir: String,
    /// Directory from the workspace root; the `workspaces` key of the lock.
    rel: String,
    /// A `package.json` document or a lock workspace entry.
    doc: &'a Value,
    gitlink: Option<RepoPath>,
    group: usize,
}

struct Group<'a> {
    dir: String,
    root: LinkFile<'a>,
    lock: LockState<'a>,
    config_fp: Fingerprint,
    /// Member indexes by package name.
    names: BTreeMap<String, Vec<usize>>,
}

struct Linker<'a> {
    members: Vec<Member<'a>>,
    groups: Vec<Group<'a>>,
    /// Member index by directory.
    by_dir: BTreeMap<String, usize>,
}

impl<'a> Linker<'a> {
    fn new(cx: &LinkCx<'a>) -> Self {
        let pkgs: Vec<LinkFile<'a>> = cx.files_of(Claim::Manifest).copied().collect();
        let mut roots: Vec<LinkFile<'a>> = pkgs
            .iter()
            .filter(|f| get(&f.parsed.0, "workspaces").is_some())
            .copied()
            .collect();
        roots.sort_by(|a, b| {
            (a.path.components().count(), a.path).cmp(&(b.path.components().count(), b.path))
        });
        // The shallowest root governs; deeper ones are members or nothing.
        let mut chosen: Vec<LinkFile<'a>> = Vec::new();
        for r in roots {
            let d = dir_of(r.path);
            if !chosen.iter().any(|c| under(&d, &dir_of(c.path)).is_some()) {
                chosen.push(r);
            }
        }
        let mut me = Self {
            members: Vec::new(),
            groups: Vec::new(),
            by_dir: BTreeMap::new(),
        };
        for root in &chosen {
            me.workspace(cx, &pkgs, *root);
        }
        for f in &pkgs {
            let d = dir_of(f.path);
            let covered = chosen.iter().any(|c| under(&d, &dir_of(c.path)).is_some());
            if !covered && get_str(&f.parsed.0, "name").is_some() {
                me.standalone(cx, *f);
            }
        }
        me
    }

    fn lock_state(cx: &LinkCx<'a>, dir: &str) -> LockState<'a> {
        let Some(path) = RepoPath::new(join(dir, "bun.lock")).ok() else {
            return LockState::Absent;
        };
        match cx.file(&path).copied() {
            Some(f) if f.eco == Ecosystem::Node && f.claim == Claim::Lockfile => {
                match Lock::view(f.parsed) {
                    Ok(l) => LockState::Usable(l),
                    Err(why) => {
                        cx.report(f.path.clone(), format!("bun.lock is unusable: {why}"));
                        LockState::Unusable(Fingerprint::from(f.id))
                    }
                }
            }
            _ => LockState::Absent,
        }
    }

    fn new_group(&mut self, cx: &LinkCx<'a>, root: LinkFile<'a>) -> usize {
        let dir = dir_of(root.path);
        let mut pairs = Vec::new();
        for f in CONFIG_FIELDS {
            if let Some(v) = get(&root.parsed.0, f) {
                pairs.push((f.to_owned(), v.clone()));
            }
        }
        let config_fp =
            Value::map(pairs).map_or_else(|_| Fingerprint::default(), |v| v.fingerprint());
        self.groups.push(Group {
            lock: Self::lock_state(cx, &dir),
            dir,
            root,
            config_fp,
            names: BTreeMap::new(),
        });
        self.groups.len() - 1
    }

    fn add(&mut self, group: usize, m: Member<'a>) {
        let i = self.members.len();
        self.groups[group]
            .names
            .entry(m.name.clone())
            .or_default()
            .push(i);
        self.by_dir.insert(m.dir.clone(), i);
        self.members.push(m);
    }

    fn standalone(&mut self, cx: &LinkCx<'a>, f: LinkFile<'a>) {
        let g = self.new_group(cx, f);
        self.add(g, member(&f, g, String::new()));
    }

    fn workspace(&mut self, cx: &LinkCx<'a>, pkgs: &[LinkFile<'a>], root: LinkFile<'a>) {
        let g = self.new_group(cx, root);
        let rd = self.groups[g].dir.clone();
        if get_str(&root.parsed.0, "name").is_some() {
            self.add(g, member(&root, g, String::new()));
        }
        let pats = Patterns::new(cx, &root);
        for f in pkgs.iter().filter(|f| f.path != root.path) {
            let d = dir_of(f.path);
            if let Some(rel) = under(&d, &rd).filter(|r| !r.is_empty() && pats.matches(r)) {
                self.add(g, member(f, g, rel.to_owned()));
            }
        }
        self.gitlink_members(cx, g, &root, &pats);
    }

    /// Members inside gitlinks come from the root lock's `workspaces` entries.
    fn gitlink_members(&mut self, cx: &LinkCx<'a>, g: usize, root: &LinkFile<'a>, pats: &Patterns) {
        let rd = self.groups[g].dir.clone();
        let mut found = Vec::new();
        if let LockState::Usable(lock) = &self.groups[g].lock {
            for (key, entry) in lock.workspaces().iter().filter(|(k, _)| !k.is_empty()) {
                let full = join(&rd, key);
                let Some(gl) = cx
                    .gitlinks()
                    .iter()
                    .find(|gl| under(&full, &String::from_utf8_lossy(gl.as_bytes())).is_some())
                else {
                    continue;
                };
                let Ok(manifest) = RepoPath::new(format!("{full}/package.json")) else {
                    continue;
                };
                if !pats.matches(key) || self.by_dir.contains_key(&full) {
                    continue;
                }
                let name = get_str(entry, "name")
                    .unwrap_or_else(|| last_segment(key))
                    .to_owned();
                found.push(Member {
                    key: ProjectKey {
                        eco: Ecosystem::Node,
                        manifest,
                    },
                    name,
                    dir: full,
                    rel: key.clone(),
                    doc: entry,
                    gitlink: Some(gl.clone()),
                    group: g,
                });
            }
        } else {
            for gl in cx.gitlinks() {
                let gd = String::from_utf8_lossy(gl.as_bytes());
                if under(&gd, &rd).is_some_and(|rel| !rel.is_empty() && pats.reaches_under(rel)) {
                    cx.report(
                        root.path.clone(),
                        format!(
                            "workspaces reach gitlink {gl} but no usable bun.lock lists its members"
                        ),
                    );
                }
            }
        }
        for m in found {
            self.add(g, m);
        }
    }

    fn projects(&self, cx: &LinkCx<'a>) -> Vec<Project> {
        self.members.iter().map(|m| self.project(cx, m)).collect()
    }

    /// The members a dependency resolves to inside the repository.
    fn internal(&self, m: &Member<'_>, d: &DepRef<'_>) -> Option<&[usize]> {
        if d.spec.starts_with("npm:") || d.spec.starts_with("catalog:") {
            return None;
        }
        if let Some(rel) = d
            .spec
            .strip_prefix("file:")
            .or_else(|| d.spec.strip_prefix("link:"))
        {
            let target = resolve_rel(&m.dir, rel)?;
            return self.by_dir.get(&target).map(std::slice::from_ref);
        }
        self.groups[m.group].names.get(d.name).map(Vec::as_slice)
    }

    fn project(&self, cx: &LinkCx<'a>, m: &Member<'a>) -> Project {
        let mut p = Project::new(m.key.clone(), m.name.as_bytes(), ProjectKind::Package);
        p.gitlink = m.gitlink.clone();
        let mut external: Vec<[String; 3]> = Vec::new();
        for d in bun::tables(m.doc) {
            match self.internal(m, &d) {
                Some(hits) => p
                    .deps
                    .extend(hits.iter().map(|&i| (self.members[i].key.clone(), d.kind))),
                None => external.push([d.table.to_owned(), d.name.to_owned(), d.spec.to_owned()]),
            }
        }
        let g = &self.groups[m.group];
        p.config_fp = g.config_fp;
        p.external_fp = match &g.lock {
            LockState::Unusable(fp) => *fp,
            LockState::Usable(lock) => match lock.workspace(&m.rel) {
                Some(entry) => lock.closure(entry, &m.name, &|ident| patch_id(cx, g, ident)),
                None => declared(external),
            },
            LockState::Absent => declared(external),
        };
        p
    }
}

fn member<'a>(f: &LinkFile<'a>, group: usize, rel: String) -> Member<'a> {
    let dir = dir_of(f.path);
    let name = get_str(&f.parsed.0, "name")
        .unwrap_or_else(|| last_segment(&dir))
        .to_owned();
    Member {
        key: ProjectKey {
            eco: Ecosystem::Node,
            manifest: f.path.clone(),
        },
        name,
        dir,
        rel,
        doc: &f.parsed.0,
        gitlink: None,
        group,
    }
}

/// The fingerprint of the declared external specs, for a workspace without a usable lock.
fn declared(mut external: Vec<[String; 3]>) -> Fingerprint {
    external.sort();
    external.dedup();
    Value::Array(
        external
            .into_iter()
            .map(|t| Value::Array(t.into_iter().map(Value::Str).collect()))
            .collect(),
    )
    .fingerprint()
}

/// The id of the patch file that root `patchedDependencies` names for `ident`.
///
/// A patch outside the manifest set has no content id here; the hash of its path stands in.
fn patch_id(cx: &LinkCx<'_>, g: &Group<'_>, ident: &str) -> Option<[u8; 32]> {
    let rel = get_str(get(&g.root.parsed.0, "patchedDependencies")?, ident)?;
    let full = resolve_rel(&g.dir, rel)?;
    match RepoPath::new(full.as_str())
        .ok()
        .and_then(|p| cx.file(&p).map(|f| f.id))
    {
        Some(id) => Some(id),
        None => Some(*blake3::hash(full.as_bytes()).as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use asc_core::object::Kind;

    use super::*;
    use crate::affected::{ChangeKind, ChangedPath, Query, Side, affected};
    use crate::config::ResolvedConfig;
    use crate::detect::claim_of;
    use crate::link::{LinkInput, link_graph};
    use crate::model::{DepKind, ManifestSet, ProjectGraph};

    macro_rules! fixture {
        ($($p:literal),* $(,)?) => {
            [$(($p, include_str!(concat!("../../tests/fixtures/node/asc/", $p)))),*]
        };
    }

    const ASC: [(&str, &str); 10] = fixture![
        "package.json",
        "bun.lock",
        "packages/ui/package.json",
        "packages/dsl/core/rust/package.json",
        "packages/dsl/core/rust/npm/linux-x64-gnu/package.json",
        "packages/soma/package.json",
        "apps/analysis/pipeline/api/package.json",
        "apps/web/package.json",
        "apps/web/tools/package.json",
        "patches/compromise@14.14.5.patch",
    ];

    const SDK_ROOT: (&str, &str) = (
        "packages/soma/sdk/package.json",
        include_str!("../../tests/fixtures/node/asc/packages/soma/sdk/package.json"),
    );

    type Tree = BTreeMap<String, String>;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn asc() -> Tree {
        let mut t: Tree = ASC
            .iter()
            .map(|(p, c)| ((*p).into(), (*c).into()))
            .collect();
        t.insert(SDK_ROOT.0.into(), SDK_ROOT.1.into());
        t
    }

    fn tree(files: &[(&str, &str)]) -> Tree {
        files
            .iter()
            .map(|(p, c)| ((*p).into(), (*c).into()))
            .collect()
    }

    fn with(mut t: Tree, path: &str, content: &str) -> Tree {
        t.insert(path.into(), content.into());
        t
    }

    fn edit(t: &Tree, path: &str, from: &str, to: &str) -> Tree {
        let old = &t[path];
        assert!(old.contains(from), "{path} lacks {from:?}");
        with(t.clone(), path, &old.replace(from, to))
    }

    fn build(t: &Tree, gitlinks: &[&str]) -> (ProjectGraph, ManifestSet) {
        let gitlinks: Vec<RepoPath> = gitlinks.iter().map(|g| rp(g)).collect();
        let dets: Vec<&dyn Detector> = vec![&Node];
        let config = ResolvedConfig::default();
        let (mut inputs, mut set) = (Vec::new(), Vec::new());
        for (p, c) in t {
            let path = rp(p);
            let Some((_, claim)) = claim_of(&dets, &config, &path) else {
                continue;
            };
            let id = *blake3::hash(c.as_bytes()).as_bytes();
            set.push((path.clone(), Kind::File, id));
            inputs.push(LinkInput {
                path,
                id,
                parsed: Node.parse(claim, c.as_bytes()),
            });
        }
        set.extend(gitlinks.iter().map(|g| (g.clone(), Kind::Gitlink, [0; 32])));
        let exists = |d: &RepoPath| {
            gitlinks.contains(d) || t.keys().any(|k| k.starts_with(&format!("{d}/")))
        };
        let g = link_graph(&dets, &inputs, &gitlinks, config, &exists).unwrap();
        (g, ManifestSet::new(set).unwrap())
    }

    fn graph(t: &Tree, gitlinks: &[&str]) -> ProjectGraph {
        build(t, gitlinks).0
    }

    fn ids(g: &ProjectGraph) -> Vec<String> {
        g.display_ids()
    }

    fn project<'g>(g: &'g ProjectGraph, name: &str) -> &'g Project {
        g.projects
            .iter()
            .find(|p| p.name == name.as_bytes())
            .unwrap_or_else(|| panic!("no project {name}: {:?}", ids(g)))
    }

    /// `Kind:id` of each dependency, in graph order.
    fn deps(g: &ProjectGraph, name: &str) -> Vec<String> {
        let ids = ids(g);
        project(g, name)
            .deps
            .iter()
            .map(|(k, kind)| {
                let i = g.projects.iter().position(|p| p.key == *k).unwrap();
                format!("{kind:?}:{}", ids[i])
            })
            .collect()
    }

    /// Names whose `external_fp` differs between two graphs of the same members.
    fn external_changed(a: &ProjectGraph, b: &ProjectGraph) -> Vec<String> {
        assert_eq!(ids(a), ids(b));
        a.projects
            .iter()
            .zip(&b.projects)
            .filter(|(x, y)| x.external_fp != y.external_fp)
            .map(|(x, _)| x.name_lossy())
            .collect()
    }

    #[test]
    fn claims_follow_the_table() {
        let c = |p: &str| Node.claim(&rp(p));
        assert_eq!(c("a/package.json"), Some(Claim::Manifest));
        assert_eq!(c("bun.lock"), Some(Claim::Lockfile));
        assert_eq!(c("patches/x@1.patch"), Some(Claim::Lockfile));
        assert_eq!(c("src/x.patch"), None);
        assert_eq!(c("turbo.json"), Some(Claim::EcoGlobal));
        assert_eq!(c("apps/a/turbo.json"), None);
        for g in [
            "bunfig.toml",
            "x/.npmrc",
            ".nvmrc",
            ".node-version",
            "bun.lockb",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ] {
            assert_eq!(c(g), Some(Claim::EcoGlobal), "{g}");
        }
        assert_eq!(c("Cargo.toml"), None);
    }

    #[test]
    fn asc_members_globs_and_negation() {
        let g = graph(&asc(), &[]);
        let mut got = ids(&g);
        got.sort();
        // `apps/*/*/*` reaches the 4-level app; `apps/web/tools` and the
        // excluded nested workspace root are not projects.
        assert_eq!(
            got,
            [
                "node:@asc/dsl-compiler",
                "node:@asc/dsl-compiler-linux-x64-gnu",
                "node:@asc/private",
                "node:@asc/soma",
                "node:@asc/ui",
                "node:analysis-pipeline-api",
                "node:web",
            ]
        );
        assert!(g.issues.is_empty(), "{:?}", g.issues);
    }

    #[test]
    fn workspace_optional_and_link_edges() {
        let g = graph(&asc(), &[]);
        assert_eq!(
            deps(&g, "web"),
            ["Dev:node:@asc/dsl-compiler", "Normal:node:@asc/ui"]
        );
        assert_eq!(
            deps(&g, "@asc/dsl-compiler"),
            ["Optional:node:@asc/dsl-compiler-linux-x64-gnu"]
        );
        assert_eq!(
            deps(&g, "analysis-pipeline-api"),
            ["Normal:node:@asc/soma"]
        );
        // `@soma/budgets` has no member without the gitlink: the edge is dropped.
        assert!(deps(&g, "@asc/soma").is_empty());
    }

    #[test]
    fn gitlink_members_come_from_the_lock() {
        let mut t = asc();
        t.remove(SDK_ROOT.0);
        let (g, set) = build(&t, &["packages/soma/sdk"]);
        let mut sdk: Vec<_> = g
            .projects
            .iter()
            .filter(|p| p.gitlink.is_some())
            .map(Project::name_lossy)
            .collect();
        sdk.sort();
        assert_eq!(sdk, ["@soma/budgets", "@soma/inference"]);
        for p in g.projects.iter().filter(|p| p.gitlink.is_some()) {
            assert_eq!(p.gitlink.as_ref().unwrap(), &rp("packages/soma/sdk"));
        }
        assert_eq!(deps(&g, "@soma/inference"), ["Normal:node:@soma/budgets"]);
        assert_eq!(deps(&g, "@asc/soma"), ["Normal:node:@soma/budgets"]);
        assert!(g.issues.is_empty(), "{:?}", g.issues);

        // A pin bump affects the gitlinked members and their dependents.
        let dets: Vec<&dyn Detector> = vec![&Node];
        let side = Side {
            graph: &g,
            set: &set,
        };
        let changed = [ChangedPath {
            path: rp("packages/soma/sdk"),
            kind: ChangeKind::Gitlink,
        }];
        let a = affected(&dets, side, side, &changed, &Query::default()).unwrap();
        let mut hit: Vec<_> = a.projects.iter().map(|p| p.project.id.clone()).collect();
        hit.sort();
        assert_eq!(
            hit,
            [
                "node:@asc/soma",
                "node:@soma/budgets",
                "node:@soma/inference",
                "node:analysis-pipeline-api",
            ]
        );
        assert!(a.unowned.is_empty());
    }

    #[test]
    fn gitlink_without_lock_reports_an_issue() {
        let mut t = asc();
        t.remove(SDK_ROOT.0);
        t.remove("bun.lock");
        let g = graph(&t, &["packages/soma/sdk"]);
        assert!(g.projects.iter().all(|p| p.gitlink.is_none()));
        assert!(
            g.issues.iter().any(|i| i.path == rp("package.json")
                && String::from_utf8_lossy(&i.message).contains("gitlink")),
            "{:?}",
            g.issues
        );
    }

    #[test]
    fn external_bump_reaches_only_members_whose_closure_has_it() {
        let t = asc();
        let base = graph(&t, &[]);
        let react = graph(
            &edit(&t, "bun.lock", "sha512-react19", "sha512-react19x"),
            &[],
        );
        assert_eq!(external_changed(&base, &react), ["web"]);
        let zod = graph(&edit(&t, "bun.lock", "sha512-zod441", "sha512-zod442"), &[]);
        assert_eq!(external_changed(&base, &zod), ["@asc/soma"]);
        // A transitive package counts for the member that reaches it.
        let efrt = graph(&edit(&t, "bun.lock", "sha512-efrt", "sha512-efrt2"), &[]);
        assert_eq!(external_changed(&base, &efrt), ["analysis-pipeline-api"]);
        let ts = graph(&edit(&t, "bun.lock", "sha512-ts7", "sha512-ts8"), &[]);
        assert_eq!(external_changed(&base, &ts), ["@asc/private"]);
    }

    #[test]
    fn patch_content_reaches_only_the_patched_closure() {
        let t = asc();
        let base = graph(&t, &[]);
        let patched = graph(
            &edit(&t, "patches/compromise@14.14.5.patch", "+new", "+newer"),
            &[],
        );
        assert_eq!(external_changed(&base, &patched), ["analysis-pipeline-api"]);
    }

    #[test]
    fn root_fields_change_config_fp_only() {
        let t = asc();
        let base = graph(&t, &[]);
        let head = graph(&edit(&t, "package.json", "bun@1.2.22", "bun@1.2.23"), &[]);
        assert!(external_changed(&base, &head).is_empty());
        assert!(
            base.projects
                .iter()
                .zip(&head.projects)
                .all(|(a, b)| a.config_fp != b.config_fp)
        );
        // Dependency edits do not touch config_fp.
        let head = graph(&edit(&t, "package.json", "7.0.2", "7.0.3"), &[]);
        assert!(
            base.projects
                .iter()
                .zip(&head.projects)
                .all(|(a, b)| a.config_fp == b.config_fp)
        );
    }

    #[test]
    fn without_a_lock_declared_specs_stand_in() {
        let mut t = asc();
        t.remove("bun.lock");
        let base = graph(&t, &[]);
        let head = graph(
            &edit(&t, "apps/web/package.json", "^19.0.0", "^19.1.0"),
            &[],
        );
        assert_eq!(external_changed(&base, &head), ["web"]);
        let locked = graph(&asc(), &[]);
        assert_ne!(
            project(&base, "web").external_fp,
            project(&locked, "web").external_fp
        );
    }

    #[test]
    fn unmatched_unnamed_and_standalone_packages() {
        // The workspace root is `ws/`; `tools/` lies outside it.
        let t = tree(&[
            ("ws/package.json", r#"{"workspaces":["pkgs/*"]}"#),
            ("ws/pkgs/in/package.json", r#"{"name":"in"}"#),
            ("ws/other/package.json", r#"{"name":"unmatched"}"#),
            (
                "tools/solo/package.json",
                r#"{"name":"solo","dependencies":{"in":"*"}}"#,
            ),
            ("tools/unnamed/package.json", r#"{"private":true}"#),
        ]);
        let g = graph(&t, &[]);
        assert_eq!(ids(&g), ["node:solo", "node:in"]);
        // A standalone package does not resolve names of the workspace.
        assert!(deps(&g, "solo").is_empty());
    }

    #[test]
    fn workspace_pattern_forms() {
        let t = tree(&[
            (
                "package.json",
                r#"{"workspaces":{"packages":["./libs/*/","tools/a/package.json","deep/**","!deep/x/y","!libs/skip/package.json"]}}"#,
            ),
            ("libs/one/package.json", r#"{"name":"one"}"#),
            ("libs/skip/package.json", r#"{"name":"skip"}"#),
            ("tools/a/package.json", r#"{"name":"tool-a"}"#),
            ("deep/x/y/package.json", r#"{"name":"xy"}"#),
            ("deep/x/z/package.json", r#"{"name":"xz"}"#),
        ]);
        let mut got = ids(&graph(&t, &[]));
        got.sort();
        assert_eq!(got, ["node:one", "node:tool-a", "node:xz"]);
    }

    #[test]
    fn aliases_catalogs_and_file_specs() {
        let t = tree(&[
            ("package.json", r#"{"workspaces":["p/*"]}"#),
            (
                "p/a/package.json",
                r#"{"name":"a","dependencies":{"b":"*","c":"npm:b@1","d":"catalog:","e":"file:../e","f":"workspace:^","tar":"file:../x.tgz"}}"#,
            ),
            ("p/b/package.json", r#"{"name":"b"}"#),
            ("p/e/package.json", r#"{"name":"e"}"#),
        ]);
        let g = graph(&t, &[]);
        assert_eq!(deps(&g, "a"), ["Normal:node:b", "Normal:node:e"]);
    }

    #[test]
    fn over_limit_inputs() {
        // A manifest over 4 MiB fails to parse; the linker makes it opaque.
        let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert!(Node.parse(Claim::Manifest, &big).is_err());
        let t = with(
            asc(),
            "apps/web/package.json",
            &" ".repeat(MAX_MANIFEST_BYTES + 1),
        );
        let g = graph(&t, &[]);
        assert!(project(&g, "web").opaque);
        assert!(
            g.issues
                .iter()
                .any(|i| i.path == rp("apps/web/package.json"))
        );

        // An unusable lock gives every member the lock's FileId.
        let t = with(asc(), "bun.lock", r#"{"packages": {"a": 3}}"#);
        let g = graph(&t, &[]);
        let id = Fingerprint::from(*blake3::hash(t["bun.lock"].as_bytes()).as_bytes());
        assert!(g.projects.iter().all(|p| p.external_fp == id));
        assert!(g.issues.iter().any(|i| i.path == rp("bun.lock")));
    }

    #[test]
    fn manifest_parse_rejects_unsupported_shapes() {
        for bad in [
            r#"[]"#,
            r#"{"name": 3}"#,
            r#"{"workspaces": 3}"#,
            r#"{"workspaces": [1]}"#,
            r#"{"workspaces": {"packages": "x"}}"#,
            r#"{"dependencies": ["a"]}"#,
            r#"{"dependencies": {"a": 1}}"#,
            "not json",
        ] {
            assert!(
                Node.parse(Claim::Manifest, bad.as_bytes()).is_err(),
                "{bad}"
            );
        }
        let ok = Node
            .parse(
                Claim::Manifest,
                br#"{"workspaces": {"nohoist": []}, "scripts": {"a": "b"}}"#,
            )
            .unwrap();
        assert_eq!(get(&ok.0, "workspaces"), Some(&Value::Array(vec![])));
        assert!(get(&ok.0, "scripts").is_none());
    }

    #[test]
    fn patch_files_parse_to_null_and_locks_to_values() {
        let patch = Node.parse(Claim::Lockfile, b"diff --git a b").unwrap();
        assert_eq!(patch, Parsed(Value::Null));
        let lock = ASC[1].1.as_bytes();
        let p = Node.parse(Claim::Lockfile, lock).unwrap();
        assert!(Lock::view(&p).is_ok());
        // Same content, same parse: results are shared by content.
        assert_eq!(p, Node.parse(Claim::Lockfile, lock).unwrap());
    }

    #[test]
    fn graph_is_stable_and_valid() {
        let g = graph(&asc(), &[]);
        assert_eq!(g, graph(&asc(), &[]));
        for p in &g.projects {
            assert_eq!(p.kind, ProjectKind::Package);
            assert!(!p.opaque);
            assert!(p.deps.iter().all(|(_, k)| *k != DepKind::Member));
        }
    }
}
