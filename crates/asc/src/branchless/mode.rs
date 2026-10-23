//! The branchless client keys and the effective-mode resolver (SPEC §21.6.1, B01).
//!
//! `asc.branchless` (default `true`) is the per-user switch; the repo's `branchless` (the server's
//! `RepoConfig`, SPEC §21.5.1) is the per-repo one. The effective mode is their AND. Both are
//! inputs to a pure function, so the cases are testable without a server.

use crate::cli::config::ConfigSnapshot;
use crate::env::GitEnv;

/// The environment variable that overrides `asc.branchless` from the config files (a `-c` still
/// wins over it).
pub const ENV_BRANCHLESS: &str = "ASC_BRANCHLESS";

pub const KEY_BRANCHLESS: &str = "asc.branchless";
pub const KEY_CLOUD: &str = "asc.cloud";
pub const KEY_CLOUD_WORKSPACE: &str = "asc.cloud.workspace";
pub const KEY_PUSH_REVIEW: &str = "asc.push.review";
pub const KEY_HIDDEN_DAYS: &str = "asc.draft.hiddenDays";
pub const KEY_BRANCH_TEMPLATE: &str = "asc.stack.branchTemplate";

/// Default `asc.stack.branchTemplate` (SPEC §20.3).
pub const DEFAULT_BRANCH_TEMPLATE: &str = "{user}/{slug}";
/// Default `asc.draft.hiddenDays` (SPEC §21.2.4).
pub const DEFAULT_HIDDEN_DAYS: u32 = 90;
/// Upper bound for `asc.draft.hiddenDays` (ten years); larger values are refused.
const MAX_HIDDEN_DAYS: u32 = 3650;
/// Longest accepted workspace name and branch template.
const MAX_NAME: usize = 128;

/// `asc.cloud`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudSetting {
    /// On when the client is branchless, the server advertises `cloud.v1` with the repo's
    /// `cloud` flag on, and a daemon runs.
    Auto,
    /// As `Auto` but without the branchless and daemon conditions; still refused on a public repo.
    On,
    Off,
}

/// `asc.push.review` (SPEC §21.4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushReview {
    Draft,
    Publish,
    None,
}

/// `asc.cloud.workspace`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Workspace {
    Default,
    /// The hostname.
    Host,
    Named(String),
}

/// The branchless keys of one invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub branchless: bool,
    pub cloud: CloudSetting,
    pub workspace: Workspace,
    pub push_review: PushReview,
    pub hidden_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            branchless: true,
            cloud: CloudSetting::Auto,
            workspace: Workspace::Default,
            push_review: PushReview::Draft,
            hidden_days: DEFAULT_HIDDEN_DAYS,
        }
    }
}

fn bad(key: &str, value: &str, want: &str) -> String {
    let shown: String = value.chars().filter(|c| !c.is_control()).take(40).collect();
    format!("asc: bad value '{shown}' for {key}: expected {want}")
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    crate::env::parse_git_bool(value).ok_or_else(|| bad(key, value, "true or false"))
}

impl Settings {
    /// Reads the keys from `config` (which already holds system, global, local and `-c`), with
    /// `ASC_BRANCHLESS` from `getenv` between the files and `-c`. A bad value is an error: a
    /// switch that cannot be read is not guessed.
    pub fn resolve(
        config: &ConfigSnapshot,
        env: &GitEnv,
        getenv: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut s = Self::default();
        let from_dash_c = env
            .config_overrides()
            .any(|p| p.key.eq_ignore_ascii_case(KEY_BRANCHLESS));
        let env_value = getenv(ENV_BRANCHLESS).filter(|v| !v.is_empty());
        s.branchless = match (&env_value, from_dash_c) {
            (Some(v), false) => parse_bool(ENV_BRANCHLESS, v)?,
            _ => match config.get(KEY_BRANCHLESS) {
                Some(v) => parse_bool(KEY_BRANCHLESS, &v)?,
                None => true,
            },
        };
        if let Some(v) = config.get(KEY_CLOUD) {
            s.cloud = match v.to_ascii_lowercase().as_str() {
                "auto" => CloudSetting::Auto,
                "on" | "true" | "yes" | "1" => CloudSetting::On,
                "off" | "false" | "no" | "0" => CloudSetting::Off,
                _ => return Err(bad(KEY_CLOUD, &v, "auto, on or off")),
            };
        }
        if let Some(v) = config.get(KEY_CLOUD_WORKSPACE) {
            s.workspace = match v.as_str() {
                "" | "default" => Workspace::Default,
                "host" => Workspace::Host,
                name if name.len() <= MAX_NAME
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) =>
                {
                    Workspace::Named(name.to_owned())
                }
                _ => {
                    return Err(bad(
                        KEY_CLOUD_WORKSPACE,
                        &v,
                        "default, host or a name of letters, digits, '-', '_' and '.'",
                    ));
                }
            };
        }
        if let Some(v) = config.get(KEY_PUSH_REVIEW) {
            s.push_review = match v.to_ascii_lowercase().as_str() {
                "draft" => PushReview::Draft,
                "publish" => PushReview::Publish,
                "none" => PushReview::None,
                _ => return Err(bad(KEY_PUSH_REVIEW, &v, "draft, publish or none")),
            };
        }
        if let Some(v) = config.get(KEY_HIDDEN_DAYS) {
            s.hidden_days = v
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|d| (1..=MAX_HIDDEN_DAYS).contains(d))
                .ok_or_else(|| bad(KEY_HIDDEN_DAYS, &v, "a number of days from 1 to 3650"))?;
        }
        Ok(s)
    }

    /// The effective mode for a repo whose server-side `branchless` is `repo` (`None` when it is
    /// not known, for example with no server remote: the default, `true`, applies).
    pub fn mode(&self, repo: Option<bool>) -> Mode {
        effective_mode(self.branchless, repo)
    }
}

/// The effective client mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Branchless,
    Branches,
}

/// Effective mode = `asc.branchless` AND the repo's `branchless`; an unknown repo flag is `true`.
pub fn effective_mode(client: bool, repo: Option<bool>) -> Mode {
    if client && repo.unwrap_or(true) {
        Mode::Branchless
    } else {
        Mode::Branches
    }
}

/// What a server tells the client about a repo (`RepoConfig`, `cloud.v1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepoPolicy {
    pub branchless: bool,
    pub cloud: bool,
    pub public: bool,
}

/// Whether the client uploads draft changesets to the cloud (SPEC §21.6.1). Needs the client key
/// (`asc.cloud` not `off`), the server's `branchless` and `cloud`, and a non-public repo. `Auto`
/// also needs a branchless client and a running daemon. `asc.cloud=on` on a public repo is an
/// error, not a silent no.
pub fn cloud_upload(
    s: &Settings,
    repo: &RepoPolicy,
    advertises_cloud_v1: bool,
    daemon_running: bool,
) -> Result<bool, String> {
    if s.cloud == CloudSetting::Off {
        return Ok(false);
    }
    if repo.public {
        return if s.cloud == CloudSetting::On {
            Err("asc: asc.cloud=on is refused: this repo is public and drafts are never uploaded to a public repo"
                .to_owned())
        } else {
            Ok(false)
        };
    }
    let server = advertises_cloud_v1 && repo.branchless && repo.cloud;
    Ok(match s.cloud {
        CloudSetting::On => server,
        _ => s.branchless && server && daemon_running,
    })
}

/// What `asc push` does remotely (SPEC §21.6.1, §21.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushStyle {
    /// Branchless client and repo: user refs plus the stack submitted as changes.
    SubmitStack,
    /// Branch-mode client on a branchless repo: user refs, no PRs.
    UserRefsOnly,
    /// A branch-mode repo: the shared branch, as git does.
    SharedBranch,
}

pub fn push_style(client_branchless: bool, repo_branchless: bool) -> PushStyle {
    match (repo_branchless, client_branchless) {
        (false, _) => PushStyle::SharedBranch,
        (true, true) => PushStyle::SubmitStack,
        (true, false) => PushStyle::UserRefsOnly,
    }
}

/// Lowercase `[a-z0-9-]` words from `text`, joined by `-`, at most `max` bytes.
fn slugify(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
            if out.len() >= max {
                break;
            }
        } else {
            dash = true;
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// The branch name for a new change from `asc.stack.branchTemplate`: `{user}` is the slug of
/// `user.name` and `{slug}` the slug of the subject (`fallback` when there is none). Any other
/// text of the template is kept; git validates the result when the branch is made.
pub fn branch_name(
    config: &ConfigSnapshot,
    subject: Option<&str>,
    fallback: &str,
) -> Result<String, String> {
    let template = config
        .get(KEY_BRANCH_TEMPLATE)
        .unwrap_or_else(|| DEFAULT_BRANCH_TEMPLATE.to_owned());
    if template.len() > MAX_NAME || template.starts_with('-') || template.contains('\0') {
        return Err(bad(
            KEY_BRANCH_TEMPLATE,
            &template,
            "a short ref-name template",
        ));
    }
    let user = config
        .get("user.name")
        .map(|n| slugify(&n, 32))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "user".to_owned());
    let slug = subject
        .map(|s| slugify(s, 48))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_owned());
    Ok(template.replace("{user}", &user).replace("{slug}", &slug))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn env(params: &[&str]) -> GitEnv {
        let mut e = GitEnv::from_vars(&|_| None);
        let mut args: Vec<OsString> = Vec::new();
        for p in params {
            args.push("-c".into());
            args.push((*p).into());
        }
        args.push("log".into());
        e.parse_globals(&args, &|_| None).unwrap();
        e
    }

    fn settings(text: &str, dash_c: &[&str], var: Option<&str>) -> Result<Settings, String> {
        let cfg = ConfigSnapshot::from_text(text);
        Settings::resolve(&cfg, &env(dash_c), &|k| {
            if k == ENV_BRANCHLESS {
                var.map(str::to_owned)
            } else {
                None
            }
        })
    }

    #[test]
    fn defaults_are_branchless_with_cloud_auto() {
        let s = settings("", &[], None).unwrap();
        assert_eq!(s, Settings::default());
        assert!(s.branchless && s.cloud == CloudSetting::Auto && s.hidden_days == 90);
    }

    #[test]
    fn env_beats_files_and_dash_c_beats_env() {
        let file = "[asc]\n\tbranchless = false\n";
        assert!(!settings(file, &[], None).unwrap().branchless);
        assert!(settings(file, &[], Some("true")).unwrap().branchless);
        assert!(!settings("", &[], Some("0")).unwrap().branchless);
        let on = "[asc]\n\tbranchless = true\n";
        assert!(
            settings(on, &["asc.branchless=true"], Some("false"))
                .unwrap()
                .branchless
        );
    }

    #[test]
    fn bad_values_are_errors() {
        assert!(settings("[asc]\n\tbranchless = maybe\n", &[], None).is_err());
        assert!(settings("", &[], Some("maybe")).is_err());
        assert!(settings("[asc]\n\tcloud = sometimes\n", &[], None).is_err());
        assert!(settings("[asc \"draft\"]\n\thiddenDays = 0\n", &[], None).is_err());
        assert!(settings("[asc \"push\"]\n\treview = all\n", &[], None).is_err());
    }

    #[test]
    fn keys_parse() {
        let s = settings(
            "[asc]\n\tcloud = off\n[asc \"cloud\"]\n\tworkspace = host\n[asc \"push\"]\n\treview = publish\n[asc \"draft\"]\n\thiddenDays = 30\n",
            &[],
            None,
        )
        .unwrap();
        assert_eq!(s.cloud, CloudSetting::Off);
        assert_eq!(s.workspace, Workspace::Host);
        assert_eq!(s.push_review, PushReview::Publish);
        assert_eq!(s.hidden_days, 30);
    }

    #[test]
    fn mode_is_the_and_of_client_and_repo() {
        assert_eq!(effective_mode(true, Some(true)), Mode::Branchless);
        assert_eq!(effective_mode(true, None), Mode::Branchless);
        assert_eq!(effective_mode(true, Some(false)), Mode::Branches);
        assert_eq!(effective_mode(false, Some(true)), Mode::Branches);
    }

    #[test]
    fn cloud_rules() {
        let s = Settings::default();
        let repo = RepoPolicy {
            branchless: true,
            cloud: true,
            public: false,
        };
        assert_eq!(cloud_upload(&s, &repo, true, true), Ok(true));
        assert_eq!(cloud_upload(&s, &repo, true, false), Ok(false));
        assert_eq!(cloud_upload(&s, &repo, false, true), Ok(false));
        // A branchless client on a branch-mode repo uploads nothing.
        let branchy = RepoPolicy {
            branchless: false,
            cloud: false,
            public: false,
        };
        assert_eq!(cloud_upload(&s, &branchy, true, true), Ok(false));
        let on = Settings {
            cloud: CloudSetting::On,
            ..Settings::default()
        };
        assert_eq!(cloud_upload(&on, &branchy, true, true), Ok(false));
        let public = RepoPolicy {
            public: true,
            ..repo
        };
        assert_eq!(cloud_upload(&s, &public, true, true), Ok(false));
        assert!(
            cloud_upload(&on, &public, true, true)
                .unwrap_err()
                .contains("public")
        );
        let off = Settings {
            cloud: CloudSetting::Off,
            ..Settings::default()
        };
        assert_eq!(cloud_upload(&off, &repo, true, true), Ok(false));
    }

    #[test]
    fn push_follows_the_server() {
        assert_eq!(push_style(true, true), PushStyle::SubmitStack);
        assert_eq!(push_style(false, true), PushStyle::UserRefsOnly);
        assert_eq!(push_style(true, false), PushStyle::SharedBranch);
        assert_eq!(push_style(false, false), PushStyle::SharedBranch);
    }

    #[test]
    fn branch_names_come_from_the_template() {
        let cfg = ConfigSnapshot::from_text("[user]\n\tname = Ada Lovelace\n");
        assert_eq!(
            branch_name(&cfg, Some("feat: Add the Thing!"), "x").unwrap(),
            "ada-lovelace/feat-add-the-thing"
        );
        assert_eq!(
            branch_name(&cfg, None, "change-1").unwrap(),
            "ada-lovelace/change-1"
        );
        let t = ConfigSnapshot::from_text("[asc \"stack\"]\n\tbranchTemplate = wip/{slug}\n");
        assert_eq!(branch_name(&t, Some("hi"), "x").unwrap(), "wip/hi");
        let evil = ConfigSnapshot::from_text("[asc \"stack\"]\n\tbranchTemplate = -x{slug}\n");
        assert!(branch_name(&evil, Some("hi"), "x").is_err());
    }
}
