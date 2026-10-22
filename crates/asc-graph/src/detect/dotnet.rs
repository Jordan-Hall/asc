//! The .NET detector (SPEC §12A.2.3, §12A.4).
//!
//! Project files and `Directory.Packages.props` are parsed to canonical XML
//! values, solutions to their project paths. `link` reads only the manifest
//! set. `Condition` attributes are ignored, so every item counts. The items of
//! every `Directory.Build.props`/`.targets` above a project count as its own,
//! as do those of each `Directory.Packages.props` above it and of the files
//! MSBuild imports by name (`<project>.user`, `obj/<project>.*.props`).
//! Extra-input globs fold ASCII case and escape glob syntax MSBuild lacks.
//! Item and attribute names match in any case, `%` escapes are not evaluated,
//! and an `Import` of a file the detector does not read is a gap.
//! `Import` fails closed: its `Project` may use only the two directory
//! properties, and an `Sdk` import needs a plain literal `Sdk` and `Project`;
//! any other property (defined in a scanned file, a command line or a response
//! file, none of which is evaluated) makes the project opaque. The SDK itself
//! imports files by property, out of sight, so a property that chooses one
//! (`CustomAfterMicrosoftCommonTargets`, `DirectoryBuildTargetsPath`,
//! `BaseIntermediateOutputPath`, ...) is also a gap. `Directory.Build.rsp` and
//! `MSBuild.rsp` set properties with no XML. MSBuild reads one from beside or
//! above the project or solution it is asked to build, and its `-p:` values
//! become global properties of every project that entry references, wherever
//! those are. So one anywhere in the repository makes every project and
//! solution opaque.
//! Build files are found by directory in any ASCII case. Where a file system
//! could join a non-ASCII spelling of a directory or name with another (case
//! folding, normalisation, ignored characters), the project is opaque; any
//! non-ASCII character outside a few scripts that never fold counts.
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
/// Most directory and name comparisons [`Linker::ambiguous`] makes in one
/// link, summed over all projects. Past it the remaining projects are opaque.
const MAX_ALIAS_CHECKS: usize = 1 << 22;

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
const REDIRECTED_IMPORT: &str =
    "a property chooses a file MSBuild imports, which the detector does not read";
const LONG_VALUE: &str = "reference, Import or item path is longer than the limit";
const RESPONSE_FILE: &str =
    "an MSBuild response file in the repository can set properties the detector does not read";
const NON_LITERAL_SDK: &str = "Import Sdk or its Project is not a plain literal";
const NON_ASCII_DIR: &str =
    "a file that applies to the project may name its directory by another non-ASCII spelling";

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
    // MSBuild finds it by a file system lookup, which ignores case on Windows and macOS.
    if name_may_be(name, CENTRAL) {
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
            n if is_build_file(n) || is_auto_import(path) => Some(Claim::EcoGlobal),
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
///
/// The response files `Directory.Build.rsp` and `MSBuild.rsp` count as build
/// files, so they are found by directory like the others: their `-p:` switches
/// set properties without any XML. Names that only a non-ASCII spelling
/// separates from these (see [`name_may_be`]) count too.
fn is_build_file(name: &[u8]) -> bool {
    name_may_be(name, b"Directory.Build.props")
        || name_may_be(name, b"Directory.Build.targets")
        || is_response_file(name)
}

/// `Directory.Build.rsp` or `MSBuild.rsp`, which MSBuild reads from the
/// project's directory or one above it.
fn is_response_file(name: &[u8]) -> bool {
    name_may_be(name, b"Directory.Build.rsp") || name_may_be(name, b"MSBuild.rsp")
}

/// Most ASCII letters one non-ASCII character may stand for when a file system
/// normalises or folds names (`ß` is `ss`, `ﬃ` is `ffi`, `İ` is `i` plus a mark).
const MAX_FOLD_EXPANSION: usize = 3;

/// A character of a script that no file system folds to an ASCII letter and
/// ignores in a name: Greek (not the question mark `;`), Cyrillic, Hebrew
/// letters, Hiragana, Katakana, CJK ideographs and Hangul syllables. Every other
/// non-ASCII character is unknown: it may fold to ASCII letters (Latin letters
/// and marks, the Kelvin sign, ligatures, full-width forms) or be ignored when
/// names are compared (HFS+ skips U+200C-200F, U+202A-202E, U+206A-206F and
/// U+FEFF; other file systems skip more).
fn known_script(c: char) -> bool {
    matches!(u32::from(c),
        0x370..=0x37D | 0x37F..=0x3FF | 0x400..=0x52F | 0x5D0..=0x5EA
        | 0x3041..=0x3096 | 0x30A1..=0x30FA | 0x4E00..=0x9FFF | 0xAC00..=0xD7A3)
}

/// Whether the non-ASCII name `wide` has a character that a file system may
/// fold to ASCII or ignore, or is not UTF-8.
fn may_fold_to_ascii(wide: &[u8]) -> bool {
    match std::str::from_utf8(wide) {
        Ok(w) => w.chars().any(|c| !c.is_ascii() && !known_script(c)),
        Err(_) => true,
    }
}

/// Whether the non-ASCII name `wide` may name the same file as the ASCII name
/// `plain` on a file system that folds case, normalises Unicode or ignores
/// some characters. The ASCII letters of `wide` must be among those of `plain`,
/// and the rest must be few enough for its non-ASCII characters to stand for.
/// A name that is not UTF-8 or has a character [`may_fold_to_ascii`] is judged
/// this way; others cannot become ASCII.
fn ascii_reachable(wide: &[u8], plain: &[u8]) -> bool {
    let Ok(wide) = std::str::from_utf8(wide) else {
        return true;
    };
    let kept: Vec<u8> = wide
        .bytes()
        .filter(u8::is_ascii)
        .map(|b| b.to_ascii_lowercase())
        .collect();
    let count = wide.chars().filter(|c| !c.is_ascii()).count();
    let plain = plain.to_ascii_lowercase();
    if !may_fold_to_ascii(wide.as_bytes())
        || plain.len() < kept.len()
        || plain.len() - kept.len() > MAX_FOLD_EXPANSION * count
    {
        return false;
    }
    let mut rest = plain.iter();
    kept.iter().all(|k| rest.any(|p| p == k))
}

/// Whether `name` is the ASCII file name `pat`, or may be on a file system that
/// folds case or normalises Unicode: ASCII case is folded, and a non-ASCII
/// spelling counts when [`ascii_reachable`].
fn name_may_be(name: &[u8], pat: &[u8]) -> bool {
    name.eq_ignore_ascii_case(pat) || (!name.is_ascii() && ascii_reachable(name, pat))
}

/// Whether two names of one path component may be one name: ASCII case is
/// folded; a non-ASCII name may be any other non-ASCII name (composed or
/// decomposed, folded) or an ASCII one it can reduce to ([`ascii_reachable`]).
fn may_alias(a: &[u8], b: &[u8]) -> bool {
    match (a.is_ascii(), b.is_ascii()) {
        (true, true) => a.eq_ignore_ascii_case(b),
        (false, false) => true,
        (false, true) => ascii_reachable(a, b),
        (true, false) => ascii_reachable(b, a),
    }
}

/// How a file's directory `f` relates to a project directory `p`.
#[derive(PartialEq, Eq)]
enum Rel {
    /// Different directories, whatever the file system does.
    Apart,
    /// The same directory, or an ancestor of it, up to ASCII case.
    Exact,
    /// Not the same by bytes, but a file system may treat it so.
    Maybe,
}

fn components(dir: &[u8]) -> impl Iterator<Item = &[u8]> {
    dir.split(|&c| c == b'/').filter(|_| !dir.is_empty())
}

/// `f` against `p` or, unless `same`, any ancestor of `p`.
fn dir_rel(f: &[u8], p: &[u8], same: bool) -> Rel {
    let (nf, np) = (components(f).count(), components(p).count());
    if nf > np || (same && nf != np) {
        return Rel::Apart;
    }
    let mut rel = Rel::Exact;
    for (a, b) in components(f).zip(components(p)) {
        if a.eq_ignore_ascii_case(b) {
            continue;
        }
        if !may_alias(a, b) {
            return Rel::Apart;
        }
        rel = Rel::Maybe;
    }
    rel
}

fn ends_with_ci(name: &[u8], suffix: &[u8]) -> bool {
    name.len() >= suffix.len() && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn starts_with_ci(name: &[u8], prefix: &[u8]) -> bool {
    name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// A file MSBuild imports for a project without any `Import` in a project
/// file: `<project file>.user` beside it, and `<project file>.*.props` and
/// `.targets` in its `obj` directory (`Microsoft.Common.CurrentVersion.targets`,
/// `Microsoft.Common.props` and `Microsoft.Common.targets`). `proj_file` is the
/// project's file name; `in_obj` says the file is in the `obj` directory.
fn is_auto_import_of(name: &[u8], proj_file: &[u8], in_obj: bool) -> bool {
    let rest = |suffix: &[u8]| {
        // `.user` follows the file name; the others have a `.` and a name between.
        let min = proj_file.len()
            + if in_obj {
                1 + suffix.len()
            } else {
                suffix.len()
            };
        name.len() >= min && starts_with_ci(name, proj_file) && ends_with_ci(name, suffix)
    };
    if in_obj {
        (rest(b".props") || rest(b".targets")) && name.get(proj_file.len()) == Some(&b'.')
    } else {
        name.len() == proj_file.len() + 5 && rest(b".user")
    }
}

/// Whether a non-ASCII file name may fold to an auto-import name. A file
/// system may change the non-ASCII characters (see [`may_fold_to_ascii`]), so
/// the name is judged by its end alone: the ASCII text after its last non-ASCII
/// byte must be the end of `.user`, `.props` or `.targets`, or contain it.
/// Which project and directory it belongs to is left to [`Linker::ambiguous`].
fn wide_auto_name(name: &[u8]) -> bool {
    let from = name
        .iter()
        .rposition(|b| !b.is_ascii())
        .map_or(0, |i| i + 1);
    let tail = &name[from..];
    may_fold_to_ascii(name)
        && [b"user".as_slice(), b"props", b"targets"]
            .iter()
            .any(|s| ends_with_ci(tail, s) || ends_with_ci(s, tail))
}

/// True for a path that may be an auto-imported file of some project.
fn is_auto_import(path: &RepoPath) -> bool {
    let name = path.file_name();
    let name = name.as_bytes();
    if !name.is_ascii() {
        return wide_auto_name(name);
    }
    if ends_with_ci(name, b".user") {
        return split_ext(&name[..name.len() - 5])
            .is_some_and(|(_, e)| PROJECT_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)));
    }
    (ends_with_ci(name, b".props") || ends_with_ci(name, b".targets"))
        && path
            .parent()
            .is_some_and(|p| name_may_be(p.file_name().as_bytes(), b"obj"))
        && PROJECT_EXTS.iter().any(|e| {
            let mut pat = vec![b'.'];
            pat.extend_from_slice(e);
            pat.push(b'.');
            name.windows(pat.len())
                .any(|w| w.eq_ignore_ascii_case(&pat))
        })
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

/// The glob that selects the files an MSBuild path names, for `extra_inputs`.
/// The matcher is case-sensitive and reads `{}`, `[]` and `\`, none of which
/// MSBuild reads, so:
/// - ASCII letters become `[xX]`, because MSBuild resolves paths by a file
///   system lookup that ignores case on Windows and macOS;
/// - a non-ASCII character becomes `*`, since its case folding is not decided
///   here; the match is wider, never narrower;
/// - `{`, `}`, `[`, `]` and `\` are escaped, so no glob is invalid or expands;
/// - with `wildcards`, `*` and `?` stay wildcards (`?` as `*`, because the
///   matcher's `?` reads one byte, not one character) and a run of them is one
///   `*`, since `**` is only valid as a whole component. Otherwise they are
///   escaped.
fn fs_glob(p: &[u8], wildcards: bool) -> Vec<u8> {
    let text = String::from_utf8_lossy(p);
    let comps: Vec<String> = text
        .split('/')
        .map(|c| {
            if wildcards && c == "**" {
                return c.to_owned();
            }
            let mut o = String::with_capacity(c.len() * 2);
            let mut star = false;
            for ch in c.chars() {
                let wide = !ch.is_ascii() || (wildcards && matches!(ch, '*' | '?'));
                if wide {
                    if !star {
                        o.push('*');
                    }
                    star = true;
                    continue;
                }
                star = false;
                if ch.is_ascii_alphabetic() {
                    o.extend(['[', ch.to_ascii_lowercase(), ch.to_ascii_uppercase(), ']']);
                } else {
                    if matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
                        o.push('\\');
                    }
                    o.push(ch);
                }
            }
            o
        })
        .collect();
    comps.join("/").into_bytes()
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

/// An `Import` `Sdk` name, or the `Project` under it, that is a plain literal:
/// letters, digits and `. _ - /` only, not rooted and with no `..`. Anything
/// else (a property, item or escape, a wildcard, `;`, a rooted path) could
/// resolve to a file the detector does not read.
fn plain_sdk(s: &str) -> bool {
    !s.starts_with('/')
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
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
    /// A property that chooses a file MSBuild imports; see [`redirects_import`].
    Redirect,
}

/// Properties that MSBuild or the .NET SDK read to choose an imported file, or
/// the `obj` directory its auto-imports come from. The names are those used in
/// the `Import` elements of SDK 9.0.319 plus the properties behind
/// `MSBuildProjectExtensionsPath`. Setting one in a project or an applied build
/// file moves an import out of sight (no `Import` element), so it is a gap.
/// Names match in any case, because MSBuild property names do.
fn redirects_import(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const EXACT: [&str; 6] = [
        "baseintermediateoutputpath",
        "intermediateoutputpath",
        "useartifactsoutput",
        "usebundledfsharptargets",
        "visualstudioversion",
        "iscrosstargetingbuild",
    ];
    const PREFIX: [&str; 11] = [
        "_",
        "msbuild",
        "custombefore",
        "customafter",
        "directorybuild",
        "directorypackages",
        "importdirectory",
        "importbywildcard",
        "artifacts",
        "publishprofile",
        "webpublishprofile",
    ];
    const SUFFIX: [&str; 8] = [
        "targets",
        "props",
        "targetspath",
        "propspath",
        "targetsfile",
        "propsfile",
        "shim",
        "targetsdir",
    ];
    EXACT.contains(&n.as_str())
        || PREFIX.iter().any(|p| n.starts_with(p))
        || SUFFIX.iter().any(|s| n.ends_with(s))
}

/// The kind of an element whose paths the detector follows. Names match in any
/// case, because MSBuild item types do.
fn path_item(name: &str) -> Option<PathItem> {
    let is = |n: &str| name.eq_ignore_ascii_case(n);
    if redirects_import(name) {
        Some(PathItem::Redirect)
    } else if is("ProjectReference") {
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
    props_ix: ByDir,
    /// Per `props` entry: its path items; empty when it is not an element.
    props_items: Vec<Vec<&'a Value>>,
    /// Directories of `Directory.Packages.props` files that failed to parse.
    failed_props: Vec<&'a RepoPath>,
    failed_props_ix: ByDir,
    /// Per `props` entry: `PackageVersion` elements by package name.
    versions: Vec<BTreeMap<&'a str, &'a Value>>,
    /// The `Directory.Build.props`/`.targets` files.
    builds: Vec<Man<'a>>,
    builds_ix: ByDir,
    /// The first `Directory.Build.rsp`/`MSBuild.rsp` in the repository, if any.
    response: Option<&'a RepoPath>,
    /// Per `builds` entry: its path items; empty when it is not an element.
    build_items: Vec<Vec<&'a Value>>,
    /// `<project>.user` and `obj/<project>.*.props`/`.targets` files.
    autos: Vec<Man<'a>>,
    /// Per `autos` entry: its path items; empty when it is not an element.
    auto_items: Vec<Vec<&'a Value>>,
    /// `autos` by lower-cased directory.
    autos_by_dir: BTreeMap<Vec<u8>, Vec<usize>>,
    /// Build-file values resolved so far, against [`MAX_BUILD_SCAN_VALUES`].
    scanned: Cell<usize>,
    /// Build-file attribute bytes walked so far, against [`MAX_BUILD_SCAN_BYTES`].
    scanned_bytes: Cell<usize>,
    /// Directories of the build, `Directory.Packages.props` and unreadable
    /// `Directory.Packages.props` files, once each.
    ancestor_dirs: Vec<Vec<u8>>,
    /// The non-ASCII ones among `ancestor_dirs`.
    wide_ancestor_dirs: Vec<Vec<u8>>,
    /// The `autos` whose directory or file name is not ASCII.
    wide_autos: Vec<usize>,
    /// Directory and name comparisons made by [`Self::ambiguous`], against
    /// [`MAX_ALIAS_CHECKS`].
    alias_checks: Cell<usize>,
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

/// A directory (bytes) as a key: ASCII letters fold to lower case and every
/// other byte is exact. Two directories with one key are the same directory on
/// any file system. Non-ASCII spellings that only a file system would join
/// (case, composed against decomposed) get different keys here, so they are
/// never matched by key: [`Linker::ambiguous`] makes the project opaque instead.
fn fold_dir(dir: &[u8]) -> Vec<u8> {
    dir.to_ascii_lowercase()
}

/// File indices by directory (bytes, empty is the root, compared by
/// [`fold_dir`]): finds the files at or above a directory by walking its
/// parents, not by scanning every file. MSBuild finds `Directory.Build.props`
/// by a file system lookup, which ignores ASCII case on Windows and macOS.
#[derive(Default)]
struct ByDir(BTreeMap<Vec<u8>, Vec<usize>>);

impl ByDir {
    fn new<'d>(dirs: impl Iterator<Item = &'d [u8]>) -> Self {
        let mut m: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
        for (i, d) in dirs.enumerate() {
            m.entry(fold_dir(d)).or_default().push(i);
        }
        Self(m)
    }

    /// Indices of the entries at or above `dir`, outermost first.
    fn at_or_above(&self, dir: &[u8]) -> Vec<usize> {
        #[cfg(test)]
        tests::LOOKUPS.with(|c| c.set(c.get() + 1));
        let dir = fold_dir(dir);
        let dir = dir.as_slice();
        let mut v = Vec::new();
        let ends = std::iter::once(0)
            .chain(
                dir.iter()
                    .enumerate()
                    .filter(|(_, c)| **c == b'/')
                    .map(|(i, _)| i),
            )
            .chain((!dir.is_empty()).then_some(dir.len()));
        for end in ends {
            if let Some(x) = self.0.get(&dir[..end]) {
                v.extend_from_slice(x);
            }
        }
        v
    }
}

/// The path items of an MSBuild file; empty when it is not an element.
fn path_items(root: &Value) -> Vec<&Value> {
    let mut all = Vec::new();
    if is_element(root) {
        elements(root, &mut all);
    }
    all.retain(|e| path_item(el_name(e)).is_some());
    all
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
        let eco_global = |keep: fn(&RepoPath) -> bool| -> Vec<Man<'a>> {
            cx.files_of(Claim::EcoGlobal)
                .filter(|f| keep(f.path))
                .map(|f| Man {
                    path: f.path,
                    dir: f
                        .path
                        .parent()
                        .map(|d| d.as_bytes().to_vec())
                        .unwrap_or_default(),
                    v: &f.parsed.0,
                })
                .collect()
        };
        let builds = eco_global(|p| is_build_file(p.file_name().as_bytes()));
        let response = builds
            .iter()
            .map(|m| m.path)
            .find(|p| is_response_file(p.file_name().as_bytes()));
        let autos = eco_global(|p| !is_build_file(p.file_name().as_bytes()) && is_auto_import(p));
        let mut autos_by_dir: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
        for (j, m) in autos.iter().enumerate() {
            autos_by_dir.entry(fold_dir(&m.dir)).or_default().push(j);
        }
        let failed_props: Vec<&'a RepoPath> = cx
            .failed_manifests()
            .iter()
            .copied()
            .filter(|p| manifest_kind(p.file_name().as_bytes()) == Some(FileKind::Central))
            .collect();
        let failed_dirs: Vec<Vec<u8>> = failed_props
            .iter()
            .map(|p| {
                p.parent()
                    .map(|d| d.as_bytes().to_vec())
                    .unwrap_or_default()
            })
            .collect();
        let ancestor_dirs: BTreeSet<&[u8]> = builds
            .iter()
            .chain(&props)
            .map(|m| m.dir.as_slice())
            .chain(failed_dirs.iter().map(Vec::as_slice))
            .collect();
        let ancestor_dirs: Vec<Vec<u8>> = ancestor_dirs.into_iter().map(<[u8]>::to_vec).collect();
        let wide_ancestor_dirs = ancestor_dirs
            .iter()
            .filter(|d| !d.is_ascii())
            .cloned()
            .collect();
        let wide_autos = autos
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.dir.is_ascii() || !m.path.file_name().as_bytes().is_ascii())
            .map(|(j, _)| j)
            .collect();
        Self {
            cx,
            ancestor_dirs,
            wide_ancestor_dirs,
            wide_autos,
            alias_checks: Cell::new(0),
            projects,
            solutions,
            props_ix: ByDir::new(props.iter().map(|m| m.dir.as_slice())),
            props_items: props.iter().map(|m| path_items(m.v)).collect(),
            props,
            failed_props,
            failed_props_ix: ByDir::new(failed_dirs.iter().map(Vec::as_slice)),
            versions,
            builds_ix: ByDir::new(builds.iter().map(|m| m.dir.as_slice())),
            response,
            build_items: builds.iter().map(|m| path_items(m.v)).collect(),
            builds,
            auto_items: autos.iter().map(|m| path_items(m.v)).collect(),
            autos,
            autos_by_dir,
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
        // The files MSBuild imports for this project without an `Import`.
        let builds = self.builds_ix.at_or_above(&m.dir);
        let ancestors = self.ancestors(&m.dir);
        let autos = self.autos_of(i);
        let applied: Vec<&[u8]> = builds
            .iter()
            .map(|&b| self.builds[b].path.as_bytes())
            .chain(ancestors.iter().map(|&j| self.props[j].path.as_bytes()))
            .chain(autos.iter().map(|&a| self.autos[a].path.as_bytes()))
            .collect();
        self.scan(i, &m.dir, &all, false, &applied, &mut scan);
        let files = builds
            .iter()
            .map(|&b| (&self.builds[b], &self.build_items[b]))
            .chain(
                ancestors
                    .iter()
                    .map(|&j| (&self.props[j], &self.props_items[j])),
            )
            .chain(autos.iter().map(|&a| (&self.autos[a], &self.auto_items[a])));
        for (f, items) in files {
            if is_response_file(f.path.file_name().as_bytes()) {
                continue; // Makes every project opaque, below.
            }
            if is_element(f.v) {
                self.scan(i, &f.dir, items, true, &applied, &mut scan);
            } else {
                self.cx.report(f.path.clone(), UNREADABLE_BUILD_FILE);
                scan.gaps.insert(UNREADABLE_BUILD_FILE);
            }
        }
        for &a in &autos {
            scan.extra
                .insert(fs_glob(self.autos[a].path.as_bytes(), false));
        }
        for f in self.failed_props_ix.at_or_above(&m.dir) {
            self.cx
                .report(self.failed_props[f].clone(), UNREADABLE_BUILD_FILE);
            scan.gaps.insert(UNREADABLE_BUILD_FILE);
        }
        if self.ambiguous(i) {
            scan.gaps.insert(NON_ASCII_DIR);
        }
        if let Some(f) = self.response {
            self.cx.report(f.clone(), RESPONSE_FILE);
            scan.gaps.insert(RESPONSE_FILE);
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

        let packages = self.packages(&all, &ancestors);
        p.config_fp = self.config_fp(&ancestors, &packages);
        p.external_fp = self.external_fp(m, &packages);
        p
    }

    /// Whether a file that applies to project `i` may be missed because a file
    /// system could join a non-ASCII spelling of a directory or name with
    /// another by case folding or normalisation, which byte keys do not decide.
    /// Only the build, `Directory.Packages.props` and auto-imported files are
    /// checked. A directory pair is ambiguous when a file's directory is not an
    /// ancestor of the project's by ASCII-folded bytes but may be one
    /// ([`dir_rel`]); an auto-imported file when its directory matches and a
    /// non-ASCII name may be the project's. Pure-ASCII pairs never are.
    /// Past [`MAX_ALIAS_CHECKS`] every project is ambiguous.
    fn ambiguous(&self, i: usize) -> bool {
        let m = &self.projects[i];
        let file = m.path.file_name();
        let file = file.as_bytes();
        let (wide_dir, wide_file) = (!m.dir.is_ascii(), !file.is_ascii());
        let spend = || {
            let n = self.alias_checks.get().saturating_add(1);
            self.alias_checks.set(n);
            n <= MAX_ALIAS_CHECKS
        };
        // A pure-ASCII project can only meet a non-ASCII directory.
        let dirs = if wide_dir {
            &self.ancestor_dirs
        } else {
            &self.wide_ancestor_dirs
        };
        for d in dirs {
            if !spend() || dir_rel(d, &m.dir, false) == Rel::Maybe {
                return true;
            }
        }
        let mut obj = m.dir.clone();
        if !obj.is_empty() {
            obj.push(b'/');
        }
        obj.extend_from_slice(b"obj");
        let every = wide_dir || wide_file;
        let autos: Box<dyn Iterator<Item = usize> + '_> = if every {
            Box::new(0..self.autos.len())
        } else {
            Box::new(self.wide_autos.iter().copied())
        };
        for j in autos {
            let a = &self.autos[j];
            let name = a.path.file_name();
            let name = name.as_bytes();
            for (dir, in_obj) in [(&m.dir, false), (&obj, true)] {
                if !spend() {
                    return true;
                }
                let rel = dir_rel(&a.dir, dir, true);
                if rel == Rel::Apart {
                    continue;
                }
                let named = is_auto_import_of(name, file, in_obj);
                let maybe = !named && (wide_file || !name.is_ascii());
                if (rel == Rel::Maybe && (named || maybe)) || (rel == Rel::Exact && maybe) {
                    return true;
                }
            }
        }
        false
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
    fn scan(
        &self,
        i: usize,
        origin: &[u8],
        elems: &[&Value],
        counted: bool,
        applied: &[&[u8]],
        acc: &mut Scan,
    ) {
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
                PathItem::Redirect => (None, bases),
                _ => (attr_ci(e, "Include"), bases),
            };
            if counted && !self.charge(raw.map_or(0, str::len)) {
                acc.gaps.insert(BUILD_SCAN_BUDGET);
                return;
            }
            if kind == PathItem::Redirect {
                acc.gaps.insert(REDIRECTED_IMPORT);
                continue;
            }
            // An SDK import is found by the SDK resolver, not by path, so it
            // names no repository file; but only when the SDK name and the
            // path under it are plain literals. Otherwise the import may
            // resolve to anything. An empty name is no SDK to rely on (MSBuild
            // fails to resolve it), so that import is followed by path like
            // any other.
            if kind == PathItem::Import
                && let Some(sdk) = attr_ci(e, "Sdk").filter(|s| !s.trim().is_empty())
            {
                if !plain_sdk(sdk) || raw.is_some_and(|p| !plain_sdk(p)) {
                    acc.gaps.insert(NON_LITERAL_SDK);
                }
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
                    PathItem::Import => self.import(i, &rs[0], applied, acc),
                    PathItem::Other => item(proj_dir, &rs, acc),
                    PathItem::Redirect => {}
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

    /// An `Import` project. A literal path is read only when it is one of the
    /// `applied` files, which the detector scans for project `i` anyway; any
    /// other file is never parsed, so what it references would be missed, and
    /// the project is opaque. A path that leaves the project directory is also
    /// an extra input. A wildcard, an unevaluated property, an absolute path or
    /// one that leaves the repository makes the project opaque.
    fn import(&self, i: usize, r: &Resolved, applied: &[&[u8]], acc: &mut Scan) {
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
                if !applied.contains(&p.as_slice()) {
                    acc.gaps.insert(UNSCANNED_IMPORT);
                }
                if !inside(proj_dir, p) {
                    acc.extra.insert(fs_glob(p, false));
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
        // A response file beside or above the solution sets global properties
        // for every member, and it is read only when the solution is the entry.
        if let Some(f) = self.response {
            self.cx.report(f.clone(), RESPONSE_FILE);
            self.cx.report(m.path.clone(), RESPONSE_FILE);
            p.opaque = true;
        }
        p
    }

    /// `Directory.Packages.props` files at or above `dir`, outermost first.
    fn ancestors(&self, dir: &[u8]) -> Vec<usize> {
        self.props_ix.at_or_above(dir)
    }

    /// The auto-imported files of project `i`: `<project file>.user` beside it
    /// and `<project file>.*.props`/`.targets` in its `obj` directory, in any
    /// case.
    fn autos_of(&self, i: usize) -> Vec<usize> {
        let m = &self.projects[i];
        let proj_file = m.path.file_name();
        let proj_file = proj_file.as_bytes();
        let dir = fold_dir(&m.dir);
        let mut obj = dir.clone();
        if !obj.is_empty() {
            obj.push(b'/');
        }
        obj.extend_from_slice(b"obj");
        let at = |d: &[u8], in_obj: bool| -> Vec<usize> {
            self.autos_by_dir
                .get(d)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&a| {
                    let n = self.autos[a].path.file_name();
                    is_auto_import_of(n.as_bytes(), proj_file, in_obj)
                })
                .collect()
        };
        let mut v = at(&dir, false);
        v.extend(at(&obj, true));
        v
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
                acc.extra.insert(fs_glob(p, true));
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

    thread_local! {
        /// Calls of `at_or_above` on this thread.
        pub(super) static LOOKUPS: Cell<usize> = const { Cell::new(0) };
    }

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
                [
                    fs_glob(b"build/common.props", false),
                    fs_glob(b"docs/**", true)
                ],
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
            [
                fs_glob(b"build/app.targets", false),
                fs_glob(b"src/Shared/*.cs", true)
            ]
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

    #[test]
    fn extra_input_globs_fold_case_and_escape_syntax() {
        assert_eq!(fs_glob(b"a/B*.c{", true), b"[aA]/[bB]*.[cC]\\{".to_vec());
        assert_eq!(fs_glob(b"a?*b", true), b"[aA]*[bB]".to_vec());
        assert_eq!(fs_glob(b"x**y/**/z", true), b"[xX]*[yY]/**/[zZ]".to_vec());
        assert_eq!(fs_glob(b"a*/**", false), b"[aA]\\*/\\*\\*".to_vec());
        assert_eq!(fs_glob("\u{e9}x".as_bytes(), false), b"*[xX]".to_vec());
    }

    const GEN_REF: &str =
        "<ItemGroup><ProjectReference Include=\"tools/Gen/Gen.csproj\" /></ItemGroup>";

    #[test]
    fn directory_packages_props_imports_and_references_are_followed() {
        // MSBuild imports it for every project (NuGet.props, ImportDirectoryPackagesProps),
        // so what it imports is as much an input as a Directory.Build file's imports.
        let mut t = with_build_file(
            "Directory.Packages.props",
            &props("<Import Project=\"eng/refs.props\" />"),
        );
        t.insert(
            "eng/refs.props".into(),
            props(
                "<ItemGroup><ProjectReference Include=\"$(MSBuildThisFileDirectory)../tools/Gen/Gen.csproj\" /></ItemGroup>",
            ),
        );
        all_opaque(&t, "import in Directory.Packages.props");
        // Its own references are dependencies.
        let t = with_build_file("Directory.Packages.props", &props(GEN_REF));
        let (g, _) = build(&t);
        assert_eq!(deps(&g, "A"), ["Normal:Gen"]);
        assert_eq!(deps(&g, "B"), ["Normal:Gen"]);
        assert_eq!(
            affected_ids(&t, GEN),
            ["dotnet:A", "dotnet:B", "dotnet:Gen"]
        );
        for body in [
            "<ItemGroup><ProjectReference Include=\"$(Lib)\" /></ItemGroup>",
            "<Import Project=\"build/*.props\" />",
            "<ItemGroup><Compile Include=\"/abs/x.cs\" /></ItemGroup>",
        ] {
            all_opaque(
                &with_build_file("Directory.Packages.props", &props(body)),
                body,
            );
        }
    }

    #[test]
    fn directory_packages_props_names_match_in_any_case() {
        for name in ["directory.packages.props", "DIRECTORY.PACKAGES.PROPS"] {
            assert_eq!(Dotnet.claim(&rp(name)), Some(Claim::Manifest), "{name}");
            let t = with_build_file(name, &props("<Import Project=\"eng/refs.props\" />"));
            all_opaque(&t, name);
        }
    }

    #[test]
    fn an_unreadable_directory_packages_props_makes_the_projects_below_opaque() {
        // Oversized, so it is never parsed; MSBuild still imports it.
        let big = props(&" ".repeat(MAX_MANIFEST_BYTES));
        all_opaque(
            &with_build_file("Directory.Packages.props", &big),
            "oversized root props",
        );
        let (g, _) = build(&with_build_file("src/Directory.Packages.props", &big));
        assert!(project(&g, "A").opaque && project(&g, "B").opaque);
        // Gen is not below the file.
        assert!(!project(&g, "Gen").opaque);
    }

    #[test]
    fn user_and_project_extension_files_are_followed() {
        // Beside the project and in its `obj` directory, in any case.
        let gen_ref =
            "<ItemGroup><ProjectReference Include=\"../../tools/Gen/Gen.csproj\" /></ItemGroup>";
        for path in [
            "src/A/A.csproj.user",
            "src/A/a.CSProj.User",
            "src/a/A.csproj.user",
            "src/A/obj/A.csproj.nuget.g.props",
            "src/A/OBJ/A.csproj.Foo.TARGETS",
        ] {
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert(path.into(), props(gen_ref));
            let (g, _) = build(&t);
            assert_eq!(deps(&g, "A"), ["Normal:Gen"], "{path}");
            assert!(deps(&g, "B").is_empty(), "{path}");
            assert_eq!(affected_ids(&t, GEN), ["dotnet:A", "dotnet:Gen"], "{path}");
            // What they import is not read.
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert(path.into(), props("<Import Project=\"x.props\" />"));
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{path}");
            assert!(!project(&g, "B").opaque, "{path}");
            // An unreadable one.
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert(path.into(), "not xml".into());
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{path}");
        }
        // A file named for another project does not apply.
        let mut t = with_build_file("Directory.Build.props", &props(""));
        t.insert("src/A/B.csproj.user".into(), props(GEN_REF));
        t.insert("src/A/obj/B.csproj.nuget.g.props".into(), props(GEN_REF));
        let (g, _) = build(&t);
        assert!(deps(&g, "A").is_empty() && deps(&g, "B").is_empty());
    }

    #[test]
    fn imports_do_not_rescan_the_build_files_for_every_value() {
        // 3 projects with 3000 literal imports each, in a tree with a build file.
        let imports = "<Import Project=\"a.props\" />".repeat(3000);
        let mut t = with_build_file("Directory.Build.props", &props(""));
        for n in ["A", "B"] {
            t.insert(format!("src/{n}/{n}.csproj"), csproj(&imports));
        }
        LOOKUPS.with(|c| c.set(0));
        let (g, _) = build(&t);
        let calls = LOOKUPS.with(Cell::get);
        assert!(calls <= 30, "{calls} directory lookups");
        assert!(opaque_with_issue(&g, "src/A/A.csproj", UNSCANNED_IMPORT));
    }

    #[test]
    fn many_build_files_and_imports_link_in_bounded_time() {
        // 5000 build files in other directories and one project of ~100k imports.
        let mut t = Tree::new();
        for i in 0..5000 {
            t.insert(format!("d{i:04}/Directory.Build.props"), props(""));
        }
        let imports = "<Import Project=\"a\" />".repeat(MAX_MANIFEST_BYTES / 40);
        t.insert("src/A/A.csproj".into(), csproj(&imports));
        let start = std::time::Instant::now();
        let (g, _) = build(&t);
        assert!(project(&g, "A").opaque);
        assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
    }

    /// The projects a change to `changed` selects when `A` holds `body` in an
    /// `ItemGroup`.
    fn selected_by_item(body: &str, changed: &str) -> Vec<String> {
        let mut t = with_build_file("Directory.Build.props", &props(""));
        t.insert(
            "src/A/A.csproj".into(),
            csproj(&format!("<ItemGroup>{body}</ItemGroup>")),
        );
        t.insert(changed.into(), "x".into());
        affected_ids(&t, changed)
    }

    #[test]
    fn item_paths_outside_the_project_match_as_the_file_system_does() {
        for (include, changed) in [
            // Another case: the same file on Windows and macOS.
            ("../SHARED/x.cs", "src/shared/x.cs"),
            ("../shared/x.cs", "src/SHARED/X.CS"),
            // Characters that are not wildcards in MSBuild.
            ("../shared/{a,b}/x.cs", "src/shared/{a,b}/x.cs"),
            (
                "../shared/{projectRoot}/x.cs",
                "src/shared/{projectRoot}/x.cs",
            ),
            ("../shared/[ab]/x.cs", "src/shared/[ab]/x.cs"),
            // An invalid glob must not stop the run.
            ("../a/[.cs", "src/a/[.cs"),
            ("../a/x**y.cs", "src/a/xzzy.cs"),
            ("../a/\\x.cs", "src/a/x.cs"),
            // Wildcards stay wildcards.
            ("../shared/*.cs", "src/shared/y.cs"),
            ("../sh?red/**", "src/shared/d/e/y.cs"),
            // Non-ASCII case folding is not decided here: match wider.
            ("../shared/\u{e9}.cs", "src/shared/\u{c9}.cs"),
        ] {
            let body = format!("<Compile Include=\"{include}\" />");
            assert_eq!(selected_by_item(&body, changed), ["dotnet:A"], "{include}");
        }
        for (include, changed) in [
            ("../shared/{a,b}/x.cs", "src/shared/a/x.cs"),
            ("../shared/[ab]/x.cs", "src/shared/a/x.cs"),
            ("../shared/*.cs", "src/other/y.cs"),
            ("../shared/*.cs", "src/shared/d/y.cs"),
        ] {
            let body = format!("<Compile Include=\"{include}\" />");
            assert!(selected_by_item(&body, changed).is_empty(), "{include}");
        }
    }

    #[test]
    fn import_paths_outside_the_project_match_as_the_file_system_does() {
        for (include, changed) in [
            ("../SHARED/Common.props", "src/shared/common.props"),
            ("../shared/{a,b}.props", "src/shared/{a,b}.props"),
            ("../shared/[x].props", "src/shared/[x].props"),
            ("../shared/a\\[.props", "src/shared/a/[.props"),
        ] {
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert(
                "src/A/A.csproj".into(),
                csproj(&format!("<Import Project=\"{include}\" />")),
            );
            let (g, _) = build(&t);
            let extra = &project(&g, "A").extra_inputs;
            let globs: Vec<&str> = extra
                .iter()
                .map(|e| std::str::from_utf8(e).unwrap())
                .collect();
            let set = crate::pattern::PatternSet::new(globs.iter().copied(), [], None)
                .unwrap_or_else(|e| panic!("{include}: {e}"));
            assert!(set.matches(&rp(changed)), "{include}: {globs:?}");
        }
    }

    #[test]
    fn an_empty_sdk_attribute_does_not_hide_an_import() {
        // MSBuild tries to resolve an SDK named "" and fails; never skip the path.
        for sdk in ["", " "] {
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert(
                "src/A/A.csproj".into(),
                csproj(&format!("<Import Project=\"../x.props\" Sdk=\"{sdk}\" />")),
            );
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{sdk:?}");
        }
    }

    /// Properties MSBuild reads to choose a file to import (verified with
    /// `dotnet msbuild -getItem` on SDK 9.0.319 for `CustomAfterMicrosoftCommonTargets`).
    const REDIRECTING: [&str; 14] = [
        "CustomAfterMicrosoftCommonTargets",
        "CustomBeforeMicrosoftCommonProps",
        "DirectoryBuildTargetsPath",
        "DirectoryBuildPropsPath",
        "DirectoryPackagesPropsPath",
        "ImportDirectoryBuildProps",
        "MSBuildProjectExtensionsPath",
        "BaseIntermediateOutputPath",
        "UseArtifactsOutput",
        "ArtifactsPath",
        "NuGetRestoreTargets",
        "_SourceLinkSdkSubDir",
        "customaftermicrosoftcommontargets",
        "DIRECTORYBUILDTARGETSPATH",
    ];

    fn redirect(name: &str) -> String {
        format!(
            "<PropertyGroup><{name}>$(MSBuildThisFileDirectory)eng/refs.targets</{name}></PropertyGroup>"
        )
    }

    #[test]
    fn a_property_that_redirects_an_import_makes_the_project_opaque() {
        for name in REDIRECTING {
            // The exploit: no `Import`, and the target file holds a reference.
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert("src/A/A.csproj".into(), csproj(&redirect(name)));
            t.insert(
                "eng/refs.targets".into(),
                props(
                    "<ItemGroup><ProjectReference Include=\"$(MSBuildThisFileDirectory)../tools/Gen/Gen.csproj\" /></ItemGroup>",
                ),
            );
            let (g, _) = build(&t);
            assert!(
                opaque_with_issue(&g, "src/A/A.csproj", REDIRECTED_IMPORT),
                "{name}"
            );
            assert!(!project(&g, "B").opaque, "{name}");
            // The same property in any file that applies to the project.
            for file in [
                "Directory.Build.props",
                "src/Directory.Build.targets",
                "Directory.Packages.props",
                "src/A/A.csproj.user",
                "src/A/obj/A.csproj.nuget.g.props",
            ] {
                let t = with_build_file(file, &props(&redirect(name)));
                let (g, _) = build(&t);
                assert!(project(&g, "A").opaque, "{name} in {file}");
                assert!(
                    g.issues
                        .iter()
                        .any(|i| i.message == REDIRECTED_IMPORT.as_bytes()),
                    "{name} in {file}"
                );
            }
        }
    }

    #[test]
    fn ordinary_properties_do_not_make_a_project_opaque() {
        let body = "<PropertyGroup><TargetFramework>net9.0</TargetFramework><OutputPath>out/</OutputPath><ManagePackageVersionsCentrally>true</ManagePackageVersionsCentrally></PropertyGroup>";
        let mut t = with_build_file("Directory.Build.props", &props(body));
        t.insert("src/A/A.csproj".into(), csproj(body));
        let (g, _) = build(&t);
        assert!(g.projects.iter().all(|p| !p.opaque), "{:?}", g.issues);
    }

    #[test]
    fn build_files_in_a_differently_cased_directory_apply() {
        let gen_up =
            "<ItemGroup><ProjectReference Include=\"../tools/Gen/Gen.csproj\" /></ItemGroup>";
        // Both land in one directory on Windows and macOS.
        for dir in ["SRC", "sRc"] {
            let t = with_build_file(&format!("{dir}/Directory.Build.props"), &props(gen_up));
            let (g, _) = build(&t);
            assert_eq!(deps(&g, "A"), ["Normal:Gen"], "{dir}");
            assert_eq!(
                affected_ids(&t, GEN),
                ["dotnet:A", "dotnet:B", "dotnet:Gen"],
                "{dir}"
            );
            // A build file that cannot be read still makes the projects opaque.
            let t = with_build_file(&format!("{dir}/Directory.Build.props"), "not xml");
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{dir}");
            // So do Directory.Packages.props, readable or not.
            let t = with_build_file(
                &format!("{dir}/Directory.Packages.props"),
                &props("<Import Project=\"eng/refs.props\" />"),
            );
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{dir}");
            let big = props(&" ".repeat(MAX_MANIFEST_BYTES));
            let (g, _) = build(&with_build_file(
                &format!("{dir}/Directory.Packages.props"),
                &big,
            ));
            assert!(project(&g, "A").opaque, "{dir}");
            assert!(!project(&g, "Gen").opaque, "{dir}");
        }
    }

    const REFS: &str = "<ItemGroup><ProjectReference Include=\"$(MSBuildThisFileDirectory)../tools/Gen/Gen.csproj\" /></ItemGroup>";

    #[test]
    fn a_response_file_makes_every_project_opaque() {
        // `-p:` sets a property with no XML, and MSBuild reads the file by itself.
        // It reads the file from beside or above the entry project or solution,
        // and its properties reach every project that entry references, so a file
        // anywhere in the repository can apply to any project.
        let rsp = [
            "-p:CustomAfterMicrosoftCommonTargets=$(MSBuildProjectDirectory)/../../eng/refs.targets\n",
            "-p:CustomAfterMicrosoftCommonTargets=eng/refs.targets",
            "@other.rsp",
            "",
            "<Project />",
        ];
        let cases = [
            "Directory.Build.rsp",
            "src/Directory.Build.rsp",
            "src/A/Directory.Build.rsp",
            "src/A/directory.build.RSP",
            "src/A/MSBuild.rsp",
            "src/B/MSBuild.rsp",
            "tools/Gen/Directory.Build.rsp",
            "docs/elsewhere/Directory.Build.rsp",
        ];
        for path in cases {
            assert_eq!(Dotnet.claim(&rp(path)), Some(Claim::EcoGlobal), "{path}");
            for content in rsp {
                let mut t = with_build_file(path, content);
                t.insert("eng/refs.targets".into(), props(REFS));
                let (g, _) = build(&t);
                for n in ["A", "B", "Gen"] {
                    assert!(project(&g, n).opaque, "{path} {n}");
                }
                let manifest = "src/A/A.csproj";
                assert!(
                    opaque_with_issue(&g, manifest, RESPONSE_FILE),
                    "{path} {content:?}"
                );
            }
        }
        // The opaque project is affected by a change to the project it would reference.
        let mut t = with_build_file("src/A/Directory.Build.rsp", "-p:X=eng/refs.targets");
        t.insert("eng/refs.targets".into(), props(REFS));
        assert_eq!(
            affected_ids(&t, GEN),
            ["dotnet:A", "dotnet:B", "dotnet:Gen"]
        );
    }

    const SLN: &str = "\nMicrosoft Visual Studio Solution File, Format Version 12.00\nProject(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"A\", \"..\\src\\A\\A.csproj\", \"{11111111-1111-1111-1111-111111111111}\"\nEndProject\n";

    #[test]
    fn a_response_file_beside_a_solution_cannot_hide_a_reference_from_its_members() {
        // `build/Directory.Build.rsp` applies when `build/All.sln` is built; its
        // `-p:` becomes a global property of A, which is outside `build/`, so
        // the SDK imports `eng/obj/A.csproj.x.props` into A. Verified with
        // `dotnet msbuild build/All.sln -t:Restore`: the switch is applied to
        // the solution, and a relative `BaseIntermediateOutputPath` resolves
        // against each project's directory.
        let mut t = with_build_file(
            "build/Directory.Build.rsp",
            "-p:BaseIntermediateOutputPath=../../eng/obj/\n",
        );
        t.insert("build/All.sln".into(), SLN.into());
        t.insert("eng/obj/A.csproj.x.props".into(), props(REFS));
        let (g, _) = build(&t);
        for n in ["A", "B", "Gen", "All.sln"] {
            assert!(project(&g, n).opaque, "{n}");
        }
        assert!(
            g.issues.iter().any(|i| {
                i.path == rp("build/All.sln") && i.message == RESPONSE_FILE.as_bytes()
            })
        );
        let ids = affected_ids(&t, GEN);
        assert!(ids.iter().any(|i| i == "dotnet:A"), "{ids:?}");
        assert!(ids.iter().any(|i| i.contains("All.sln")), "{ids:?}");
    }

    #[test]
    fn an_import_with_a_computed_sdk_or_project_makes_the_project_opaque() {
        let computed = [
            "<Import Sdk=\"$(SdkName)\" Project=\"Sdk.props\" />",
            "<Import Sdk=\"Microsoft.NET.Sdk/$(V)\" Project=\"Sdk.props\" />",
            "<Import Sdk=\"@(SdkItem)\" />",
            "<Import Sdk=\"Name%2fBad\" Project=\"Sdk.props\" />",
            "<Import Sdk=\"..\\..\\eng\" Project=\"refs.targets\" />",
            "<Import Sdk=\"Microsoft.NET.Sdk\" Project=\"$(Dir)/refs.targets\" />",
            "<Import Sdk=\"Microsoft.NET.Sdk\" Project=\"%2e%2e/refs.targets\" />",
            "<Import Sdk=\"Microsoft.NET.Sdk\" Project=\"../../eng/refs.targets\" />",
            "<Import Sdk=\"Microsoft.NET.Sdk\" Project=\"*.props\" />",
        ];
        for import in computed {
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert("src/A/A.csproj".into(), csproj(import));
            let (g, _) = build(&t);
            assert!(
                opaque_with_issue(&g, "src/A/A.csproj", NON_LITERAL_SDK),
                "{import}"
            );
            assert!(!project(&g, "B").opaque, "{import}");
            // The same import in a build file that applies.
            let t = with_build_file("src/A/Directory.Build.props", &props(import));
            let (g, _) = build(&t);
            assert!(
                opaque_with_issue(&g, "src/A/A.csproj", NON_LITERAL_SDK),
                "{import} in a build file"
            );
            assert!(!project(&g, "B").opaque, "{import}");
        }
        for import in [
            "<Import Sdk=\"Microsoft.NET.Sdk\" Project=\"Sdk.props\" />",
            "<Import Sdk=\"Microsoft.NET.Sdk\" />",
            "<Import Sdk=\"Microsoft.NET.Sdk/9.0.100\" Project=\"Sdk.targets\" />",
            "<Import Sdk=\"Aspire.AppHost.Sdk\" Project=\"Sdk.props\" />",
        ] {
            let mut t = with_build_file("Directory.Build.props", &props(""));
            t.insert("src/A/A.csproj".into(), csproj(import));
            let (g, _) = build(&t);
            assert!(g.projects.iter().all(|p| !p.opaque), "{import}");
        }
    }

    #[test]
    fn a_property_in_an_import_path_is_never_resolved() {
        // Not even when a scanned file defines it; one set by a command line
        // or response file cannot be seen at all.
        for name in [
            "ToolsDir",
            "NuGetPackageRoot",
            "RestorePackagesPath",
            "RestoreOutputPath",
            "ProjectAssetsFile",
            "VSToolsPath",
            "SolutionDir",
            "PkgSome_Package",
        ] {
            let defined = format!(
                "<PropertyGroup><{name}>$(MSBuildThisFileDirectory)eng/</{name}></PropertyGroup>"
            );
            let import = format!("<Import Project=\"$({name})refs.targets\" />");
            let mut t = with_build_file("Directory.Build.props", &props(&defined));
            t.insert("src/A/A.csproj".into(), csproj(&import));
            let (g, _) = build(&t);
            assert!(
                opaque_with_issue(&g, "src/A/A.csproj", UNEVALUATED_ITEM),
                "{name}"
            );
            assert!(!project(&g, "B").opaque, "{name}");
            let mut t = with_build_file("src/A/Directory.Build.rsp", &format!("-p:{name}=eng/"));
            t.insert("src/A/A.csproj".into(), csproj(&import));
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{name}");
        }
    }

    const GEN_UP: &str =
        "<ItemGroup><ProjectReference Include=\"../tools/Gen/Gen.csproj\" /></ItemGroup>";
    const NFC: &str = "caf\u{e9}";
    const NFD: &str = "cafe\u{301}";

    /// Spellings that a file system may treat as one directory.
    const ALIASES: [(&str, &str); 7] = [
        (NFC, NFD),
        (NFD, NFC),
        ("\u{c9}", "\u{e9}"),
        ("src/caf\u{e9}", "src/cafe\u{301}"),
        ("\u{212a}", "k"),
        ("k", "\u{212a}"),
        ("\u{17f}rc", "src"),
    ];

    /// `Gen`, a far project `docs/B`, a project `A` in `proj_dir`, and `file`
    /// in `file_dir`.
    fn alias_tree(file_dir: &str, file: &str, content: &str, proj_dir: &str) -> Tree {
        let mut t = Tree::new();
        t.insert(GEN.into(), csproj(""));
        t.insert("docs/B/B.csproj".into(), csproj(""));
        t.insert(format!("{file_dir}/{file}"), content.into());
        t.insert(format!("{proj_dir}/A/A.csproj"), csproj(""));
        t
    }

    fn assert_alias_opaque(t: &Tree, proj_dir: &str, what: &str) {
        let (g, _) = build(t);
        let manifest = format!("{proj_dir}/A/A.csproj");
        assert!(opaque_with_issue(&g, &manifest, NON_ASCII_DIR), "{what}");
        assert!(!project(&g, "B").opaque, "{what}");
        assert!(!project(&g, "Gen").opaque, "{what}");
    }

    #[test]
    fn non_ascii_directory_spellings_that_may_alias_make_the_project_opaque() {
        // The file system may fold the case of, or normalise, non-ASCII names,
        // which byte comparison does not decide.
        for (file_dir, proj_dir) in ALIASES {
            let what = format!("{file_dir:?} {proj_dir:?}");
            // Directory.Build.props, readable.
            let t = alias_tree(file_dir, "Directory.Build.props", &props(GEN_UP), proj_dir);
            assert_alias_opaque(&t, proj_dir, &what);
            // Directory.Packages.props, readable and not.
            let imp = props("<Import Project=\"eng/refs.props\" />");
            let t = alias_tree(file_dir, "Directory.Packages.props", &imp, proj_dir);
            assert_alias_opaque(&t, proj_dir, &what);
            let t = alias_tree(file_dir, "Directory.Packages.props", "not xml", proj_dir);
            assert_alias_opaque(&t, proj_dir, &what);
            // An auto-imported file beside the project, and one in its `obj`.
            let t = alias_tree(
                &format!("{file_dir}/A"),
                "A.csproj.user",
                &props(GEN_UP),
                proj_dir,
            );
            assert_alias_opaque(&t, proj_dir, &what);
            let t = alias_tree(
                &format!("{file_dir}/A/obj"),
                "A.csproj.nuget.g.props",
                &props(GEN_UP),
                proj_dir,
            );
            assert_alias_opaque(&t, proj_dir, &what);
        }
    }

    #[test]
    fn non_ascii_project_files_that_may_alias_an_auto_import_make_the_project_opaque() {
        let mut t = Tree::new();
        t.insert(GEN.into(), csproj(""));
        t.insert("docs/B/B.csproj".into(), csproj(""));
        t.insert(format!("p/{NFC}.csproj"), csproj(""));
        t.insert(format!("p/{NFD}.csproj.user"), props(GEN_UP));
        let (g, _) = build(&t);
        assert!(project(&g, NFC).opaque);
        assert!(!project(&g, "B").opaque);
        assert!(!project(&g, "Gen").opaque);
    }

    #[test]
    fn a_non_ascii_directory_with_the_same_bytes_applies_normally() {
        let t = alias_tree(NFC, "Directory.Build.props", &props(GEN_UP), NFC);
        let (g, _) = build(&t);
        assert_eq!(deps(&g, "A"), ["Normal:Gen"]);
        assert!(g.projects.iter().all(|p| !p.opaque), "{:?}", g.issues);
        // A different script, or unrelated Latin names, are not aliases.
        for other in [
            "\u{65e5}\u{672c}/Directory.Build.props",
            "docs/x\u{e9}/Directory.Build.props",
        ] {
            let mut t = with_build_file("src/Directory.Build.props", &props(""));
            t.insert(other.into(), props(GEN_UP));
            let (g, _) = build(&t);
            assert!(!project(&g, "A").opaque, "{other}");
        }
    }

    #[test]
    fn build_file_names_with_a_look_alike_non_ascii_letter_are_build_files() {
        for name in [
            "Directory.Build.prop\u{17f}",
            "Directory.Build.target\u{17f}",
            "Directory.Packages.prop\u{17f}",
        ] {
            let c = Dotnet.claim(&rp(&format!("src/{name}")));
            assert!(c.is_some(), "{name}");
        }
        // A look-alike build file is read like the real one.
        let t = with_build_file("src/Directory.Build.prop\u{17f}", &props(GEN_UP));
        let (g, _) = build(&t);
        assert_eq!(deps(&g, "A"), ["Normal:Gen"]);
        let t = with_build_file("src/Directory.Build.r\u{17f}p", "-p:X=1");
        assert_eq!(
            Dotnet.claim(&rp("src/Directory.Build.r\u{17f}p")),
            Some(Claim::EcoGlobal)
        );
        let (g, _) = build(&t);
        assert!(project(&g, "A").opaque);
    }

    #[test]
    fn auto_imports_with_a_look_alike_non_ascii_letter_make_the_project_opaque() {
        // On a case-insensitive file system `ſ` folds to `s`, so MSBuild's
        // existence check for `A.csproj.user` finds these.
        for (dir, name) in [
            ("src/A", "A.csproj.u\u{17f}er"),
            ("src/A", "A.c\u{17f}proj.user"),
            ("src/A", "A.csproj.user\u{200c}"),
            ("src/A/obj", "A.csproj.nuget.g.prop\u{17f}"),
            ("src/A/obj", "A.csproj.nuget.g.targets\u{200c}"),
            ("src/A/obj", "A.c\u{17f}proj.nuget.g.props"),
            ("src/A/\u{ff4f}bj", "A.csproj.nuget.g.props"),
            ("src/A/o\u{200c}bj", "A.csproj.nuget.g.props"),
        ] {
            let what = format!("{dir}/{name}");
            assert_eq!(Dotnet.claim(&rp(&what)), Some(Claim::EcoGlobal), "{what}");
            let t = alias_tree(dir, name, &props(GEN_UP), "src");
            assert_alias_opaque(&t, "src", &what);
        }
        // Names that cannot become an auto-import are left alone.
        for name in [
            "caf\u{e9}.md",
            "\u{65e5}\u{672c}.user",
            "A.csproj.\u{65e5}.props",
        ] {
            let t = alias_tree("src/A", name, &props(GEN_UP), "src");
            let (g, _) = build(&t);
            assert!(g.projects.iter().all(|p| !p.opaque), "{name}");
        }
    }

    #[test]
    fn hfs_ignorable_characters_in_a_name_or_directory_may_alias() {
        // HFS+ ignores U+200C-200F, U+202A-202E, U+206A-206F and U+FEFF when it
        // compares names, so these name the same directory and file.
        for ignored in ["\u{200c}", "\u{200f}", "\u{202e}", "\u{206a}", "\u{feff}"] {
            let dir = format!("src{ignored}");
            let t = alias_tree(&dir, "Directory.Build.props", &props(GEN_UP), "src");
            assert_alias_opaque(&t, "src", &format!("dir {ignored:?}"));
            let t = alias_tree(
                "src",
                "Directory.Packages.props",
                "not xml",
                &format!("s{ignored}rc"),
            );
            assert_alias_opaque(
                &t,
                &format!("s{ignored}rc"),
                &format!("project dir {ignored:?}"),
            );
            for file in ["Directory.Build.props", "Directory.Build.targets"] {
                let name = format!("{file}{ignored}");
                assert_eq!(
                    Dotnet.claim(&rp(&format!("src/{name}"))),
                    Some(Claim::EcoGlobal)
                );
                let t = with_build_file(&format!("src/{name}"), &props(GEN_UP));
                let (g, _) = build(&t);
                assert_eq!(deps(&g, "A"), ["Normal:Gen"], "{name:?}");
            }
            let name = format!("Directory.Build.rsp{ignored}");
            let t = with_build_file(&format!("src/{name}"), "-p:X=1");
            let (g, _) = build(&t);
            assert!(project(&g, "A").opaque, "{name:?}");
        }
    }
}
