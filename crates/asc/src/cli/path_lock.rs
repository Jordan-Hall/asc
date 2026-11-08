//! Path rules on disk (SPEC §15.5.1): files the path rules let the caller read but not write are
//! read-only in the work tree, and `add` and `commit` refuse changes to them. The repository level
//! alone locks nothing: as in git, a read-only credential still commits locally and the server
//! refuses the push.
//!
//! `fetch`, `pull` and `clone` from the default remote ask the server for the caller's own levels
//! (`PathRules.GetMyPathAccess`) and keep them in `asc-path-access` beside the shared config.
//! After every command that writes the work tree the files below a read-only level get the
//! read-only bit; the files this worktree locked are listed in its `asc-path-locked`, so a level
//! that rises unlocks them again. On Windows a locked file cannot be replaced, so those commands
//! first unlock the listed files and lock them again after.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use asc_core::pathacl::Level;
use asc_rpc::server::wire;
use asc_rpc::services::path_rules_client::PathRulesClient;

use super::common::{CliError, Ctx, Result, WsCtx};
use crate::remote::Opened;

const ACCESS_FILE: &str = "asc-path-access";
const LOCKED_FILE: &str = "asc-path-locked";
const HEADER: &str = "asc path-access 1";

/// The caller's levels on one repository, as the server sent them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    pub remote: String,
    pub repo_level: Level,
    /// Rule directories (`""` = the root) with the level they give, sorted.
    pub dirs: Vec<(String, Level)>,
}

impl Access {
    /// The level on `path`: the deepest listed ancestor's, else the repository's, capped at the
    /// repository's.
    pub fn level(&self, path: &str) -> Level {
        self.rule_level(path)
            .map_or(self.repo_level, |l| l.min(self.repo_level))
    }

    /// The level the path rules give `path`, when a listed directory holds it.
    fn rule_level(&self, path: &str) -> Option<Level> {
        self.dirs
            .iter()
            .filter(|(d, _)| under(path, d))
            .max_by_key(|(d, _)| d.len())
            .map(|(_, l)| *l)
    }

    /// Whether the path rules leave `path` writable (see the module docs).
    pub fn writable(&self, path: &str) -> bool {
        self.rule_level(path).is_none_or(|l| l >= Level::Write)
    }

    /// Whether the path rules keep some path below write.
    pub fn limits(&self) -> bool {
        self.dirs.iter().any(|(_, l)| *l < Level::Write)
    }

    fn render(&self) -> String {
        let mut s = format!(
            "{HEADER}\nremote {}\nrepo {}\n",
            self.remote,
            self.repo_level.as_str()
        );
        for (d, l) in &self.dirs {
            s.push_str(&format!("dir {}\t{d}\n", l.as_str()));
        }
        s
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != HEADER {
            return None;
        }
        let remote = lines.next()?.strip_prefix("remote ")?.to_owned();
        let repo_level = Level::parse(lines.next()?.strip_prefix("repo ")?)?;
        let mut dirs = Vec::new();
        for line in lines {
            let (l, d) = line.strip_prefix("dir ")?.split_once('\t')?;
            if !safe_rel(d) {
                return None;
            }
            dirs.push((d.to_owned(), Level::parse(l)?));
        }
        Some(Self {
            remote,
            repo_level,
            dirs,
        })
    }
}

/// Whether `p` is a plain repository-relative path (`""` = the root): `/`-separated names, none
/// empty, `.`, `..` or `.git`, and no backslash, colon, NUL or line break, so it never leaves the
/// work tree. Server-sent and stored paths both pass through this.
fn safe_rel(p: &str) -> bool {
    p.is_empty()
        || (!p.contains(['\\', ':', '\0', '\n', '\r'])
            && p.split('/')
                // Windows drops trailing dots and spaces and expands `~` short names, so
                // `.git.` or `GIT~1` would reach the git dir.
                .all(|c| {
                    !c.is_empty()
                        && !c.ends_with(['.', ' '])
                        && !c.contains('~')
                        && !c.eq_ignore_ascii_case(".git")
                }))
}

/// Whether `path` is `dir` or below it (`""` holds everything).
fn under(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path == dir
        || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

fn access_path(ws: &WsCtx) -> PathBuf {
    ws.projection.common_dir().join(ACCESS_FILE)
}

fn locked_path(ws: &WsCtx) -> PathBuf {
    ws.projection.git_dir().join(LOCKED_FILE)
}

/// The stored levels of this repository, when the server sent any that limit something.
pub fn load(ws: &WsCtx) -> Option<Access> {
    let text = fs::read_to_string(access_path(ws)).ok()?;
    Access::parse(&text)
}

/// A check of whether the path rules keep a path read-only, for a mounted worktree: it reads the
/// stored levels again when the file changes (a fetch refreshes it), at most once a second.
pub fn live_check(ws: &WsCtx) -> impl Fn(&str) -> bool + Send + Sync + 'static {
    live_check_at(access_path(ws), Duration::from_secs(1))
}

fn live_check_at(path: PathBuf, every: Duration) -> impl Fn(&str) -> bool + Send + Sync + 'static {
    struct Seen {
        checked: Instant,
        stamp: Option<SystemTime>,
        access: Option<Access>,
    }
    let stamp = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    let read = |p: &Path| fs::read_to_string(p).ok().and_then(|t| Access::parse(&t));
    let seen = Mutex::new(Seen {
        checked: Instant::now(),
        stamp: stamp(&path),
        access: read(&path),
    });
    move |p| {
        let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
        if s.checked.elapsed() >= every {
            s.checked = Instant::now();
            let now = stamp(&path);
            if now != s.stamp {
                s.stamp = now;
                s.access = read(&path);
            }
        }
        s.access.as_ref().is_some_and(|a| !a.writable(p))
    }
}

fn store(ws: &WsCtx, access: Option<&Access>) -> Result<()> {
    let path = access_path(ws);
    match access {
        Some(a) if a.limits() => write_atomic(&path, a.render().as_bytes()),
        _ => match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CliError::Fatal(format!("asc: {}: {e}", path.display()))),
        },
    }
}

/// `Err` naming the first of `paths` the stored levels keep read-only.
pub fn require_writable<'p>(ws: &WsCtx, paths: impl IntoIterator<Item = &'p str>) -> Result<()> {
    match load(ws) {
        Some(access) => require_writable_in(&access, paths),
        None => Ok(()),
    }
}

/// `Err` naming the first of `paths` that `access` keeps read-only.
pub fn require_writable_in<'p>(
    access: &Access,
    paths: impl IntoIterator<Item = &'p str>,
) -> Result<()> {
    for p in paths {
        if !access.writable(p) {
            return Err(CliError::Fatal(format!(
                "asc: {p}: read-only for you by the repository's path rules ({})",
                access.rule_level(p).unwrap_or(Level::None).as_str()
            )));
        }
    }
    Ok(())
}

/// Asks the server behind `opened` for the caller's levels and keeps them, when `opened` is the
/// default remote; then locks the work tree. A server without `GetMyPathAccess` or a caller it
/// refuses leaves nothing limited; an unreachable one keeps what was stored.
pub async fn refresh(ctx: &Ctx, ws: &WsCtx, opened: &Opened) -> Result<()> {
    let Some(server) = &opened.server else {
        return Ok(());
    };
    if !opened.configured || super::fetch::default_remote(ctx, ws)? != opened.name {
        return Ok(());
    }
    let session = server.session();
    let msg = wire::GetMyPathAccessRequest {
        repo: session.wire.repo.clone(),
    };
    let answer = PathRulesClient::new(session.wire.channel.clone())
        .get_my_path_access(session.wire.request(msg, None))
        .await;
    let access = match answer {
        Ok(r) => {
            let r = r.into_inner();
            let level = |s: &str| {
                Level::parse(s).ok_or_else(|| {
                    CliError::Fatal(format!("asc: the server sent an unknown level {s:?}"))
                })
            };
            let mut dirs = Vec::with_capacity(r.dirs.len());
            for d in r.dirs {
                if !safe_rel(&d.path) {
                    return Err(CliError::Fatal(format!(
                        "asc: the server sent an invalid path rule directory {:?}",
                        d.path
                    )));
                }
                dirs.push((d.path, level(&d.level)?));
            }
            dirs.sort();
            Some(Access {
                remote: opened.name.clone(),
                repo_level: level(&r.repo_level)?,
                dirs,
            })
        }
        Err(s)
            if matches!(
                s.code(),
                tonic::Code::Unimplemented | tonic::Code::NotFound | tonic::Code::PermissionDenied
            ) =>
        {
            None
        }
        Err(_) => return Ok(()),
    };
    store(ws, access.as_ref())?;
    lock(ws)
}

/// Before a command that writes the work tree: on Windows a read-only file can be neither
/// replaced nor removed, so the files this worktree locked are made writable until [`lock`].
pub fn before_write(ws: &WsCtx) {
    if cfg!(windows) {
        let root = ws.projection.toplevel();
        for p in read_locked(ws) {
            let _ = set_readonly(&root.join(&p), false);
        }
    }
}

/// Makes every tracked-looking file below a read-only level read-only, and writable again any
/// file this worktree locked that is no longer below one.
pub fn lock(ws: &WsCtx) -> Result<()> {
    let root = ws.projection.toplevel().to_owned();
    let before = read_locked(ws);
    let access = load(ws);
    let mut now = BTreeSet::new();
    if let Some(a) = &access {
        // The outermost read-only directories; a writable one inside is skipped file by file.
        let low = |l: &Level| *l < Level::Write;
        let starts: Vec<String> = a
            .dirs
            .iter()
            .filter(|(d, l)| {
                low(l)
                    && !a
                        .dirs
                        .iter()
                        .any(|(o, ol)| low(ol) && o != d && under(d, o))
            })
            .map(|(d, _)| d.clone())
            .collect();
        for start in starts {
            walk(&root, &start, &mut |rel, abs| {
                if !a.writable(rel) {
                    set_readonly(abs, true)
                        .map_err(|e| CliError::Fatal(format!("asc: {rel}: {e}")))?;
                    now.insert(rel.to_owned());
                }
                Ok(())
            })?;
        }
    }
    for p in before.difference(&now) {
        let _ = set_readonly(&root.join(p), false);
    }
    write_locked(ws, &now)
}

fn read_locked(ws: &WsCtx) -> BTreeSet<String> {
    fs::read_to_string(locked_path(ws))
        .map(|t| {
            t.lines()
                .filter(|l| safe_rel(l))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn write_locked(ws: &WsCtx, set: &BTreeSet<String>) -> Result<()> {
    let path = locked_path(ws);
    if set.is_empty() {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CliError::Fatal(format!("asc: {}: {e}", path.display()))),
        };
    }
    let mut text = String::new();
    for p in set {
        text.push_str(p);
        text.push('\n');
    }
    write_atomic(&path, text.as_bytes())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| CliError::Fatal(format!("asc: {}: {e}", path.display())))
}

/// Calls `f(relative, absolute)` for every regular file at or below `start`, without following
/// links, skipping `.git` and directories that hold another repository (a mount or submodule).
fn walk(root: &Path, start: &str, f: &mut dyn FnMut(&str, &Path) -> Result<()>) -> Result<()> {
    let mut stack = vec![start.to_owned()];
    while let Some(rel) = stack.pop() {
        let abs = if rel.is_empty() {
            root.to_owned()
        } else {
            root.join(&rel)
        };
        let Ok(meta) = asc_wc::shared::metadata(&abs) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_file() {
            f(&rel, &abs)?;
            continue;
        }
        if !meta.is_dir() || (!rel.is_empty() && abs.join(".git").exists()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&abs) else {
            continue;
        };
        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name == ".git" {
                continue;
            }
            stack.push(if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_readonly(path: &Path, on: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Ok(());
    }
    let mode = meta.permissions().mode();
    let want = if on { mode & !0o222 } else { mode | 0o200 };
    if want == mode {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(want))
}

#[cfg(not(unix))]
#[allow(clippy::permissions_set_readonly_false)] // Windows only: clears the read-only attribute.
fn set_readonly(path: &Path, on: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Ok(());
    }
    let mut perms = meta.permissions();
    if perms.readonly() == on {
        return Ok(());
    }
    perms.set_readonly(on);
    fs::set_permissions(path, perms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access() -> Access {
        Access {
            remote: "origin".into(),
            repo_level: Level::Write,
            dirs: vec![
                ("".into(), Level::Write),
                ("docs".into(), Level::Read),
                ("docs/drafts".into(), Level::Write),
                ("secret".into(), Level::None),
            ],
        }
    }

    #[test]
    fn the_live_check_follows_the_stored_file() {
        let dir = std::env::temp_dir().join(format!("asc-live-check-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ACCESS_FILE);
        let _ = fs::remove_file(&path);
        let check = live_check_at(path.clone(), Duration::ZERO);
        assert!(!check("docs/a.md"));
        fs::write(&path, access().render()).unwrap();
        assert!(check("docs/a.md") && check("secret/x") && !check("docs/drafts/b.md"));
        assert!(!check("src/main.rs"));
        fs::remove_file(&path).unwrap();
        assert!(!check("docs/a.md"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_deepest_listed_directory_decides() {
        let a = access();
        assert_eq!(a.level("src/main.rs"), Level::Write);
        assert_eq!(a.level("docs/a.md"), Level::Read);
        assert_eq!(a.level("docs/drafts/b.md"), Level::Write);
        assert_eq!(a.level("docsx/a.md"), Level::Write);
        assert_eq!(a.level("secret"), Level::None);
        assert!(a.limits());
        let reader = Access {
            repo_level: Level::Read,
            ..a
        };
        assert_eq!(reader.level("docs/drafts/b.md"), Level::Read);
        assert!(
            reader.writable("src/main.rs"),
            "the repository level locks nothing"
        );
        assert!(!reader.writable("docs/a.md"));
    }

    #[test]
    fn only_plain_relative_paths_are_accepted() {
        for good in ["", "docs", "a/b.c/d"] {
            assert!(safe_rel(good), "{good:?}");
        }
        for bad in [
            "..",
            "a/../b",
            "/abs",
            "a//b",
            "./a",
            "a\\b",
            "C:/x",
            ".git/hooks",
            "a\nb",
            ".git./x",
            "GIT~1/x",
            "a /b",
        ] {
            assert!(!safe_rel(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_stored_form_round_trips() {
        let a = access();
        assert_eq!(Access::parse(&a.render()), Some(a));
        assert_eq!(Access::parse("something else"), None);
    }

    #[test]
    fn locking_sets_and_clears_the_read_only_bit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("docs/drafts")).unwrap();
        fs::write(root.join("docs/a.md"), "a").unwrap();
        fs::write(root.join("docs/drafts/b.md"), "b").unwrap();
        let a = access();
        let mut seen = Vec::new();
        walk(root, "docs", &mut |rel, abs| {
            if !a.writable(rel) {
                set_readonly(abs, true).unwrap();
            }
            seen.push(rel.to_owned());
            Ok(())
        })
        .unwrap();
        seen.sort();
        assert_eq!(seen, ["docs/a.md", "docs/drafts/b.md"]);
        let ro = |p: &str| fs::metadata(root.join(p)).unwrap().permissions().readonly();
        assert!(ro("docs/a.md"));
        assert!(!ro("docs/drafts/b.md"));
        set_readonly(&root.join("docs/a.md"), false).unwrap();
        assert!(!ro("docs/a.md"));
    }
}
