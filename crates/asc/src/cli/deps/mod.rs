//! `asc deps`: dependency scanning and update proposals (`DepsService`, docs/DEPENDENCIES.md).
//!
//! ```text
//! asc deps scan [--rev REV] [--ecosystem E]... [--refresh]
//! asc deps findings [--state S]... [--ecosystem E]... [--severity S] [--package NAME]
//! asc deps updates [--rev REV] [--ecosystem E]... [--security] [--ignored]
//! asc deps propose [--security] [--max-kind patch|minor|major] [--dry-run] [--max-groups N] [PACKAGE]...
//! asc deps dismiss <id> --reason not-used|tolerable|inaccurate|fix-started [--note TEXT]
//! asc deps reopen <id>
//! asc deps config [--rev REV]
//! ```
//!
//! Every command works against a server (`--remote`, a configured remote or the URL of the repository,
//! default `origin`) and needs no workspace. The server reads the manifests, asks the advisory source and
//! the registries, and decides who may do what; `propose` records changes on `agent/deps/...` refs and never
//! writes trunk. `--token-env NAME` takes the credential from the environment variable `NAME` (for a
//! scheduled workflow, whose secrets cannot be called `ASC_*`); a secret is never taken from an argument.

mod findings;
mod propose;
mod scan;
mod updates;

use std::process::ExitCode;

use asc_rpc::server::wire;
use asc_rpc::services::deps_service_client::DepsServiceClient;
use tonic::transport::Channel;

use super::agent::conn::MAX_MESSAGE;
use super::common::{CliError, Ctx, Result};
use crate::native::NativeSpec;
use crate::remote::server::Session;

pub(crate) fn client(s: &Session) -> DepsServiceClient<Channel> {
    DepsServiceClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

/// A usage failure with the text of the command.
pub(crate) fn bad(text: impl Into<String>) -> CliError {
    CliError::Usage(format!("asc deps: {}", text.into()))
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "deps",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Sub,
}

#[derive(clap::Subcommand, Debug)]
pub enum Sub {
    /// Reads the dependency files of a revision, looks the packages up and records the findings.
    Scan(scan::ScanArgs),
    /// Lists the recorded findings.
    Findings(findings::FindingsArgs),
    /// Lists the updates that are available.
    Updates(updates::UpdatesArgs),
    /// Records updates as changes on agent/deps refs.
    Propose(propose::ProposeArgs),
    /// Dismisses a finding.
    Dismiss(findings::DismissArgs),
    /// Reopens a dismissed finding.
    Reopen(findings::ReopenArgs),
    /// Shows the settings (.asc/deps.toml) and the scheduled workflow that goes with them.
    Config(updates::ConfigArgs),
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "deps",
    flags: &[
        "--remote",
        "--token-env",
        "--json",
        "--rev",
        "--ecosystem",
        "--refresh",
        "--state",
        "--severity",
        "--package",
        "--limit",
        "--page",
        "--security",
        "--ignored",
        "--max-kind",
        "--max-groups",
        "--dry-run",
        "--reason",
        "--note",
    ],
    placeholders: &[],
}];

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    match a.cmd {
        Sub::Scan(x) => scan::run(ctx, x).await,
        Sub::Findings(x) => findings::list(ctx, x).await,
        Sub::Updates(x) => updates::run(ctx, x).await,
        Sub::Propose(x) => propose::run(ctx, x).await,
        Sub::Dismiss(x) => findings::dismiss(ctx, x).await,
        Sub::Reopen(x) => findings::reopen(ctx, x).await,
        Sub::Config(x) => updates::config(ctx, x).await,
    }
}

/// The remote to open: `remote` as given, or with the token of `token_env` as the password of its URL.
/// A token in the environment can only go to an https URL named on the command line, never to a
/// configured remote it was not meant for.
pub(crate) fn remote_with_token(
    remote: Option<&str>,
    token_env: Option<&str>,
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    let Some(var) = token_env else {
        return Ok(remote.map(str::to_owned));
    };
    let name = remote.ok_or_else(|| bad("--token-env needs --remote <https URL of the repository>"))?;
    let mut url = url::Url::parse(name)
        .map_err(|_| bad("--token-env needs --remote to be the https URL of the repository"))?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(bad(
            "--token-env needs --remote to be an https URL without a user name or password",
        ));
    }
    let token = get(var)
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| CliError::Fatal(format!("asc: the environment variable {var} is not set")))?;
    if token.contains(['\0', '\r', '\n']) {
        return Err(CliError::Fatal(format!(
            "asc: the environment variable {var} is not a token"
        )));
    }
    url.set_username("token")
        .map_err(|()| bad("--remote cannot carry a credential"))?;
    url.set_password(Some(&token))
        .map_err(|()| bad("--remote cannot carry a credential"))?;
    Ok(Some(url.to_string()))
}

/// The ecosystem names of `--ecosystem` flags, checked here so a typo fails with a message.
pub(crate) fn ecosystems(raw: &[String]) -> Result<Vec<String>> {
    const KNOWN: [&str; 12] = [
        "crates.io", "npm", "pypi", "go", "maven", "nuget", "packagist", "rubygems", "hex", "pub",
        "swifturl", "cocoapods",
    ];
    const ALIASES: [&str; 20] = [
        "cargo", "rust", "yarn", "pnpm", "pip", "python", "poetry", "uv", "golang", "gradle",
        "java", "dotnet", "composer", "php", "bundler", "ruby", "mix", "elixir", "dart", "swift",
    ];
    for e in raw {
        let l = e.to_ascii_lowercase();
        if !KNOWN.contains(&l.as_str()) && !ALIASES.contains(&l.as_str()) {
            return Err(bad(format!("{e:?} is not an ecosystem the server reads")));
        }
    }
    Ok(raw.to_vec())
}

pub(crate) fn severity_name(n: i32) -> &'static str {
    match wire::DepSeverity::try_from(n) {
        Ok(wire::DepSeverity::Low) => "low",
        Ok(wire::DepSeverity::Moderate) => "moderate",
        Ok(wire::DepSeverity::High) => "high",
        Ok(wire::DepSeverity::Critical) => "critical",
        _ => "unrated",
    }
}

pub(crate) fn kind_name(n: i32) -> &'static str {
    match wire::DepUpdateKind::try_from(n) {
        Ok(wire::DepUpdateKind::Patch) => "patch",
        Ok(wire::DepUpdateKind::Minor) => "minor",
        Ok(wire::DepUpdateKind::Major) => "major",
        _ => "-",
    }
}

pub(crate) fn lookup_name(n: i32) -> &'static str {
    match wire::DepLookupStatus::try_from(n) {
        Ok(wire::DepLookupStatus::Complete) => "complete",
        Ok(wire::DepLookupStatus::Partial) => "partial",
        Ok(wire::DepLookupStatus::Failed) => "failed",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_from_the_environment_goes_into_the_url_it_was_meant_for() {
        let env = |k: &str| (k == "DEPS_TOKEN").then(|| " s3cret\n".to_owned());
        let got = remote_with_token(Some("https://asc.example.com/o/r"), Some("DEPS_TOKEN"), env)
            .unwrap()
            .unwrap();
        assert_eq!(got, "https://token:s3cret@asc.example.com/o/r");
        // No variable: the remote as given.
        assert_eq!(
            remote_with_token(Some("origin"), None, env).unwrap().as_deref(),
            Some("origin")
        );
        assert_eq!(remote_with_token(None, None, env).unwrap(), None);
    }

    #[test]
    fn a_token_is_never_sent_somewhere_it_was_not_named_for() {
        let env = |_: &str| Some("t".to_owned());
        for remote in [
            None,
            Some("origin"),
            Some("http://asc.example.com/o/r"),
            Some("https://user@asc.example.com/o/r"),
            Some("https://user:pw@asc.example.com/o/r"),
            Some("ssh://asc.example.com/o/r"),
        ] {
            assert!(
                remote_with_token(remote, Some("DEPS_TOKEN"), env).is_err(),
                "{remote:?}"
            );
        }
        let none = |_: &str| None;
        assert!(remote_with_token(Some("https://h/o/r"), Some("DEPS_TOKEN"), none).is_err());
        let blank = |_: &str| Some("  \n".to_owned());
        assert!(remote_with_token(Some("https://h/o/r"), Some("DEPS_TOKEN"), blank).is_err());
        let multi = |_: &str| Some("a\nb".to_owned());
        assert!(remote_with_token(Some("https://h/o/r"), Some("DEPS_TOKEN"), multi).is_err());
        let special = |_: &str| Some("a/b c@d".to_owned());
        let got = remote_with_token(Some("https://h/o/r"), Some("T"), special).unwrap().unwrap();
        assert!(!got.contains("a/b c@d"), "the password is percent-encoded: {got}");
        assert_eq!(url::Url::parse(&got).unwrap().host_str(), Some("h"));
    }

    #[test]
    fn ecosystem_flags_are_checked_before_the_round_trip() {
        assert!(ecosystems(&["cargo".into(), "NPM".into(), "PyPI".into()]).is_ok());
        assert!(ecosystems(&["cobol".into()]).is_err());
        assert!(ecosystems(&[]).unwrap().is_empty());
    }

    #[test]
    fn enum_values_have_short_names() {
        assert_eq!(severity_name(wire::DepSeverity::Critical as i32), "critical");
        assert_eq!(severity_name(0), "unrated");
        assert_eq!(kind_name(wire::DepUpdateKind::Minor as i32), "minor");
        assert_eq!(kind_name(0), "-");
        assert_eq!(lookup_name(wire::DepLookupStatus::Partial as i32), "partial");
        assert_eq!(lookup_name(99), "unknown");
    }
}
