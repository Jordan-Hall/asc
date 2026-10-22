//! The Rust (Cargo) detector (SPEC §12A.2.1, §12A.4).
//!
//! Pure: manifests are parsed to canonical values by content, and `link`
//! reads only the manifest set. Workspace members are found by matching the
//! `members` globs against directories that hold a `Cargo.toml`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use asc_core::path::RepoPath;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::{Claim, Detector, LinkCx, ParseFailure};
use crate::canon::{self, Value};
use crate::lock::cargo as lock;
use crate::model::{
    DepKind, Ecosystem, Fingerprint, MAX_MANIFEST_BYTES, Parsed, Project, ProjectKey, ProjectKind,
};

/// The Cargo detector.
pub struct Rust;

impl Detector for Rust {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Rust
    }

    fn claim(&self, path: &RepoPath) -> Option<Claim> {
        match path.file_name().as_bytes() {
            b"Cargo.toml" => Some(Claim::Manifest),
            b"Cargo.lock" => Some(Claim::Lockfile),
            b"rust-toolchain" | b"rust-toolchain.toml" => Some(Claim::EcoGlobal),
            b"config.toml" | b"config"
                if path
                    .parent()
                    .is_some_and(|p| p.file_name().as_bytes() == b".cargo") =>
            {
                Some(Claim::EcoGlobal)
            }
            _ => None,
        }
    }

    fn parse(&self, claim: Claim, bytes: &[u8]) -> Result<Parsed, ParseFailure> {
        match claim {
            Claim::Manifest => parse_manifest(bytes),
            Claim::Lockfile => Ok(lock::parse(bytes)),
            Claim::EcoGlobal => Ok(Parsed(Value::Null)),
        }
    }

    fn link(&self, cx: &LinkCx<'_>) -> Vec<Project> {
        Linker::new(cx).run()
    }
}

fn parse_manifest(bytes: &[u8]) -> Result<Parsed, ParseFailure> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ParseFailure::new("manifest larger than 4 MiB"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ParseFailure::new("not UTF-8"))?;
    let doc: toml::Value = toml::from_str(text).map_err(|e| ParseFailure::new(e.to_string()))?;
    canon::from_toml(&doc)
        .map(Parsed)
        .map_err(|e| ParseFailure::new(e.to_string()))
}

// --- value helpers ---------------------------------------------------------

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

fn get_in<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v.and_then(|v| get(v, key))
}

fn text(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

fn strings(v: Option<&Value>) -> Vec<&str> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| text(Some(x))).collect(),
        _ => Vec::new(),
    }
}

fn is_true(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true)))
}

fn owned(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or(Value::Null)
}

// --- paths -----------------------------------------------------------------

/// Joins `rel` onto directory `base` (bytes, empty is the root). `None` when
/// the result leaves the repository or `rel` is absolute.
fn join_dir(base: &[u8], rel: &str) -> Option<Vec<u8>> {
    if rel.starts_with(['/', '\\']) || rel.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let mut parts: Vec<&[u8]> = if base.is_empty() {
        Vec::new()
    } else {
        base.split(|&c| c == b'/').collect()
    };
    for c in rel.split(['/', '\\']) {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c.as_bytes()),
        }
    }
    Some(parts.join(&b'/'))
}

fn path_in(dir: &[u8], file: &str) -> Option<RepoPath> {
    let mut b = dir.to_vec();
    if !b.is_empty() {
        b.push(b'/');
    }
    b.extend_from_slice(file.as_bytes());
    RepoPath::new(b).ok()
}

fn escape_glob(b: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(b.len());
    for &c in b {
        if matches!(c, b'*' | b'?' | b'[' | b']' | b'{' | b'}' | b'\\') {
            o.push(b'\\');
        }
        o.push(c);
    }
    o
}

fn compile(patterns: &[&str]) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let p = p.replace('\\', "/");
        let p = p.trim_start_matches("./").trim_end_matches('/');
        if p.is_empty() || p == "." {
            continue;
        }
        let g = GlobBuilder::new(p)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("glob {p:?}: {}", e.kind()))?;
        b.add(g);
    }
    b.build().map_err(|e| e.to_string())
}

// --- linking ---------------------------------------------------------------

struct Man<'a> {
    path: &'a RepoPath,
    /// Manifest directory; empty is the repository root.
    dir: Vec<u8>,
    v: &'a Value,
    package: Option<&'a Value>,
    workspace: Option<&'a Value>,
}

enum LockState {
    Usable(lock::Lock),
    /// The lockfile's content id stands in for the closure (SPEC §12A.10).
    Unusable(Fingerprint),
}

struct Linker<'c, 'a> {
    cx: &'c LinkCx<'a>,
    mans: Vec<Man<'a>>,
    /// Directory to the index of the manifest with `[package]`.
    pkg_dirs: BTreeMap<Vec<u8>, usize>,
    /// Workspaces with a member glob that does not compile.
    bad_ws: BTreeSet<usize>,
    /// The workspace manifest that governs each manifest.
    gov: Vec<Option<usize>>,
    /// Packages governed by each workspace manifest, itself excluded.
    children: BTreeMap<usize, Vec<usize>>,
    locks: RefCell<BTreeMap<RepoPath, Option<Rc<LockState>>>>,
}

/// One dependency declaration, resolved.
struct Dep<'a> {
    target: Option<&'a str>,
    kind: DepKind,
    key: &'a str,
    /// The workspace entry that `workspace = true` inherits.
    inherited: Option<&'a Value>,
    internal: Option<usize>,
    merged: Value,
}

const TABLES: [(&str, DepKind); 5] = [
    ("dependencies", DepKind::Normal),
    ("dev-dependencies", DepKind::Dev),
    ("dev_dependencies", DepKind::Dev),
    ("build-dependencies", DepKind::Build),
    ("build_dependencies", DepKind::Build),
];

type Entry<'a> = (Option<&'a str>, DepKind, &'a str, &'a Value);

/// Every dependency entry of a manifest, plain and `[target.'<cfg>'.*]`.
fn dep_entries(v: &Value) -> Vec<Entry<'_>> {
    let mut out = Vec::new();
    scan_deps(v, None, &mut out);
    if let Some(Value::Map(ts)) = get(v, "target") {
        for (cfg, t) in ts {
            scan_deps(t, Some(cfg), &mut out);
        }
    }
    out
}

fn scan_deps<'a>(holder: &'a Value, target: Option<&'a str>, out: &mut Vec<Entry<'a>>) {
    for (table, kind) in TABLES {
        if let Some(Value::Map(m)) = get(holder, table) {
            out.extend(m.iter().map(|(k, e)| (target, kind, k.as_str(), e)));
        }
    }
}

/// A dependency entry with the workspace entry merged in: the workspace
/// entry's fields, the union of both `features`, and the member's `optional`.
fn merge_dep(member: &Value, ws_entry: Option<&Value>) -> Value {
    if !is_true(get(member, "workspace")) {
        return member.clone();
    }
    let mut fields: BTreeMap<String, Value> = match ws_entry {
        Some(Value::Map(m)) => m.iter().cloned().collect(),
        Some(Value::Str(v)) => BTreeMap::from([("version".to_owned(), Value::Str(v.clone()))]),
        _ => BTreeMap::new(),
    };
    let mut features: BTreeSet<&str> = strings(get_in(ws_entry, "features")).into_iter().collect();
    features.extend(strings(get(member, "features")));
    if !features.is_empty() {
        let list = features
            .into_iter()
            .map(|f| Value::Str(f.to_owned()))
            .collect();
        fields.insert("features".to_owned(), Value::Array(list));
    }
    if let Some(o) = get(member, "optional") {
        fields.insert("optional".to_owned(), o.clone());
    }
    Value::map(fields).unwrap_or(Value::Null)
}

impl<'c, 'a> Linker<'c, 'a> {
    fn new(cx: &'c LinkCx<'a>) -> Self {
        let mans: Vec<Man<'a>> = cx
            .files_of(Claim::Manifest)
            .map(|f| {
                let v = &f.parsed.0;
                Man {
                    path: f.path,
                    dir: f
                        .path
                        .parent()
                        .map(|d| d.as_bytes().to_vec())
                        .unwrap_or_default(),
                    v,
                    package: get(v, "package"),
                    workspace: get(v, "workspace"),
                }
            })
            .collect();
        let pkg_dirs: BTreeMap<Vec<u8>, usize> = mans
            .iter()
            .enumerate()
            .filter(|(_, m)| m.package.is_some())
            .map(|(i, m)| (m.dir.clone(), i))
            .collect();
        let ws_dirs: BTreeMap<Vec<u8>, usize> = mans
            .iter()
            .enumerate()
            .filter(|(_, m)| m.workspace.is_some())
            .map(|(i, m)| (m.dir.clone(), i))
            .collect();

        let mut bad_ws = BTreeSet::new();
        let mut listed: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for &wi in ws_dirs.values() {
            let m = &mans[wi];
            let ws = m.workspace;
            let (inc, exc) = (
                compile(&strings(get_in(ws, "members"))),
                compile(&strings(get_in(ws, "exclude"))),
            );
            let (inc, exc) = match (inc, exc) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    cx.report(m.path.clone(), format!("workspace members: {e}"));
                    bad_ws.insert(wi);
                    continue;
                }
            };
            let set = listed.entry(wi).or_default();
            let prefix = if m.dir.is_empty() {
                Vec::new()
            } else {
                [m.dir.as_slice(), b"/"].concat()
            };
            for (dir, &ci) in pkg_dirs.range(prefix.clone()..) {
                if !dir.starts_with(&prefix) {
                    break;
                }
                let Ok(rel) = std::str::from_utf8(&dir[prefix.len()..]) else {
                    continue;
                };
                if !rel.is_empty() && inc.is_match(rel) && !exc.is_match(rel) {
                    set.insert(ci);
                }
            }
        }

        let mut gov = vec![None; mans.len()];
        let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, m) in mans.iter().enumerate() {
            if m.package.is_none() {
                continue;
            }
            let g = if m.workspace.is_some() {
                Some(i)
            } else {
                explicit_workspace(m, &ws_dirs).or_else(|| nearest_listing(m, i, &ws_dirs, &listed))
            };
            gov[i] = g;
            if let Some(g) = g.filter(|&g| g != i) {
                children.entry(g).or_default().push(i);
            }
        }
        Self {
            cx,
            mans,
            pkg_dirs,
            bad_ws,
            gov,
            children,
            locks: RefCell::new(BTreeMap::new()),
        }
    }

    fn run(self) -> Vec<Project> {
        (0..self.mans.len()).map(|i| self.project(i)).collect()
    }

    fn key_of(&self, i: usize) -> ProjectKey {
        ProjectKey {
            eco: Ecosystem::Rust,
            manifest: self.mans[i].path.clone(),
        }
    }

    fn opaque(&self, i: usize, name: &[u8], why: &str) -> Project {
        self.cx.report(self.mans[i].path.clone(), why);
        let mut p = Project::new(self.key_of(i), name, ProjectKind::Package);
        p.opaque = true;
        p
    }

    fn dir_name(&self, i: usize) -> Vec<u8> {
        let dir = &self.mans[i].dir;
        match dir.rsplit(|&c| c == b'/').next() {
            Some(n) if !n.is_empty() => n.to_vec(),
            _ => b"workspace".to_vec(),
        }
    }

    fn project(&self, i: usize) -> Project {
        let m = &self.mans[i];
        let Some(pkg) = m.package else {
            return self.workspace_project(i);
        };
        let Some(name) = text(get(pkg, "name")) else {
            return self.opaque(i, &self.dir_name(i), "package has no string name");
        };
        let mut p = Project::new(self.key_of(i), name, ProjectKind::Package);
        let gov = self.gov[i];
        let root = &self.mans[gov.unwrap_or(i)];
        let ws_table = gov.and_then(|g| self.mans[g].workspace);
        let deps = self.resolve_deps(i, root, ws_table);

        let mut inherited = BTreeMap::new();
        for d in &deps {
            if let Some(e) = d.inherited {
                inherited.insert(d.key.to_owned(), e.clone());
            }
            if let Some(j) = d.internal {
                p.deps.push((self.key_of(j), d.kind));
            }
        }
        p.config_fp = config_value(root.v, inherited).fingerprint();
        p.extra_inputs = extra_inputs(m);
        p.external_fp = self.external_fp(i, name, gov, ws_table, &deps);
        if m.workspace.is_some() {
            p.opaque = self.bad_ws.contains(&i);
            self.add_members(i, &mut p);
        }
        p
    }

    fn workspace_project(&self, i: usize) -> Project {
        if self.mans[i].workspace.is_none() {
            return self.opaque(
                i,
                &self.dir_name(i),
                "manifest has neither [package] nor [workspace]",
            );
        }
        let mut p = Project::new(self.key_of(i), self.dir_name(i), ProjectKind::Workspace);
        p.config_fp = config_value(self.mans[i].v, BTreeMap::new()).fingerprint();
        p.opaque = self.bad_ws.contains(&i);
        self.add_members(i, &mut p);
        p
    }

    fn add_members(&self, ws: usize, p: &mut Project) {
        for &c in self.children.get(&ws).into_iter().flatten() {
            p.deps.push((self.key_of(c), DepKind::Member));
        }
    }

    /// Resolves every dependency of manifest `i`; `root` is the governing
    /// manifest (patches, workspace dependencies) and `ws` its workspace table.
    fn resolve_deps(&self, i: usize, root: &Man<'a>, ws: Option<&'a Value>) -> Vec<Dep<'a>> {
        let m = &self.mans[i];
        let ws_deps = get_in(ws, "dependencies");
        dep_entries(m.v)
            .into_iter()
            .map(|(target, kind, key, entry)| {
                let inherit = is_true(get(entry, "workspace"));
                let ws_entry = if inherit { get_in(ws_deps, key) } else { None };
                let (base, path) = if inherit {
                    (&root.dir, text(get_in(ws_entry, "path")))
                } else {
                    (&m.dir, text(get(entry, "path")))
                };
                let real = text(get(entry, "package"))
                    .or(text(get_in(ws_entry, "package")))
                    .unwrap_or(key);
                let dir = match path {
                    Some(p) => join_dir(base, p),
                    None => patch_dir(root, real, key),
                };
                let internal = dir
                    .and_then(|d| self.pkg_dirs.get(&d).copied())
                    .filter(|&j| j != i);
                Dep {
                    target,
                    kind,
                    key,
                    inherited: ws_entry,
                    internal,
                    merged: merge_dep(entry, ws_entry),
                }
            })
            .collect()
    }

    fn lock_at(&self, dir: &[u8]) -> Option<Rc<LockState>> {
        let path = path_in(dir, "Cargo.lock")?;
        if let Some(hit) = self.locks.borrow().get(&path) {
            return hit.clone();
        }
        let state = self
            .cx
            .file(&path)
            .filter(|f| f.eco == Ecosystem::Rust && f.claim == Claim::Lockfile)
            .map(|f| {
                Rc::new(match lock::Lock::from_parsed(f.parsed) {
                    Ok(l) => LockState::Usable(l),
                    Err(e) => {
                        self.cx.report(path.clone(), e.to_string());
                        LockState::Unusable(Fingerprint::from(f.id))
                    }
                })
            });
        self.locks.borrow_mut().insert(path, state.clone());
        state
    }

    /// The `external_fp` of package `i` (SPEC §12A.4).
    fn external_fp(
        &self,
        i: usize,
        name: &str,
        gov: Option<usize>,
        ws: Option<&Value>,
        deps: &[Dep<'_>],
    ) -> Fingerprint {
        let own = &self.mans[i].dir;
        let lock = gov
            .and_then(|g| self.lock_at(&self.mans[g].dir))
            .or_else(|| self.lock_at(own));
        match lock.as_deref() {
            Some(LockState::Unusable(fp)) => return *fp,
            Some(LockState::Usable(l)) => {
                let version = self.version(i, ws);
                if let Some(fp) = l.closure(name, version) {
                    return fp;
                }
                self.cx.report(
                    self.mans[i].path.clone(),
                    format!("{name} is not in Cargo.lock; using declared dependencies"),
                );
            }
            None => {}
        }
        declared_fp(deps)
    }

    /// The package version, inherited from `[workspace.package]` when asked.
    fn version<'v>(&'v self, i: usize, ws: Option<&'v Value>) -> Option<&'v str> {
        let v = get_in(self.mans[i].package, "version");
        match v {
            Some(Value::Str(s)) => Some(s),
            Some(t) if is_true(get(t, "workspace")) => {
                text(get_in(get_in(ws, "package"), "version"))
            }
            _ => None,
        }
    }
}

/// `package.workspace`: the directory (or its `Cargo.toml`) of the workspace.
fn explicit_workspace(m: &Man<'_>, ws_dirs: &BTreeMap<Vec<u8>, usize>) -> Option<usize> {
    let rel = text(get_in(m.package, "workspace"))?;
    let rel = rel.strip_suffix("Cargo.toml").unwrap_or(rel);
    ws_dirs.get(&join_dir(&m.dir, rel)?).copied()
}

/// The nearest ancestor workspace that lists manifest `i`.
fn nearest_listing(
    m: &Man<'_>,
    i: usize,
    ws_dirs: &BTreeMap<Vec<u8>, usize>,
    listed: &BTreeMap<usize, BTreeSet<usize>>,
) -> Option<usize> {
    let mut dir = m.dir.as_slice();
    while !dir.is_empty() {
        dir = dir
            .iter()
            .rposition(|&c| c == b'/')
            .map_or(&[][..], |at| &dir[..at]);
        if let Some(&w) = ws_dirs.get(dir)
            && listed.get(&w).is_some_and(|s| s.contains(&i))
        {
            return Some(w);
        }
    }
    None
}

/// A `[patch.<source>]` entry in the governing manifest that gives `name` a path.
fn patch_dir(root: &Man<'_>, real: &str, key: &str) -> Option<Vec<u8>> {
    let Some(Value::Map(sources)) = get(root.v, "patch") else {
        return None;
    };
    sources.iter().find_map(|(_, table)| {
        let Value::Map(entries) = table else {
            return None;
        };
        entries.iter().find_map(|(k, e)| {
            let name = text(get(e, "package")).unwrap_or(k);
            (name == real || name == key)
                .then(|| text(get(e, "path")))
                .flatten()
                .and_then(|p| join_dir(&root.dir, p))
        })
    })
}

/// The `config_fp` value: shared configuration of the governing manifest and
/// the workspace dependency entries a crate inherits.
fn config_value(root: &Value, inherited: BTreeMap<String, Value>) -> Value {
    let ws = get(root, "workspace");
    let resolver = get_in(ws, "resolver").or_else(|| get_in(get(root, "package"), "resolver"));
    let pairs = [
        ("ws_package", owned(get_in(ws, "package"))),
        ("profile", owned(get(root, "profile"))),
        ("patch", owned(get(root, "patch"))),
        ("replace", owned(get(root, "replace"))),
        ("resolver", owned(resolver)),
        ("lints", owned(get_in(ws, "lints"))),
        ("inherited", Value::map(inherited).unwrap_or(Value::Null)),
    ];
    Value::map(pairs.map(|(k, v)| (k.to_owned(), v))).unwrap_or(Value::Null)
}

/// With no usable lock: the external declarations (name, requirement, features).
fn declared_fp(deps: &[Dep<'_>]) -> Fingerprint {
    let rows = deps
        .iter()
        .filter(|d| d.internal.is_none())
        .map(|d| {
            Value::Array(vec![
                d.target.map_or(Value::Null, |t| Value::Str(t.to_owned())),
                Value::Int(d.kind as i64),
                Value::Str(d.key.to_owned()),
                d.merged.clone(),
            ])
        })
        .collect();
    Value::Array(rows).fingerprint()
}

/// `path` values of the target tables and `package.build` that leave the crate root.
fn extra_inputs(m: &Man<'_>) -> Vec<Vec<u8>> {
    let mut paths: Vec<&str> = Vec::new();
    paths.extend(text(get_in(m.package, "build")));
    paths.extend(text(get_in(get(m.v, "lib"), "path")));
    for table in ["bin", "test", "bench", "example"] {
        if let Some(Value::Array(a)) = get(m.v, table) {
            paths.extend(a.iter().filter_map(|t| text(get(t, "path"))));
        }
    }
    let mut out: Vec<Vec<u8>> = paths
        .into_iter()
        .filter_map(|p| join_dir(&m.dir, p))
        .filter(|p| {
            !p.is_empty()
                && !(p.starts_with(&m.dir)
                    && (m.dir.is_empty() || p.get(m.dir.len()) == Some(&b'/')))
        })
        .map(|p| escape_glob(&p))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResolvedConfig;
    use crate::detect::testkit::{Tree, rp};
    use crate::link::{LinkInput, link_graph};
    use crate::model::ProjectGraph;

    fn fixture(name: &str) -> Tree {
        fn walk(dir: &std::path::Path, prefix: &str, out: &mut Tree) {
            for e in std::fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let name = e.file_name().into_string().unwrap();
                let rel = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                if e.file_type().unwrap().is_dir() {
                    walk(&e.path(), &rel, out);
                } else {
                    out.set(&rel, &std::fs::read_to_string(e.path()).unwrap());
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rust")
            .join(name);
        let mut t = Tree::default();
        walk(&root, "", &mut t);
        t
    }

    fn try_graph(t: &Tree) -> ProjectGraph {
        let files: Vec<LinkInput> =
            t.0.iter()
                .filter_map(|(p, c)| {
                    let path = rp(p);
                    let claim = Rust.claim(&path)?;
                    Some(LinkInput {
                        path,
                        id: *blake3::hash(c.as_bytes()).as_bytes(),
                        parsed: Rust.parse(claim, c.as_bytes()),
                    })
                })
                .collect();
        link_graph(&[&Rust], &files, &[], ResolvedConfig::default(), &|_| true).unwrap()
    }

    fn project<'g>(g: &'g ProjectGraph, root: &str) -> &'g Project {
        let manifest = rp(&if root == "." {
            "Cargo.toml".to_owned()
        } else {
            format!("{root}/Cargo.toml")
        });
        g.projects
            .iter()
            .find(|p| p.key.manifest == manifest)
            .unwrap_or_else(|| panic!("no project at {root}"))
    }

    fn edges(g: &ProjectGraph, root: &str) -> Vec<(String, DepKind)> {
        project(g, root)
            .deps
            .iter()
            .map(|(k, d)| (k.manifest.to_string(), *d))
            .collect()
    }

    fn e(manifest: &str, kind: DepKind) -> (String, DepKind) {
        (manifest.to_owned(), kind)
    }

    fn asc() -> (Tree, ProjectGraph) {
        let t = fixture("asc");
        let g = try_graph(&t);
        (t, g)
    }

    #[test]
    fn claims_by_basename() {
        let c = |p: &str| Rust.claim(&rp(p));
        assert_eq!(c("a/Cargo.toml"), Some(Claim::Manifest));
        assert_eq!(c("Cargo.lock"), Some(Claim::Lockfile));
        assert_eq!(c("rust-toolchain"), Some(Claim::EcoGlobal));
        assert_eq!(c("x/rust-toolchain.toml"), Some(Claim::EcoGlobal));
        assert_eq!(c("x/.cargo/config.toml"), Some(Claim::EcoGlobal));
        assert_eq!(c("x/.cargo/config"), Some(Claim::EcoGlobal));
        assert_eq!(c("x/config.toml"), None);
        assert_eq!(c("x/cargo.toml"), None);
    }

    #[test]
    fn two_workspaces_with_duplicate_names() {
        let (_, g) = asc();
        let ids = g.display_ids();
        for want in [
            "rust:core@dsl/crates/core",
            "rust:core@pipeline/crates/core",
        ] {
            assert!(ids.iter().any(|i| i == want), "{want} in {ids:?}");
        }
        // Each `core` resolves its own sibling, not the other workspace's.
        assert_eq!(
            edges(&g, "pipeline/crates/core"),
            [e("pipeline/crates/util/Cargo.toml", DepKind::Normal)]
        );
        let dsl: Vec<_> = edges(&g, "dsl/crates/core");
        assert!(dsl.iter().all(|(m, _)| m.starts_with("dsl/")), "{dsl:?}");
    }

    #[test]
    fn workspaces_have_member_edges() {
        let (_, g) = asc();
        let dsl = project(&g, "dsl");
        assert_eq!(dsl.kind, ProjectKind::Workspace);
        assert_eq!(dsl.name, b"dsl");
        assert_eq!(
            edges(&g, "dsl"),
            [
                e("dsl/crates/core/Cargo.toml", DepKind::Member),
                e("dsl/crates/ir/Cargo.toml", DepKind::Member),
                e("dsl/crates/testkit/Cargo.toml", DepKind::Member),
            ]
        );
        // The vendored crate is no member: no glob lists it.
        assert_eq!(project(&g, "dsl/vendor/extlib").kind, ProjectKind::Package);
        assert_eq!(edges(&g, "pipeline").len(), 2);
    }

    #[test]
    fn inherited_path_dependencies_and_patches_are_internal() {
        let (_, g) = asc();
        let core = edges(&g, "dsl/crates/core");
        // `ir.workspace = true` -> the workspace entry's path.
        assert!(core.contains(&e("dsl/crates/ir/Cargo.toml", DepKind::Normal)));
        // `extlib = "1"` is given a path by `[patch.crates-io]`.
        assert!(core.contains(&e("dsl/vendor/extlib/Cargo.toml", DepKind::Normal)));
        // `serde.workspace = true` has no path: external.
        assert!(!core.iter().any(|(m, _)| m.contains("serde")));
    }

    #[test]
    fn target_specific_tables_give_edges_of_their_kind() {
        let (_, g) = asc();
        let core = edges(&g, "dsl/crates/core");
        assert!(core.contains(&e("dsl/crates/testkit/Cargo.toml", DepKind::Dev)));
        // A path dependency across workspaces from a standalone crate.
        assert_eq!(
            edges(&g, "standalone/risk"),
            [
                e("dsl/crates/ir/Cargo.toml", DepKind::Normal),
                e("pipeline/crates/util/Cargo.toml", DepKind::Build),
            ]
        );
    }

    #[test]
    fn extra_inputs_are_paths_that_leave_the_crate() {
        let (_, g) = asc();
        let util = project(&g, "pipeline/crates/util");
        assert_eq!(util.extra_inputs, [b"pipeline/shared/main.rs".to_vec()]);
        assert!(project(&g, "pipeline/crates/core").extra_inputs.is_empty());
    }

    #[test]
    fn a_profile_edit_affects_only_its_workspace() {
        let (mut t, before) = asc();
        let p = t.0["pipeline/Cargo.toml"].replace("opt-level = 2", "opt-level = 1");
        t.set("pipeline/Cargo.toml", &p);
        let after = try_graph(&t);
        for root in ["pipeline/crates/core", "pipeline/crates/util"] {
            assert_ne!(
                project(&before, root).config_fp,
                project(&after, root).config_fp
            );
            assert_eq!(
                project(&before, root).external_fp,
                project(&after, root).external_fp
            );
        }
        for root in [
            "dsl/crates/core",
            "dsl/crates/ir",
            "dsl/crates/testkit",
            "dsl/vendor/extlib",
            "standalone/risk",
        ] {
            assert_eq!(project(&before, root), project(&after, root), "{root}");
        }
    }

    #[test]
    fn an_inherited_dependency_edit_affects_only_its_users() {
        let (mut t, before) = asc();
        let p = t.0["dsl/Cargo.toml"].replace("serde = \"1.0\"", "serde = \"1.1\"");
        t.set("dsl/Cargo.toml", &p);
        let after = try_graph(&t);
        let cfg = |g: &ProjectGraph, r: &str| project(g, r).config_fp;
        assert_ne!(
            cfg(&before, "dsl/crates/core"),
            cfg(&after, "dsl/crates/core")
        );
        assert_eq!(cfg(&before, "dsl/crates/ir"), cfg(&after, "dsl/crates/ir"));
        assert_eq!(
            cfg(&before, "dsl/crates/testkit"),
            cfg(&after, "dsl/crates/testkit")
        );
    }

    #[test]
    fn a_lock_bump_affects_only_crates_that_reach_it() {
        let (mut t, before) = asc();
        let l = t.0["dsl/Cargo.lock"]
            .replace("version = \"0.2.1\"", "version = \"0.2.2\"")
            .replace("\"c1\"", "\"c2\"");
        assert_ne!(l, t.0["dsl/Cargo.lock"]);
        t.set("dsl/Cargo.lock", &l);
        let after = try_graph(&t);
        let ext = |g: &ProjectGraph, r: &str| project(g, r).external_fp;
        assert_ne!(
            ext(&before, "dsl/crates/core"),
            ext(&after, "dsl/crates/core")
        );
        for root in ["dsl/crates/ir", "dsl/crates/testkit", "dsl/vendor/extlib"] {
            assert_eq!(ext(&before, root), ext(&after, root), "{root}");
        }
        // The other workspace has no lock and is untouched.
        assert_eq!(
            ext(&before, "pipeline/crates/core"),
            ext(&after, "pipeline/crates/core")
        );
    }

    #[test]
    fn with_a_lock_declared_requirements_do_not_count() {
        let (mut t, before) = asc();
        let m = t.0["dsl/crates/ir/Cargo.toml"].replace("\"1\"", "\"1.5\"");
        assert_ne!(m, t.0["dsl/crates/ir/Cargo.toml"]);
        t.set("dsl/crates/ir/Cargo.toml", &m);
        let after = try_graph(&t);
        assert_eq!(
            project(&before, "dsl/crates/ir").external_fp,
            project(&after, "dsl/crates/ir").external_fp
        );
    }

    #[test]
    fn without_a_lock_declarations_are_the_external_closure() {
        let (mut t, before) = asc();
        let ext = |g: &ProjectGraph, r: &str| project(g, r).external_fp;
        let m = t.0["pipeline/crates/core/Cargo.toml"].replace("\"2\"", "\"2.1\"");
        assert_ne!(m, t.0["pipeline/crates/core/Cargo.toml"]);
        t.set("pipeline/crates/core/Cargo.toml", &m);
        let after = try_graph(&t);
        assert_ne!(
            ext(&before, "pipeline/crates/core"),
            ext(&after, "pipeline/crates/core")
        );
        assert_eq!(
            ext(&before, "pipeline/crates/util"),
            ext(&after, "pipeline/crates/util")
        );
        // Features count too.
        let m = t.0["pipeline/crates/core/Cargo.toml"]
            .replace("\"2.1\"", "{ version = \"2.1\", features = [\"x\"] }");
        t.set("pipeline/crates/core/Cargo.toml", &m);
        let again = try_graph(&t);
        assert_ne!(
            ext(&after, "pipeline/crates/core"),
            ext(&again, "pipeline/crates/core")
        );
        // An internal path dependency is not an external declaration.
        let m = t.0["pipeline/crates/core/Cargo.toml"].replace("../util", "../util/");
        t.set("pipeline/crates/core/Cargo.toml", &m);
        assert_eq!(
            ext(&again, "pipeline/crates/core"),
            ext(&try_graph(&t), "pipeline/crates/core")
        );
    }

    fn mini(files: &[(&str, &str)]) -> ProjectGraph {
        try_graph(&Tree::new(files))
    }

    #[test]
    fn exclude_and_explicit_workspace() {
        let g = mini(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skip\"]\n",
            ),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("crates/skip/Cargo.toml", "[package]\nname = \"skip\"\n"),
            (
                "elsewhere/b/Cargo.toml",
                "[package]\nname = \"b\"\nworkspace = \"../..\"\n",
            ),
        ]);
        assert_eq!(
            edges(&g, "."),
            [
                e("crates/a/Cargo.toml", DepKind::Member),
                e("elsewhere/b/Cargo.toml", DepKind::Member),
            ]
        );
        assert!(edges(&g, "crates/skip").is_empty());
    }

    #[test]
    fn root_package_with_workspace_is_a_package_with_member_edges() {
        let g = mini(&[
            (
                "Cargo.toml",
                "[package]\nname = \"root\"\n[workspace]\nmembers = [\"m\"]\n",
            ),
            ("m/Cargo.toml", "[package]\nname = \"m\"\n"),
        ]);
        let root = g.projects.iter().find(|p| p.name == b"root").unwrap();
        assert_eq!(root.kind, ProjectKind::Package);
        assert_eq!(root.deps.len(), 1);
        assert_eq!(root.deps[0].1, DepKind::Member);
    }

    #[test]
    fn bad_manifests_are_opaque_and_reported() {
        let g = mini(&[
            ("a/Cargo.toml", "[dependencies]\nx = \"1\"\n"),
            ("b/Cargo.toml", "[package]\nversion = \"1\"\n"),
            ("c/Cargo.toml", "[workspace]\nmembers = [\"[\"]\n"),
            ("d/Cargo.toml", "[package]\nname = \"d\"\n"),
        ]);
        for r in ["a", "b", "c"] {
            assert!(project(&g, r).opaque, "{r}");
        }
        assert!(!project(&g, "d").opaque);
        // Opaque projects depend on the rest of the ecosystem.
        assert!(edges(&g, "a").contains(&e("d/Cargo.toml", DepKind::Implicit)));
        let paths: Vec<_> = g.issues.iter().map(|i| i.path.to_string()).collect();
        assert!(paths.contains(&"c/Cargo.toml".to_owned()), "{paths:?}");
    }

    #[test]
    fn over_limit_manifest_and_lockfile() {
        let big = " ".repeat(MAX_MANIFEST_BYTES + 1);
        let err = Rust.parse(Claim::Manifest, big.as_bytes()).unwrap_err();
        assert!(err.message.contains("4 MiB"), "{err}");
        // An unparsable manifest is an opaque project, not a failed graph.
        let g = mini(&[
            ("a/Cargo.toml", &big),
            ("b/Cargo.toml", "[package]\nname = \"b\"\n"),
        ]);
        assert!(project(&g, "a").opaque);

        // A lock over the limit makes the lockfile's id the closure of every governed crate.
        let t = Tree::new(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"m\"]\n"),
            ("m/Cargo.toml", "[package]\nname = \"m\"\n"),
            ("Cargo.lock", &" ".repeat(64 * 1024 * 1024 + 1)),
        ]);
        let g = try_graph(&t);
        let id = *blake3::hash(t.0["Cargo.lock"].as_bytes()).as_bytes();
        assert_eq!(project(&g, "m").external_fp, Fingerprint::from(id));
        assert!(g.issues.iter().any(|i| i.path == rp("Cargo.lock")));
    }

    #[test]
    fn deeply_nested_manifests_are_rejected() {
        let deep = format!("a = {}1{}", "[".repeat(200), "]".repeat(200));
        assert!(Rust.parse(Claim::Manifest, deep.as_bytes()).is_err());
    }
}
