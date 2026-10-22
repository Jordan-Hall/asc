//! `$ASC_HOME`: location and layout (SPEC §3.5).

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Object classes with a directory under `cache/` and `store/` (SPEC §3.1).
const OBJECT_DIRS: [&str; 9] = ["f", "l", "k", "t", "c", "g", "o", "m", "d"];
/// Top-level directories created with the home.
const TOP_DIRS: [&str; 10] = [
    "cache", "store", "tmp", "repos", "wc", "wt", "redirect", "tools", "run", "log",
];

/// Failures locating or creating `$ASC_HOME`.
#[derive(Debug, thiserror::Error)]
pub enum HomeError {
    #[error("cannot determine a default $ASC_HOME: set ASC_HOME")]
    NoDefault,
    #[error("gitdir {0} is not <home>/wc/<workspace>/git")]
    NotWorkspaceGitdir(PathBuf),
    #[error("cannot create {path}: {source}")]
    Create { path: PathBuf, source: io::Error },
}

macro_rules! uuid_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A fresh random id.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.as_hyphenated().fmt(f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    };
}

uuid_newtype!(
    /// Stable id of a repository (`repos.id`, `repos/<repo-id>/`).
    RepoId
);
uuid_newtype!(
    /// Stable id of a workspace (`workspaces.id`, `wc/<ws-id>/`).
    WorkspaceId
);

/// A resolved `$ASC_HOME` directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AscHome {
    root: PathBuf,
}

impl AscHome {
    /// Outside a workspace: env `ASC_HOME`, else global git config `asc.home`, else the platform
    /// default. Creates the SPEC §3.5 directories.
    pub fn locate() -> Result<Self, HomeError> {
        let configured = gix_config::File::from_globals().ok().and_then(|c| {
            let path = c.path("asc.home")?;
            path.interpolate(Default::default()).ok()
        });
        Self::resolve(|k| std::env::var_os(k), configured)
    }

    /// [`AscHome::locate`] with the environment and the `asc.home` value supplied by the caller.
    pub fn resolve(
        env: impl Fn(&str) -> Option<OsString>,
        configured: Option<PathBuf>,
    ) -> Result<Self, HomeError> {
        let non_empty = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let root = match non_empty("ASC_HOME").or(configured) {
            Some(root) => root,
            None => default_root(&non_empty)?,
        };
        Self::open(root)
    }

    /// Inside a workspace: derives the home from the gitfile target `<home>/wc/<ws>/git`.
    /// `ASC_HOME` and `asc.home` are never consulted (SPEC §3.5).
    pub fn for_workspace(gitdir: &Path) -> Result<Self, HomeError> {
        let bad = || HomeError::NotWorkspaceGitdir(gitdir.to_owned());
        if gitdir.file_name().is_none_or(|n| n != "git") {
            return Err(bad());
        }
        let ws = gitdir.parent().ok_or_else(bad)?;
        let wc = ws.parent().ok_or_else(bad)?;
        if wc.file_name().is_none_or(|n| n != "wc") {
            return Err(bad());
        }
        let root = wc.parent().ok_or_else(bad)?;
        Self::open(root.to_owned())
    }

    /// Uses `root` as the home, creating the layout.
    pub fn open(root: PathBuf) -> Result<Self, HomeError> {
        let create = |path: PathBuf| {
            std::fs::create_dir_all(&path).map_err(|source| HomeError::Create { path, source })
        };
        create(root.clone())?;
        for top in TOP_DIRS {
            create(root.join(top))?;
        }
        for tier in ["cache", "store"] {
            for class in OBJECT_DIRS {
                create(root.join(tier).join(class))?;
            }
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_db(&self) -> PathBuf {
        self.root.join("state.db")
    }

    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn run(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn log(&self) -> PathBuf {
        self.root.join("log")
    }

    /// `repos/<id>/`.
    pub fn repo_dir(&self, repo: RepoId) -> PathBuf {
        self.root.join("repos").join(repo.to_string())
    }

    /// `repos/<id>/.git`: the git-format common directory (SPEC §9.4).
    pub fn common_dir(&self, repo: RepoId) -> PathBuf {
        self.repo_dir(repo).join(".git")
    }

    /// `wc/<id>/`.
    pub fn workspace_dir(&self, ws: WorkspaceId) -> PathBuf {
        self.root.join("wc").join(ws.to_string())
    }

    /// `wc/<id>/git`: the synthetic per-workspace gitdir.
    pub fn gitdir(&self, ws: WorkspaceId) -> PathBuf {
        self.workspace_dir(ws).join("git")
    }

    /// `wt/<id>`: the mount root of a nested worktree (SPEC §9.3).
    pub fn worktree_mount(&self, ws: WorkspaceId) -> PathBuf {
        self.root.join("wt").join(ws.to_string())
    }
}

fn default_root(var: &impl Fn(&str) -> Option<PathBuf>) -> Result<PathBuf, HomeError> {
    if cfg!(windows) {
        return var("LOCALAPPDATA")
            .map(|p| p.join("asc"))
            .ok_or(HomeError::NoDefault);
    }
    let data = var("XDG_DATA_HOME")
        .or_else(|| var("HOME").map(|h| h.join(".local").join("share")))
        .ok_or(HomeError::NoDefault)?;
    Ok(data.join("asc"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).into())
        }
    }

    #[test]
    fn env_beats_config_beats_default() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        let from_env = AscHome::resolve(
            |k| (k == "ASC_HOME").then(|| a.clone().into_os_string()),
            Some(b.clone()),
        )
        .unwrap();
        assert_eq!(from_env.root(), a);
        let from_cfg = AscHome::resolve(|_| None, Some(b.clone())).unwrap();
        assert_eq!(from_cfg.root(), b);
    }

    #[test]
    fn default_uses_platform_variable() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().to_owned();
        let got = AscHome::resolve(
            |k| {
                (k == "LOCALAPPDATA" || k == "XDG_DATA_HOME").then(|| base.clone().into_os_string())
            },
            None,
        )
        .unwrap();
        assert_eq!(got.root(), base.join("asc"));
        assert!(matches!(
            AscHome::resolve(env(&[]), None),
            Err(HomeError::NoDefault)
        ));
    }

    #[test]
    fn creates_the_layout() {
        let d = tempfile::tempdir().unwrap();
        let home = AscHome::open(d.path().join("h")).unwrap();
        for p in [
            "cache/f", "cache/d", "store/m", "tmp", "repos", "wc", "wt", "run", "log",
        ] {
            assert!(home.root().join(p).is_dir(), "{p}");
        }
    }

    #[test]
    fn workspace_gitdir_gives_the_home_and_ignores_asc_home() {
        let d = tempfile::tempdir().unwrap();
        let home = AscHome::open(d.path().join("h")).unwrap();
        let ws = WorkspaceId::new();
        let derived = AscHome::for_workspace(&home.gitdir(ws)).unwrap();
        assert_eq!(derived, home);
        // `for_workspace` takes no environment, so ASC_HOME cannot override it; resolve() with a
        // different ASC_HOME is the outside-workspace path.
        let other = d.path().join("other");
        let outside = AscHome::resolve(
            |k| (k == "ASC_HOME").then(|| other.clone().into_os_string()),
            None,
        )
        .unwrap();
        assert_ne!(outside, derived);
        assert!(matches!(
            AscHome::for_workspace(&d.path().join("x/y")),
            Err(HomeError::NotWorkspaceGitdir(_))
        ));
    }
}
