//! Git-format config of a workspace (SPEC §9.4): the common dir's `config` written with
//! `gix-config`, and the merged view (system, global, local) with `includeIf` evaluated against
//! `<root>/.git`, the path a plain clone would have.

use std::path::{Path, PathBuf};

use gix_config::file::{Metadata, includes, init};
use gix_config::path::interpolate;
use gix_config::{File, Source};

use super::layout::{Head, Projection, RemoteName};
use super::{ProjectionError, Result, write_locked};

/// The clone refspecs for remote `name`: all branches, minus agent branches (SPEC §2.7).
pub fn fetch_refspecs(name: &RemoteName) -> [String; 2] {
    [
        format!("+refs/heads/*:refs/remotes/{name}/*"),
        "^refs/heads/agent/*".to_owned(),
    ]
}

/// A merged, read-only config view.
pub struct Config {
    file: File,
    home: Option<PathBuf>,
}

impl Config {
    /// The last value of `key` (`section.key` or `section.subsection.key`).
    pub fn get(&self, key: &str) -> Option<String> {
        self.file.string(key).map(|v| v.to_string())
    }

    /// Every value of a multi-valued `key`, in order.
    pub fn get_all(&self, key: &str) -> Vec<String> {
        let values = self.file.strings(key).unwrap_or_default();
        values.iter().map(|v| v.to_string()).collect()
    }

    /// The last value of `key` as a git boolean; `None` when unset or malformed.
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.file.boolean(key).ok().flatten()
    }

    /// The last value of `key` as a path with `~/` expanded.
    pub fn path(&self, key: &str) -> Option<PathBuf> {
        let value = self.file.path(key)?;
        let ctx = interpolate::Context {
            git_install_dir: None,
            home_dir: self.home.as_deref(),
            home_for_user: Some(interpolate::home_for_user),
        };
        value.interpolate(ctx).ok()
    }
}

/// The config files git reads outside a repository, in precedence order (lowest first).
fn global_paths() -> Vec<PathBuf> {
    let mut var = |k: &str| std::env::var_os(k);
    [
        Source::GitInstallation,
        Source::System,
        Source::Git,
        Source::User,
    ]
    .into_iter()
    .filter_map(|s| s.storage_location(&mut var))
    .filter(|p| p.is_file())
    .collect()
}

/// Loads the merged config of `ws`: system and global files, then the common dir's `config`
/// (so local values win, including `core.autocrlf=false`).
pub fn load(ws: &Projection) -> Result<Config> {
    load_with(ws, &global_paths())
}

/// [`load`] with the global files given by the caller (lowest precedence first).
pub fn load_with(ws: &Projection, globals: &[PathBuf]) -> Result<Config> {
    let local = ws.common_dir().join("config");
    let git_dir = ws.toplevel().join(".git");
    let branch = match ws.read_head() {
        Ok(Head::Branch(b)) => gix::refs::FullName::try_from(format!("refs/heads/{b}")).ok(),
        _ => None,
    };
    let home = std::env::home_dir();
    let mut includes = includes::Options::follow(
        interpolate::Context {
            git_install_dir: None,
            home_dir: home.as_deref(),
            home_for_user: Some(interpolate::home_for_user),
        },
        includes::conditional::Context {
            git_dir: Some(&git_dir),
            branch_name: branch.as_ref().map(|b| b.as_ref()),
        },
    );
    includes.err_on_missing_config_path = false;
    let options = init::Options {
        includes,
        ..Default::default()
    };
    let metas = globals
        .iter()
        .map(|p| meta(Source::User, p))
        .chain(std::iter::once(meta(Source::Local, &local)));
    let file = File::from_paths_metadata(metas, options)
        .map_err(|e| ProjectionError::config(&local, e))?
        .unwrap_or_default();
    Ok(Config { file, home })
}

fn meta(source: Source, path: &Path) -> Metadata {
    Metadata {
        path: Some(path.to_owned()),
        source,
        level: 0,
        trust: gix::sec::Trust::Full,
    }
}

/// The initial common-dir config for a new repo (SPEC §9.4).
pub(crate) fn initial_file() -> Result<File> {
    let mut file = File::new(Metadata::from(Source::Local));
    let unix = !cfg!(windows);
    let keys = [
        ("repositoryformatversion", "0".to_owned()),
        ("bare", "false".to_owned()),
        ("autocrlf", "false".to_owned()),
        (
            "ignorecase",
            cfg!(any(windows, target_os = "macos")).to_string(),
        ),
        ("symlinks", unix.to_string()),
        ("filemode", unix.to_string()),
        ("longpaths", "true".to_owned()),
    ];
    for (name, value) in keys {
        file.set_raw_value(&format!("core.{name}"), value.as_str())
            .map_err(|e| ProjectionError::config(Path::new("config"), e))?;
    }
    Ok(file)
}

/// Writes `file` to `path` through a lock file.
pub(crate) fn write_file(path: &Path, file: &File) -> Result<()> {
    write_locked(path, &file.to_bstring())
}

/// Sets `remote.<name>.url` exactly as given and, if the remote has none, its fetch refspecs.
pub(crate) fn add_remote(common_dir: &Path, name: &RemoteName, url: &str) -> Result<()> {
    let path = common_dir.join("config");
    let err = |e: &dyn std::fmt::Display| ProjectionError::config(&path, e);
    let mut file = File::from_path_no_includes(path.clone(), Source::Local).map_err(|e| err(&e))?;
    file.set_raw_value_by("remote", name.as_str(), "url", url)
        .map_err(|e| err(&e))?;
    if file.strings(&format!("remote.{name}.fetch")).is_none() {
        let mut section = file
            .section_mut_or_create_new("remote", name.as_str())
            .map_err(|e| err(&e))?;
        for spec in fetch_refspecs(name) {
            section.push("fetch", spec.as_str()).map_err(|e| err(&e))?;
        }
    }
    write_file(&path, &file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_state::home::{AscHome, RepoId, WorkspaceId};

    fn workspace(dir: &Path, root: &Path) -> Projection {
        let home = AscHome::open(dir.join("home")).expect("home");
        Projection::create(&home, RepoId::new(), WorkspaceId::new(), root).expect("create")
    }

    #[test]
    fn autocrlf_false_beats_global_true() {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().join("ws");
        let ws = workspace(dir.path(), &root);
        let global = dir.path().join("gitconfig");
        std::fs::write(&global, "[core]\n\tautocrlf = true\n").expect("write");
        let cfg = load_with(&ws, std::slice::from_ref(&global)).expect("load");
        assert_eq!(cfg.bool("core.autocrlf"), Some(false));
        assert_eq!(cfg.bool("core.bare"), Some(false));
        assert_eq!(cfg.bool("core.longpaths"), Some(true));
    }

    #[test]
    fn include_if_gitdir_matches_workspace_root() {
        let dir = tempfile::tempdir().expect("tmp");
        let work = dir.path().join("work");
        let root = work.join("proj");
        let other = dir.path().join("other").join("proj");
        let work_id = dir.path().join("work.gitconfig");
        std::fs::write(&work_id, "[user]\n\temail = me@work.example\n").expect("write");
        let pattern = format!("{}/", work.display()).replace('\\', "/");
        let global = dir.path().join("gitconfig");
        let text = format!(
            "[user]\n\temail = me@home.example\n[includeIf \"gitdir:{pattern}\"]\n\tpath = {}\n",
            work_id.display().to_string().replace('\\', "/")
        );
        std::fs::write(&global, text).expect("write");

        let ws = workspace(dir.path(), &root);
        let cfg = load_with(&ws, std::slice::from_ref(&global)).expect("load");
        assert_eq!(cfg.get("user.email").as_deref(), Some("me@work.example"));

        let ws = workspace(dir.path(), &other);
        let cfg = load_with(&ws, std::slice::from_ref(&global)).expect("load");
        assert_eq!(cfg.get("user.email").as_deref(), Some("me@home.example"));
    }

    #[test]
    fn remote_url_is_kept_as_given_with_agent_negation() {
        let dir = tempfile::tempdir().expect("tmp");
        let ws = workspace(dir.path(), &dir.path().join("ws"));
        let url = "https://forge.example/Org/Repo.git";
        let origin = RemoteName::new("origin").expect("name");
        ws.add_remote(&origin, url).expect("remote");
        let cfg = load_with(&ws, &[]).expect("load");
        assert_eq!(cfg.get("remote.origin.url").as_deref(), Some(url));
        assert_eq!(
            cfg.get_all("remote.origin.fetch"),
            ["+refs/heads/*:refs/remotes/origin/*", "^refs/heads/agent/*"]
        );
    }
}
