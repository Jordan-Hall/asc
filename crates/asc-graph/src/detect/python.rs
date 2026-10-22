//! The Python detector (SPEC §12A.2.2, §12A.4).
//!
//! Pure: files are parsed to canonical values by content, and `link` reads
//! only the manifest set. `pyproject.toml`, `setup.py` and `setup.cfg` are
//! manifests; `uv.lock` and `requirements*.txt` are lockfiles, told apart by
//! their first line because `parse` does not see the path.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use asc_core::path::RepoPath;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::{Claim, Detector, LinkCx, LinkFile, ParseFailure};
use crate::canon::{self, Value};
use crate::lock::uv as lock;
use crate::model::{
    DepKind, Ecosystem, Fingerprint, MAX_MANIFEST_BYTES, Parsed, Project, ProjectKey, ProjectKind,
};

/// Files that change every Python project under their directory.
const ECO_GLOBALS: [&[u8]; 2] = [b"uv.toml", b".python-version"];

/// The Python (uv, pip, setuptools) detector.
pub struct Python;

fn is_requirements(name: &[u8]) -> bool {
    name.starts_with(b"requirements") && name.ends_with(b".txt")
}

impl Detector for Python {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Python
    }

    fn claim(&self, path: &RepoPath) -> Option<Claim> {
        match path.file_name().as_bytes() {
            b"pyproject.toml" | b"setup.py" | b"setup.cfg" => Some(Claim::Manifest),
            b"uv.lock" => Some(Claim::Lockfile),
            n if is_requirements(n) => Some(Claim::Lockfile),
            n if ECO_GLOBALS.contains(&n) => Some(Claim::EcoGlobal),
            _ => None,
        }
    }

    fn parse(&self, claim: Claim, bytes: &[u8]) -> Result<Parsed, ParseFailure> {
        match claim {
            Claim::Manifest => parse_manifest(bytes),
            Claim::Lockfile if is_uv_lock(bytes) => Ok(lock::parse(bytes)),
            Claim::Lockfile => parse_requirements(bytes),
            Claim::EcoGlobal => Ok(Parsed(Value::Null)),
        }
    }

    fn link(&self, cx: &LinkCx<'_>) -> Vec<Project> {
        Linker::new(cx).run()
    }
}

// --- parsing ---------------------------------------------------------------

/// True when the first significant line is `version = ...`, which a
/// requirements file never starts with.
fn is_uv_lock(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    head.split(|&c| c == b'\n')
        .map(<[u8]>::trim_ascii)
        .find(|l| !l.is_empty() && l[0] != b'#')
        .and_then(|l| l.strip_prefix(b"version"))
        .map(<[u8]>::trim_ascii_start)
        .is_some_and(|r| r.starts_with(b"=") && !r.starts_with(b"=="))
}

/// PEP 503: lowercase, every run of `-`, `_`, `.` becomes one `-`.
fn normalise(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
            continue;
        }
        if sep && !out.is_empty() {
            out.push('-');
        }
        sep = false;
        out.extend(c.to_lowercase());
    }
    out
}

/// The PEP 508 leading identifier, normalised; extras and markers are dropped.
fn req_name(req: &str) -> Option<String> {
    let req = req.trim();
    let end = req
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(req.len());
    let id = &req[..end];
    id.starts_with(|c: char| c.is_ascii_alphanumeric())
        .then(|| normalise(id))
}

fn parse_manifest(bytes: &[u8]) -> Result<Parsed, ParseFailure> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ParseFailure::new("manifest larger than 4 MiB"));
    }
    let text = std::str::from_utf8(bytes).ok();
    let mut pairs = Vec::new();
    if let Some(name) = text.and_then(cfg_name) {
        pairs.push(("cfg_name".to_owned(), Value::Str(name)));
    }
    match text.map(toml::from_str::<toml::Value>) {
        Some(Ok(doc)) => {
            let v = canon::from_toml(&doc).map_err(|e| ParseFailure::new(e.to_string()))?;
            pairs.push(("py".to_owned(), pyproject_facet(&v)));
        }
        Some(Err(e)) => pairs.push(("toml_error".to_owned(), Value::Str(e.message().to_owned()))),
        None => pairs.push(("toml_error".to_owned(), Value::Str("not UTF-8".to_owned()))),
    }
    Value::map(pairs)
        .map(Parsed)
        .map_err(|e| ParseFailure::new(e.to_string()))
}

/// The `name` of a `[metadata]` section in a `setup.cfg`.
fn cfg_name(text: &str) -> Option<String> {
    let mut in_metadata = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[') {
            in_metadata = section.trim_end_matches(']').trim() == "metadata";
        } else if in_metadata
            && let Some(rest) = line.strip_prefix("name")
            && let Some(value) = rest.trim_start().strip_prefix(['=', ':'])
        {
            let name = value.trim().trim_matches(['"', '\'']);
            return (!name.is_empty() && !name.contains(':')).then(|| name.to_owned());
        }
    }
    None
}

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

/// Only what the linker reads: `project`, `groups`, `uv` and `build`.
fn pyproject_facet(v: &Value) -> Value {
    let keep = |v: &Value, keys: &[&str]| match v {
        Value::Map(m) => Value::Map(
            m.iter()
                .filter(|(k, _)| keys.contains(&k.as_str()))
                .cloned()
                .collect(),
        ),
        _ => Value::Map(Vec::new()),
    };
    let mut pairs = Vec::new();
    if let Some(p) = get(v, "project") {
        let keys = [
            "name",
            "requires-python",
            "dependencies",
            "optional-dependencies",
        ];
        pairs.push(("project".to_owned(), keep(p, &keys)));
    }
    if let Some(g) = get(v, "dependency-groups") {
        pairs.push(("groups".to_owned(), g.clone()));
    }
    if let Some(u) = get_in(get(v, "tool"), "uv") {
        pairs.push(("uv".to_owned(), u.clone()));
    }
    if let Some(r) = get_in(get(v, "build-system"), "requires") {
        pairs.push(("build".to_owned(), r.clone()));
    }
    Value::Map(pairs)
}

/// Requirement rows are `[tag, text]`: `r`/`c` include files, `e`/`p`/`f`
/// local paths and `x` everything else, which is external.
fn parse_requirements(bytes: &[u8]) -> Result<Parsed, ParseFailure> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ParseFailure::new("requirements file larger than 4 MiB"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ParseFailure::new("not UTF-8"))?;
    let rows = logical_lines(text)
        .iter()
        .map(|l| {
            let (tag, arg) = classify(l);
            Value::Array(vec![Value::Str(tag.to_owned()), Value::Str(arg)])
        })
        .collect();
    Ok(Parsed(Value::Map(vec![(
        "req".to_owned(),
        Value::Array(rows),
    )])))
}

/// Lines with comments removed and `\` continuations joined; blanks dropped.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let mut line = raw.trim();
        if line.starts_with('#') {
            line = "";
        } else if let Some(at) = line.find(" #").or_else(|| line.find("\t#")) {
            line = line[..at].trim_end();
        }
        match line.strip_suffix('\\') {
            Some(head) => {
                cur.push_str(head);
                cur.push(' ');
            }
            None => {
                cur.push_str(line);
                let joined = cur.split_whitespace().collect::<Vec<_>>().join(" ");
                if !joined.is_empty() {
                    out.push(joined);
                }
                cur.clear();
            }
        }
    }
    let tail = cur.split_whitespace().collect::<Vec<_>>().join(" ");
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// The argument of `-r file`, `-rfile`, `--requirement file` or `--requirement=file`.
fn opt_arg<'a>(line: &'a str, short: &str, long: &str) -> Option<&'a str> {
    let rest = match line.strip_prefix(long) {
        Some(r) if r.starts_with(|c: char| c.is_whitespace() || c == '=') => r,
        Some(_) => return None,
        None => line.strip_prefix(short).filter(|r| !r.starts_with('-'))?,
    };
    let arg = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
    (!arg.is_empty()).then_some(arg)
}

/// A local path without extras or a `#egg=` fragment.
fn clean_path(p: &str) -> &str {
    let p = p.split('#').next().unwrap_or(p);
    p.split('[').next().unwrap_or(p).trim()
}

fn classify(line: &str) -> (&'static str, String) {
    let external = || ("x", line.to_owned());
    if let Some(f) = opt_arg(line, "-r", "--requirement") {
        return if f.contains("://") {
            external()
        } else {
            ("r", f.to_owned())
        };
    }
    if let Some(f) = opt_arg(line, "-c", "--constraint") {
        return if f.contains("://") {
            external()
        } else {
            ("c", f.to_owned())
        };
    }
    if let Some(p) = opt_arg(line, "-e", "--editable") {
        return if p.contains("://") || p.starts_with("git+") {
            external()
        } else {
            ("e", clean_path(p).to_owned())
        };
    }
    if line.starts_with('-') {
        return external();
    }
    let head = line.split(';').next().unwrap_or(line).trim();
    if let Some((_, target)) = head.split_once('@') {
        return match target.trim().strip_prefix("file:") {
            Some(p) if !p.starts_with('/') => ("f", clean_path(p).to_owned()),
            _ => external(),
        };
    }
    let path_like = head == "." || head == ".." || head.contains(['/', '\\']);
    if path_like && !head.contains("://") {
        return ("p", clean_path(head).to_owned());
    }
    external()
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

fn inside(dir: &[u8], path: &[u8]) -> bool {
    dir.is_empty() || (path.starts_with(dir) && path.get(dir.len()) == Some(&b'/'))
}

// --- linking ---------------------------------------------------------------

enum Role<'a> {
    /// A `pyproject.toml` with `[project]` and a static name.
    Package { v: &'a Value, project: &'a Value },
    /// A `[tool.uv.workspace]` without `[project]`.
    Workspace { v: &'a Value },
    /// Requirements files with no other manifest in their directory.
    Reqs,
    /// A manifest the detector cannot read; the reason is reported.
    Opaque(String),
}

struct Ent<'a> {
    manifest: &'a RepoPath,
    /// Manifest directory; empty is the repository root.
    dir: Vec<u8>,
    name: Vec<u8>,
    role: Role<'a>,
    /// The requirements files that belong to this entry.
    reqs: Vec<LinkFile<'a>>,
}

impl<'a> Ent<'a> {
    /// The `pyproject.toml` facet of a Package or Workspace.
    fn v(&self) -> Option<&'a Value> {
        match self.role {
            Role::Package { v, .. } | Role::Workspace { v } => Some(v),
            _ => None,
        }
    }

    fn workspace(&self) -> Option<&'a Value> {
        match get_in(get_in(self.v(), "uv"), "workspace") {
            Some(w @ Value::Map(_)) => Some(w),
            _ => None,
        }
    }

    fn is_pyproject_package(&self) -> bool {
        !matches!(self.role, Role::Workspace { .. } | Role::Reqs)
            && self.manifest.file_name().as_bytes() == b"pyproject.toml"
    }
}

#[derive(Default)]
struct DirFiles<'a> {
    pyproject: Option<LinkFile<'a>>,
    setups: Vec<LinkFile<'a>>,
    reqs: Vec<LinkFile<'a>>,
}

enum LockState {
    Usable(lock::Lock),
    /// The lockfile's content id stands in for the closure (SPEC §12A.10).
    Unusable(Fingerprint),
}

/// One declared dependency of a pyproject.
struct Dep {
    kind: DepKind,
    req: String,
    internal: Option<usize>,
    /// The `[tool.uv.sources]` entry for the name.
    source: Option<Value>,
}

struct Linker<'c, 'a> {
    cx: &'c LinkCx<'a>,
    ents: Vec<Ent<'a>>,
    /// Directory to the entry that is a Package there.
    pkg_dirs: BTreeMap<Vec<u8>, usize>,
    /// Workspaces with a member glob that does not compile.
    bad_ws: BTreeSet<usize>,
    /// The workspace entry that governs each entry.
    gov: Vec<Option<usize>>,
    /// Members of each workspace entry, itself excluded.
    children: BTreeMap<usize, Vec<usize>>,
    locks: RefCell<BTreeMap<RepoPath, Option<Rc<LockState>>>>,
}

fn dir_bytes(p: &RepoPath) -> Vec<u8> {
    p.parent()
        .map(|d| d.as_bytes().to_vec())
        .unwrap_or_default()
}

fn dir_name(dir: &[u8]) -> Vec<u8> {
    match dir.rsplit(|&c| c == b'/').next() {
        Some(n) if !n.is_empty() => n.to_vec(),
        _ => b"workspace".to_vec(),
    }
}

fn group_by_dir<'a>(cx: &LinkCx<'a>) -> BTreeMap<Vec<u8>, DirFiles<'a>> {
    let mut dirs: BTreeMap<Vec<u8>, DirFiles<'a>> = BTreeMap::new();
    for f in cx.files() {
        let name = f.path.file_name();
        let dir = dir_bytes(f.path);
        match (f.claim, name.as_bytes()) {
            (Claim::Manifest, b"pyproject.toml") => {
                dirs.entry(dir).or_default().pyproject = Some(*f);
            }
            (Claim::Manifest, b"setup.py" | b"setup.cfg") => {
                dirs.entry(dir).or_default().setups.push(*f);
            }
            (Claim::Lockfile, n) if is_requirements(n) => {
                dirs.entry(dir).or_default().reqs.push(*f);
            }
            _ => {}
        }
    }
    dirs
}

fn entries<'a>(dirs: BTreeMap<Vec<u8>, DirFiles<'a>>) -> Vec<Ent<'a>> {
    let mut ents = Vec::new();
    for (dir, d) in dirs {
        let base = |manifest, name: Vec<u8>, role, reqs| Ent {
            manifest,
            dir: dir.clone(),
            name,
            role,
            reqs,
        };
        if let Some(f) = d.pyproject {
            let Some(v) = get(&f.parsed.0, "py") else {
                let why = text(get(&f.parsed.0, "toml_error")).unwrap_or("invalid TOML");
                let role = Role::Opaque(format!("pyproject.toml: {why}"));
                ents.push(base(f.path, dir_name(&dir), role, Vec::new()));
                continue;
            };
            if let Some(project) = get(v, "project") {
                let (name, role) = match text(get(project, "name")) {
                    Some(n) => (normalise(n).into_bytes(), Role::Package { v, project }),
                    None => (
                        dir_name(&dir),
                        Role::Opaque("project has no static name".to_owned()),
                    ),
                };
                ents.push(base(f.path, name, role, d.reqs));
                continue;
            }
            if matches!(get_in(get(v, "uv"), "workspace"), Some(Value::Map(_))) {
                let role = Role::Workspace { v };
                ents.push(base(f.path, dir_name(&dir), role, Vec::new()));
            }
        }
        let setup_py = d
            .setups
            .iter()
            .find(|f| f.path.file_name().as_bytes() == b"setup.py");
        if let Some(first) = setup_py.or(d.setups.first()) {
            let name = d
                .setups
                .iter()
                .find_map(|f| text(get(&f.parsed.0, "cfg_name")))
                .map_or_else(|| dir_name(&dir), |n| normalise(n).into_bytes());
            let role = Role::Opaque("setup.py or setup.cfg without [project]".to_owned());
            ents.push(base(first.path, name, role, Vec::new()));
        } else if let Some(first) = d.reqs.first() {
            let name = if dir.is_empty() {
                b".".to_vec()
            } else {
                dir.clone()
            };
            ents.push(base(first.path, name, Role::Reqs, d.reqs));
        }
    }
    ents
}

/// `(name, text)` rows of a parsed requirements file.
fn req_rows<'a>(f: &LinkFile<'a>) -> impl Iterator<Item = (&'a str, &'a str)> {
    let rows: &'a [Value] = match get(&f.parsed.0, "req") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    rows.iter().filter_map(|r| match r {
        Value::Array(p) => match p.as_slice() {
            [Value::Str(t), Value::Str(a)] => Some((t.as_str(), a.as_str())),
            _ => None,
        },
        _ => None,
    })
}

/// What requirements files contribute to one entry.
#[derive(Default)]
struct ReqFacts {
    edges: Vec<usize>,
    extras: Vec<Vec<u8>>,
    external: Vec<String>,
}

fn sources(v: Option<&Value>) -> BTreeMap<String, &Value> {
    match get_in(get_in(v, "uv"), "sources") {
        Some(Value::Map(m)) => m.iter().map(|(k, v)| (normalise(k), v)).collect(),
        _ => BTreeMap::new(),
    }
}

/// Every declared requirement with its kind.
fn dep_entries(v: &Value) -> Vec<(DepKind, &str)> {
    let mut out: Vec<(DepKind, &str)> = Vec::new();
    let project = get(v, "project");
    out.extend(
        strings(get_in(project, "dependencies"))
            .into_iter()
            .map(|r| (DepKind::Normal, r)),
    );
    let tables = [
        (get_in(project, "optional-dependencies"), DepKind::Optional),
        (get(v, "groups"), DepKind::Dev),
        (get_in(get(v, "uv"), "dev-dependencies"), DepKind::Dev),
    ];
    for (table, kind) in tables {
        if let Some(Value::Map(m)) = table {
            for (_, list) in m {
                out.extend(strings(Some(list)).into_iter().map(|r| (kind, r)));
            }
        }
        // `tool.uv.dev-dependencies` is a plain list.
        if let Some(Value::Array(_)) = table {
            out.extend(strings(table).into_iter().map(|r| (kind, r)));
        }
    }
    out.extend(
        strings(get(v, "build"))
            .into_iter()
            .map(|r| (DepKind::Build, r)),
    );
    out
}

impl<'c, 'a> Linker<'c, 'a> {
    fn new(cx: &'c LinkCx<'a>) -> Self {
        let ents = entries(group_by_dir(cx));
        let pkg_dirs: BTreeMap<Vec<u8>, usize> = ents
            .iter()
            .enumerate()
            .filter(|(_, e)| !matches!(e.role, Role::Workspace { .. }))
            .map(|(i, e)| (e.dir.clone(), i))
            .collect();
        let ws_dirs: BTreeMap<Vec<u8>, usize> = ents
            .iter()
            .enumerate()
            .filter(|(_, e)| e.workspace().is_some())
            .map(|(i, e)| (e.dir.clone(), i))
            .collect();

        let mut bad_ws = BTreeSet::new();
        let mut listed: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for &wi in ws_dirs.values() {
            let w = &ents[wi];
            let ws = w.workspace();
            let (inc, exc) = (
                compile(&strings(get_in(ws, "members"))),
                compile(&strings(get_in(ws, "exclude"))),
            );
            let (inc, exc) = match (inc, exc) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    cx.report(w.manifest.clone(), format!("workspace members: {e}"));
                    bad_ws.insert(wi);
                    continue;
                }
            };
            let set = listed.entry(wi).or_default();
            let prefix = if w.dir.is_empty() {
                Vec::new()
            } else {
                [w.dir.as_slice(), b"/"].concat()
            };
            for (dir, &ci) in pkg_dirs.range(prefix.clone()..) {
                if !dir.starts_with(&prefix) {
                    break;
                }
                let Ok(rel) = std::str::from_utf8(&dir[prefix.len()..]) else {
                    continue;
                };
                if !rel.is_empty()
                    && ents[ci].is_pyproject_package()
                    && inc.is_match(rel)
                    && !exc.is_match(rel)
                {
                    set.insert(ci);
                }
            }
        }

        let mut gov = vec![None; ents.len()];
        let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, e) in ents.iter().enumerate() {
            if !e.is_pyproject_package() {
                continue;
            }
            let g = if e.workspace().is_some() {
                Some(i)
            } else {
                nearest_listing(e, i, &ws_dirs, &listed)
            };
            gov[i] = g;
            if let Some(g) = g.filter(|&g| g != i) {
                children.entry(g).or_default().push(i);
            }
        }
        Self {
            cx,
            ents,
            pkg_dirs,
            bad_ws,
            gov,
            children,
            locks: RefCell::new(BTreeMap::new()),
        }
    }

    fn run(self) -> Vec<Project> {
        (0..self.ents.len()).map(|i| self.project(i)).collect()
    }

    fn key_of(&self, i: usize) -> ProjectKey {
        ProjectKey {
            eco: Ecosystem::Python,
            manifest: self.ents[i].manifest.clone(),
        }
    }

    fn project(&self, i: usize) -> Project {
        let e = &self.ents[i];
        match &e.role {
            Role::Opaque(why) => {
                self.cx.report(e.manifest.clone(), why.as_str());
                let mut p = Project::new(self.key_of(i), e.name.clone(), ProjectKind::Package);
                p.opaque = true;
                p
            }
            Role::Workspace { v } => {
                let mut p = Project::new(self.key_of(i), e.name.clone(), ProjectKind::Workspace);
                p.config_fp = config_value(Some(v), Some(v), BTreeMap::new()).fingerprint();
                p.opaque = self.bad_ws.contains(&i);
                self.add_members(i, &mut p);
                p
            }
            Role::Package { .. } | Role::Reqs => self.package(i),
        }
    }

    fn package(&self, i: usize) -> Project {
        let e = &self.ents[i];
        let mut p = Project::new(self.key_of(i), e.name.clone(), ProjectKind::Package);
        let gov = self.gov[i];
        let deps = self.resolve_deps(i, gov);
        let facts = self.req_facts(i);

        let mut used = BTreeMap::new();
        for d in &deps {
            if let Some(j) = d.internal {
                p.deps.push((self.key_of(j), d.kind));
            }
            if let (Some(name), Some(s)) = (req_name(&d.req), &d.source) {
                used.insert(name, s.clone());
            }
        }
        p.deps.extend(
            facts
                .edges
                .iter()
                .map(|&j| (self.key_of(j), DepKind::Normal)),
        );
        let own = e.v();
        let root = gov.and_then(|g| self.ents[g].v()).or(own);
        p.config_fp = config_value(own, root, used).fingerprint();
        p.extra_inputs = facts.extras;
        p.extra_inputs.sort();
        p.extra_inputs.dedup();
        p.external_fp = self.external_fp(i, gov, &deps, &facts.external);
        if e.workspace().is_some() {
            p.opaque = self.bad_ws.contains(&i);
            self.add_members(i, &mut p);
        }
        p
    }

    fn add_members(&self, ws: usize, p: &mut Project) {
        for &c in self.children.get(&ws).into_iter().flatten() {
            p.deps.push((self.key_of(c), DepKind::Member));
        }
    }

    /// The members of workspace entry `g`, itself included when it is a Package.
    fn members(&self, g: usize) -> impl Iterator<Item = usize> + '_ {
        let own = matches!(self.ents[g].role, Role::Package { .. }).then_some(g);
        own.into_iter()
            .chain(self.children.get(&g).into_iter().flatten().copied())
    }

    /// The entry a `[tool.uv.sources]` value points at, declared in `dir`.
    fn source_target(
        &self,
        name: &str,
        gov: Option<usize>,
        dir: &[u8],
        src: &Value,
    ) -> Option<usize> {
        let one = std::slice::from_ref(src);
        let cands = match src {
            Value::Array(a) => a.as_slice(),
            _ => one,
        };
        cands.iter().find_map(|m| {
            if is_true(get(m, "workspace")) {
                return self.members(gov?).find(|&j| {
                    matches!(self.ents[j].role, Role::Package { .. })
                        && self.ents[j].name == name.as_bytes()
                });
            }
            let d = join_dir(dir, text(get(m, "path"))?)?;
            self.pkg_dirs.get(&d).copied()
        })
    }

    fn resolve_deps(&self, i: usize, gov: Option<usize>) -> Vec<Dep> {
        let e = &self.ents[i];
        let Some(v) = e.v() else {
            return Vec::new();
        };
        let own = sources(Some(v));
        let root = gov
            .filter(|&g| g != i)
            .map(|g| (self.ents[g].dir.as_slice(), sources(self.ents[g].v())));
        dep_entries(v)
            .into_iter()
            .map(|(kind, req)| {
                let name = req_name(req);
                let found = name.as_ref().and_then(|n| {
                    own.get(n).map(|s| (e.dir.as_slice(), *s)).or_else(|| {
                        let (dir, table) = root.as_ref()?;
                        table.get(n).map(|s| (*dir, *s))
                    })
                });
                let internal = found
                    .and_then(|(dir, s)| self.source_target(name.as_deref()?, gov, dir, s))
                    .filter(|&j| j != i);
                Dep {
                    kind,
                    req: req.to_owned(),
                    internal,
                    source: found.map(|(_, s)| s.clone()),
                }
            })
            .collect()
    }

    /// Edges, extra inputs and external lines from the entry's requirements files.
    fn req_facts(&self, i: usize) -> ReqFacts {
        let e = &self.ents[i];
        let mut out = ReqFacts::default();
        for f in &e.reqs {
            let rdir = dir_bytes(f.path);
            for (tag, arg) in req_rows(f) {
                match tag {
                    "r" | "c" => {
                        if let Some(p) = join_dir(&rdir, arg)
                            && !p.is_empty()
                            && !inside(&e.dir, &p)
                        {
                            out.extras.push(escape_glob(&p));
                        }
                    }
                    "e" | "p" | "f" => {
                        let target = join_dir(&rdir, arg).and_then(|d| self.pkg_dirs.get(&d));
                        out.edges.extend(target.copied().filter(|&j| j != i));
                    }
                    _ => out.external.push(arg.to_owned()),
                }
            }
        }
        out.external.sort();
        out.external.dedup();
        out
    }

    fn lock_at(&self, dir: &[u8]) -> Option<Rc<LockState>> {
        let path = path_in(dir, "uv.lock")?;
        if let Some(hit) = self.locks.borrow().get(&path) {
            return hit.clone();
        }
        let state = self
            .cx
            .file(&path)
            .filter(|f| f.eco == Ecosystem::Python && f.claim == Claim::Lockfile)
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

    /// The `external_fp` of entry `i` (SPEC §12A.4).
    fn external_fp(
        &self,
        i: usize,
        gov: Option<usize>,
        deps: &[Dep],
        external: &[String],
    ) -> Fingerprint {
        let e = &self.ents[i];
        if let Role::Package { project, .. } = e.role {
            let name = text(get(project, "name"))
                .map(normalise)
                .unwrap_or_default();
            let lock = gov
                .and_then(|g| self.lock_at(&self.ents[g].dir))
                .or_else(|| self.lock_at(&e.dir));
            match lock.as_deref() {
                Some(LockState::Unusable(fp)) => return *fp,
                Some(LockState::Usable(l)) => {
                    if let Some(fp) = l.closure(&name) {
                        return fold(fp, external);
                    }
                    self.cx.report(
                        e.manifest.clone(),
                        format!("{name} is not in uv.lock; using declared dependencies"),
                    );
                }
                None => {}
            }
        }
        declared_fp(deps, external)
    }
}

/// A lock closure plus the external lines of the requirements files.
fn fold(fp: Fingerprint, external: &[String]) -> Fingerprint {
    if external.is_empty() {
        return fp;
    }
    let hex: String = fp.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let lines = external.iter().cloned().map(Value::Str).collect();
    Value::Array(vec![Value::Str(hex), Value::Array(lines)]).fingerprint()
}

/// The nearest ancestor workspace that lists entry `i`.
fn nearest_listing(
    e: &Ent<'_>,
    i: usize,
    ws_dirs: &BTreeMap<Vec<u8>, usize>,
    listed: &BTreeMap<usize, BTreeSet<usize>>,
) -> Option<usize> {
    let mut dir = e.dir.as_slice();
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

/// The `config_fp` value: the root's `[tool.uv]` table without the per-project
/// `sources` and `dev-dependencies`, `requires-python`, and the sources used.
fn config_value(own: Option<&Value>, root: Option<&Value>, used: BTreeMap<String, Value>) -> Value {
    let uv = match get_in(root, "uv") {
        Some(Value::Map(m)) => Value::Map(
            m.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "sources" | "dev-dependencies"))
                .cloned()
                .collect(),
        ),
        _ => Value::Null,
    };
    let requires = |v: Option<&Value>| {
        get_in(get_in(v, "project"), "requires-python")
            .cloned()
            .unwrap_or(Value::Null)
    };
    let pairs = [
        ("uv", uv),
        (
            "requires_python",
            Value::Array(vec![requires(own), requires(root)]),
        ),
        ("sources", Value::map(used).unwrap_or(Value::Null)),
    ];
    Value::map(pairs.map(|(k, v)| (k.to_owned(), v))).unwrap_or(Value::Null)
}

/// With no usable lock: the external declarations and requirement lines.
fn declared_fp(deps: &[Dep], external: &[String]) -> Fingerprint {
    let rows = deps
        .iter()
        .filter(|d| d.internal.is_none())
        .map(|d| {
            Value::Array(vec![
                Value::Int(d.kind as i64),
                Value::Str(d.req.clone()),
                d.source.clone().unwrap_or(Value::Null),
            ])
        })
        .collect();
    let lines = external.iter().cloned().map(Value::Str).collect();
    Value::Array(vec![Value::Array(rows), Value::Array(lines)]).fingerprint()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResolvedConfig;
    use crate::detect::testkit::{Tree, rp};
    use crate::link::{LinkInput, link_graph};
    use crate::model::{MAX_LOCKFILE_BYTES, ProjectGraph};

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
            .join("tests/fixtures/python")
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
                    let claim = Python.claim(&path)?;
                    Some(LinkInput {
                        path,
                        id: *blake3::hash(c.as_bytes()).as_bytes(),
                        parsed: Python.parse(claim, c.as_bytes()),
                    })
                })
                .collect();
        link_graph(&[&Python], &files, &[], ResolvedConfig::default(), &|_| {
            true
        })
        .unwrap()
    }

    fn project<'g>(g: &'g ProjectGraph, manifest: &str) -> &'g Project {
        let want = rp(manifest);
        g.projects
            .iter()
            .find(|p| p.key.manifest == want)
            .unwrap_or_else(|| panic!("no project for {manifest}"))
    }

    fn edges(g: &ProjectGraph, manifest: &str) -> Vec<(String, DepKind)> {
        project(g, manifest)
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

    fn mini(files: &[(&str, &str)]) -> ProjectGraph {
        try_graph(&Tree::new(files))
    }

    const SOMA: &str = "packages/asc-soma/pyproject.toml";
    const SHARED: &str = "packages/shared/pyproject.toml";
    const SOMA_LIB: &str = "packages/soma/pyproject.toml";

    #[test]
    fn claims_by_basename() {
        let c = |p: &str| Python.claim(&rp(p));
        assert_eq!(c("a/pyproject.toml"), Some(Claim::Manifest));
        assert_eq!(c("setup.py"), Some(Claim::Manifest));
        assert_eq!(c("x/setup.cfg"), Some(Claim::Manifest));
        assert_eq!(c("uv.lock"), Some(Claim::Lockfile));
        assert_eq!(c("requirements.txt"), Some(Claim::Lockfile));
        assert_eq!(c("x/requirements-dev.txt"), Some(Claim::Lockfile));
        assert_eq!(c("uv.toml"), Some(Claim::EcoGlobal));
        assert_eq!(c("x/.python-version"), Some(Claim::EcoGlobal));
        assert_eq!(c("x/dev-requirements.txt"), None);
        assert_eq!(c("x/pyproject.json"), None);
    }

    #[test]
    fn names_follow_pep_503() {
        assert_eq!(normalise("ASC_Soma.Core"), "asc-soma-core");
        assert_eq!(normalise("A--b__c"), "a-b-c");
        assert_eq!(
            req_name("soma[onnx]>=1; python_version>'3'").as_deref(),
            Some("soma")
        );
        assert_eq!(req_name("Foo_Bar ==1").as_deref(), Some("foo-bar"));
        assert_eq!(req_name("-r x"), None);
        let g = asc().1;
        assert_eq!(project(&g, SOMA).name, b"asc-soma");
    }

    #[test]
    fn workspace_members_come_from_globs_minus_exclude() {
        let (_, g) = asc();
        let root = project(&g, "pyproject.toml");
        assert_eq!(root.kind, ProjectKind::Workspace);
        assert_eq!(
            edges(&g, "pyproject.toml"),
            [
                e(SOMA, DepKind::Member),
                e(SHARED, DepKind::Member),
                e(SOMA_LIB, DepKind::Member),
            ]
        );
        // The excluded directory is still a Package, just no member.
        assert_eq!(
            project(&g, "packages/skip/pyproject.toml").kind,
            ProjectKind::Package
        );
    }

    #[test]
    fn workspace_and_path_sources_are_internal_and_git_stays_external() {
        let (_, g) = asc();
        // `shared = { workspace = true }` is an edge; `soma = { git = ... }` is not,
        // although a member named `soma` exists.
        assert_eq!(
            edges(&g, SOMA),
            [e(SHARED, DepKind::Normal), e(SHARED, DepKind::Dev)]
        );
        // `{ path = "../../packages/shared", editable = true }`.
        assert_eq!(
            edges(&g, "standalone/pathy/pyproject.toml"),
            [e(SHARED, DepKind::Normal)]
        );
        // A dependency that names a member without a source is external.
        assert!(edges(&g, "packages/skip/pyproject.toml").is_empty());
    }

    #[test]
    fn dependency_tables_set_the_edge_kind() {
        let g = mini(&[
            (
                "pyproject.toml",
                "[tool.uv.workspace]\nmembers = [\"p/*\"]\n[tool.uv.sources]\nb = { workspace = true }\nc = { workspace = true }\nd = { workspace = true }\ne = { workspace = true }\n",
            ),
            (
                "p/a/pyproject.toml",
                "[project]\nname = \"a\"\ndependencies = [\"b\"]\n[project.optional-dependencies]\nx = [\"c\"]\n[dependency-groups]\ndev = [\"d\", { include-group = \"other\" }]\n[build-system]\nrequires = [\"e\"]\n",
            ),
            ("p/b/pyproject.toml", "[project]\nname = \"b\"\n"),
            ("p/c/pyproject.toml", "[project]\nname = \"c\"\n"),
            (
                "p/d/pyproject.toml",
                "[project]\nname = \"d\"\n[tool.uv]\ndev-dependencies = [\"b\"]\n",
            ),
            ("p/e/pyproject.toml", "[project]\nname = \"e\"\n"),
        ]);
        let mut got = edges(&g, "p/a/pyproject.toml");
        got.sort();
        let mut want = vec![
            e("p/b/pyproject.toml", DepKind::Normal),
            e("p/c/pyproject.toml", DepKind::Optional),
            e("p/d/pyproject.toml", DepKind::Dev),
            e("p/e/pyproject.toml", DepKind::Build),
        ];
        want.sort();
        assert_eq!(got, want);
        // `tool.uv.dev-dependencies` is a Dev list.
        assert_eq!(
            edges(&g, "p/d/pyproject.toml"),
            [e("p/b/pyproject.toml", DepKind::Dev)]
        );
    }

    #[test]
    fn requirements_belong_to_the_package_in_their_directory() {
        let (_, g) = asc();
        let m = "libs/mixed/pyproject.toml";
        assert_eq!(
            edges(&g, m),
            [e(SHARED, DepKind::Normal)],
            "no extra project for libs/mixed/requirements.txt"
        );
        assert!(
            g.projects
                .iter()
                .all(|p| p.key.manifest != rp("libs/mixed/requirements.txt"))
        );
        assert_eq!(project(&g, m).extra_inputs, [b"libs/common.txt".to_vec()]);
    }

    #[test]
    fn requirements_without_a_package_make_one_named_by_directory() {
        let (_, g) = asc();
        let p = project(&g, "services/api/requirements-dev.txt");
        assert_eq!(p.name, b"services/api");
        assert!(!p.opaque);
        assert_eq!(
            edges(&g, "services/api/requirements-dev.txt"),
            [e(SOMA, DepKind::Normal), e(SHARED, DepKind::Normal)]
        );
        assert_eq!(
            p.extra_inputs,
            [
                b"services/constraints.txt".to_vec(),
                b"services/shared/base.txt".to_vec()
            ]
        );
    }

    #[test]
    fn requirement_lines_are_classified() {
        let lines = logical_lines(
            "# c\nflask==2 \\\n  --hash=sha256:aa  # tail\n-r base.txt\n--constraint=c.txt\n-e ../x[extra]#egg=x\n./libs/y\npkg @ file:../z\npkg2 @ https://h/p.whl\n-e git+https://h/r.git#egg=r\n--index-url https://i/simple\n\nnumpy; python_version<'3.12'\n",
        );
        let got: Vec<_> = lines.iter().map(|l| classify(l)).collect();
        assert_eq!(
            got,
            [
                ("x", "flask==2 --hash=sha256:aa".to_owned()),
                ("r", "base.txt".to_owned()),
                ("c", "c.txt".to_owned()),
                ("e", "../x".to_owned()),
                ("p", "./libs/y".to_owned()),
                ("f", "../z".to_owned()),
                ("x", "pkg2 @ https://h/p.whl".to_owned()),
                ("x", "-e git+https://h/r.git#egg=r".to_owned()),
                ("x", "--index-url https://i/simple".to_owned()),
                ("x", "numpy; python_version<'3.12'".to_owned()),
            ]
        );
    }

    #[test]
    fn setup_files_make_opaque_packages() {
        let (_, g) = asc();
        let p = project(&g, "tools/legacy/setup.py");
        assert!(p.opaque);
        assert_eq!(p.name, b"legacy-tool");
        // Opaque projects depend on the rest of the ecosystem.
        assert!(edges(&g, "tools/legacy/setup.py").contains(&e(SHARED, DepKind::Implicit)));
        assert!(
            g.issues
                .iter()
                .any(|i| i.path == rp("tools/legacy/setup.py"))
        );
        // Without setup.cfg the directory names the package.
        let g = mini(&[("lib/setup.py", "from setuptools import setup\nsetup()\n")]);
        assert_eq!(project(&g, "lib/setup.py").name, b"lib");
        // A pyproject with [project] wins over setup.py.
        let g = mini(&[
            ("lib/setup.py", "setup()\n"),
            ("lib/pyproject.toml", "[project]\nname = \"lib\"\n"),
        ]);
        assert!(!project(&g, "lib/pyproject.toml").opaque);
        assert!(
            g.projects
                .iter()
                .all(|p| p.key.manifest != rp("lib/setup.py"))
        );
    }

    #[test]
    fn bad_manifests_are_opaque_and_reported() {
        let g = mini(&[
            ("a/pyproject.toml", "[project]\nversion = \"1\"\n"),
            ("b/pyproject.toml", "[project\nname = "),
            (
                "c/pyproject.toml",
                "[tool.uv.workspace]\nmembers = [\"[\"]\n",
            ),
            ("d/pyproject.toml", "[project]\nname = \"d\"\n"),
            ("e/pyproject.toml", "[tool.ruff]\nline-length = 100\n"),
        ]);
        for r in ["a", "b", "c"] {
            assert!(project(&g, &format!("{r}/pyproject.toml")).opaque, "{r}");
        }
        assert!(!project(&g, "d/pyproject.toml").opaque);
        // A tool-config-only pyproject is no project at all.
        assert!(
            g.projects
                .iter()
                .all(|p| p.key.manifest != rp("e/pyproject.toml"))
        );
        let paths: Vec<_> = g.issues.iter().map(|i| i.path.to_string()).collect();
        assert!(paths.contains(&"b/pyproject.toml".to_owned()), "{paths:?}");
        assert!(paths.contains(&"c/pyproject.toml".to_owned()), "{paths:?}");
    }

    #[test]
    fn a_lock_bump_affects_only_members_that_reach_it() {
        let (mut t, before) = asc();
        let l = t.0["uv.lock"].replace("sha256:np1", "sha256:np2");
        assert_ne!(l, t.0["uv.lock"]);
        t.set("uv.lock", &l);
        let after = try_graph(&t);
        let ext = |g: &ProjectGraph, m: &str| project(g, m).external_fp;
        // numpy is reached by asc-soma and shared (through numpy), not by `soma`.
        assert_ne!(ext(&before, SOMA), ext(&after, SOMA));
        assert_ne!(ext(&before, SHARED), ext(&after, SHARED));
        assert_eq!(ext(&before, SOMA_LIB), ext(&after, SOMA_LIB));
        assert_eq!(
            ext(&before, "packages/skip/pyproject.toml"),
            ext(&after, "packages/skip/pyproject.toml")
        );
        // Nothing else about the members changed.
        assert_eq!(
            project(&before, SOMA).config_fp,
            project(&after, SOMA).config_fp
        );
    }

    #[test]
    fn a_git_source_bump_affects_only_its_user() {
        let (mut t, before) = asc();
        let l = t.0["uv.lock"].replace("rev=aaa", "rev=bbb");
        assert_ne!(l, t.0["uv.lock"]);
        t.set("uv.lock", &l);
        let after = try_graph(&t);
        assert_ne!(
            project(&before, SOMA).external_fp,
            project(&after, SOMA).external_fp
        );
        assert_eq!(
            project(&before, SHARED).external_fp,
            project(&after, SHARED).external_fp
        );
    }

    #[test]
    fn with_a_lock_declared_requirements_do_not_count() {
        let (mut t, before) = asc();
        let m = t.0[SHARED].replace("numpy", "numpy>=1.20");
        assert_ne!(m, t.0[SHARED]);
        t.set(SHARED, &m);
        let after = try_graph(&t);
        assert_eq!(
            project(&before, SHARED).external_fp,
            project(&after, SHARED).external_fp
        );
    }

    #[test]
    fn without_a_lock_declarations_are_the_external_closure() {
        let (mut t, before) = asc();
        let ext = |g: &ProjectGraph, m: &str| project(g, m).external_fp;
        let m = "standalone/pathy/pyproject.toml";
        let edited = t.0[m].replace("rich", "rich>=13");
        assert_ne!(edited, t.0[m]);
        t.set(m, &edited);
        let after = try_graph(&t);
        assert_ne!(ext(&before, m), ext(&after, m));
        assert_eq!(ext(&before, SHARED), ext(&after, SHARED));
        // A requirement line of a requirements-only package counts too.
        let r = "services/api/requirements-dev.txt";
        let edited = t.0[r].replace("pytest", "pytest==8");
        assert_ne!(edited, t.0[r]);
        let mut t2 = t.clone();
        t2.set(r, &edited);
        let again = try_graph(&t2);
        assert_ne!(ext(&after, r), ext(&again, r));
    }

    #[test]
    fn config_edits_stay_in_their_scope() {
        let (mut t, before) = asc();
        // A `[tool.uv]` edit at the workspace root reaches every member.
        let root =
            t.0["pyproject.toml"].replace("prerelease = \"allow\"", "prerelease = \"disallow\"");
        assert_ne!(root, t.0["pyproject.toml"]);
        t.set("pyproject.toml", &root);
        let after = try_graph(&t);
        for m in [SOMA, SHARED, SOMA_LIB] {
            assert_ne!(
                project(&before, m).config_fp,
                project(&after, m).config_fp,
                "{m}"
            );
            assert_eq!(
                project(&before, m).external_fp,
                project(&after, m).external_fp,
                "{m}"
            );
        }
        // The standalone project has its own root.
        let m = "standalone/pathy/pyproject.toml";
        assert_eq!(project(&before, m).config_fp, project(&after, m).config_fp);

        // A source entry only changes the projects that use it.
        let (mut t, before) = asc();
        let s = t.0[SOMA].replace("rev = \"aaa\"", "rev = \"bbb\"");
        assert_ne!(s, t.0[SOMA]);
        t.set(SOMA, &s);
        let after = try_graph(&t);
        assert_ne!(
            project(&before, SOMA).config_fp,
            project(&after, SOMA).config_fp
        );
        assert_eq!(
            project(&before, SHARED).config_fp,
            project(&after, SHARED).config_fp
        );
    }

    #[test]
    fn over_limit_files() {
        let big = " ".repeat(MAX_MANIFEST_BYTES + 1);
        let err = Python.parse(Claim::Manifest, big.as_bytes()).unwrap_err();
        assert!(err.message.contains("4 MiB"), "{err}");
        let err = Python.parse(Claim::Lockfile, big.as_bytes()).unwrap_err();
        assert!(err.message.contains("4 MiB"), "{err}");
        // An unparsable manifest is an opaque project, not a failed graph.
        let g = mini(&[
            ("a/pyproject.toml", &big),
            ("b/pyproject.toml", "[project]\nname = \"b\"\n"),
        ]);
        assert!(project(&g, "a/pyproject.toml").opaque);

        // A uv.lock over the limit makes its id the closure of every governed member.
        let lock = format!("version = 1\n{}", " ".repeat(MAX_LOCKFILE_BYTES));
        let t = Tree::new(&[
            ("pyproject.toml", "[project]\nname = \"m\"\n"),
            ("uv.lock", &lock),
        ]);
        let g = try_graph(&t);
        let id = *blake3::hash(t.0["uv.lock"].as_bytes()).as_bytes();
        assert_eq!(
            project(&g, "pyproject.toml").external_fp,
            Fingerprint::from(id)
        );
        assert!(g.issues.iter().any(|i| i.path == rp("uv.lock")));
    }

    #[test]
    fn lock_and_requirements_are_told_apart_by_content() {
        assert!(is_uv_lock(b"# c\n\nversion = 1\n"));
        assert!(is_uv_lock(b"version=1\n"));
        assert!(!is_uv_lock(b"version==1.0\n"));
        assert!(!is_uv_lock(b"flask\n"));
        assert!(!is_uv_lock(b""));
    }

    #[test]
    fn deeply_nested_manifests_are_rejected() {
        let deep = format!("a = {}1{}", "[".repeat(200), "]".repeat(200));
        // Too deep for TOML: the file is kept as unreadable, so its project is opaque.
        let g = mini(&[("a/pyproject.toml", &deep)]);
        assert!(project(&g, "a/pyproject.toml").opaque);
    }
}
