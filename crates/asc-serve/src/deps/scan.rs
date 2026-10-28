//! Reading the dependency files of a revision.
//!
//! The tree of a changeset is walked for the files [`Format::of`] recognises (and for nothing else), the
//! directories that hold other people's code (`node_modules`, `.git`, a `vendor` or `target` beside the
//! manifest it belongs to) are not entered, and each file is read and parsed under the bounds of
//! [`Config`]. A file that cannot be read whole is reported with its error; the others are used.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use asc_core::id::{ChangesetId, FileId, TreeId};
use asc_core::object::Kind;
use tonic::Status;

use super::config::Config;
use super::model::{Dep, Ecosystem, base_of};
use super::parse::{Format, MAX_FILE_BYTES, Parsed, combine, parse_file};
use crate::idmode::Eng;
use crate::update::internal;

/// Most tree entries one walk looks at.
pub const MAX_ENTRIES: usize = 2_000_000;
/// Deepest directory the walk enters.
pub const MAX_DEPTH: usize = 64;

/// Directories that are never entered.
const SKIP: [&str; 6] = [
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    "bower_components",
    "Pods",
];

/// What was learned about one file.
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub path: String,
    pub format: Format,
    pub packages: usize,
    pub direct: usize,
    /// Why the file was not read, or empty.
    pub error: String,
    pub notes: Vec<String>,
}

/// A revision's dependency files, read.
pub struct Snapshot {
    pub rev: ChangesetId,
    pub files: Vec<FileInfo>,
    /// The text of every file that parsed, for the rewriters.
    pub texts: HashMap<String, (Format, Arc<str>)>,
    /// The dependencies after [`combine`].
    pub deps: Vec<Dep>,
    /// The walk or the budget stopped before every file was read.
    pub truncated: bool,
    /// The npm packages the JavaScript, TypeScript, TSX and TSRX sources import (see [`super::imports`]),
    /// when npm was asked for.
    pub imported: std::collections::BTreeSet<String>,
    /// Source files read for [`Snapshot::imported`].
    pub sources_scanned: usize,
}

/// Whether a directory called `name`, beside `siblings` (file names), is skipped.
pub fn skip_dir(name: &str, siblings: &[String]) -> bool {
    if SKIP.contains(&name) {
        return true;
    }
    let beside = |f: &str| siblings.iter().any(|s| s == f);
    match name {
        "vendor" => beside("go.mod") || beside("composer.json") || beside("Gemfile"),
        "target" => beside("Cargo.toml") || beside("pom.xml"),
        _ => false,
    }
}

struct Candidate {
    path: String,
    format: Format,
    file: FileId,
    size: u64,
}

/// Reads the dependency files under `root`, those of `ecosystems` (all when empty).
pub async fn read(
    engine: &Eng,
    rev: ChangesetId,
    root: TreeId,
    cfg: &Config,
    ecosystems: &[Ecosystem],
) -> Result<Snapshot, Status> {
    let mut truncated = false;
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut sources: Vec<Candidate> = Vec::new();
    let want_imports = ecosystems.is_empty() || ecosystems.contains(&Ecosystem::Npm);
    let mut stack: Vec<(String, TreeId, usize)> = vec![(String::new(), root, 0)];
    let mut visited = 0usize;
    'walk: while let Some((prefix, tree, depth)) = stack.pop() {
        let t = engine
            .get_tree(tree)
            .await
            .map_err(|e| internal("deps: tree", &e))?;
        let names = |kinds: fn(Kind) -> bool| -> Vec<String> {
            t.entries()
                .iter()
                .filter(|e| kinds(e.kind))
                .map(|e| String::from_utf8_lossy(e.name.as_bytes()).into_owned())
                .collect()
        };
        let siblings = names(|k| matches!(k, Kind::File | Kind::Exec));
        for e in t.entries() {
            visited += 1;
            if visited > MAX_ENTRIES {
                truncated = true;
                break 'walk;
            }
            let name = String::from_utf8_lossy(e.name.as_bytes()).into_owned();
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            match e.kind {
                Kind::Dir => {
                    if depth < MAX_DEPTH && !skip_dir(&name, &siblings) {
                        stack.push((path, TreeId::from_bytes(e.id), depth + 1));
                    } else if depth >= MAX_DEPTH {
                        truncated = true;
                    }
                }
                Kind::File | Kind::Exec => {
                    let Some(format) = Format::of(&name) else {
                        if want_imports
                            && sources.len() < super::imports::MAX_SOURCES
                            && e.size <= super::imports::MAX_SOURCE_BYTES
                            && super::imports::is_source(&name)
                        {
                            sources.push(Candidate {
                                path,
                                format: Format::PackageJson,
                                file: FileId::from_bytes(e.id),
                                size: e.size,
                            });
                        }
                        continue;
                    };
                    if !ecosystems.is_empty() && !ecosystems.contains(&format.ecosystem()) {
                        continue;
                    }
                    candidates.push(Candidate {
                        path,
                        format,
                        file: FileId::from_bytes(e.id),
                        size: e.size,
                    });
                }
                Kind::Symlink | Kind::Gitlink => {}
            }
        }
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    if candidates.len() > cfg.max_manifest_files {
        candidates.truncate(cfg.max_manifest_files);
        truncated = true;
    }

    let mut files: Vec<FileInfo> = Vec::new();
    let mut parsed: Vec<Parsed> = Vec::new();
    let mut texts: HashMap<String, (Format, Arc<str>)> = HashMap::new();
    let mut budget = cfg.max_manifest_bytes.0;
    for c in candidates {
        let info = |error: String| FileInfo {
            path: c.path.clone(),
            format: c.format,
            packages: 0,
            direct: 0,
            error,
            notes: Vec::new(),
        };
        if c.size > MAX_FILE_BYTES as u64 {
            files.push(info(format!(
                "the file is larger than {MAX_FILE_BYTES} bytes"
            )));
            continue;
        }
        if c.size > budget {
            truncated = true;
            files.push(info("the scan's byte budget is spent".into()));
            continue;
        }
        budget -= c.size;
        let bytes = engine
            .read_file(c.file, c.size)
            .await
            .map_err(|e| internal("deps: read", &e))?;
        let path = c.path.clone();
        let data = bytes.clone();
        let result = tokio::task::spawn_blocking(move || parse_file(&path, &data))
            .await
            .map_err(|e| internal("deps: parse", &e))?;
        match result {
            Some(Ok(p)) => {
                if let Ok(text) =
                    std::str::from_utf8(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes))
                {
                    texts.insert(c.path.clone(), (c.format, Arc::from(text)));
                }
                files.push(FileInfo {
                    path: c.path.clone(),
                    format: c.format,
                    packages: p.deps.len(),
                    direct: 0,
                    error: String::new(),
                    notes: p.notes.clone(),
                });
                parsed.push(p);
            }
            Some(Err(e)) => files.push(info(e)),
            None => {}
        }
    }
    let deps = combine(&parsed).deps;
    let mut direct: HashMap<&str, usize> = HashMap::new();
    for d in deps.iter().filter(|d| d.direct) {
        *direct.entry(d.path.as_str()).or_default() += 1;
    }
    for f in &mut files {
        f.direct = direct.get(f.path.as_str()).copied().unwrap_or(0);
    }
    // The sources: which packages they import. A file that cannot be read is skipped, and the byte
    // budget of the sources is its own so a large repository cannot starve the manifests.
    let mut imported = std::collections::BTreeSet::new();
    let mut sources_scanned = 0usize;
    let mut source_budget = cfg.max_manifest_bytes.0.max(8 << 20);
    sources.sort_by(|a, b| a.path.cmp(&b.path));
    for c in sources {
        if c.size > source_budget {
            truncated = true;
            break;
        }
        source_budget -= c.size;
        let Ok(bytes) = engine.read_file(c.file, c.size).await else {
            continue;
        };
        let path = c.path.clone();
        let found = tokio::task::spawn_blocking(move || super::imports::package_imports(&path, &bytes))
            .await
            .map_err(|e| internal("deps: imports", &e))?;
        sources_scanned += 1;
        imported.extend(found);
    }
    Ok(Snapshot {
        rev,
        files,
        texts,
        deps,
        truncated,
        imported,
        sources_scanned,
    })
}

/// The distinct package versions of `deps` worth asking about, at most `max`; `true` when there were
/// more.
pub fn distinct(deps: &[Dep], max: usize) -> (Vec<(Ecosystem, String, String)>, bool) {
    // Names are compared as written: the advisory source may tell two spellings apart.
    let mut seen: HashSet<(Ecosystem, String, String)> = HashSet::new();
    let mut out = Vec::new();
    let mut more = false;
    for d in deps.iter().filter(|d| !d.version.is_empty()) {
        if seen.insert((d.ecosystem, d.name.clone(), d.version.clone())) {
            if out.len() >= max {
                more = true;
                break;
            }
            out.push((d.ecosystem, d.name.clone(), d.version.clone()));
        }
    }
    (out, more)
}

/// The name of the format of `path`, for messages.
pub fn format_name(path: &str) -> &'static str {
    Format::of(base_of(path)).map_or("", Format::name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_that_hold_other_peoples_code_are_skipped() {
        let none: Vec<String> = Vec::new();
        for d in ["node_modules", ".git", "Pods", "bower_components"] {
            assert!(skip_dir(d, &none), "{d}");
        }
        assert!(!skip_dir("src", &none));
        assert!(
            !skip_dir("vendor", &none),
            "a vendor directory of nothing in particular"
        );
        assert!(skip_dir("vendor", &["go.mod".to_owned()]));
        assert!(skip_dir("vendor", &["composer.json".to_owned()]));
        assert!(skip_dir("target", &["Cargo.toml".to_owned()]));
        assert!(!skip_dir("target", &["package.json".to_owned()]));
    }

    #[test]
    fn distinct_package_versions_are_counted_once_and_bounded() {
        let mk = |n: &str, v: &str, p: &str| Dep::lock(Ecosystem::Npm, n, v, p);
        let deps = vec![
            mk("a", "1.0.0", "x/package-lock.json"),
            mk("a", "1.0.0", "y/package-lock.json"),
            mk("a", "2.0.0", "x/package-lock.json"),
            mk("b", "", "x/package.json"),
            mk("c", "1.0.0", "x/package-lock.json"),
        ];
        let (all, more) = distinct(&deps, 10);
        assert_eq!(all.len(), 3);
        assert!(!more);
        let (some, more) = distinct(&deps, 2);
        assert_eq!(some.len(), 2);
        assert!(more);
    }
}
