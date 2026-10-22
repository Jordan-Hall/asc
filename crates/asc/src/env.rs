//! Git environment variables and global options (SPEC §11.1), parsed once into [`GitEnv`].

use std::ffi::OsString;
use std::path::PathBuf;

use crate::msg::Msg;

/// One `-c name[=value]` or `GIT_CONFIG_PARAMETERS` entry. `value: None` is a bare key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigParam {
    pub key: String,
    pub value: Option<String>,
}

impl ConfigParam {
    /// Parses `name=value` (the first `=` splits) or a bare `name`.
    pub fn parse(text: &str) -> Self {
        match text.split_once('=') {
            Some((k, v)) => Self {
                key: k.to_owned(),
                value: Some(v.to_owned()),
            },
            None => Self {
                key: text.to_owned(),
                value: None,
            },
        }
    }

    fn render(&self) -> String {
        match &self.value {
            Some(v) => format!("{}={v}", self.key),
            None => self.key.clone(),
        }
    }
}

/// The four pathspec-mode switches (`GIT_*_PATHSPECS` and the matching global flags).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathspecMode {
    pub literal: bool,
    pub glob: bool,
    pub noglob: bool,
    pub icase: bool,
}

/// What the user asked of the pager.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PagerChoice {
    /// The command and the config decide.
    #[default]
    Auto,
    /// `-p` / `--paginate`.
    Always,
    /// `--no-pager`.
    Never,
}

/// Where `GIT_TRACE` writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TraceTarget {
    #[default]
    Off,
    Stderr,
    File(PathBuf),
}

/// An author or committer override from `GIT_{AUTHOR,COMMITTER}_*`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentEnv {
    pub name: Option<String>,
    pub email: Option<String>,
    pub date: Option<String>,
}

/// A failure in the global options or the environment.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnvError {
    /// `-C` without a path.
    #[error("no directory given for '-C' option")]
    MissingDir,
    /// `-c` without a string.
    #[error("-c expects a configuration string")]
    MissingConfig,
    /// `--config-env` malformed or naming an unset variable.
    #[error("{0:?}")]
    ConfigEnv(Msg),
    /// A git global option that asc does not implement.
    #[error("option '{0}' is not supported")]
    Unsupported(String),
    /// Not a git global option.
    #[error("unknown option: {0}")]
    Unknown(String),
    /// An option that needs a value.
    #[error("no directory given for '{0}' option")]
    MissingValue(String),
}

/// What the leading global options asked for besides changing [`GitEnv`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlobalOpts {
    /// `--version` / `-v`.
    pub version: bool,
    /// `-h` / `--help`.
    pub help: bool,
    /// `--exec-path` without a value.
    pub print_exec_path: bool,
    /// The `-C` directories, in order.
    pub chdir: Vec<PathBuf>,
}

/// Every environment variable and global option of SPEC §11.1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitEnv {
    pub git_dir: Option<PathBuf>,
    pub work_tree: Option<PathBuf>,
    pub common_dir: Option<PathBuf>,
    pub index_file: Option<PathBuf>,
    pub ceiling_dirs: Vec<PathBuf>,
    pub discovery_across_filesystem: bool,
    /// `GIT_CONFIG_PARAMETERS`, then `GIT_CONFIG_COUNT` entries, as inherited.
    pub inherited_config: Vec<ConfigParam>,
    /// The raw `GIT_CONFIG_PARAMETERS`, kept so children see it unchanged.
    inherited_raw: Option<OsString>,
    /// `-c` and `--config-env` values of this invocation, in order.
    pub cli_config: Vec<ConfigParam>,
    pub config_global: Option<PathBuf>,
    pub config_system: Option<PathBuf>,
    pub config_nosystem: bool,
    pub author: IdentEnv,
    pub committer: IdentEnv,
    pub editor: Option<OsString>,
    pub sequence_editor: Option<OsString>,
    pub pager: Option<OsString>,
    /// The `PAGER` fallback.
    pub pager_fallback: Option<OsString>,
    pub merge_autoedit: Option<bool>,
    pub reflog_action: Option<String>,
    pub optional_locks: bool,
    pub askpass: Option<OsString>,
    pub ssh_askpass: Option<OsString>,
    pub terminal_prompt: bool,
    pub ssh: Option<OsString>,
    pub ssh_command: Option<OsString>,
    pub pathspec: PathspecMode,
    pub trace: TraceTarget,
    pub trace2: Option<OsString>,
    pub no_color: bool,
    /// `ASC_NO_ALIASES=1`.
    pub no_aliases: bool,
    pub pager_choice: PagerChoice,
    pub no_replace_objects: bool,
    /// `--exec-path=<path>`.
    pub exec_path: Option<PathBuf>,
    /// `ASC_SHIM_DEPTH` as inherited.
    pub shim_depth: u32,
}

/// Git's boolean spelling: `true|yes|on|1` and `false|no|off|0|""`; other integers are non-zero.
pub fn parse_git_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" | "" => Some(false),
        other => other.parse::<i64>().ok().map(|n| n != 0),
    }
}

/// Splits `GIT_CONFIG_PARAMETERS`: single-quoted words separated by spaces, each `key=value`
/// or `key` (the old format), or `'key'='value'` (the new one).
pub fn parse_config_parameters(raw: &str) -> Vec<ConfigParam> {
    let mut out = Vec::new();
    let mut chars = raw.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let first = unquote(&mut chars);
        if chars.next_if_eq(&'=').is_some() {
            if chars.peek() == Some(&'\'') {
                let value = unquote(&mut chars);
                out.push(ConfigParam {
                    key: first,
                    value: Some(value),
                });
                continue;
            }
            // Old format: the word was `key=value` quoted as a whole, so this is unreachable
            // unless the quote closed early; keep the remainder as the value.
            let rest: String = std::iter::from_fn(|| chars.next_if(|c| !c.is_whitespace())).collect();
            out.push(ConfigParam {
                key: first,
                value: Some(rest),
            });
            continue;
        }
        out.push(ConfigParam::parse(&first));
    }
    out
}

/// Reads one word made of quoted runs (`'...'` with `'\''` escapes) and bare text.
fn unquote(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut word = String::new();
    while let Some(&c) = chars.peek() {
        match c {
            '\'' => {
                chars.next();
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '\\' => {
                chars.next();
                if let Some(e) = chars.next() {
                    word.push(e);
                }
            }
            '=' | ' ' | '\t' | '\n' => break,
            _ => {
                chars.next();
                word.push(c);
            }
        }
    }
    word
}

/// Quotes `text` for `GIT_CONFIG_PARAMETERS`: single quotes, `'` as `'\''`.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn utf8(v: Option<OsString>) -> Option<String> {
    v.and_then(|s| s.into_string().ok())
}

fn path(v: Option<OsString>) -> Option<PathBuf> {
    v.filter(|s| !s.is_empty()).map(PathBuf::from)
}

impl GitEnv {
    /// Reads the process environment.
    pub fn from_process() -> Self {
        Self::from_vars(&|k| std::env::var_os(k))
    }

    /// Reads variables through `get` (tests pass a map).
    pub fn from_vars(get: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let flag = |name: &str| {
            get(name)
                .and_then(|v| v.into_string().ok())
                .and_then(|v| parse_git_bool(&v))
        };
        let ident = |prefix: &str| IdentEnv {
            name: utf8(get(&format!("GIT_{prefix}_NAME"))),
            email: utf8(get(&format!("GIT_{prefix}_EMAIL"))),
            date: utf8(get(&format!("GIT_{prefix}_DATE"))),
        };
        let raw = get("GIT_CONFIG_PARAMETERS").filter(|v| !v.is_empty());
        let mut inherited = raw
            .as_ref()
            .and_then(|r| r.to_str())
            .map(parse_config_parameters)
            .unwrap_or_default();
        inherited.extend(config_count_entries(get));
        let ceiling = get("GIT_CEILING_DIRECTORIES")
            .map(|v| {
                std::env::split_paths(&v)
                    .filter(|p| p.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        let trace = match get("GIT_TRACE").and_then(|v| v.into_string().ok()) {
            Some(v) if matches!(v.as_str(), "1" | "2" | "true" | "yes" | "on") => TraceTarget::Stderr,
            Some(v) if std::path::Path::new(&v).is_absolute() => TraceTarget::File(PathBuf::from(v)),
            _ => TraceTarget::Off,
        };
        let pathspec = PathspecMode {
            literal: flag("GIT_LITERAL_PATHSPECS").unwrap_or(false),
            glob: flag("GIT_GLOB_PATHSPECS").unwrap_or(false),
            noglob: flag("GIT_NOGLOB_PATHSPECS").unwrap_or(false),
            icase: flag("GIT_ICASE_PATHSPECS").unwrap_or(false),
        };
        Self {
            git_dir: path(get("GIT_DIR")),
            work_tree: path(get("GIT_WORK_TREE")),
            common_dir: path(get("GIT_COMMON_DIR")),
            index_file: path(get("GIT_INDEX_FILE")),
            ceiling_dirs: ceiling,
            discovery_across_filesystem: flag("GIT_DISCOVERY_ACROSS_FILESYSTEM").unwrap_or(false),
            inherited_config: inherited,
            inherited_raw: raw,
            cli_config: Vec::new(),
            config_global: path(get("GIT_CONFIG_GLOBAL")),
            config_system: path(get("GIT_CONFIG_SYSTEM")),
            config_nosystem: flag("GIT_CONFIG_NOSYSTEM").unwrap_or(false),
            author: ident("AUTHOR"),
            committer: ident("COMMITTER"),
            editor: get("GIT_EDITOR").filter(|v| !v.is_empty()),
            sequence_editor: get("GIT_SEQUENCE_EDITOR").filter(|v| !v.is_empty()),
            pager: get("GIT_PAGER"),
            pager_fallback: get("PAGER"),
            merge_autoedit: flag("GIT_MERGE_AUTOEDIT"),
            reflog_action: utf8(get("GIT_REFLOG_ACTION")),
            optional_locks: flag("GIT_OPTIONAL_LOCKS").unwrap_or(true),
            askpass: get("GIT_ASKPASS").filter(|v| !v.is_empty()),
            ssh_askpass: get("SSH_ASKPASS").filter(|v| !v.is_empty()),
            terminal_prompt: flag("GIT_TERMINAL_PROMPT").unwrap_or(true),
            ssh: get("GIT_SSH").filter(|v| !v.is_empty()),
            ssh_command: get("GIT_SSH_COMMAND").filter(|v| !v.is_empty()),
            pathspec,
            trace,
            trace2: get("GIT_TRACE2").filter(|v| !v.is_empty()),
            no_color: get("NO_COLOR").is_some_and(|v| !v.is_empty()),
            no_aliases: get("ASC_NO_ALIASES").is_some_and(|v| v == "1"),
            pager_choice: PagerChoice::Auto,
            no_replace_objects: false,
            exec_path: None,
            shim_depth: utf8(get("ASC_SHIM_DEPTH"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }
    }

    /// Consumes the leading global options of `args` (after argv[0]) and returns what they asked
    /// for plus the index of the command word. Mirrors git's `handle_options`.
    pub fn parse_globals(
        &mut self,
        args: &[OsString],
        get: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<(GlobalOpts, usize), EnvError> {
        let mut opts = GlobalOpts::default();
        let mut i = 0;
        while let Some(arg) = args.get(i) {
            let Some(text) = arg.to_str() else {
                break;
            };
            if !text.starts_with('-') {
                break;
            }
            // `--name value` and `--name=value` spellings.
            let (name, inline) = match text.split_once('=') {
                Some((n, v)) if text.starts_with("--") => (n, Some(v.to_owned())),
                _ => (text, None),
            };
            let value = |i: &mut usize| -> Result<String, EnvError> {
                if let Some(v) = inline.clone() {
                    return Ok(v);
                }
                *i += 1;
                args.get(*i)
                    .and_then(|v| v.to_str())
                    .map(str::to_owned)
                    .ok_or_else(|| EnvError::MissingValue(name.to_owned()))
            };
            match name {
                "-v" | "--version" => opts.version = true,
                "-h" | "--help" => opts.help = true,
                "-C" => {
                    i += 1;
                    let dir = args.get(i).ok_or(EnvError::MissingDir)?;
                    if !dir.is_empty() {
                        opts.chdir.push(PathBuf::from(dir));
                    }
                }
                "-c" => {
                    i += 1;
                    let cfg = args
                        .get(i)
                        .and_then(|v| v.to_str())
                        .ok_or(EnvError::MissingConfig)?;
                    self.cli_config.push(ConfigParam::parse(cfg));
                }
                "--config-env" => {
                    let spec = value(&mut i)?;
                    self.cli_config.push(config_env(&spec, get)?);
                }
                "--git-dir" => self.git_dir = Some(PathBuf::from(value(&mut i)?)),
                "--work-tree" => self.work_tree = Some(PathBuf::from(value(&mut i)?)),
                "--exec-path" => match inline {
                    Some(v) => self.exec_path = Some(PathBuf::from(v)),
                    None => opts.print_exec_path = true,
                },
                "-p" | "--paginate" => self.pager_choice = PagerChoice::Always,
                "-P" | "--no-pager" => self.pager_choice = PagerChoice::Never,
                "--no-optional-locks" => self.optional_locks = false,
                "--literal-pathspecs" => self.pathspec.literal = true,
                "--no-literal-pathspecs" => self.pathspec.literal = false,
                "--glob-pathspecs" => self.pathspec.glob = true,
                "--noglob-pathspecs" => self.pathspec.noglob = true,
                "--icase-pathspecs" => self.pathspec.icase = true,
                "--no-replace-objects" => self.no_replace_objects = true,
                "--bare" | "--namespace" | "--html-path" | "--man-path" | "--info-path"
                | "--no-lazy-fetch" | "--no-advice" | "--super-prefix" | "--attr-source" => {
                    return Err(EnvError::Unsupported(name.to_owned()));
                }
                _ => return Err(EnvError::Unknown(text.to_owned())),
            }
            i += 1;
        }
        Ok((opts, i))
    }

    /// All `-c`-style overrides in precedence order (lowest first): inherited, then this
    /// invocation's.
    pub fn config_overrides(&self) -> impl Iterator<Item = &ConfigParam> {
        self.inherited_config.iter().chain(self.cli_config.iter())
    }

    /// The `GIT_CONFIG_PARAMETERS` value children get: the inherited value, then asc's own `-c`
    /// values. `None` when there is nothing to export.
    pub fn config_parameters(&self) -> Option<OsString> {
        if self.cli_config.is_empty() {
            return self.inherited_raw.clone();
        }
        let mut text = self
            .inherited_raw
            .as_ref()
            .and_then(|r| r.to_str())
            .unwrap_or_default()
            .to_owned();
        for p in &self.cli_config {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&shell_quote(&p.render()));
        }
        Some(OsString::from(text))
    }

    /// Variables a child process gets beyond the inherited environment (SPEC §11.1, §9.4.1).
    pub fn child_env(&self) -> Vec<(OsString, OsString)> {
        let mut out = Vec::new();
        if let Some(params) = self.config_parameters() {
            out.push((OsString::from("GIT_CONFIG_PARAMETERS"), params));
        }
        out.push((
            OsString::from("ASC_SHIM_DEPTH"),
            OsString::from((self.shim_depth + 1).to_string()),
        ));
        out
    }

    /// Whether `index_file` names a file other than the workspace's own `<gitdir>/index`.
    pub fn foreign_index(&self, gitdir: &std::path::Path) -> Option<&std::path::Path> {
        let file = self.index_file.as_deref()?;
        let own = gitdir.join("index");
        let same = |a: &std::path::Path, b: &std::path::Path| {
            a == b || (cfg!(windows) && a.as_os_str().eq_ignore_ascii_case(b.as_os_str()))
        };
        (!same(file, &own)).then_some(file)
    }
}

fn config_count_entries(get: &dyn Fn(&str) -> Option<OsString>) -> Vec<ConfigParam> {
    let count = utf8(get("GIT_CONFIG_COUNT"))
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0);
    (0..count)
        .filter_map(|n| {
            let key = utf8(get(&format!("GIT_CONFIG_KEY_{n}")))?;
            let value = utf8(get(&format!("GIT_CONFIG_VALUE_{n}")))?;
            Some(ConfigParam {
                key,
                value: Some(value),
            })
        })
        .collect()
}

/// `--config-env=<name>=<envvar>`.
fn config_env(
    spec: &str,
    get: &dyn Fn(&str) -> Option<OsString>,
) -> Result<ConfigParam, EnvError> {
    let Some((name, var)) = spec.split_once('=') else {
        return Err(EnvError::ConfigEnv(Msg::ConfigEnvFormat {
            spec: spec.to_owned(),
        }));
    };
    if var.is_empty() {
        return Err(EnvError::ConfigEnv(Msg::ConfigEnvFormat {
            spec: spec.to_owned(),
        }));
    }
    match utf8(get(var)) {
        Some(value) => Ok(ConfigParam {
            key: name.to_owned(),
            value: Some(value),
        }),
        None => Err(EnvError::ConfigEnv(Msg::ConfigEnvMissing {
            var: var.to_owned(),
            name: name.to_owned(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> GitEnv {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        GitEnv::from_vars(&|k| map.get(k).cloned())
    }

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    fn none(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn parses_variables() {
        let e = env(&[
            ("GIT_DIR", "/x/.git"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_TERMINAL_PROMPT", "false"),
            ("GIT_AUTHOR_NAME", "A"),
            ("GIT_LITERAL_PATHSPECS", "1"),
            ("NO_COLOR", "1"),
            ("ASC_SHIM_DEPTH", "2"),
        ]);
        assert_eq!(e.git_dir, Some(PathBuf::from("/x/.git")));
        assert!(!e.optional_locks);
        assert!(!e.terminal_prompt);
        assert_eq!(e.author.name.as_deref(), Some("A"));
        assert!(e.pathspec.literal && !e.pathspec.glob);
        assert!(e.no_color);
        assert_eq!(e.shim_depth, 2);
        assert!(env(&[]).optional_locks);
    }

    #[test]
    fn config_count_and_parameters() {
        let e = env(&[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "a.b"),
            ("GIT_CONFIG_VALUE_0", "c"),
            ("GIT_CONFIG_PARAMETERS", "'x.y=1' 'z.w'"),
        ]);
        let keys: Vec<_> = e.config_overrides().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["x.y", "z.w", "a.b"]);
        assert_eq!(e.inherited_config[1].value, None);
    }

    #[test]
    fn new_format_parameters() {
        let p = parse_config_parameters("'core.editor'='vi it' 'a.b'='c'\\''d'");
        assert_eq!(p[0].value.as_deref(), Some("vi it"));
        assert_eq!(p[1].value.as_deref(), Some("c'd"));
    }

    #[test]
    fn globals_stop_at_the_command() {
        let mut e = env(&[]);
        let a = args(&["-C", "d", "-c", "a.b=c", "--no-pager", "status", "-s"]);
        let (opts, at) = e.parse_globals(&a, &none).unwrap();
        assert_eq!(at, 5);
        assert_eq!(opts.chdir, [PathBuf::from("d")]);
        assert_eq!(e.cli_config[0].key, "a.b");
        assert_eq!(e.pager_choice, PagerChoice::Never);
    }

    #[test]
    fn global_errors() {
        let mut e = env(&[]);
        assert_eq!(e.parse_globals(&args(&["-C"]), &none), Err(EnvError::MissingDir));
        assert_eq!(e.parse_globals(&args(&["-c"]), &none), Err(EnvError::MissingConfig));
        assert_eq!(
            e.parse_globals(&args(&["--foo"]), &none),
            Err(EnvError::Unknown("--foo".into()))
        );
        assert!(matches!(
            e.parse_globals(&args(&["--bare"]), &none),
            Err(EnvError::Unsupported(_))
        ));
    }

    #[test]
    fn config_env_reads_the_variable() {
        let get = |k: &str| (k == "TOK").then(|| OsString::from("secret"));
        let mut e = env(&[]);
        e.parse_globals(&args(&["--config-env=a.b=TOK", "log"]), &get)
            .unwrap();
        assert_eq!(e.cli_config[0].value.as_deref(), Some("secret"));
        let err = e
            .parse_globals(&args(&["--config-env=a.b=NOPE"]), &get)
            .unwrap_err();
        assert_eq!(
            err,
            EnvError::ConfigEnv(Msg::ConfigEnvMissing {
                var: "NOPE".into(),
                name: "a.b".into()
            })
        );
    }

    #[test]
    fn exports_own_config_to_children() {
        let mut e = env(&[("GIT_CONFIG_PARAMETERS", "'x.y=1'")]);
        e.cli_config.push(ConfigParam::parse("core.editor=vi'm"));
        let exported = e.config_parameters().unwrap();
        assert_eq!(exported, "'x.y=1' 'core.editor=vi'\\''m'");
        let round = parse_config_parameters(exported.to_str().unwrap());
        assert_eq!(round[1].value.as_deref(), Some("vi'm"));
        let depth = e
            .child_env()
            .into_iter()
            .find(|(k, _)| k == "ASC_SHIM_DEPTH")
            .unwrap();
        assert_eq!(depth.1, "1");
    }

    #[test]
    fn foreign_index_detection() {
        let e = env(&[("GIT_INDEX_FILE", "/t/x")]);
        assert!(e.foreign_index(std::path::Path::new("/g")).is_some());
        let own = env(&[("GIT_INDEX_FILE", "/g/index")]);
        assert!(own.foreign_index(std::path::Path::new("/g")).is_none());
    }
}
