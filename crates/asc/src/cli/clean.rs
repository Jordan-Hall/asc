//! `clean` (SPEC §11.2): removes untracked files, and with `-d` untracked directories. `-x`
//! removes ignored files too, `-X` only ignored ones, `-n` lists what would go and `-f` is
//! required unless `clean.requireForce` is false. Every deletion goes through `asc_wc::SafeRoot`:
//! a path under a link is outside the work tree and is never touched, the walk never follows a
//! link, `.git`, the `.asc` metadata directory and redirected names (SPEC §10.5) are never
//! listed, and a directory holding the current directory is never removed.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use asc_core::path::RepoPath;
use asc_wc::SafeRoot;
use asc_wc::ignore::{IgnoreStack, shadow_warning};
use asc_wc::redirect::Redirections;
use asc_wc::safe_path::is_link;

use super::add::cwd_prefix;
use super::add::spec::Spec;
use super::add::target::Target;
use super::check_ignore::{ignore_sources, relative, write_quoted};
use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "clean", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Show what would be removed.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
    /// Remove (twice: also nested repositories).
    #[arg(short = 'f', long = "force", action = clap::ArgAction::Count)]
    pub force: u8,
    /// Remove untracked directories too.
    #[arg(short = 'd')]
    pub dirs: bool,
    /// Remove ignored files too.
    #[arg(short = 'x')]
    pub ignored: bool,
    /// Remove only ignored files.
    #[arg(short = 'X')]
    pub only_ignored: bool,
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
    pub pathspec: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "clean",
    flags: &["-n", "--dry-run", "-f", "--force", "-d", "-x", "-X", "-q", "--quiet"],
    placeholders: &[],
}];

/// Most entries (files and directories) one walk visits; a bigger tree stops the command.
const MAX_VISITED: usize = 5_000_000;
/// Most tracked paths held for the walk.
const MAX_TRACKED: usize = 10_000_000;
/// Deepest directory nesting walked and removed.
const MAX_DEPTH: usize = 512;

/// The tracked paths, sorted, with the lookups the walk needs.
struct TrackedSet {
    paths: Vec<Vec<u8>>,
    fold: bool,
}

impl TrackedSet {
    fn key(&self, p: &[u8]) -> Vec<u8> {
        if self.fold { String::from_utf8_lossy(p).to_lowercase().into_bytes() } else { p.to_vec() }
    }

    /// Whether `p` itself is tracked (a file, a link or a gitlink).
    fn contains(&self, p: &[u8]) -> bool {
        let key = self.key(p);
        self.paths.binary_search_by(|x| x.as_slice().cmp(&key)).is_ok()
    }

    /// Whether any tracked path is below directory `d`.
    fn has_dir(&self, d: &[u8]) -> bool {
        let mut key = self.key(d);
        key.push(b'/');
        let at = self.paths.partition_point(|p| p.as_slice() < key.as_slice());
        self.paths.get(at).is_some_and(|p| p.starts_with(&key))
    }
}

/// One item to remove.
enum Entry {
    File(Vec<u8>),
    Dir(Vec<u8>),
}

/// What a walked directory holds: whether any file is wanted, whether any is kept, and the
/// entries to list when the directory is not removed whole.
#[derive(Default)]
struct Sum {
    wanted: bool,
    unwanted: bool,
    list: Vec<Entry>,
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() { name.to_vec() } else { [dir, b"/", name].concat() }
}

fn parent(p: &[u8]) -> Option<&[u8]> {
    p.iter().rposition(|&b| b == b'/').map(|i| &p[..i])
}

fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    super::check_ignore::os_bytes(n)
}

fn io_fatal(path: &Path, e: &io::Error) -> CliError {
    CliError::Fatal(format!("{}: {e}", path.display()))
}

struct Walk<'a> {
    gate: &'a SafeRoot,
    tracked: &'a TrackedSet,
    redirections: Redirections,
    ignore: IgnoreStack,
    spec: &'a Spec,
    /// Pathspecs were typed (a default of the current directory is not).
    explicit: bool,
    dirs: bool,
    all: bool,
    only_ignored: bool,
    force2: bool,
    cwd: &'a [u8],
    visited: usize,
    shadowed: BTreeSet<String>,
    /// Directories that hold the current directory and would otherwise have been removed.
    refused: Vec<Vec<u8>>,
    /// Nested repositories `-ff` asked for and asc keeps.
    kept_repos: Vec<Vec<u8>>,
}

impl Walk<'_> {
    fn contains_cwd(&self, d: &[u8]) -> bool {
        self.cwd == d || (self.cwd.starts_with(d) && self.cwd.get(d.len()) == Some(&b'/'))
    }

    /// Whether a typed pathspec names directory `d` whole. git matches a directory entry with its
    /// trailing slash, so `u1/*` names `u1/`.
    fn covered(&self, d: &[u8]) -> bool {
        self.explicit && (self.spec.matches(d) || self.spec.matches(&[d, b"/"].concat()))
    }

    fn wanted(&self, ignored: bool) -> bool {
        if self.only_ignored { ignored } else { self.all || !ignored }
    }

    fn ignored(&self, abs: &Path, is_dir: bool) -> bool {
        self.ignore.explain(abs, is_dir).is_some_and(|h| !h.negated)
    }

    fn visit(&mut self) -> Result<()> {
        self.visited += 1;
        if self.visited > MAX_VISITED {
            return Err(CliError::Fatal(format!("asc: clean walked more than {MAX_VISITED} entries; give a pathspec")));
        }
        Ok(())
    }

    /// Walks directory `rel` (at `abs`, which came from the gate). `inherited` is true when an
    /// ignored directory above holds it: nothing inside can be re-included.
    fn dir(&mut self, rel: &[u8], abs: &Path, inherited: bool, depth: usize) -> Result<Sum> {
        if depth > MAX_DEPTH {
            return Err(CliError::Fatal(format!("asc: clean: directories are nested deeper than {MAX_DEPTH}")));
        }
        if self.ignore.push_dir(abs).map_err(|e| io_fatal(abs, &e))? {
            self.shadowed.insert(String::from_utf8_lossy(rel).into_owned());
        }
        let done = self.entries(rel, abs, inherited, depth);
        self.ignore.pop_dir();
        done
    }

    fn entries(&mut self, rel: &[u8], abs: &Path, inherited: bool, depth: usize) -> Result<Sum> {
        // A real directory (the gate resolved it): the listing never follows a link.
        let listing = fs::read_dir(abs).map_err(|e| io_fatal(abs, &e))?; // lint:allow-worktree-fs
        let mut items: Vec<(Vec<u8>, bool)> = Vec::new();
        // Not a file, a link or a directory (a device, a socket): never touched.
        let mut skipped = false;
        for entry in listing {
            let entry = entry.map_err(|e| io_fatal(abs, &e))?;
            let meta = entry.metadata().map_err(|e| io_fatal(abs, &e))?;
            let is_dir = meta.is_dir() && !is_link(&meta);
            if is_dir || meta.is_file() || is_link(&meta) {
                items.push((name_bytes(&entry.file_name()), is_dir));
            } else {
                skipped = true;
            }
        }
        // git's order: a directory sorts as its name followed by `/`.
        items.sort_by_key(|(n, d)| if *d { [n.as_slice(), b"/"].concat() } else { n.clone() });
        let mut sum = Sum { unwanted: skipped, ..Sum::default() };
        if items.is_empty() && !skipped {
            sum.wanted = !self.only_ignored;
        }
        for (name, is_dir) in items {
            self.visit()?;
            if name == b".git" || (rel.is_empty() && name == b".asc") || self.redirections.is_redirected_name(&name) {
                sum.unwanted = true;
                continue;
            }
            let child = join(rel, &name);
            let Some(child_abs) = RepoPath::new(child.clone()).ok().and_then(|rp| self.gate.resolve(&rp)) else {
                sum.unwanted = true;
                continue;
            };
            if self.tracked.contains(&child) {
                continue; // a tracked file, or a submodule's directory
            }
            if is_dir {
                self.subdir(&child, &child_abs, inherited, depth, &mut sum)?;
            } else {
                let ignored = inherited || self.ignored(&child_abs, false);
                if self.wanted(ignored) {
                    sum.wanted = true;
                    if self.spec.matches(&child) {
                        sum.list.push(Entry::File(child));
                    }
                } else {
                    sum.unwanted = true;
                }
            }
        }
        Ok(sum)
    }

    fn subdir(&mut self, child: &[u8], child_abs: &Path, inherited: bool, depth: usize, sum: &mut Sum) -> Result<()> {
        let ignored = inherited || self.ignored(child_abs, true);
        if self.tracked.has_dir(child) {
            if self.spec.may_contain_dir(child) {
                let inner = self.dir(child, child_abs, ignored, depth + 1)?;
                sum.list.extend(inner.list);
            }
            return Ok(());
        }
        if !self.spec.may_contain_dir(child) {
            return Ok(());
        }
        let covered = self.covered(child);
        let collapse = (self.dirs && !self.explicit && self.spec.matches(child)) || covered;
        let nested = fs::symlink_metadata(child_abs.join(".git")).is_ok();
        if nested {
            // `.git` is not a path the gate can delete, so even `-ff` keeps a nested repository.
            if self.force2 {
                self.kept_repos.push(child.to_vec());
            }
            sum.unwanted = true;
            return Ok(());
        }
        let descend = self.dirs || self.explicit || self.only_ignored;
        if (!collapse && !descend)
            // An ignored directory is a unit: without `-d` or a pathspec naming it, skip it.
            || (ignored && !collapse && !self.explicit)
        {
            return Ok(());
        }
        let inner = self.dir(child, child_abs, ignored, depth + 1)?;
        if !collapse && self.only_ignored && !self.explicit && !inner.unwanted {
            // Only ignored content: git treats the directory as an ignored unit, which needs `-d`.
            return Ok(());
        }
        if collapse && inner.wanted && !inner.unwanted {
            if self.contains_cwd(child) {
                self.refused.push(child.to_vec());
                sum.list.extend(inner.list);
            } else {
                sum.list.push(Entry::Dir(child.to_vec()));
            }
            sum.wanted = true;
        } else {
            sum.wanted |= inner.wanted;
            sum.unwanted |= inner.unwanted;
            sum.list.extend(inner.list);
        }
        Ok(())
    }
}

/// Removes the contents of directory `rel` through the gate, and the directory itself unless
/// `keep_root`. `Ok(false)` when something was left: a nested repository (asc never deletes a
/// `.git`), a special file or a directory still in use.
fn remove_tree(gate: &SafeRoot, rel: &[u8], keep_root: bool, depth: usize) -> io::Result<bool> {
    if depth > MAX_DEPTH {
        return Err(io::Error::other("directories are nested too deeply"));
    }
    let Some(abs) = RepoPath::new(rel.to_vec()).ok().and_then(|rp| gate.resolve(&rp)) else {
        return Ok(false);
    };
    let meta = fs::symlink_metadata(&abs)?;
    if is_link(&meta) || !meta.is_dir() {
        return Ok(false);
    }
    let mut clear = true;
    let listing = fs::read_dir(&abs)?; // lint:allow-worktree-fs
    for entry in listing {
        let entry = entry?;
        let name = name_bytes(&entry.file_name());
        let child = join(rel, &name);
        let Ok(rp) = RepoPath::new(child.clone()) else {
            clear = false;
            continue;
        };
        let cm = entry.metadata()?;
        if name == b".git" {
            clear = false;
        } else if is_link(&cm) || cm.is_file() {
            gate.remove_file(&rp)?;
        } else if cm.is_dir() && fs::symlink_metadata(entry.path().join(".git")).is_err() {
            clear &= remove_tree(gate, &child, false, depth + 1)?;
        } else {
            clear = false;
        }
    }
    if !clear {
        return Ok(false);
    }
    if keep_root {
        return Ok(true);
    }
    // The gate removes empty directories only as parents of a path; `rel/x` makes `rel` the first.
    let probe = RepoPath::new(join(rel, b"x")).map_err(|e| io::Error::other(format!("{e}")))?;
    gate.remove_empty_parents(&probe);
    Ok(fs::symlink_metadata(&abs).is_err())
}

impl Walk<'_> {
    /// Whether removing directory `d` would also remove only parents that are safe to lose:
    /// `SafeRoot` removes the directories a removal leaves empty, innermost first.
    fn climb_safe(&self, d: &[u8]) -> bool {
        let mut cur = d;
        while let Some(p) = parent(cur) {
            let Some(abs) = RepoPath::new(p.to_vec()).ok().and_then(|rp| self.gate.resolve(&rp)) else {
                return false;
            };
            let count = fs::read_dir(&abs).map(|l| l.take(2).count()); // lint:allow-worktree-fs
            match count {
                Ok(n) if n > 1 => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
            if self.contains_cwd(p) || self.tracked.has_dir(p) || !self.spec.matches(p) {
                return false;
            }
            cur = p;
        }
        true
    }
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    if a.ignored && a.only_ignored {
        return Err(CliError::Fatal("options '-x' and '-X' cannot be used together".into()));
    }
    let ws = ctx.ws()?;
    let require = ctx.config.bool("clean.requireForce").unwrap_or(true);
    if !a.dry_run && a.force == 0 && require {
        return Err(CliError::Fatal("clean.requireForce is true and -f not given: refusing to clean".into()));
    }
    let root = ws.projection.toplevel();
    let prefix = cwd_prefix(ctx, ws);
    let explicit = !a.pathspec.is_empty();
    let mut items = a.pathspec.clone();
    if items.is_empty() && !prefix.is_empty() {
        items.push(OsString::from("."));
    }
    let spec = Spec::parse(&items, &prefix, root)?;

    let target = Target::open(ctx).await?;
    let fold = ctx.config.bool("core.ignorecase").unwrap_or(cfg!(any(windows, target_os = "macos")));
    let mut set = TrackedSet { paths: Vec::new(), fold };
    let mut too_many = false;
    target
        .tracked(ws, &Spec::default(), |path, _| {
            if set.paths.len() >= MAX_TRACKED {
                too_many = true;
                return Ok(());
            }
            let key = set.key(path.as_bytes());
            set.paths.push(key);
            Ok(())
        })
        .await?;
    if too_many {
        return Err(CliError::Fatal(format!("asc: clean: more than {MAX_TRACKED} tracked paths")));
    }
    set.paths.sort();

    let gate = SafeRoot::new(root);
    let mut walk = Walk {
        gate: &gate,
        tracked: &set,
        redirections: Redirections::load(root).map_err(|e| io_fatal(root, &e))?,
        ignore: IgnoreStack::new(root, &ignore_sources(ctx, ws)),
        spec: &spec,
        explicit,
        dirs: a.dirs,
        all: a.ignored,
        only_ignored: a.only_ignored,
        force2: a.force >= 2,
        cwd: prefix.as_bytes(),
        visited: 0,
        shadowed: BTreeSet::new(),
        refused: Vec::new(),
        kept_repos: Vec::new(),
    };
    let sum = walk.dir(b"", root, false, 0)?;
    for dir in &walk.shadowed {
        eprintln!("{}", shadow_warning(dir));
    }
    for repo in &walk.kept_repos {
        eprintln!(
            "warning: asc: not removing the nested repository {}/ (asc never deletes a .git directory)",
            String::from_utf8_lossy(repo)
        );
    }

    let quote_high = ctx.config.bool("core.quotePath").unwrap_or(true);
    let strip = if prefix.is_empty() { Vec::new() } else { format!("{prefix}/").into_bytes() };
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut errors = 0usize;
    let say = |out: &mut dyn Write, head: &str, name: &[u8]| -> io::Result<()> {
        out.write_all(head.as_bytes())?;
        write_quoted(out, name, quote_high)?;
        out.write_all(&[10])
    };
    if !a.quiet && !walk.refused.is_empty() {
        let head = if a.dry_run {
            "Would refuse to remove current working directory"
        } else {
            "Refusing to remove current working directory"
        };
        let _ = writeln!(out, "{head}");
    }
    let (verb_dry, verb) = ("Would remove ", "Removing ");
    for entry in &sum.list {
        let (path, is_dir) = match entry {
            Entry::File(p) => (p.as_slice(), false),
            Entry::Dir(p) => (p.as_slice(), true),
        };
        let shown = {
            let mut s = relative(path, &strip);
            if is_dir {
                s.push(b'/');
            }
            s
        };
        if a.dry_run {
            if !a.quiet && stopped(say(&mut out, verb_dry, &shown))? {
                break;
            }
            continue;
        }
        let gone = if is_dir {
            let safe = walk.climb_safe(path);
            let done = remove_tree(&gate, path, !safe, 0);
            if safe || !matches!(done, Ok(true)) {
                done
            } else {
                // The gate would also take the parents the removal leaves empty, which are not
                // all named: the contents are gone, the empty directory stays.
                let _ = out.flush();
                eprintln!(
                    "warning: asc: left the empty directory {} (removing it would also remove an unnamed parent)",
                    String::from_utf8_lossy(&shown)
                );
                Ok(true)
            }
        } else {
            RepoPath::new(path.to_vec())
                .map_err(|e| io::Error::other(format!("{e}")))
                .and_then(|rp| gate.remove_file(&rp))
                .map(|_| true)
        };
        match gone {
            Ok(true) => {
                if !a.quiet && stopped(say(&mut out, verb, &shown))? {
                    break;
                }
            }
            Ok(false) => {
                let _ = out.flush();
                eprintln!("warning: failed to remove {}: something in it could not be removed", String::from_utf8_lossy(&shown));
                errors += 1;
            }
            Err(e) => {
                let _ = out.flush();
                eprintln!("warning: failed to remove {}: {e}", String::from_utf8_lossy(&shown));
                errors += 1;
            }
        }
    }
    match out.flush() {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e.into()),
        _ => {}
    }
    Ok(ExitCode::from(u8::from(errors > 0)))
}

/// Whether output stopped because stdout closed; any other write error is an error.
fn stopped(written: io::Result<()>) -> Result<bool> {
    match written {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_lookups() {
        let mut set = TrackedSet { paths: vec![b"a/b".to_vec(), b"a.txt".to_vec(), b"sub".to_vec()], fold: false };
        set.paths.sort();
        assert!(set.contains(b"a.txt") && set.contains(b"sub") && !set.contains(b"a"));
        assert!(set.has_dir(b"a") && !set.has_dir(b"a.txt") && !set.has_dir(b"b"));
        let folded = TrackedSet { paths: vec![b"readme".to_vec()], fold: true };
        assert!(folded.contains(b"README"));
    }

    #[test]
    fn parents() {
        assert_eq!(parent(b"a/b/c"), Some(&b"a/b"[..]));
        assert_eq!(parent(b"a"), None);
    }
}
