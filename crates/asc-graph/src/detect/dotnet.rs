//! The .NET detector (SPEC §12A.2.3, §12A.4).
//!
//! Project files and `Directory.Packages.props` are parsed to canonical XML
//! values, solutions to their project paths. `link` reads only the manifest
//! set. `Condition` attributes are ignored, so every item counts. The items of
//! every `Directory.Build.props`/`.targets` above a project count as its own.
//! Item and attribute names match in any case, `%` escapes are not evaluated,
//! and an `Import` of a file the detector does not read is a gap.
//! Anything the detector cannot resolve to a known project or a repository
//! path makes the project opaque.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use asc_core::path::RepoPath;

use super::{Claim, Detector, LinkCx, ParseFailure};
use crate::canon::{self, Value};
use crate::lock::nuget as lock;
use crate::model::{
    DepKind, Ecosystem, Fingerprint, MAX_MANIFEST_BYTES, Parsed, Project, ProjectKey, ProjectKind,
};

/// Map key of the project paths in a parsed `.sln`.
const SOLUTION: &str = "solution";
const PROJECT_EXTS: [&[u8]; 3] = [b"csproj", b"fsproj", b"vbproj"];
const THIS_DIR: &str = "$(MSBuildThisFileDirectory)";
const PROJECT_DIR: &str = "$(MSBuildProjectDirectory)";
const CENTRAL: &[u8] = b"Directory.Packages.props";
const LOCKFILE: &str = "packages.lock.json";
/// Most `;`-separated values the `Directory.Build.*` items may resolve in one
/// link, summed over all projects, plus the `Directory.Build.*` path elements
/// each project walks (an element with no value counts as one). Past it the
/// remaining projects are opaque.
const MAX_BUILD_SCAN_VALUES: usize = 1 << 20;
/// Most bytes of `Include`/`Project` attribute text the `Directory.Build.*`
/// items may walk in one link, summed over all projects. This bounds the
/// `extra_inputs` copied into the projects. Past it the remaining projects are
/// opaque.
const MAX_BUILD_SCAN_BYTES: usize = 1 << 26;
/// Longest single path value; a longer one makes the project opaque.
const MAX_PATH_VALUE_BYTES: usize = 4096;

/// Why a project is opaque; each is also reported once per project.
const UNEVALUATED_REFERENCE: &str =
    "ProjectReference uses an unevaluated MSBuild property or wildcard";
const UNRESOLVED_REFERENCE: &str = "ProjectReference does not resolve to exactly one known project";
const UNEVALUATED_ITEM: &str = "Import or item uses an unevaluated MSBuild property";
const WILDCARD_IMPORT: &str = "Import uses a wildcard";
const OUTSIDE_PATH: &str = "reference, Import or item is absolute or leaves the repository";
const UNREADABLE_BUILD_FILE: &str =
    "Directory.Build file above the project is not a readable MSBuild file";
const BUILD_SCAN_BUDGET: &str = "Directory.Build files above the project exceed the scan budget";
const UNSCANNED_IMPORT: &str = "Import names a file the detector does not read";
const LONG_VALUE: &str = "reference, Import or item path is longer than the limit";

/// The .NET detector.
pub struct Dotnet;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Project,
    Sln,
    Slnx,
    Central,
}

fn split_ext(name: &[u8]) -> Option<(&[u8], &[u8])> {
    let dot = name.iter().rposition(|&c| c == b'.').filter(|&i| i > 0)?;
    Some((&name[..dot], &name[dot + 1..]))
}

fn manifest_kind(name: &[u8]) -> Option<FileKind> {
    if name == CENTRAL {
        return Some(FileKind::Central);
    }
    let (_, ext) = split_ext(name)?;
    if PROJECT_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
        Some(FileKind::Project)
    } else if ext.eq_ignore_ascii_case(b"sln") {
        Some(FileKind::Sln)
    } else if ext.eq_ignore_ascii_case(b"slnx") {
        Some(FileKind::Slnx)
    } else {
        None
    }
}

impl Detector for Dotnet {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::Dotnet
    }

    fn claim(&self, path: &RepoPath) -> Option<Claim> {
        let name = path.file_name();
        let name = name.as_bytes();
        if manifest_kind(name).is_some() {
            return Some(Claim::Manifest);
        }
        match name {
            b"packages.lock.json" => Some(Claim::Lockfile),
            n if is_build_file(n) => Some(Claim::EcoGlobal),
            b"global.json" => Some(Claim::EcoGlobal),
            b"dotnet-tools.json"
                if path
                    .parent()
                    .is_some_and(|p| p.file_name().as_bytes() == b".config") =>
            {
                Some(Claim::EcoGlobal)
            }
            n if n.eq_ignore_ascii_case(b"nuget.config") => Some(Claim::EcoGlobal),
            _ => None,
        }
    }

    fn parse(&self, claim: Claim, bytes: &[u8]) -> Result<Parsed, ParseFailure> {
        match claim {
            Claim::Manifest => parse_manifest(bytes),
            Claim::Lockfile => Ok(lock::parse(bytes)),
            Claim::EcoGlobal => Ok(parse_global(bytes)),
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
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|_| ParseFailure::new("not UTF-8"))?;
    if !text.trim_start().starts_with('<') {
        return parse_sln(text).map(Parsed);
    }
    canon::from_xml(bytes)
        .map(Parsed)
        .map_err(|e| ParseFailure::new(e.to_string()))
}

/// An eco-global file as an XML element, so the items of `Directory.Build.*`
/// can be read at link time. Anything else (JSON, oversized, malformed) is
/// `Null`; `link` treats a `Directory.Build.*` file that is not an element as
/// unreadable and makes the projects below it opaque.
fn parse_global(bytes: &[u8]) -> Parsed {
    let null = Parsed(Value::Null);
    if bytes.len() > MAX_MANIFEST_BYTES {
        return null;
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let Ok(text) = std::str::from_utf8(bytes) else {
        return null;
    };
    if !text.trim_start().starts_with('<') {
        return null;
    }
    canon::from_xml(bytes).map_or(null, Parsed)
}

/// `Directory.Build.props`/`.targets` in any case: MSBuild finds them by a
/// case-insensitive file system lookup on Windows and macOS, so a file in any
/// case may apply.
fn is_build_file(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"Directory.Build.props")
        || name.eq_ignore_ascii_case(b"Directory.Build.targets")
}

/// The project paths of a `.sln`: the `Project("{type}") = "name", "path", "{guid}"` lines
/// whose path has a project extension, so solution folders are skipped.
fn parse_sln(text: &str) -> Result<Value, ParseFailure> {
    if !text.contains("Microsoft Visual Studio Solution File") {
        return Err(ParseFailure::new("neither XML nor a solution file"));
    }
    let paths: BTreeSet<&str> = text
        .lines()
        .filter(|l| l.trim_start().starts_with("Project("))
        .filter_map(|l| l.split('"').nth(5))
        .filter(|p| is_project_path(p))
        .collect();
    let list = paths
        .into_iter()
        .map(|p| Value::Str(p.to_owned()))
        .collect();
    Value::map([(SOLUTION.to_owned(), Value::Array(list))])
        .map_err(|e| ParseFailure::new(e.to_string()))
}

fn is_project_path(p: &str) -> bool {
    let file = p.rsplit(['/', '\\']).next().unwrap_or(p);
    split_ext(file.as_bytes())
        .is_some_and(|(_, e)| PROJECT_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

// --- value helpers ---------------------------------------------------------

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

fn text(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

fn el_name(e: &Value) -> &str {
    text(get(e, "name")).unwrap_or_default()
}

fn attr<'a>(e: &'a Value, key: &str) -> Option<&'a str> {
    text(get(get(e, "attrs")?, key))
}

/// [`attr`] with the attribute name matched in any case, so a spelling MSBuild
/// might accept is never skipped.
fn attr_ci<'a>(e: &'a Value, key: &str) -> Option<&'a str> {
    match get(e, "attrs")? {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| text(Some(v))),
        _ => None,
    }
}

fn kids(e: &Value) -> &[Value] {
    match get(e, "children") {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

/// The text of the first child element `name`.
fn kid_text<'a>(e: &'a Value, name: &str) -> Option<&'a str> {
    kids(e)
        .iter()
        .find(|k| el_name(k) == name)
        .and_then(|k| text(get(k, "text")))
}

fn is_element(v: &Value) -> bool {
    text(get(v, "name")).is_some()
}

/// Every element under `e`, `e` included, in document order.
fn elements<'a>(e: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(e);
    for k in kids(e) {
        elements(k, out);
    }
}

/// `e` without its `PackageVersion` elements.
fn strip_versions(e: &Value) -> Value {
    let Value::Map(m) = e else { return e.clone() };
    Value::Map(
        m.iter()
            .map(|(k, v)| match (k.as_str(), v) {
                ("children", Value::Array(c)) => (
                    k.clone(),
                    Value::Array(
                        c.iter()
                            .filter(|x| el_name(x) != "PackageVersion")
                            .map(strip_versions)
                            .collect(),
                    ),
                ),
                _ => (k.clone(), v.clone()),
            })
            .collect(),
    )
}

// --- paths -----------------------------------------------------------------

/// Joins `rel` onto directory `base` (bytes, empty is the root). `None` when
/// the result leaves the repository or `rel` is absolute.
fn join_dir(base: &[u8], rel: &str) -> Option<Vec<u8>> {
    if rel.starts_with('/') || rel.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let mut parts: Vec<&[u8]> = if base.is_empty() {
        Vec::new()
    } else {
        base.split(|&c| c == b'/').collect()
    };
    for c in rel.split('/') {
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

fn inside(dir: &[u8], p: &[u8]) -> bool {
    dir.is_empty() || (p.starts_with(dir) && p.get(dir.len()) == Some(&b'/'))
}

/// One `;`-separated value of an MSBuild path attribute.
enum Resolved {
    Path(Vec<u8>),
    /// Leaves the repository, or is absolute.
    Outside,
    /// Uses a property or item this detector does not evaluate.
    Unknown,
}

fn resolve_one(dir: &[u8], raw: &str) -> Resolved {
    let s = raw
        .replace('\\', "/")
        .replace(THIS_DIR, "./")
        .replace(PROJECT_DIR, "./");
    // `%XX` is an MSBuild escape (`%2E` is `.`); any `%` is not evaluated.
    // A path built from both directory properties has no single base.
    if ["$(", "@(", "%"].iter().any(|m| s.contains(m))
        || (raw.contains(THIS_DIR) && raw.contains(PROJECT_DIR))
    {
        return Resolved::Unknown;
    }
    join_dir(dir, &s).map_or(Resolved::Outside, Resolved::Path)
}

/// The `;`-separated values of an MSBuild path attribute.
fn values(raw: Option<&str>) -> impl Iterator<Item = &str> {
    raw.unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn has_wildcard(path: &[u8]) -> bool {
    path.iter().any(|b| matches!(b, b'*' | b'?'))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathItem {
    Reference,
    Import,
    /// `Compile`, `Content`, `None` or `EmbeddedResource`.
    Other,
}

/// The kind of an element whose paths the detector follows. Names match in any
/// case, because MSBuild item types do.
fn path_item(name: &str) -> Option<PathItem> {
    let is = |n: &str| name.eq_ignore_ascii_case(n);
    if is("ProjectReference") {
        Some(PathItem::Reference)
    } else if is("Import") {
        Some(PathItem::Import)
    } else if is("Compile") || is("Content") || is("None") || is("EmbeddedResource") {
        Some(PathItem::Other)
    } else {
        None
    }
}

// --- linking ---------------------------------------------------------------

struct Man<'a> {
    path: &'a RepoPath,
    /// Manifest directory; empty is the repository root.
    dir: Vec<u8>,
    v: &'a Value,
}

struct Linker<'c, 'a> {
    cx: &'c LinkCx<'a>,
    projects: Vec<Man<'a>>,
    solutions: Vec<(Man<'a>, FileKind)>,
    props: Vec<Man<'a>>,
    /// Per `props` entry: `PackageVersion` elements by package name.
    versions: Vec<BTreeMap<&'a str, &'a Value>>,
    /// The `Directory.Build.props`/`.targets` files.
    builds: Vec<Man<'a>>,
    /// Per `builds` entry: its path items; empty when it is not an element.
    build_items: Vec<Vec<&'a Value>>,
    /// Build-file values resolved so far, against [`MAX_BUILD_SCAN_VALUES`].
    scanned: Cell<usize>,
    /// Build-file attribute bytes walked so far, against [`MAX_BUILD_SCAN_BYTES`].
    scanned_bytes: Cell<usize>,
    exact: BTreeMap<Vec<u8>, usize>,
    folded: BTreeMap<Vec<u8>, Vec<usize>>,
}

/// What a project's own items and the items of the build files above it name.
#[derive(Default)]
struct Scan {
    refs: Vec<usize>,
    extra: BTreeSet<Vec<u8>>,
    /// Reasons the project is opaque.
    gaps: BTreeSet<&'static str>,
}

/// What a project path names.
enum Lookup {
    One(usize),
    /// Several projects differ from the path only by case.
    Ambiguous,
    Missing,
}

/// Indices of the `mans` at or above `dir`, outermost first.
fn at_or_above(mans: &[Man<'_>], dir: &[u8]) -> Vec<usize> {
    let mut v: Vec<usize> = (0..mans.len())
        .filter(|&j| mans[j].dir == dir || inside(&mans[j].dir, dir))
        .collect();
    v.sort_by_key(|&j| mans[j].dir.len());
    v
}

/// One `PackageReference`.
struct Package {
    name: String,
    /// Version of the item, else the central one; empty when neither exists.
    version: String,
}

impl<'c, 'a> Linker<'c, 'a> {
    fn new(cx: &'c LinkCx<'a>) -> Self {
        let (mut projects, mut solutions, mut props) = (Vec::new(), Vec::new(), Vec::new());
        for f in cx.files_of(Claim::Manifest) {
            let Some(kind) = manifest_kind(f.path.file_name().as_bytes()) else {
                continue;
            };
            let m = Man {
                path: f.path,
                dir: f
                    .path
                    .parent()
                    .map(|d| d.as_bytes().to_vec())
                    .unwrap_or_default(),
                v: &f.parsed.0,
            };
            match kind {
                FileKind::Project => projects.push(m),
                FileKind::Central => props.push(m),
                k => solutions.push((m, k)),
            }
        }
        let mut exact = BTreeMap::new();
        let mut folded: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
        for (i, m) in projects.iter().enumerate() {
            exact.insert(m.path.as_bytes().to_vec(), i);
            folded
                .entry(m.path.as_bytes().to_ascii_lowercase())
                .or_default()
                .push(i);
        }
        let versions = props.iter().map(|m| package_versions(m.v)).collect();
        let builds: Vec<Man<'a>> = cx
            .files_of(Claim::EcoGlobal)
            .filter(|f| is_build_file(f.path.file_name().as_bytes()))
            .map(|f| Man {
                path: f.path,
                dir: f
                    .path
                    .parent()
                    .map(|d| d.as_bytes().to_vec())
                    .unwrap_or_default(),
                v: &f.parsed.0,
            })
            .collect();
        let build_items = builds
            .iter()
            .map(|m| {
                let mut all = Vec::new();
                if is_element(m.v) {
                    elements(m.v, &mut all);
                }
                all.retain(|e| path_item(el_name(e)).is_some());
                all
            })
            .collect();
        Self {
            cx,
            projects,
            solutions,
            props,
            versions,
            builds,
            build_items,
            scanned: Cell::new(0),
            scanned_bytes: Cell::new(0),
            exact,
            folded,
        }
    }

    fn run(self) -> Vec<Project> {
        let mut out: Vec<Project> = (0..self.projects.len()).map(|i| self.project(i)).collect();
        out.extend(self.solutions.iter().map(|(m, k)| self.solution(m, *k)));
        out
    }

    /// The project at `path`: exact match first, then a unique ASCII case-insensitive one.
    fn lookup(&self, path: &[u8]) -> Lookup {
        if let Some(&i) = self.exact.get(path) {
            return Lookup::One(i);
        }
        match self
            .folded
            .get(&path.to_ascii_lowercase())
            .map(Vec::as_slice)
        {
            Some(&[i]) => Lookup::One(i),
            Some([_, _, ..]) => Lookup::Ambiguous,
            _ => Lookup::Missing,
        }
    }

    fn key_of(&self, i: usize) -> ProjectKey {
        ProjectKey {
            eco: Ecosystem::Dotnet,
            manifest: self.projects[i].path.clone(),
        }
    }

    fn opaque(&self, i: usize, name: &[u8], why: &str) -> Project {
        self.cx.report(self.projects[i].path.clone(), why);
        let mut p = Project::new(self.key_of(i), name, ProjectKind::Package);
        p.opaque = true;
        p
    }

    fn project(&self, i: usize) -> Project {
        let m = &self.projects[i];
        let file = m.path.file_name();
        let name = split_ext(file.as_bytes()).map_or(file.as_bytes(), |(s, _)| s);
        if !is_element(m.v) {
            return self.opaque(i, name, "not an MSBuild project file");
        }
        let mut all = Vec::new();
        elements(m.v, &mut all);

        let mut p = Project::new(self.key_of(i), name, ProjectKind::Package);
        let mut scan = Scan::default();
        self.scan(i, &m.dir, &all, false, &mut scan);
        for b in at_or_above(&self.builds, &m.dir) {
            let f = &self.builds[b];
            if is_element(f.v) {
                self.scan(i, &f.dir, &self.build_items[b], true, &mut scan);
            } else {
                self.cx.report(f.path.clone(), UNREADABLE_BUILD_FILE);
                scan.gaps.insert(UNREADABLE_BUILD_FILE);
            }
        }
        scan.refs.sort_unstable();
        scan.refs.dedup();
        p.deps = scan
            .refs
            .into_iter()
            .map(|j| (self.key_of(j), DepKind::Normal))
            .collect();
        for why in &scan.gaps {
            self.cx.report(m.path.clone(), *why);
        }
        p.opaque = !scan.gaps.is_empty();
        p.extra_inputs = scan.extra.into_iter().collect();

        let ancestors = self.ancestors(&m.dir);
        let packages = self.packages(&all, &ancestors);
        p.config_fp = self.config_fp(&ancestors, &packages);
        p.external_fp = self.external_fp(m, &packages);
        p
    }

    /// Counts one build-file element or value of `bytes` attribute bytes; false
    /// once [`MAX_BUILD_SCAN_VALUES`] or [`MAX_BUILD_SCAN_BYTES`] is spent.
    fn charge(&self, bytes: usize) -> bool {
        let n = self.scanned.get().saturating_add(1);
        self.scanned.set(n);
        let b = self.scanned_bytes.get().saturating_add(bytes);
        self.scanned_bytes.set(b);
        n <= MAX_BUILD_SCAN_VALUES && b <= MAX_BUILD_SCAN_BYTES
    }

    /// Follows the path items `elems` of the file in directory `origin` for
    /// project `i`. An item of a `Directory.Build.*` file is resolved against
    /// both the file and the project directory, because MSBuild resolves item
    /// paths against either depending on the item; `Import` is relative to the
    /// file. `counted` charges the build-file scan budget for every element
    /// and value, including those with no value.
    fn scan(&self, i: usize, origin: &[u8], elems: &[&Value], counted: bool, acc: &mut Scan) {
        let proj_dir = self.projects[i].dir.as_slice();
        let both = [origin, proj_dir];
        let bases = if origin == proj_dir {
            &both[..1]
        } else {
            &both[..]
        };
        for e in elems {
            let Some(kind) = path_item(el_name(e)) else {
                continue;
            };
            let (raw, bases) = match kind {
                PathItem::Import => (attr_ci(e, "Project"), &bases[..1]),
                _ => (attr_ci(e, "Include"), bases),
            };
            if counted && !self.charge(raw.map_or(0, str::len)) {
                acc.gaps.insert(BUILD_SCAN_BUDGET);
                return;
            }
            // An SDK import is found by the SDK resolver, not by path.
            if kind == PathItem::Import && attr_ci(e, "Sdk").is_some() {
                continue;
            }
            for value in values(raw) {
                if counted && !self.charge(0) {
                    acc.gaps.insert(BUILD_SCAN_BUDGET);
                    return;
                }
                if value.len() > MAX_PATH_VALUE_BYTES {
                    acc.gaps.insert(LONG_VALUE);
                    continue;
                }
                let rs: Vec<Resolved> = bases.iter().map(|b| resolve_one(b, value)).collect();
                match kind {
                    PathItem::Reference => self.reference(i, &rs, acc),
                    PathItem::Import => self.import(i, &rs[0], acc),
                    PathItem::Other => item(proj_dir, &rs, acc),
                }
            }
        }
    }

    /// One `ProjectReference` value, resolved against each base. It must name a
    /// known project: unevaluated, wildcard, absolute, outside, missing and
    /// ambiguous values all make the project opaque. A base that is ambiguous
    /// is a gap even when another base finds a project, or the real target
    /// would be dropped.
    fn reference(&self, i: usize, rs: &[Resolved], acc: &mut Scan) {
        let (mut found, mut unevaluated, mut ambiguous) = (Vec::new(), false, false);
        for r in rs {
            match r {
                Resolved::Unknown => unevaluated = true,
                Resolved::Path(t) if has_wildcard(t) => unevaluated = true,
                Resolved::Path(t) => match self.lookup(t) {
                    Lookup::One(j) => found.push(j),
                    Lookup::Ambiguous => ambiguous = true,
                    Lookup::Missing => {}
                },
                Resolved::Outside => {}
            }
        }
        if unevaluated {
            acc.gaps.insert(UNEVALUATED_REFERENCE);
        } else if ambiguous {
            acc.gaps.insert(UNRESOLVED_REFERENCE);
        } else if found.is_empty() {
            acc.gaps
                .insert(if rs.iter().all(|r| matches!(r, Resolved::Outside)) {
                    OUTSIDE_PATH
                } else {
                    UNRESOLVED_REFERENCE
                });
        }
        acc.refs.extend(found.into_iter().filter(|&j| j != i));
    }

    /// An `Import` project. A literal path is read only when it is a
    /// `Directory.Build.*` file that applies to project `i`; any other file is
    /// never parsed, so what it references would be missed, and the project is
    /// opaque. A path that leaves the project directory is also an extra
    /// input. A wildcard, an unevaluated property, an absolute path or one that
    /// leaves the repository makes the project opaque.
    fn import(&self, i: usize, r: &Resolved, acc: &mut Scan) {
        let proj_dir = self.projects[i].dir.as_slice();
        match r {
            Resolved::Unknown => {
                acc.gaps.insert(UNEVALUATED_ITEM);
            }
            Resolved::Outside => {
                acc.gaps.insert(OUTSIDE_PATH);
            }
            Resolved::Path(p) if has_wildcard(p) => {
                acc.gaps.insert(WILDCARD_IMPORT);
            }
            Resolved::Path(p) => {
                let scanned = at_or_above(&self.builds, proj_dir)
                    .into_iter()
                    .any(|b| self.builds[b].path.as_bytes() == p.as_slice());
                if !scanned {
                    acc.gaps.insert(UNSCANNED_IMPORT);
                }
                if !inside(proj_dir, p) {
                    acc.extra.insert(escape_glob(p));
                }
            }
        }
    }

    fn solution(&self, m: &Man<'a>, kind: FileKind) -> Project {
        let paths: Vec<&str> = if kind == FileKind::Sln {
            match get(m.v, SOLUTION) {
                Some(Value::Array(a)) => a.iter().filter_map(|x| text(Some(x))).collect(),
                _ => Vec::new(),
            }
        } else {
            let mut all = Vec::new();
            elements(m.v, &mut all);
            all.into_iter()
                .filter(|e| el_name(e) == "Project")
                .filter_map(|e| attr(e, "Path"))
                .filter(|p| is_project_path(p))
                .collect()
        };
        let key = ProjectKey {
            eco: Ecosystem::Dotnet,
            manifest: m.path.clone(),
        };
        let mut p = Project::new(key, m.path.file_name().as_bytes(), ProjectKind::Solution);
        let mut members: Vec<usize> = paths
            .into_iter()
            .filter_map(|r| match resolve_one(&m.dir, r) {
                Resolved::Path(t) => match self.lookup(&t) {
                    Lookup::One(i) => Some(i),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        members.sort_unstable();
        members.dedup();
        p.deps = members
            .into_iter()
            .map(|j| (self.key_of(j), DepKind::Member))
            .collect();
        p
    }

    /// `Directory.Packages.props` files at or above `dir`, outermost first.
    fn ancestors(&self, dir: &[u8]) -> Vec<usize> {
        at_or_above(&self.props, dir)
    }

    /// The package references of a project with their versions resolved.
    fn packages(&self, all: &[&Value], ancestors: &[usize]) -> Vec<Package> {
        let mut out = Vec::new();
        for e in all.iter().filter(|e| el_name(e) == "PackageReference") {
            let Some(name) = attr(e, "Include").or(attr(e, "Update")) else {
                continue;
            };
            let version = attr(e, "VersionOverride")
                .or(attr(e, "Version"))
                .or(kid_text(e, "VersionOverride"))
                .or(kid_text(e, "Version"))
                .or_else(|| {
                    ancestors
                        .iter()
                        .rev()
                        .find_map(|&j| self.versions[j].get(name))
                        .and_then(|v| attr(v, "Version"))
                })
                .unwrap_or_default();
            out.push(Package {
                name: name.to_owned(),
                version: version.to_owned(),
            });
        }
        out
    }

    /// The non-`PackageVersion` content of every ancestor props file, plus the
    /// `PackageVersion` entries for the packages this project references.
    fn config_fp(&self, ancestors: &[usize], packages: &[Package]) -> Fingerprint {
        if ancestors.is_empty() {
            return Fingerprint::default();
        }
        let names: BTreeSet<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        let rows = ancestors
            .iter()
            .map(|&j| {
                let entries = names
                    .iter()
                    .filter_map(|n| self.versions[j].get(n))
                    .map(|e| (*e).clone())
                    .collect();
                Value::Array(vec![strip_versions(self.props[j].v), Value::Array(entries)])
            })
            .collect();
        Value::Array(rows).fingerprint()
    }

    /// The lockfile's content id, else the sorted `(name, version)` references.
    fn external_fp(&self, m: &Man<'_>, packages: &[Package]) -> Fingerprint {
        let lock_file = path_in(&m.dir, LOCKFILE)
            .and_then(|p| self.cx.file(&p))
            .filter(|f| f.eco == Ecosystem::Dotnet && f.claim == Claim::Lockfile);
        if let Some(f) = lock_file {
            if let Some(why) = lock::unusable(f.parsed) {
                self.cx.report(f.path.clone(), why);
            }
            return Fingerprint::from(f.id);
        }
        let rows: BTreeSet<(&str, &str)> = packages
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .collect();
        let rows = rows
            .into_iter()
            .map(|(n, v)| Value::Array(vec![Value::Str(n.to_owned()), Value::Str(v.to_owned())]))
            .collect();
        Value::Array(rows).fingerprint()
    }
}

/// `PackageVersion` elements of a props file by package name.
fn package_versions(root: &Value) -> BTreeMap<&str, &Value> {
    let mut all = Vec::new();
    if is_element(root) {
        elements(root, &mut all);
    }
    all.into_iter()
        .filter(|e| el_name(e) == "PackageVersion")
        .filter_map(|e| Some((attr(e, "Include").or(attr(e, "Update"))?, e)))
        .collect()
}

/// A `Compile`/`Content`/`None`/`EmbeddedResource` value resolved against each
/// base: paths that leave the project directory are extra inputs and globs stay
/// globs. An unevaluated property, or a value that is absolute or leaves the
/// repository for every base, makes the project opaque.
fn item(proj_dir: &[u8], rs: &[Resolved], acc: &mut Scan) {
    for r in rs {
        match r {
            Resolved::Unknown => {
                acc.gaps.insert(UNEVALUATED_ITEM);
            }
            Resolved::Path(p) if !inside(proj_dir, p) => {
                acc.extra.insert(p.clone());
            }
            Resolved::Path(_) | Resolved::Outside => {}
        }
    }
    if rs.iter().all(|r| matches!(r, Resolved::Outside)) {
        acc.gaps.insert(OUTSIDE_PATH);
    }
}

#[cfg(test)]
mod tests {
    use asc_core::object::Kind;

    use super::*;
    use crate::affected::{ChangeKind, ChangedPath, Query, Side, affected};
    use crate::config::ResolvedConfig;
    use crate::detect::claim_of;
    use crate::link::{LinkInput, link_graph};
    use crate::model::{ManifestSet, ProjectGraph};

    macro_rules! fixture {
        ($($p:literal),* $(,)?) => {
            [$(($p, include_str!(concat!("../../tests/fixtures/dotnet/shop/", $p)))),*]
        };
    }

    const SHOP: [(&str, &str); 11] = fixture![
        "Directory.Build.props",
        "Directory.Packages.props",
        "global.json",
        "Modern.slnx",
        "Shop.sln",
        "src/App/App.csproj",
        "src/App/packages.lock.json",
        "src/Core/Core.csproj",
        "src/Fs/Fs.fsproj",
        "src/Lib/Lib.csproj",
        "src/Weird/Weird.csproj",
    ];

    type Tree = BTreeMap<String, String>;

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn shop() -> Tree {
        SHOP.iter()
            .map(|(p, c)| ((*p).into(), (*c).into()))
            .collect()
    }

    fn edit(t: &Tree, path: &str, from: &str, to: &str) -> Tree {
        let mut t = t.clone();
        let old = &t[path];
        assert!(old.contains(from), "{path} lacks {from:?}");
        let new = old.replace(from, to);
        t.insert(path.into(), new);
        t
    }

    fn build(t: &Tree) -> (ProjectGraph, ManifestSet) {
        let dets: Vec<&dyn Detector> = vec![&Dotnet];
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
                parsed: Dotnet.parse(claim, c.as_bytes()),
            });
        }
        let exists = |d: &RepoPath| t.keys().any(|k| k.starts_with(&format!("{d}/")));
        let g = link_graph(&dets, &inputs, &[], config, &exists).unwrap();
        (g, ManifestSet::new(set).unwrap())
    }

    fn project<'g>(g: &'g ProjectGraph, name: &str) -> &'g Project {
        g.projects
            .iter()
            .find(|p| p.name == name.as_bytes())
            .unwrap_or_else(|| panic!("no project {name}: {:?}", g.display_ids()))
    }

    /// `Kind:name` of each dependency except Contains and Implicit.
    fn deps(g: &ProjectGraph, name: &str) -> Vec<String> {
        project(g, name)
            .deps
            .iter()
            .filter(|(_, k)| !matches!(k, DepKind::Contains | DepKind::Implicit))
            .map(|(k, kind)| {
                let p = g.projects.iter().find(|p| p.key == *k).unwrap();
                format!("{kind:?}:{}", p.name_lossy())
            })
            .collect()
    }

    /// Names whose fingerprint `f` differs between two graphs of the same members.
    fn changed(a: &ProjectGraph, b: &ProjectGraph, f: fn(&Project) -> Fingerprint) -> Vec<String> {
        assert_eq!(a.display_ids(), b.display_ids());
        let mut v: Vec<String> = a
            .projects
            .iter()
            .zip(&b.projects)
            .filter(|(x, y)| f(x) != f(y))
            .map(|(x, _)| x.name_lossy())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn claims_follow_the_table() {
        let c = |p: &str| Dotnet.claim(&rp(p));
        for m in [
            "a/x.csproj",
            "x.FSPROJ",
            "x.vbproj",
            "S.sln",
            "S.slnx",
            "a/Directory.Packages.props",
        ] {
            assert_eq!(c(m), Some(Claim::Manifest), "{m}");
        }
        assert_eq!(c("a/packages.lock.json"), Some(Claim::Lockfile));
        for g in [
            "Directory.Build.props",
            "x/Directory.Build.targets",
            "global.json",
            "NuGet.Config",
            "nuget.config",
            ".config/dotnet-tools.json",
        ] {
            assert_eq!(c(g), Some(Claim::EcoGlobal), "{g}");
        }
        for n in [
            "dotnet-tools.json",
            "a/.csproj",
            "x.cs",
            "Directory.Packages.props.bak",
        ] {
            assert_eq!(c(n), None, "{n}");
        }
    }

    #[test]
    fn project_references_resolve_backslashes_variables_and_case() {
        let (g, _) = build(&shop());
        // Weird (unevaluated property) and App (conditional, missing target; and an
        // Import of a file the detector does not read).
        assert_eq!(g.issues.len(), 3, "{:?}", g.issues);
        assert_eq!(deps(&g, "App"), ["Normal:Core", "Normal:Lib"]);
        assert_eq!(deps(&g, "Lib"), ["Normal:Core"]);
        assert!(deps(&g, "Core").is_empty());
        let app = project(&g, "App");
        assert_eq!(app.root_lossy(), "src/App");
        assert!(!project(&g, "Lib").opaque);
    }

    #[test]
    fn an_ambiguous_case_insensitive_match_makes_the_project_opaque() {
        let mut t = shop();
        t.insert("src/CORE/Core.csproj".into(), "<Project/>".into());
        let (g, _) = build(&t);
        assert!(deps(&g, "Lib").is_empty());
        assert!(opaque_with_issue(
            &g,
            "src/Lib/Lib.csproj",
            UNRESOLVED_REFERENCE
        ));
        // The exact spelling still resolves.
        let t = edit(
            &t,
            "src/Lib/Lib.csproj",
            "..\\core\\CORE.CSPROJ",
            "..\\Core\\Core.csproj",
        );
        let (g, _) = build(&t);
        assert_eq!(deps(&g, "Lib"), ["Normal:Core"]);
        assert!(!project(&g, "Lib").opaque);
    }

    #[test]
    fn an_unknown_variable_makes_the_project_opaque() {
        let (g, _) = build(&shop());
        assert!(project(&g, "Weird").opaque);
        assert!(
            g.issues
                .iter()
                .any(|i| i.path == rp("src/Weird/Weird.csproj"))
        );
        assert!(!project(&g, "Lib").opaque);
    }

    #[test]
    fn wildcard_references_and_unknown_imports_fail_closed() {
        let proj = |body: &str| format!("<Project Sdk=\"Microsoft.NET.Sdk\">{body}</Project>");
        let mut t = shop();
        t.insert(
            "src/Wild/Wild.csproj".into(),
            proj("<ItemGroup><ProjectReference Include=\"../**/*.csproj\" /></ItemGroup>"),
        );
        t.insert(
            "src/Imp/Imp.csproj".into(),
            proj("<Import Project=\"$(SolutionDir)build.props\" />"),
        );
        t.insert(
            "src/Item/Item.csproj".into(),
            proj("<ItemGroup><Compile Include=\"$(Shared)/**\" /></ItemGroup>"),
        );
        t.insert(
            "src/Gone/Gone.csproj".into(),
            proj("<ItemGroup><ProjectReference Include=\"../Nope/Nope.csproj\" /></ItemGroup>"),
        );
        let (g, _) = build(&t);
        for n in ["Wild", "Imp", "Item", "Gone"] {
            assert!(project(&g, n).opaque, "{n}");
            let path = rp(&format!("src/{n}/{n}.csproj"));
            assert!(g.issues.iter().any(|i| i.path == path), "{n}");
        }
    }

    fn opaque_with_issue(g: &ProjectGraph, manifest: &str, why: &str) -> bool {
        let path = rp(manifest);
        let name = path.file_name();
        let stem = split_ext(name.as_bytes()).map_or(name.as_bytes(), |(s, _)| s);
        g.projects.iter().any(|p| p.name == stem && p.opaque)
            && g.issues
                .iter()
                .any(|i| i.path == path && i.message == why.as_bytes())
    }

    fn csproj(body: &str) -> String {
        format!("<Project Sdk=\"Microsoft.NET.Sdk\">{body}</Project>")
    }

    #[test]
    fn a_wildcard_import_fails_closed() {
        let mut t = shop();
        t.insert(
            "src/Glob/Glob.csproj".into(),
            csproj("<Import Project=\"../shared/*.props\" />"),
        );
        t.insert(
            "src/Qm/Qm.csproj".into(),
            csproj("<Import Project=\"../shared/build?.props\" />"),
        );
        let (g, _) = build(&t);
        for n in ["Glob", "Qm"] {
            let m = format!("src/{n}/{n}.csproj");
            assert!(opaque_with_issue(&g, &m, WILDCARD_IMPORT), "{n}");
            // The wildcard is not kept as a literal file name that matches nothing.
            assert!(project(&g, n).extra_inputs.is_empty(), "{n}");
        }
    }

    #[test]
    fn absolute_and_escaping_paths_fail_closed() {
        let mut t = shop();
        let cases = [
            (
                "AbsRef",
                "<ProjectReference Include=\"/src/Lib/Lib.csproj\" />",
            ),
            (
                "WinRef",
                "<ProjectReference Include=\"C:\\src\\Lib\\Lib.csproj\" />",
            ),
            (
                "UpRef",
                "<ProjectReference Include=\"..\\..\\..\\Lib.csproj\" />",
            ),
            ("AbsImp", "<Import Project=\"/src/build.props\" />"),
            ("AbsItem", "<Compile Include=\"C:\\gen\\x.cs\" />"),
            ("UpItem", "<Content Include=\"../../../x.txt\" />"),
        ];
        for (n, body) in cases {
            t.insert(
                format!("src/{n}/{n}.csproj"),
                csproj(&format!("<ItemGroup>{body}</ItemGroup>")),
            );
        }
        let (g, _) = build(&t);
        for (n, _) in cases {
            let m = format!("src/{n}/{n}.csproj");
            assert!(opaque_with_issue(&g, &m, OUTSIDE_PATH), "{n}");
        }
    }

    #[test]
    fn a_literal_reference_to_no_known_project_makes_the_project_opaque() {
        let (g, _) = build(&shop());
        // App has a conditional reference to a project that does not exist.
        assert!(opaque_with_issue(
            &g,
            "src/App/App.csproj",
            UNRESOLVED_REFERENCE
        ));
        assert_eq!(deps(&g, "App"), ["Normal:Core", "Normal:Lib"]);
    }

    const GEN: &str = "tools/Gen/Gen.csproj";

    /// A tree of `Gen` and two projects `A` and `B` with one build file.
    fn with_build_file(path: &str, content: &str) -> Tree {
        let mut t = Tree::new();
        t.insert(path.into(), content.into());
        t.insert(GEN.into(), csproj(""));
        for n in ["A", "B"] {
            t.insert(format!("src/{n}/{n}.csproj"), csproj(""));
        }
        t
    }

    fn props(body: &str) -> String {
        format!("<Project>{body}</Project>")
    }

    fn affected_ids(t: &Tree, path: &str) -> Vec<String> {
        let mut head = t.clone();
        head.entry(path.into()).or_default().push(' ');
        let ((bg, bs), (hg, hs)) = (build(t), build(&head));
        let dets: Vec<&dyn Detector> = vec![&Dotnet];
        let base = Side {
            graph: &bg,
            set: &bs,
        };
        let head = Side {
            graph: &hg,
            set: &hs,
        };
        let changed = [ChangedPath {
            path: rp(path),
            kind: ChangeKind::Write,
        }];
        let a = affected(&dets, base, head, &changed, &Query::default()).unwrap();
        let mut ids: Vec<_> = a.projects.iter().map(|p| p.project.id.clone()).collect();
        ids.sort();
        ids
    }

    #[test]
    fn project_references_in_directory_build_props_are_dependencies() {
        // `$(MSBuildThisFileDirectory)` in a props file below the root.
        let t = with_build_file(
            "src/Directory.Build.props",
            &props(
                "<ItemGroup><ProjectReference Include=\"$(MSBuildThisFileDirectory)../tools/Gen/Gen.csproj\" /></ItemGroup>",
            ),
        );
        let (g, _) = build(&t);
        assert_eq!(deps(&g, "A"), ["Normal:Gen"]);
        assert_eq!(deps(&g, "B"), ["Normal:Gen"]);
        assert!(deps(&g, "Gen").is_empty());
        assert!(g.projects.iter().all(|p| !p.opaque), "{:?}", g.issues);
        assert_eq!(
            affected_ids(&t, GEN),
            ["dotnet:A", "dotnet:B", "dotnet:Gen"]
        );
        // A relative path in a root `.targets`.
        let t = with_build_file(
            "Directory.Build.targets",
            &props(
                "<ItemGroup><ProjectReference Include=\"tools\\Gen\\Gen.csproj\" /></ItemGroup>",
            ),
        );
        let (g, _) = build(&t);
        assert_eq!(deps(&g, "A"), ["Normal:Gen"]);
        assert_eq!(
            affected_ids(&t, GEN),
            ["dotnet:A", "dotnet:B", "dotnet:Gen"]
        );
    }

    #[test]
    fn imports_and_items_in_directory_build_props_are_inputs() {
        let t = with_build_file(
            "Directory.Build.props",
            &props(
                "<Import Project=\"build/common.props\" /><ItemGroup><None Include=\"docs/**\" /></ItemGroup>",
            ),
        );
        let (g, _) = build(&t);
        for n in ["A", "B", "Gen"] {
            assert_eq!(
                project(&g, n).extra_inputs,
                [b"build/common.props".to_vec(), b"docs/**".to_vec()],
                "{n}"
            );
        }
        assert_eq!(
            affected_ids(&t, "build/common.props"),
            ["dotnet:A", "dotnet:B", "dotnet:Gen"]
        );
    }

    #[test]
    fn build_file_gaps_make_the_projects_below_opaque() {
        for body in [
            "<ItemGroup><ProjectReference Include=\"$(Lib)\" /></ItemGroup>",
            "<ItemGroup><ProjectReference Include=\"tools/*/*.csproj\" /></ItemGroup>",
            "<ItemGroup><ProjectReference Include=\"tools/Nope/Nope.csproj\" /></ItemGroup>",
            "<Import Project=\"build/*.props\" />",
            "<Import Project=\"$(Shared)/common.props\" />",
            "<ItemGroup><Compile Include=\"/abs/x.cs\" /></ItemGroup>",
        ] {
            let (g, _) = build(&with_build_file("Directory.Build.props", &props(body)));
            for n in ["A", "B", "Gen"] {
                assert!(project(&g, n).opaque, "{n}: {body}");
            }
        }
    }

    #[test]
    fn an_unreadable_directory_build_file_makes_the_projects_below_opaque() {
        let big = props(&" ".repeat(MAX_MANIFEST_BYTES));
        for bad in [
            "<Project><Item></Project>",
            "not xml",
            "",
            "{\"msbuild\": true}",
            big.as_str(),
        ] {
            let (g, _) = build(&with_build_file("src/Directory.Build.props", bad));
            for n in ["A", "B"] {
                assert!(
                    project(&g, n).opaque,
                    "{n}: {:?}",
                    &bad[..bad.len().min(20)]
                );
            }
            assert!(g.issues.iter().any(|i| {
                i.path == rp("src/Directory.Build.props")
                    && i.message == UNREADABLE_BUILD_FILE.as_bytes()
            }));
            // Gen is not below the file.
            assert!(!project(&g, "Gen").opaque);
        }
    }

    #[test]
    fn build_file_values_over_the_scan_budget_fail_closed() {
        // Each project resolves 1000 values of the root props.
        let values = [GEN; 1000].join(";");
        let mut t = with_build_file(
            "Directory.Build.props",
            &props(&format!(
                "<ItemGroup><ProjectReference Include=\"{values}\" /></ItemGroup>"
            )),
        );
        let n = MAX_BUILD_SCAN_VALUES / 1000 + 100;
        for i in 0..n {
            t.insert(format!("p/P{i:05}/P{i:05}.csproj"), csproj(""));
        }
        let (g, _) = build(&t);
        let opaque = g.projects.iter().filter(|p| p.opaque).count();
        assert!(opaque > 0 && opaque < n, "{opaque} of {n}");
        assert!(
            g.issues
                .iter()
                .any(|i| i.message == BUILD_SCAN_BUDGET.as_bytes())
        );
        // Projects are scanned in path order: the first one is within budget, the last is not.
        assert_eq!(deps(&g, "P00000"), ["Normal:Gen"]);
        assert!(!project(&g, "P00000").opaque);
        assert!(project(&g, "Gen").opaque);
    }

    /// Every project of a tree with one build file is opaque.
    fn all_opaque(t: &Tree, what: &str) {
        let (g, _) = build(t);
        for n in ["A", "B", "Gen"] {
            assert!(project(&g, n).opaque, "{n}: {what}");
        }
    }

    #[test]
    fn an_import_of_a_file_the_detector_does_not_read_makes_the_project_opaque() {
        // The imported file holds a reference that would be a dependency.
        let mut t = with_build_file(
            "Directory.Build.props",
            &props("<Import Project=\"build/refs.props\" />"),
        );
        t.insert(
            "build/refs.props".into(),
            props(
                "<ItemGroup><ProjectReference Include=\"$(MSBuildThisFileDirectory)../tools/Gen/Gen.csproj\" /></ItemGroup>",
            ),
        );
        all_opaque(&t, "import in a build file");
        // The same in a project, for a file beside the project and one above it.
        for imp in ["common.props", "../shared/common.targets"] {
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert(
                "src/A/A.csproj".into(),
                csproj(&format!("<Import Project=\"{imp}\" />")),
            );
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{imp}");
            assert!(!project(&g, "B").opaque, "{imp}");
        }
        // A build file that already applies, and an SDK import, are read or external.
        let mut t = with_build_file("Directory.Build.props", &props(""));
        t.insert(
            "src/A/A.csproj".into(),
            csproj(
                "<Import Project=\"../../Directory.Build.props\" /><Import Project=\"Sdk.props\" Sdk=\"Microsoft.NET.Sdk\" />",
            ),
        );
        let (g, _) = build(&t);
        assert!(!project(&g, "A").opaque, "{:?}", g.issues);
    }

    #[test]
    fn item_and_attribute_names_are_case_insensitive() {
        let t = with_build_file(
            "Directory.Build.props",
            &props("<ItemGroup><projectreference include=\"tools/Gen/Gen.csproj\" /></ItemGroup>"),
        );
        let (g, _) = build(&t);
        assert_eq!(deps(&g, "A"), ["Normal:Gen"]);
        for body in [
            "<ItemGroup><COMPILE Include=\"/abs/x.cs\" /></ItemGroup>",
            "<ItemGroup><none include=\"C:\\x.txt\" /></ItemGroup>",
            "<ItemGroup><PROJECTREFERENCE Include=\"tools/Nope/Nope.csproj\" /></ItemGroup>",
            "<IMPORT PROJECT=\"build/*.props\" />",
        ] {
            all_opaque(
                &with_build_file("Directory.Build.props", &props(body)),
                body,
            );
        }
        // The same in a project file.
        let mut t = with_build_file("Directory.Build.props", &props(""));
        t.insert(
            "src/A/A.csproj".into(),
            csproj("<ItemGroup><projectreference Include=\"../Gone/Gone.csproj\" /></ItemGroup>"),
        );
        t.insert(
            "src/B/B.csproj".into(),
            csproj("<ItemGroup><COMPILE Include=\"../../../x.cs\" /></ItemGroup>"),
        );
        let (g, _) = build(&t);
        assert!(project(&g, "A").opaque);
        assert!(project(&g, "B").opaque);
    }

    #[test]
    fn build_file_names_apply_in_any_case() {
        for name in [
            "directory.build.props",
            "DIRECTORY.BUILD.TARGETS",
            "Directory.build.Props",
        ] {
            assert_eq!(Dotnet.claim(&rp(name)), Some(Claim::EcoGlobal), "{name}");
            let t = with_build_file(
                &format!("src/{name}"),
                &props("<ItemGroup><ProjectReference Include=\"$(Lib)\" /></ItemGroup>"),
            );
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{name}");
            assert!(!project(&g, "Gen").opaque, "{name}");
        }
    }

    #[test]
    fn percent_escapes_make_the_path_unevaluated() {
        let mut t = with_build_file("Directory.Build.props", &props(""));
        let cases = [
            (
                "Imp",
                "<Import Project=\"%2E%2E/%2E%2E/shared/common.props\" />",
            ),
            (
                "Item",
                "<ItemGroup><Compile Include=\"%2E%2E/shared/*.cs\" /></ItemGroup>",
            ),
            (
                "Ref",
                "<ItemGroup><ProjectReference Include=\"%2E%2E/Gen/Gen.csproj\" /></ItemGroup>",
            ),
        ];
        for (n, body) in cases {
            t.insert(format!("src/{n}/{n}.csproj"), csproj(body));
        }
        let (g, _) = build(&t);
        for (n, _) in cases {
            assert!(project(&g, n).opaque, "{n}");
        }
        assert!(!project(&g, "A").opaque);
    }

    #[test]
    fn an_ambiguous_project_directory_match_is_not_hidden_by_the_file_directory() {
        // From src/A the reference is src/tools/Gen/Gen.csproj, which two
        // case variants match; from the build file's directory it is the real Gen.
        let mut t = with_build_file(
            "src/Directory.Build.props",
            &props(
                "<ItemGroup><ProjectReference Include=\"../tools/Gen/Gen.csproj\" /></ItemGroup>",
            ),
        );
        t.insert("src/tools/gen/Gen.csproj".into(), csproj(""));
        t.insert("src/tools/GEN/Gen.csproj".into(), csproj(""));
        let (g, _) = build(&t);
        assert!(project(&g, "A").opaque);
        assert!(project(&g, "B").opaque);
    }

    #[test]
    fn oversized_and_unbudgeted_build_file_content_fails_closed() {
        // One value past the per-value cap.
        let long = "a".repeat(MAX_PATH_VALUE_BYTES + 1);
        let t = with_build_file(
            "Directory.Build.props",
            &props(&format!(
                "<ItemGroup><None Include=\"shared/{long}\" /></ItemGroup>"
            )),
        );
        let (g, _) = build(&t);
        for n in ["A", "B", "Gen"] {
            assert!(project(&g, n).opaque, "{n}");
            assert!(
                project(&g, n)
                    .extra_inputs
                    .iter()
                    .all(|e| e.len() <= MAX_PATH_VALUE_BYTES),
                "{n}"
            );
        }
        // Elements without a value are charged.
        let empty = "<None />".repeat(MAX_BUILD_SCAN_VALUES / 100);
        let mut t = with_build_file(
            "Directory.Build.props",
            &props(&format!("<ItemGroup>{empty}</ItemGroup>")),
        );
        for i in 0..200 {
            t.insert(format!("p/P{i:03}/P{i:03}.csproj"), csproj(""));
        }
        let (g, _) = build(&t);
        let opaque = g.projects.iter().filter(|p| p.opaque).count();
        assert!(opaque > 0 && opaque < g.projects.len(), "{opaque}");
        // Empty values in one attribute are charged by their bytes.
        let filler = ";".repeat(MAX_BUILD_SCAN_BYTES / 32);
        let mut t = with_build_file(
            "Directory.Build.props",
            &props(&format!(
                "<ItemGroup><None Include=\"{filler}\" /></ItemGroup>"
            )),
        );
        for i in 0..60 {
            t.insert(format!("p/P{i:03}/P{i:03}.csproj"), csproj(""));
        }
        let (g, _) = build(&t);
        let opaque = g.projects.iter().filter(|p| p.opaque).count();
        assert!(opaque > 0 && opaque < g.projects.len(), "{opaque}");
        // Many maximal values are charged by their bytes.
        let v = format!("shared/{}", "a".repeat(MAX_PATH_VALUE_BYTES - 7));
        let list = vec![v; 256].join(";");
        let mut t = with_build_file(
            "Directory.Build.props",
            &props(&format!(
                "<ItemGroup><None Include=\"{list}\" /></ItemGroup>"
            )),
        );
        for i in 0..(MAX_BUILD_SCAN_BYTES / (256 * MAX_PATH_VALUE_BYTES) + 8) {
            t.insert(format!("p/P{i:03}/P{i:03}.csproj"), csproj(""));
        }
        let (g, _) = build(&t);
        let opaque = g.projects.iter().filter(|p| p.opaque).count();
        assert!(opaque > 0 && opaque < g.projects.len(), "{opaque}");
    }

    #[test]
    fn solutions_list_members_and_unlisted_projects_stay() {
        let (g, _) = build(&shop());
        assert_eq!(project(&g, "Shop.sln").kind, ProjectKind::Solution);
        assert_eq!(deps(&g, "Shop.sln"), ["Member:App", "Member:Lib"]);
        assert_eq!(deps(&g, "Modern.slnx"), ["Member:Core", "Member:Fs"]);
        // Weird is in no solution but is still a project.
        project(&g, "Weird");
    }

    #[test]
    fn extra_inputs_leave_the_project_directory() {
        let (g, _) = build(&shop());
        assert_eq!(
            project(&g, "App").extra_inputs,
            [b"build/app.targets".to_vec(), b"src/Shared/*.cs".to_vec()]
        );
        assert!(project(&g, "Lib").extra_inputs.is_empty());
    }

    #[test]
    fn a_central_version_bump_affects_only_its_referencing_projects() {
        let (before, _) = build(&shop());
        let t = edit(&shop(), "Directory.Packages.props", "13.0.3", "13.0.4");
        let (after, _) = build(&t);
        // Fs references Newtonsoft.Json and has no lockfile: both fingerprints move.
        // App references it too, but its unchanged lockfile stands for the closure.
        assert_eq!(changed(&before, &after, |p| p.config_fp), ["App", "Fs"]);
        assert_eq!(changed(&before, &after, |p| p.external_fp), ["Fs"]);
    }

    #[test]
    fn item_versions_beat_central_ones() {
        let (before, _) = build(&shop());
        let t = edit(&shop(), "Directory.Packages.props", "3.1.1", "3.2.0");
        let (after, _) = build(&t);
        // Lib overrides the Serilog version and Fs sets its own `<Version>`.
        assert!(changed(&before, &after, |p| p.external_fp).is_empty());
        // Their config still records the central entry they reference.
        assert_eq!(changed(&before, &after, |p| p.config_fp), ["Fs", "Lib"]);
    }

    #[test]
    fn other_props_content_changes_every_project_under_it() {
        let (before, _) = build(&shop());
        let t = edit(
            &shop(),
            "Directory.Packages.props",
            "<ManagePackageVersionsCentrally>true",
            "<ManagePackageVersionsCentrally>false",
        );
        let (after, _) = build(&t);
        let hit = changed(&before, &after, |p| p.config_fp);
        assert_eq!(hit, ["App", "Core", "Fs", "Lib", "Weird"]);
        assert!(changed(&before, &after, |p| p.external_fp).is_empty());
    }

    #[test]
    fn the_lockfile_id_is_the_external_fingerprint() {
        let (before, _) = build(&shop());
        let t = edit(&shop(), "src/App/packages.lock.json", "HrC5BXdl", "Other");
        let (after, _) = build(&t);
        assert_eq!(changed(&before, &after, |p| p.external_fp), ["App"]);
        assert!(changed(&before, &after, |p| p.config_fp).is_empty());
    }

    #[test]
    fn an_unusable_lockfile_still_stands_for_the_closure() {
        let t = edit(
            &shop(),
            "src/App/packages.lock.json",
            "\"dependencies\"",
            "\"deps\"",
        );
        let (g, _) = build(&t);
        assert!(
            g.issues
                .iter()
                .any(|i| i.path == rp("src/App/packages.lock.json"))
        );
        assert_ne!(project(&g, "App").external_fp, Fingerprint::default());
    }

    #[test]
    fn a_build_props_edit_affects_every_dotnet_project_below_it() {
        let (g, set) = build(&shop());
        let dets: Vec<&dyn Detector> = vec![&Dotnet];
        let side = Side {
            graph: &g,
            set: &set,
        };
        let changed = [ChangedPath {
            path: rp("Directory.Build.props"),
            kind: ChangeKind::Write,
        }];
        let a = affected(&dets, side, side, &changed, &Query::default()).unwrap();
        let mut hit: Vec<_> = a.projects.iter().map(|p| p.project.id.clone()).collect();
        hit.sort();
        assert_eq!(hit.len(), 7, "{hit:?}");
        assert!(hit.contains(&"dotnet:Shop.sln".to_owned()), "{hit:?}");
    }

    #[test]
    fn malformed_manifests_fail_to_parse() {
        assert!(
            Dotnet
                .parse(Claim::Manifest, b"<Project><Item></Project>")
                .is_err()
        );
        assert!(Dotnet.parse(Claim::Manifest, b"hello").is_err());
        assert!(Dotnet.parse(Claim::Manifest, &[0xff, 0xfe]).is_err());
        let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert!(Dotnet.parse(Claim::Manifest, &big).is_err());
    }
}
