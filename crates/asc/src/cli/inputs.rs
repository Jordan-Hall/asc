//! `inputs` (docs/designs/build-cache.md §1): the content identity of work-tree paths, for build
//! caches. A directory with nothing changed below it is its `TreeId`; a clean file is the
//! `FileId` the tree records; only changed and untracked files are read and hashed. Paths are
//! relative to the repository root, so the same content gives the same answer in every worktree.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use asc_core::id::{FileHasher, FileId, TreeId};
use asc_core::object::Kind;
use asc_core::path::RepoPath;
use asc_wc::SafeRoot;
use asc_wc::status;

use super::add::cwd_prefix;
use super::check_ignore::ignore_sources;
use super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::restore::WcParts;
use crate::native::NativeSpec;

/// The key context of `--digest`.
pub const DIGEST_CONTEXT: &str = "asc build inputs v1";

/// The most paths one call may name.
const MAX_PATHS: usize = 4096;

#[derive(clap::Parser, Debug)]
#[command(name = "inputs", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub digest: bool,
    #[arg(long = "allow-missing")]
    pub allow_missing: bool,
    #[arg(short = 'z')]
    pub nul: bool,
    #[arg(num_args = 0..)]
    pub paths: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "inputs",
    flags: &["--json", "--digest", "--allow-missing", "-z"],
    placeholders: &[],
}];

/// What one entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputKind {
    Tree,
    File,
    Exec,
    Link,
}

impl InputKind {
    pub fn name(self) -> &'static str {
        match self {
            InputKind::Tree => "tree",
            InputKind::File => "file",
            InputKind::Exec => "exec",
            InputKind::Link => "link",
        }
    }
}

/// One entry: a clean directory or a file, by repository-relative path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub kind: InputKind,
    pub id: [u8; 32],
    pub path: Vec<u8>,
}

/// The entries, sorted by path, and their digest.
#[derive(Debug, PartialEq, Eq)]
pub struct Inputs {
    pub entries: Vec<Input>,
    pub digest: [u8; 32],
}

/// One blake3 over the sorted entries: kind, path, NUL, id.
pub fn digest(entries: &[Input]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key(DIGEST_CONTEXT);
    for e in entries {
        h.update(e.kind.name().as_bytes());
        h.update(b" ");
        h.update(&e.path);
        h.update(&[0]);
        h.update(&e.id);
    }
    *h.finalize().as_bytes()
}

fn hex(id: &[u8; 32]) -> String {
    FileId::from_bytes(*id).to_string()
}

fn usage(text: impl std::fmt::Display) -> CliError {
    CliError::Usage(format!("error: {text}"))
}

fn fatal(e: impl std::fmt::Display) -> CliError {
    CliError::Fatal(e.to_string())
}

/// `p` canonical as far as it exists: a path not written yet (an output) is its nearest existing
/// directory, canonical, with the rest appended, so it compares with a canonical root.
fn canon(p: &Path) -> std::path::PathBuf {
    let mut base = p;
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = base.canonicalize() {
            return rest.iter().rev().fold(c, |acc, n| acc.join(n));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                base = parent;
            }
            _ => return p.to_owned(),
        }
    }
}

/// `arg` (relative to `prefix`, the current directory below the root, or absolute below `root`)
/// as a repository path; empty for the root itself.
pub fn normalize(root: &Path, prefix: &str, arg: &OsString) -> Result<Vec<u8>> {
    let raw = arg.to_string_lossy().replace('\\', "/");
    let path = Path::new(arg);
    let joined = if path.is_absolute() {
        canon(path)
            .strip_prefix(canon(root))
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .map_err(|_| usage(format!("'{raw}' is outside the repository")))?
    } else if prefix.is_empty() {
        raw.clone()
    } else {
        format!("{prefix}/{raw}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in joined.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(usage(format!("'{raw}' is outside the repository")));
                }
            }
            c => parts.push(c),
        }
    }
    Ok(parts.join("/").into_bytes())
}

fn under(path: &[u8], root: &[u8]) -> bool {
    root.is_empty()
        || path == root
        || (path.len() > root.len() && path.starts_with(root) && path[root.len()] == b'/')
}

/// The roots that no other root contains, sorted.
fn outermost(mut roots: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    roots.sort();
    roots.dedup();
    let mut out: Vec<Vec<u8>> = Vec::new();
    for r in roots {
        if !out.iter().any(|o| under(&r, o)) {
            out.push(r);
        }
    }
    out
}

/// The files changed against HEAD (staged, unstaged, unmerged or untracked, untracked
/// directories expanded with the ignore rules), and every directory above one of them.
pub struct Dirty {
    pub files: BTreeSet<Vec<u8>>,
    pub dirs: HashSet<Vec<u8>>,
}

impl Dirty {
    pub fn new(files: BTreeSet<Vec<u8>>) -> Self {
        let mut dirs = HashSet::new();
        for f in &files {
            dirs.insert(Vec::new());
            for (at, b) in f.iter().enumerate() {
                if *b == b'/' {
                    dirs.insert(f[..at].to_vec());
                }
            }
        }
        Dirty { files, dirs }
    }
}

async fn dirty(ctx: &Ctx, ws: &WsCtx) -> Result<Dirty> {
    let root = ws.projection.toplevel().to_owned();
    let parts = WcParts::new(ctx, ws, ws.projection.git_dir().join("index"))?;
    let st = status::compute(&parts.wc(&ws.engine), &ws.db, &ws.workspace)
        .await
        .map_err(fatal)?;
    let mut files = BTreeSet::new();
    for c in st.staged.iter().chain(&st.unstaged) {
        files.insert(c.path.as_bytes().to_vec());
    }
    for p in &st.unmerged {
        files.insert(p.as_bytes().to_vec());
    }
    let sources = ignore_sources(ctx, ws);
    for u in &st.untracked {
        if u.dir {
            let mut out = Vec::new();
            super::status::expand_dir(&root, &sources, u, &mut out).map_err(CliError::Io)?;
            // A nested repository (`name/`) is its own build's input, not this one's.
            files.extend(out.into_iter().filter(|p| !p.ends_with(b"/")));
        } else {
            files.insert(u.path.as_bytes().to_vec());
        }
    }
    Ok(Dirty::new(files))
}

/// Hashes the work-tree file at `path`: its bytes, or a symlink's target text. `None` when it
/// is gone or not a file (a deletion, or a directory where a file was).
fn hash_local(gate: &SafeRoot, path: &[u8], head: Option<Kind>) -> Result<Option<Input>> {
    let Ok(rp) = RepoPath::new(path.to_vec()) else {
        return Ok(None);
    };
    let Some(abs) = gate.resolve(&rp) else {
        return Ok(None);
    };
    let meta = match asc_wc::shared::metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CliError::Io(e)),
    };
    let mut h = FileHasher::new();
    let kind = if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&abs).map_err(CliError::Io)?;
        h.update(target.to_string_lossy().replace('\\', "/").as_bytes());
        InputKind::Link
    } else if meta.is_file() {
        let mut f = std::fs::File::open(&abs).map_err(CliError::Io)?; // lint:allow-worktree-fs
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = f.read(&mut buf).map_err(CliError::Io)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        if executable(&meta, head) {
            InputKind::Exec
        } else {
            InputKind::File
        }
    } else {
        return Ok(None);
    };
    Ok(Some(Input {
        kind,
        id: *h.finalize().as_bytes(),
        path: path.to_vec(),
    }))
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata, _head: Option<Kind>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// Windows records no executable bit: the tree's mode stands.
#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata, head: Option<Kind>) -> bool {
    head == Some(Kind::Exec)
}

fn leaf(kind: Kind) -> Option<InputKind> {
    match kind {
        Kind::File => Some(InputKind::File),
        Kind::Exec => Some(InputKind::Exec),
        Kind::Symlink => Some(InputKind::Link),
        // Submodules are not followed: their content is their own repository's inputs.
        Kind::Dir | Kind::Gitlink => None,
    }
}

/// The entries below `roots` (repository paths, empty for the whole tree) of the tree `head`
/// with `dirty` overlaid from the work tree at `gate`. Each root that matches nothing is
/// returned in the second list.
pub async fn collect(
    engine: &CliEngine,
    head: TreeId,
    gate: &SafeRoot,
    dirty: &Dirty,
    roots: Vec<Vec<u8>>,
) -> Result<(Vec<Input>, Vec<Vec<u8>>)> {
    let mut out: BTreeMap<Vec<u8>, Input> = BTreeMap::new();
    let mut missing = Vec::new();
    let mut head_kinds: BTreeMap<Vec<u8>, Kind> = BTreeMap::new();
    for root in outermost(roots) {
        let before = out.len();
        // (path, kind, id) still to visit from the tree.
        let mut stack: Vec<(Vec<u8>, Kind, [u8; 32])> = Vec::new();
        if root.is_empty() {
            stack.push((Vec::new(), Kind::Dir, *head.as_bytes()));
        } else {
            let rp = RepoPath::new(root.clone()).map_err(|e| usage(e.to_string()))?;
            if let Some(e) = engine.lookup(head, &rp).await? {
                stack.push((root.clone(), e.kind, e.id));
            }
        }
        while let Some((path, kind, id)) = stack.pop() {
            if kind == Kind::Dir {
                if !dirty.dirs.contains(&path) {
                    out.insert(
                        path.clone(),
                        Input {
                            kind: InputKind::Tree,
                            id,
                            path,
                        },
                    );
                    continue;
                }
                let tree = engine.get_tree(TreeId::from_bytes(id)).await?;
                for e in tree.entries() {
                    let mut child = path.clone();
                    if !child.is_empty() {
                        child.push(b'/');
                    }
                    child.extend_from_slice(e.name.as_bytes());
                    stack.push((child, e.kind, e.id));
                }
                continue;
            }
            if dirty.files.contains(&path) {
                head_kinds.insert(path, kind);
                continue;
            }
            if let Some(k) = leaf(kind) {
                out.insert(path.clone(), Input { kind: k, id, path });
            }
        }
        for f in dirty.files.iter().filter(|f| under(f, &root)) {
            if let Some(input) = hash_local(gate, f, head_kinds.get(f).copied())? {
                out.insert(f.clone(), input);
            }
        }
        if out.len() == before {
            missing.push(root);
        }
    }
    Ok((out.into_values().collect(), missing))
}

/// The inputs of `paths` in the current workspace.
pub async fn compute(
    ctx: &Ctx,
    ws: &WsCtx,
    paths: &[OsString],
    allow_missing: bool,
) -> Result<Inputs> {
    if paths.len() > MAX_PATHS {
        return Err(usage("too many paths"));
    }
    let root = ws.projection.toplevel().to_owned();
    let prefix = cwd_prefix(ctx, ws);
    let roots = if paths.is_empty() {
        vec![prefix.clone().into_bytes()]
    } else {
        paths
            .iter()
            .map(|p| normalize(&root, &prefix, p))
            .collect::<Result<Vec<_>>>()?
    };
    let head = match ws.workspace.ws.base_cs {
        Some(cs) => ws.engine.get_changeset(cs).await?.root,
        None => CliEngine::empty_tree_id(),
    };
    let dirty = dirty(ctx, ws).await?;
    let gate = SafeRoot::new(&root);
    let (entries, missing) = collect(&ws.engine, head, &gate, &dirty, roots).await?;
    if !allow_missing && let Some(m) = missing.first() {
        return Err(CliError::Raw(
            format!(
                "asc: path '{}' matches no tracked or untracked file",
                String::from_utf8_lossy(m)
            ),
            1,
        ));
    }
    let digest = digest(&entries);
    Ok(Inputs { entries, digest })
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let got = compute(ctx, ws, &a.paths, a.allow_missing).await?;
    let mut out = std::io::stdout().lock();
    if a.json {
        let body = if a.digest {
            serde_json::json!({ "digest": hex(&got.digest) })
        } else {
            let entries: Vec<_> = got
                .entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "kind": e.kind.name(),
                        "id": hex(&e.id),
                        "path": String::from_utf8_lossy(&e.path),
                    })
                })
                .collect();
            serde_json::json!({ "digest": hex(&got.digest), "entries": entries })
        };
        writeln!(out, "{body}").map_err(CliError::Io)?;
    } else if a.digest {
        writeln!(out, "{}", hex(&got.digest)).map_err(CliError::Io)?;
    } else {
        let end = if a.nul { b'\0' } else { b'\n' };
        for e in &got.entries {
            write!(out, "{} {} ", e.kind.name(), hex(&e.id)).map_err(CliError::Io)?;
            out.write_all(if e.path.is_empty() { b"." } else { &e.path })
                .map_err(CliError::Io)?;
            out.write_all(&[end]).map_err(CliError::Io)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: InputKind, path: &str, byte: u8) -> Input {
        Input {
            kind,
            id: [byte; 32],
            path: path.as_bytes().to_vec(),
        }
    }

    #[test]
    fn normalize_resolves_relative_to_the_current_directory() {
        let root = Path::new("/nonexistent-root");
        let n = |prefix: &str, arg: &str| normalize(root, prefix, &OsString::from(arg));
        assert_eq!(n("", ".").unwrap(), b"");
        assert_eq!(n("pkg/ui", "src").unwrap(), b"pkg/ui/src");
        assert_eq!(n("pkg/ui", "../core/./lib").unwrap(), b"pkg/core/lib");
        assert_eq!(n("", "a\\b").unwrap(), b"a/b");
        assert!(n("pkg", "../..").is_err(), "above the root");
    }

    #[test]
    fn an_absolute_path_not_written_yet_is_below_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let later = dir.path().join("app").join("dist");
        assert_eq!(
            normalize(&root, "", &later.into_os_string()).unwrap(),
            b"app/dist"
        );
    }

    #[test]
    fn outermost_drops_contained_roots() {
        let r = |v: &[&str]| outermost(v.iter().map(|s| s.as_bytes().to_vec()).collect());
        assert_eq!(r(&["a/b", "a", "ab"]), vec![b"a".to_vec(), b"ab".to_vec()]);
        assert_eq!(r(&["x", ""]), vec![Vec::<u8>::new()]);
    }

    #[test]
    fn dirty_marks_every_ancestor() {
        let d = Dirty::new([b"a/b/c.rs".to_vec()].into());
        for dir in ["", "a", "a/b"] {
            assert!(d.dirs.contains(dir.as_bytes()), "{dir}");
        }
        assert!(!d.dirs.contains(b"a/b/c.rs".as_slice()));
    }

    #[test]
    fn digest_covers_kind_path_and_id() {
        let base = vec![
            input(InputKind::Tree, "a", 1),
            input(InputKind::File, "b", 2),
        ];
        let d = digest(&base);
        assert_eq!(d, digest(&base.clone()), "deterministic");
        let mut kind = base.clone();
        kind[1].kind = InputKind::Exec;
        let mut path = base.clone();
        path[1].path = b"c".to_vec();
        let mut id = base.clone();
        id[1].id = [3; 32];
        for other in [kind, path, id] {
            assert_ne!(d, digest(&other));
        }
        // The NUL separator keeps a path from running into the id.
        assert_ne!(
            digest(&[input(InputKind::File, "a", 0)]),
            digest(&[input(InputKind::File, "a\0", 0)])
        );
    }
}
