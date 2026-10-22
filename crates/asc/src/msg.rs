//! User-facing messages (SPEC §11.6). A [`Msg`] that git also has copies git 2.55's English
//! text; [`Msg::source`] names the msgid it copies, and a test checks each against the pinned
//! git build. Messages that only asc has carry no source and use git's style with an `asc: ` tag.

use std::fmt::Write as _;

use crate::dispatch::Program;

/// The prefix git puts before a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prefix {
    None,
    Fatal,
    Error,
    Warning,
    Hint,
}

impl Prefix {
    fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Fatal => "fatal: ",
            Self::Error => "error: ",
            Self::Warning => "warning: ",
            Self::Hint => "hint: ",
        }
    }
}

/// git's `usage: git [-v | --version] ...` body (`git.c`, `git_usage_string`).
pub const GIT_USAGE: &str = "git [-v | --version] [-h | --help] [-C <path>] [-c <name>=<value>]\n           [--exec-path[=<path>]] [--html-path] [--man-path] [--info-path]\n           [-p | --paginate | -P | --no-pager] [--no-replace-objects] [--no-lazy-fetch]\n           [--no-optional-locks] [--no-advice] [--bare] [--git-dir=<path>]\n           [--work-tree=<path>] [--namespace=<name>] [--config-env=<name>=<envvar>]\n           <command> [<args>]";

/// A message with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    /// `usage: git [-v | --version] ...`.
    Usage,
    /// `unknown option: <opt>`.
    UnknownOption { opt: String },
    /// `no directory given for '<opt>' option`.
    MissingDir { opt: String },
    /// `-c expects a configuration string`.
    MissingConfigString,
    /// `cannot change to '<dir>': <os error>`.
    CannotChange { dir: String, reason: String },
    /// `git: '<name>' is not a git command. See 'git --help'.`
    NotACommand { name: String },
    /// `alias loop detected: expansion of '<alias>' does not terminate:<chain>`.
    AliasLoop { alias: String, chain: String },
    /// `WARNING: You called a Git command named '<name>', which does not exist.`
    AutocorrectWarning { name: String },
    /// `Continuing under the assumption that you meant '<cmd>'.`
    AutocorrectContinue { cmd: String },
    /// `The most similar command is`.
    SimilarOne,
    /// `The most similar commands are`.
    SimilarMany,
    /// `not a git repository (or any of the parent directories): <cwd>`.
    NotARepository { cwd: String },
    /// `Initialized empty Git repository in <path>`.
    InitializedEmpty { path: String },
    /// `Reinitialized existing Git repository in <path>`.
    Reinitialized { path: String },
    /// `invalid initial branch name: '<name>'`.
    InvalidInitialBranch { name: String },
    /// `key does not contain a section: <key>`.
    ConfigNoSection { key: String },
    /// `key does not contain variable name: <key>`.
    ConfigNoVariable { key: String },
    /// `invalid key: <key>`.
    ConfigInvalidKey { key: String },
    /// `not in a git directory`.
    NotInGitDir,
    /// `unable to read config file '<path>'`.
    ConfigUnreadable { path: String, reason: String },
    /// `no such section: <name>`.
    NoSuchSection { name: String },
    /// `invalid config format: <spec>`.
    ConfigEnvFormat { spec: String },
    /// `missing environment variable '<var>' for configuration '<name>'`.
    ConfigEnvMissing { var: String, name: String },
    /// `asc: option '<flag>' is not supported`.
    AscOptionUnsupported { flag: String },
    /// `asc: '<cmd>' is not supported yet`.
    AscNotYet { cmd: String },
    /// `asc: '<cmd>' is not supported in an asc workspace`.
    AscNotInWorkspace { cmd: String },
    /// `asc: cannot create <dir>; run asc doctor --fix`.
    AscShimDir { dir: String },
    /// `asc: shim recursion limit reached`.
    AscShimDepth,
}

impl Msg {
    /// The prefix this message is printed with.
    pub fn prefix(&self) -> Prefix {
        match self {
            Self::Usage
            | Self::SimilarOne
            | Self::SimilarMany
            | Self::AutocorrectContinue { .. }
            | Self::AutocorrectWarning { .. }
            | Self::UnknownOption { .. }
            | Self::MissingDir { .. }
            | Self::MissingConfigString
            | Self::NotACommand { .. } => Prefix::None,
            Self::ConfigNoSection { .. }
            | Self::ConfigNoVariable { .. }
            | Self::ConfigInvalidKey { .. } => Prefix::Error,
            _ => Prefix::Fatal,
        }
    }

    /// Exit code git gives this condition: 129 for usage errors, 1 for "not a command",
    /// otherwise 128.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage
            | Self::UnknownOption { .. }
            | Self::MissingDir { .. }
            | Self::MissingConfigString
            | Self::AscOptionUnsupported { .. }
            | Self::AscNotYet { .. }
            | Self::AscNotInWorkspace { .. } => 129,
            Self::NotACommand { .. }
            | Self::ConfigNoSection { .. }
            | Self::ConfigNoVariable { .. }
            | Self::ConfigInvalidKey { .. } => 1,
            _ => 128,
        }
    }

    /// The msgids of git 2.55 this message copies, exactly as `po/git.pot` spells them.
    /// Empty for messages only asc has.
    pub fn source(&self) -> &'static [&'static str] {
        match self {
            Self::Usage => &[GIT_USAGE],
            Self::UnknownOption { .. } => &["unknown option: %s\n"],
            Self::MissingDir { .. } => &["no directory given for '%s' option\n"],
            Self::MissingConfigString => &["-c expects a configuration string\n"],
            Self::CannotChange { .. } => &["cannot change to '%s'"],
            Self::NotACommand { .. } => &["git: '%s' is not a git command. See 'git --help'."],
            Self::AliasLoop { .. } => &[
                "alias loop detected: expansion of '%s' does not terminate:%s",
            ],
            Self::AutocorrectWarning { .. } => {
                &["WARNING: You called a Git command named '%s', which does not exist."]
            }
            Self::AutocorrectContinue { .. } => {
                &["Continuing under the assumption that you meant '%s'."]
            }
            Self::SimilarOne => &["The most similar command is"],
            Self::SimilarMany => &["The most similar commands are"],
            Self::NotARepository { .. } => {
                &["not a git repository (or any of the parent directories): %s"]
            }
            Self::InitializedEmpty { .. } => &["Initialized empty Git repository in %s%s\n"],
            Self::Reinitialized { .. } => &["Reinitialized existing Git repository in %s%s\n"],
            Self::InvalidInitialBranch { .. } => &["invalid initial branch name: '%s'"],
            Self::ConfigNoSection { .. } => &["key does not contain a section: %s"],
            Self::ConfigNoVariable { .. } => &["key does not contain variable name: %s"],
            Self::ConfigInvalidKey { .. } => &["invalid key: %s"],
            Self::NotInGitDir => &["not in a git directory"],
            Self::ConfigUnreadable { .. } => &["unable to read config file '%s'"],
            Self::NoSuchSection { .. } => &["no such section: %s"],
            Self::ConfigEnvFormat { .. } => &["invalid config format: %s"],
            Self::ConfigEnvMissing { .. } => {
                &["missing environment variable '%s' for configuration '%.*s'"]
            }
            Self::AscOptionUnsupported { .. }
            | Self::AscNotYet { .. }
            | Self::AscNotInWorkspace { .. }
            | Self::AscShimDir { .. }
            | Self::AscShimDepth => &[],
        }
    }

    /// The message text without its prefix, naming the program as the user invoked it.
    pub fn text(&self, prog: Program) -> String {
        let name = prog.name();
        match self {
            Self::Usage => format!("usage: {}", GIT_USAGE.replacen("git", name, 1)),
            Self::UnknownOption { opt } => format!("unknown option: {opt}"),
            Self::MissingDir { opt } => format!("no directory given for '{opt}' option"),
            Self::MissingConfigString => "-c expects a configuration string".to_owned(),
            Self::CannotChange { dir, reason } => format!("cannot change to '{dir}': {reason}"),
            Self::NotACommand { name: cmd } => {
                format!("{name}: '{cmd}' is not a git command. See '{name} --help'.")
            }
            Self::AliasLoop { alias, chain } => format!(
                "alias loop detected: expansion of '{alias}' does not terminate:{chain}"
            ),
            Self::AutocorrectWarning { name: cmd } => format!(
                "WARNING: You called a Git command named '{cmd}', which does not exist."
            ),
            Self::AutocorrectContinue { cmd } => {
                format!("Continuing under the assumption that you meant '{cmd}'.")
            }
            Self::SimilarOne => "The most similar command is".to_owned(),
            Self::SimilarMany => "The most similar commands are".to_owned(),
            Self::NotARepository { cwd } => {
                format!("not a git repository (or any of the parent directories): {cwd}")
            }
            Self::InitializedEmpty { path } => format!("Initialized empty Git repository in {path}"),
            Self::Reinitialized { path } => {
                format!("Reinitialized existing Git repository in {path}")
            }
            Self::InvalidInitialBranch { name } => {
                format!("invalid initial branch name: '{name}'")
            }
            Self::ConfigNoSection { key } => format!("key does not contain a section: {key}"),
            Self::ConfigNoVariable { key } => {
                format!("key does not contain variable name: {key}")
            }
            Self::ConfigInvalidKey { key } => format!("invalid key: {key}"),
            Self::NotInGitDir => "not in a git directory".to_owned(),
            Self::ConfigUnreadable { path, reason } => {
                format!("unable to read config file '{path}': {reason}")
            }
            Self::NoSuchSection { name: section } => format!("no such section: {section}"),
            Self::ConfigEnvFormat { spec } => format!("invalid config format: {spec}"),
            Self::ConfigEnvMissing { var, name: key } => {
                format!("missing environment variable '{var}' for configuration '{key}'")
            }
            Self::AscOptionUnsupported { flag } => {
                format!("asc: option '{flag}' is not supported")
            }
            Self::AscNotYet { cmd } => format!("asc: '{cmd}' is not supported yet"),
            Self::AscNotInWorkspace { cmd } => {
                format!("asc: '{cmd}' is not supported in an asc workspace")
            }
            Self::AscShimDir { dir } => format!("asc: cannot create {dir}; run asc doctor --fix"),
            Self::AscShimDepth => "asc: shim recursion limit reached".to_owned(),
        }
    }

    /// The full line as printed, with prefix and newline.
    pub fn line(&self, prog: Program) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{}{}", self.prefix().text(), self.text(prog));
        out
    }

    /// One value of every variant, for the source check.
    #[cfg(test)]
    pub(crate) fn samples() -> Vec<Self> {
        let s = |t: &str| t.to_owned();
        vec![
            Self::Usage,
            Self::UnknownOption { opt: s("--x") },
            Self::MissingDir { opt: s("-C") },
            Self::MissingConfigString,
            Self::CannotChange { dir: s("d"), reason: s("r") },
            Self::NotACommand { name: s("x") },
            Self::AliasLoop { alias: s("a"), chain: s("\n  a") },
            Self::AutocorrectWarning { name: s("x") },
            Self::AutocorrectContinue { cmd: s("y") },
            Self::SimilarOne,
            Self::SimilarMany,
            Self::NotARepository { cwd: s("c") },
            Self::InitializedEmpty { path: s("p") },
            Self::Reinitialized { path: s("p") },
            Self::InvalidInitialBranch { name: s("n") },
            Self::ConfigNoSection { key: s("k") },
            Self::ConfigNoVariable { key: s("k") },
            Self::ConfigInvalidKey { key: s("k") },
            Self::NotInGitDir,
            Self::ConfigUnreadable { path: s("p"), reason: s("r") },
            Self::NoSuchSection { name: s("n") },
            Self::ConfigEnvFormat { spec: s("x") },
            Self::ConfigEnvMissing { var: s("v"), name: s("n") },
            Self::AscOptionUnsupported { flag: s("--f") },
            Self::AscNotYet { cmd: s("c") },
            Self::AscNotInWorkspace { cmd: s("c") },
            Self::AscShimDir { dir: s("d") },
            Self::AscShimDepth,
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The big git binary that holds the builtins' strings.
    fn git_binary() -> Option<Vec<u8>> {
        let real = asc_git::realgit::RealGit::resolve(None, None).ok()?;
        let out = real.command().ok()?.arg("--exec-path").output().ok()?;
        let exec = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
        ["git.exe", "git"]
            .iter()
            .map(|n| exec.join(n))
            .find(|p| p.is_file())
            .or_else(|| Some(real.path().to_owned()))
            .and_then(|p| std::fs::read(p).ok())
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn every_cited_string_is_in_the_pinned_git() {
        let Some(binary) = git_binary() else {
            eprintln!("skipped: real git not found");
            return;
        };
        let mut checked = 0;
        for msg in Msg::samples() {
            for id in msg.source() {
                // Stored NUL-terminated, so the terminator makes the match exact at the end.
                let mut needle = id.as_bytes().to_vec();
                needle.push(0);
                assert!(contains(&binary, &needle), "{msg:?}: msgid not in git: {id:?}");
                checked += 1;
            }
        }
        assert!(checked >= 20);
    }

    #[test]
    fn samples_cover_every_variant() {
        // `source` and `prefix` are exhaustive matches, so a new variant fails to compile there;
        // this keeps `samples` in step.
        let kinds: std::collections::HashSet<_> = Msg::samples()
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(kinds.len(), Msg::samples().len());
        assert_eq!(kinds.len(), 28);
    }

    #[test]
    fn hints_name_the_invoked_program() {
        let m = Msg::NotACommand { name: "x".into() };
        assert_eq!(m.text(Program::Git), "git: 'x' is not a git command. See 'git --help'.");
        assert_eq!(m.text(Program::Asc), "asc: 'x' is not a git command. See 'asc --help'.");
        assert!(Msg::Usage.text(Program::Asc).starts_with("usage: asc [-v"));
    }

    #[test]
    fn asc_only_messages_use_the_tag_and_git_exit_codes() {
        let m = Msg::AscOptionUnsupported { flag: "--bogus".into() };
        assert_eq!(m.line(Program::Asc), "fatal: asc: option '--bogus' is not supported\n");
        assert_eq!(m.exit_code(), 129);
        assert_eq!(Msg::AscNotYet { cmd: "x".into() }.exit_code(), 129);
    }
}
