//! Creating and discovering the synthetic `.git` layout (SPEC §9.4).

use std::fmt;
use std::path::{Component, Path, PathBuf};

use asc_core::id::GitOid;
use asc_state::home::{AscHome, RepoId, WorkspaceId};

use super::config::{self, Config};
use super::{ProjectionError, Result, write_locked};

/// The default branch a new workspace's `HEAD` names until asc writes the real one.
const DEFAULT_BRANCH: &str = "master";
/// Written to the repo-local exclude file in effect (SPEC §2.4.2).
const WORKTREES_EXCLUDE: &str = ".claude/worktrees/";
const EXCLUDE_HEADER: &str = "# git ls-files --others --exclude-from=.git/info/exclude\n\
# Lines that start with '#' are comments.\n";

/// A validated git remote name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteName(String);

impl RemoteName {
    /// Rejects empty names and names with control characters or double quotes.
    pub fn new(name: &str) -> Result<Self> {
        let bad = name.is_empty() || name.chars().any(|c| c.is_control() || c == '"');
        if bad {
            return Err(ProjectionError::BadRemote(name.to_owned()));
        }
        Ok(Self(name.to_owned()))
    }

    /// The name as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the workspace's `HEAD` file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// `ref: refs/heads/<name>`.
    Branch(String),
    /// A detached git SHA.
    Detached(GitOid),
}

impl Head {
    fn render(&self) -> Result<String> {
        match self {
            Self::Branch(name) => {
                let full = format!("refs/heads/{name}");
                gix::refs::FullName::try_from(full.as_str())
                    .map_err(|_| ProjectionError::BadBranch(name.clone()))?;
                Ok(format!("ref: {full}\n"))
            }
            Self::Detached(oid) => Ok(format!("{oid}\n")),
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let line = text.lines().next()?.trim();
        match line.strip_prefix("ref: refs/heads/") {
            Some(name) if !name.is_empty() => Some(Self::Branch(name.to_owned())),
            Some(_) => None,
            None => line.parse().ok().map(Self::Detached),
        }
    }
}

/// A synthetic `.git` bound to one workspace root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projection {
    toplevel: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    workspace: WorkspaceId,
}

impl Projection {
    /// Writes the gitfile, the per-workspace gitdir and, if absent, the shared common dir.
    pub fn create(home: &AscHome, repo: RepoId, ws: WorkspaceId, root: &Path) -> Result<Self> {
        let git_dir = home.gitdir(ws);
        let common_dir = home.common_dir(repo);
        let gitfile = root.join(".git");
        if gitfile.is_dir() {
            return Err(ProjectionError::RealGitDir(gitfile));
        }
        for dir in [
            root,
            &git_dir,
            &common_dir.join("info"),
            &common_dir.join("hooks"),
        ] {
            std::fs::create_dir_all(dir).map_err(|e| ProjectionError::io(dir, e))?;
        }
        let proj = Self {
            toplevel: root.to_owned(),
            git_dir,
            common_dir,
            workspace: ws,
        };
        let commondir = format!("../../../repos/{repo}/.git\n");
        proj.write(&gitfile, format!("gitdir: {}\n", proj.git_dir.display()))?;
        proj.write(&proj.git_dir.join("commondir"), commondir)?;
        proj.write(&proj.git_dir.join("asc-workspace"), format!("{ws}\n"))?;
        proj.write_head(&Head::Branch(DEFAULT_BRANCH.to_owned()))?;
        proj.init_common()?;
        proj.exclude_worktrees()?;
        Ok(proj)
    }

    /// Discovers the synthetic `.git` by walking up from `root` to the first `.git` entry, which
    /// must be a gitfile whose gitdir holds `asc-workspace`.
    pub fn open(root: &Path) -> Result<Self> {
        for dir in root.ancestors() {
            let gitfile = dir.join(".git");
            let Ok(meta) = std::fs::symlink_metadata(&gitfile) else {
                continue;
            };
            if !meta.is_file() {
                return Err(ProjectionError::NotWorkspace(gitfile));
            }
            return Self::from_gitfile(dir, &gitfile);
        }
        Err(ProjectionError::NotFound(root.to_owned()))
    }

    fn from_gitfile(toplevel: &Path, gitfile: &Path) -> Result<Self> {
        let read = |p: &Path| std::fs::read_to_string(p).map_err(|e| ProjectionError::io(p, e));
        let text = read(gitfile)?;
        let target = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("gitdir:"))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ProjectionError::Malformed(gitfile.to_owned()))?;
        let git_dir = normalise(&toplevel.join(target));
        let marker = git_dir.join("asc-workspace");
        let id = std::fs::read_to_string(&marker)
            .map_err(|_| ProjectionError::NotWorkspace(gitfile.to_owned()))?;
        let workspace = id
            .trim()
            .parse()
            .map_err(|_| ProjectionError::Malformed(marker))?;
        let common = read(&git_dir.join("commondir"))?;
        let common_dir = normalise(&git_dir.join(common.trim()));
        Ok(Self {
            toplevel: toplevel.to_owned(),
            git_dir,
            common_dir,
            workspace,
        })
    }

    /// The same workspace with its gitdir and common dir replaced by `git_dir` and `common_dir`
    /// (the canonical paths a caller checked).
    #[must_use]
    pub fn with_dirs(mut self, git_dir: PathBuf, common_dir: PathBuf) -> Self {
        self.git_dir = git_dir;
        self.common_dir = common_dir;
        self
    }

    /// The workspace root holding the `.git` gitfile.
    pub fn toplevel(&self) -> &Path {
        &self.toplevel
    }

    /// `wc/<ws>/git`.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// `repos/<id>/.git`.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The workspace id from the `asc-workspace` marker.
    pub fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    /// Writes `HEAD` atomically.
    pub fn write_head(&self, head: &Head) -> Result<()> {
        self.write(&self.git_dir.join("HEAD"), head.render()?)
    }

    /// Reads `HEAD`.
    pub fn read_head(&self) -> Result<Head> {
        let path = self.git_dir.join("HEAD");
        let text = std::fs::read_to_string(&path).map_err(|e| ProjectionError::io(&path, e))?;
        Head::parse(&text).ok_or(ProjectionError::Malformed(path))
    }

    /// The merged config (see [`config::load`]).
    pub fn config(&self) -> Result<Config> {
        config::load(self)
    }

    /// Adds remote `name` with the user's `url` verbatim and the clone refspecs (SPEC §9.4).
    pub fn add_remote(&self, name: &RemoteName, url: &str) -> Result<()> {
        config::add_remote(&self.common_dir, name, url)
    }

    /// `core.hooksPath` (relative to the workspace root) if set, else `<common>/hooks`.
    pub fn hooks_dir(&self) -> Result<PathBuf> {
        let configured = self.config()?.path("core.hooksPath");
        Ok(match configured {
            Some(path) => self.toplevel.join(path),
            None => self.common_dir.join("hooks"),
        })
    }

    /// The repo-local exclude file in effect: `.asc/info/exclude` if it exists, else
    /// `<common>/info/exclude` (SPEC §2.4.2).
    pub fn exclude_file(&self) -> PathBuf {
        let asc = self.toplevel.join(".asc").join("info").join("exclude");
        if asc.is_file() {
            asc
        } else {
            self.common_dir.join("info").join("exclude")
        }
    }

    fn write(&self, path: &Path, text: String) -> Result<()> {
        write_locked(path, text.as_bytes())
    }

    /// Writes the shared `config` and `info/exclude` unless another workspace already did.
    fn init_common(&self) -> Result<()> {
        let config_path = self.common_dir.join("config");
        if !config_path.exists() {
            config::write_file(&config_path, &config::initial_file()?)?;
        }
        let exclude = self.common_dir.join("info").join("exclude");
        if !exclude.exists() {
            write_locked(&exclude, EXCLUDE_HEADER.as_bytes())?;
        }
        Ok(())
    }

    fn exclude_worktrees(&self) -> Result<()> {
        let path = self.exclude_file();
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        if text.lines().any(|l| l.trim_end() == WORKTREES_EXCLUDE) {
            return Ok(());
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(WORKTREES_EXCLUDE);
        text.push('\n');
        write_locked(&path, text.as_bytes())
    }
}

/// Resolves `.` and `..` lexically; the paths involved may not exist yet.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realgit::RealGit;

    struct Fixture {
        _dir: tempfile::TempDir,
        home: AscHome,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tmp");
        let home = AscHome::open(dir.path().join("home")).expect("home");
        let root = dir.path().join("ws");
        Fixture {
            _dir: dir,
            home,
            root,
        }
    }

    #[test]
    fn create_then_open_returns_the_expected_paths() {
        let f = fixture();
        let (repo, ws) = (RepoId::new(), WorkspaceId::new());
        Projection::create(&f.home, repo, ws, &f.root).expect("create");
        let sub = f.root.join("a").join("b");
        std::fs::create_dir_all(&sub).expect("sub");

        let p = Projection::open(&sub).expect("open");
        assert_eq!(p.toplevel(), f.root);
        assert_eq!(p.git_dir(), f.home.gitdir(ws));
        assert_eq!(p.common_dir(), f.home.common_dir(repo));
        assert_eq!(p.workspace(), ws);
        assert_eq!(p.read_head().expect("head"), Head::Branch("master".into()));
    }

    #[test]
    fn gitdir_holds_nothing_but_the_named_files() {
        let f = fixture();
        let ws = WorkspaceId::new();
        let p = Projection::create(&f.home, RepoId::new(), ws, &f.root).expect("create");
        let mut names: Vec<_> = std::fs::read_dir(p.git_dir())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["HEAD", "asc-workspace", "commondir"]);
        let mut common: Vec<_> = std::fs::read_dir(p.common_dir())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        common.sort();
        assert_eq!(common, ["config", "hooks", "info"]);
    }

    #[test]
    fn write_head_round_trips_branch_and_detached() {
        let f = fixture();
        let p = Projection::create(&f.home, RepoId::new(), WorkspaceId::new(), &f.root)
            .expect("create");
        let detached = Head::Detached(GitOid::from_bytes([0xab; 20]));
        for head in [Head::Branch("feature/x".into()), detached] {
            p.write_head(&head).expect("write");
            assert_eq!(p.read_head().expect("read"), head);
        }
        let raw = std::fs::read_to_string(p.git_dir().join("HEAD")).expect("raw");
        assert_eq!(raw, format!("{}\n", "ab".repeat(20)));
        assert!(matches!(
            p.write_head(&Head::Branch("bad..name".into())),
            Err(ProjectionError::BadBranch(_))
        ));
    }

    #[test]
    fn open_rejects_real_git_dirs_and_missing_gitfiles() {
        let f = fixture();
        std::fs::create_dir_all(f.root.join(".git")).expect("mkdir");
        assert!(matches!(
            Projection::open(&f.root),
            Err(ProjectionError::NotWorkspace(_))
        ));
        let g = fixture();
        std::fs::create_dir_all(&g.root).expect("mkdir");
        std::fs::write(g.root.join(".git"), "gitdir: nowhere\n").expect("write");
        assert!(Projection::open(&g.root).is_err());
    }

    #[test]
    fn worktrees_dir_is_excluded_once_in_the_effective_file() {
        let f = fixture();
        let repo = RepoId::new();
        let p = Projection::create(&f.home, repo, WorkspaceId::new(), &f.root).expect("create");
        let common = p.common_dir().join("info").join("exclude");
        let second = f.root.with_file_name("ws2");
        Projection::create(&f.home, repo, WorkspaceId::new(), &second).expect("create");
        let text = std::fs::read_to_string(&common).expect("exclude");
        assert_eq!(text.matches(WORKTREES_EXCLUDE).count(), 1);

        let third = f.root.with_file_name("ws3");
        let asc_exclude = third.join(".asc").join("info").join("exclude");
        std::fs::create_dir_all(asc_exclude.parent().expect("parent")).expect("mkdir");
        std::fs::write(&asc_exclude, "target/\n").expect("write");
        Projection::create(&f.home, repo, WorkspaceId::new(), &third).expect("create");
        let text = std::fs::read_to_string(&asc_exclude).expect("exclude");
        assert_eq!(text, "target/\n.claude/worktrees/\n");
    }

    #[test]
    fn hooks_dir_defaults_and_follows_core_hooks_path() {
        let f = fixture();
        let p = Projection::create(&f.home, RepoId::new(), WorkspaceId::new(), &f.root)
            .expect("create");
        assert_eq!(p.hooks_dir().expect("hooks"), p.common_dir().join("hooks"));
        let cfg = p.common_dir().join("config");
        let mut text = std::fs::read_to_string(&cfg).expect("config");
        text.push_str("[core]\n\thooksPath = .husky/_\n");
        std::fs::write(&cfg, text).expect("write");
        assert_eq!(p.hooks_dir().expect("hooks"), f.root.join(".husky/_"));
    }

    #[test]
    fn real_git_does_not_treat_the_workspace_as_a_repository() {
        let f = fixture();
        Projection::create(&f.home, RepoId::new(), WorkspaceId::new(), &f.root).expect("create");
        let git = RealGit::resolve(None, None).expect("real git");
        let out = git
            .command()
            .expect("command")
            .args(["status"])
            .current_dir(&f.root)
            .env("GIT_CEILING_DIRECTORIES", f.root.parent().expect("parent"))
            .output()
            .expect("run");
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("not a git repository"), "stderr: {err}");
    }
}
