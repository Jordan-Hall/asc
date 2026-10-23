//! Git's credential helper protocol through real git (SPEC §15.6.1, steps 5 and 6).
//!
//! `fill`, `approve` and `reject` run `git credential fill|approve|reject` through
//! [`RealGit::command`], so helper discovery, `credential.helper` lists, URL-scoped
//! `credential.<url>.*` and every helper git users already have behave as in git. The
//! request and the answer travel on stdin and stdout, never in argv. A value with NUL, CR or
//! LF is refused before any helper runs and after one answers (the injection class of
//! CVE-2020-5260). [`Prompt::Never`] is what libraries use: nothing may ask a person.
//!
//! The resolver that orders these calls with the other credential sources is
//! `asc::credential`; this module holds what needs git: the protocol, the trusted view of the
//! config ([`HelperConfig`]), [`netrc`] and [`urlmatch`].

use std::fmt;
use std::io::{Read as _, Write as _};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gix_config::File;
use gix_sec::Trust;

use crate::realgit::{RealGit, RealGitError};

pub mod netrc;
pub mod urlmatch;

/// The longest answer read from `git credential`.
const MAX_ANSWER: u64 = 64 * 1024;
/// How long a non-interactive helper run may take before it is killed.
const NEVER_TIMEOUT: Duration = Duration::from_secs(60);

/// Failures of a credential lookup.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// Real git could not be run.
    #[error(transparent)]
    RealGit(#[from] RealGitError),
    /// Spawning or talking to git failed.
    #[error("git credential: {0}")]
    Io(#[from] std::io::Error),
    /// A request or answer value is empty where it must not be, or holds NUL, CR or LF.
    #[error("git credential: invalid {0}")]
    Invalid(&'static str),
    /// `approve` or `reject` did not succeed.
    #[error("git credential: {0} failed")]
    Failed(&'static str),
    /// A non-interactive run did not finish in time.
    #[error("git credential: timed out")]
    TimedOut,
}

/// Whether anything may ask a person for a credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// Libraries, the daemon, MCP and the IDE bridge: `GIT_TERMINAL_PROMPT=0`,
    /// `GCM_INTERACTIVE=never`, `credential.interactive=false`, and `GIT_ASKPASS`,
    /// `SSH_ASKPASS` and `core.askPass` neutralised.
    Never,
    /// An `asc` command on a terminal: `GIT_ASKPASS`, `core.askPass`, `SSH_ASKPASS`, then the
    /// terminal, exactly as git does. `GIT_TERMINAL_PROMPT=0` still disables the terminal.
    Allowed,
}

/// A username and password, or an `authtype` and `credential`, returned by a credential
/// helper. `Debug` shows no value.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Credential {
    pub username: String,
    pub password: String,
    /// The helper's own scheme (`Bearer`, `Basic`, ...) when real git supports capabilities.
    pub authtype: Option<String>,
    /// The helper's ready-made credential for `authtype`.
    pub credential: Option<String>,
    /// Unix seconds after which the credential is expired.
    pub password_expiry_utc: Option<i64>,
    pub oauth_refresh_token: Option<String>,
    /// Opaque `state[]` lines handed back to the helper on `approve` and `reject`.
    pub state: Vec<String>,
    pub ephemeral: bool,
}

impl Credential {
    /// A plain username and password.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            ..Self::default()
        }
    }

    /// Whether `password_expiry_utc` is in the past.
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.password_expiry_utc.is_some_and(|t| t <= now)
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("authtype", &self.authtype.as_ref().map(|_| "<redacted>"))
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

fn clean(v: &str) -> bool {
    !v.contains(['\0', '\n', '\r'])
}

fn require(v: &str, what: &'static str) -> Result<(), CredentialError> {
    if v.is_empty() || !clean(v) {
        return Err(CredentialError::Invalid(what));
    }
    Ok(())
}

/// What is being asked for. `host` is the bare host; `port` is set only when it is not the
/// scheme's default, and goes out as `host:port`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CredentialRequest {
    pub protocol: String,
    pub host: String,
    pub port: Option<u16>,
    /// Sent only when `credential.useHttpPath` is set for the URL (the caller decides).
    pub path: Option<String>,
    pub username: Option<String>,
    /// The 401's `WWW-Authenticate` lines.
    pub wwwauth: Vec<String>,
}

impl CredentialRequest {
    /// A request for `protocol` and `host` and nothing else.
    pub fn new(protocol: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            protocol: protocol.into(),
            host: host.into(),
            ..Self::default()
        }
    }

    /// The `host` attribute value: `host`, or `host:port` for a non-default port.
    pub fn host_attribute(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.host),
            None => self.host.clone(),
        }
    }

    /// The helper protocol text for `fill`, or for `approve` and `reject` when `credential`
    /// is given. Fails before anything runs if any value holds NUL, CR or LF.
    pub fn encode(&self, credential: Option<&Credential>) -> Result<String, CredentialError> {
        require(&self.protocol, "protocol")?;
        require(&self.host, "host")?;
        let mut out = format!(
            "protocol={}\nhost={}\n",
            self.protocol,
            self.host_attribute()
        );
        let mut put = |key: &str, value: &str, what: &'static str| {
            if !clean(value) {
                return Err(CredentialError::Invalid(what));
            }
            out.push_str(&format!("{key}={value}\n"));
            Ok(())
        };
        if let Some(path) = self.path.as_deref().filter(|p| !p.is_empty()) {
            put("path", path, "path")?;
        }
        let username = credential
            .map(|c| c.username.as_str())
            .filter(|u| !u.is_empty())
            .or(self.username.as_deref().filter(|u| !u.is_empty()));
        if let Some(u) = username {
            put("username", u, "username")?;
        }
        if let Some(c) = credential {
            if !c.password.is_empty() {
                put("password", &c.password, "password")?;
            }
            if let Some(v) = &c.authtype {
                put("authtype", v, "authtype")?;
            }
            if let Some(v) = &c.credential {
                put("credential", v, "credential")?;
            }
            if let Some(v) = c.password_expiry_utc {
                put("password_expiry_utc", &v.to_string(), "password_expiry_utc")?;
            }
            if let Some(v) = &c.oauth_refresh_token {
                put("oauth_refresh_token", v, "oauth_refresh_token")?;
            }
            for s in &c.state {
                put("state[]", s, "state")?;
            }
            if c.ephemeral {
                put("ephemeral", "1", "ephemeral")?;
            }
        }
        for line in &self.wwwauth {
            put("wwwauth[]", line, "wwwauth")?;
        }
        // Real git before 2.46 does not know the key and ignores it.
        put("capability[]", "authtype", "capability")?;
        out.push('\n');
        Ok(out)
    }
}

/// Parses `git credential fill` output. `Ok(None)` when no usable credential is in it,
/// `Err` when any value holds NUL, CR or LF.
pub fn parse_answer(output: &str) -> Result<Option<Credential>, CredentialError> {
    let mut found = Credential::default();
    let (mut have_user, mut have_pass) = (false, false);
    for line in output.split('\n') {
        if !clean(line) {
            return Err(CredentialError::Invalid("helper answer"));
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "username" => {
                found.username = value.to_owned();
                have_user = !value.is_empty();
            }
            "password" => {
                found.password = value.to_owned();
                have_pass = !value.is_empty();
            }
            "authtype" if !value.is_empty() => found.authtype = Some(value.to_owned()),
            "credential" if !value.is_empty() => found.credential = Some(value.to_owned()),
            "password_expiry_utc" => found.password_expiry_utc = value.parse().ok(),
            "oauth_refresh_token" if !value.is_empty() => {
                found.oauth_refresh_token = Some(value.to_owned());
            }
            "state[]" => found.state.push(value.to_owned()),
            "ephemeral" => found.ephemeral = matches!(value, "1" | "true"),
            _ => {}
        }
    }
    let by_type = found.authtype.is_some() && found.credential.is_some();
    Ok(((have_user && have_pass) || by_type).then_some(found))
}

/// One key of a config file, with the trust of the section it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEntry {
    /// The subsection (a URL for `http` and `credential`), as written.
    pub sub: Option<String>,
    /// The variable name, lowercased.
    pub name: String,
    /// `None` for a key written without `=`.
    pub value: Option<String>,
    /// The section came from a file the current user owns, or from the command line.
    pub trusted: bool,
}

/// Every key of every `section` section of `file`, in file order.
pub fn section_entries(file: &File, section: &str) -> Vec<RawEntry> {
    let mut out = Vec::new();
    for sec in file.sections() {
        let header = sec.header();
        if !header.name().to_string().eq_ignore_ascii_case(section) {
            continue;
        }
        let sub = header.subsection_name().map(|s| s.to_string());
        let trusted = sec.meta().trust == Trust::Full;
        let body = sec.body();
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for name in body.value_names() {
            let lower = name.to_ascii_lowercase();
            let index = seen.entry(lower.clone()).or_insert(0);
            let value = body.values(&name).get(*index).map(|v| v.to_string());
            *index += 1;
            out.push(RawEntry {
                sub: sub.clone(),
                name: lower,
                value,
                trusted,
            });
        }
    }
    out
}

fn parse_bool(value: Option<&str>) -> bool {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None => true,
        Some("true" | "yes" | "on" | "1") => true,
        Some(v) => v.parse::<i64>().is_ok_and(|n| n != 0),
    }
}

/// What `credential.*` says about one URL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UrlSettings {
    /// `credential.useHttpPath`: the `path` attribute is sent to helpers.
    pub use_http_path: bool,
    /// `credential.username`.
    pub username: Option<String>,
}

/// The `credential.*` keys and `core.askPass` of a config, from trusted sections only: the
/// reduced-trust rules (§11.4, `cli::config::runs_programs_or_redirects`) already drop them
/// from a repo config another user owns, and this view keeps only `Trust::Full` sections
/// even if a caller hands it a config that was not filtered. Passed to real git, it replaces
/// git's own reading of `credential.*`, so a foreign repository cannot make asc run its
/// helper or collect a credential.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelperConfig {
    credential: Vec<RawEntry>,
    ask_pass: Option<String>,
}

impl HelperConfig {
    /// Reads the trusted `credential.*` entries and `core.askPass` of `file`.
    pub fn from_file(file: &File) -> Self {
        let credential = section_entries(file, "credential")
            .into_iter()
            .filter(|e| e.trusted)
            .collect();
        let ask_pass = section_entries(file, "core")
            .into_iter()
            .filter(|e| e.trusted && e.name == "askpass")
            .filter_map(|e| e.value)
            .next_back();
        Self {
            credential,
            ask_pass,
        }
    }

    /// `credential.useHttpPath` and `credential.username` for `url` with git's URL matching.
    pub fn settings_for(&self, url: &str) -> UrlSettings {
        let mut path = urlmatch::Cascade::new(url);
        let mut user = urlmatch::Cascade::new(url);
        let mut out = UrlSettings::default();
        for e in &self.credential {
            match e.name.as_str() {
                "usehttppath" if path.offer(e.sub.as_deref()) => {
                    out.use_http_path = parse_bool(e.value.as_deref());
                }
                "username" if user.offer(e.sub.as_deref()) => {
                    out.username = e.value.clone().filter(|v| !v.is_empty());
                }
                _ => {}
            }
        }
        out
    }

    /// The `core.askPass` program, trusted sources only.
    pub fn ask_pass(&self) -> Option<&str> {
        self.ask_pass.as_deref().filter(|v| !v.is_empty())
    }

    /// The config to hand to real git: an empty `credential.helper` first (it resets
    /// whatever git read from its own files), then every trusted `credential.*` entry in
    /// order.
    fn git_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![("credential.helper".to_owned(), String::new())];
        for e in &self.credential {
            let key = match &e.sub {
                Some(sub) => format!("credential.{sub}.{}", e.name),
                None => format!("credential.{}", e.name),
            };
            if !clean(&key) {
                continue;
            }
            pairs.push((key, e.value.clone().unwrap_or_else(|| "true".to_owned())));
        }
        pairs
    }
}

/// How one `git credential` run is set up.
#[derive(Clone, Debug)]
pub struct Session {
    pub prompt: Prompt,
    /// `Some`: run outside any repository and give git exactly this `credential.*` config.
    /// `None`: git reads its own config from the current directory (S3 helper use).
    pub config: Option<HelperConfig>,
}

impl Session {
    /// Libraries: no prompt, git's own config.
    pub fn never() -> Self {
        Self {
            prompt: Prompt::Never,
            config: None,
        }
    }
}

fn is_config_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "GIT_CONFIG_PARAMETERS"
        || upper == "GIT_CONFIG_COUNT"
        || upper.starts_with("GIT_CONFIG_KEY_")
        || upper.starts_with("GIT_CONFIG_VALUE_")
}

/// Runs `git credential <verb>` with `input` on stdin. `Ok(None)` when git exits non-zero.
fn run(
    real: &RealGit,
    verb: &'static str,
    input: &str,
    session: &Session,
) -> Result<Option<String>, CredentialError> {
    let never = session.prompt == Prompt::Never;
    let mut cmd = real.command()?;
    if never {
        cmd.args(["-c", "credential.interactive=false", "-c", "core.askPass="])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_ASKPASS", "")
            .env_remove("SSH_ASKPASS");
    }
    // Held until git exits: the directory is removed when it drops.
    let mut private: Option<tempfile::TempDir> = None;
    if let Some(config) = &session.config {
        for (key, _) in std::env::vars_os() {
            if is_config_env(&key.to_string_lossy()) {
                cmd.env_remove(key);
            }
        }
        let mut pairs = config.git_pairs();
        // Nothing in a program-running slot comes from anywhere but the trusted snapshot.
        pairs.push(("core.fsmonitor".to_owned(), "false".to_owned()));
        if !never {
            match config.ask_pass() {
                Some(program) => pairs.push(("core.askPass".to_owned(), program.to_owned())),
                None => {
                    // Git's order is GIT_ASKPASS, core.askPass, SSH_ASKPASS. Name no
                    // core.askPass, and keep the SSH_ASKPASS step that an empty one would end.
                    pairs.push(("core.askPass".to_owned(), String::new()));
                    let git_askpass = std::env::var_os("GIT_ASKPASS");
                    if let (None, Some(ssh)) = (git_askpass, std::env::var_os("SSH_ASKPASS")) {
                        cmd.env("GIT_ASKPASS", ssh);
                    }
                }
            }
        }
        cmd.env("GIT_CONFIG_COUNT", pairs.len().to_string());
        for (i, (key, value)) in pairs.iter().enumerate() {
            cmd.env(format!("GIT_CONFIG_KEY_{i}"), key);
            cmd.env(format!("GIT_CONFIG_VALUE_{i}"), value);
        }
        // No repository: run in a fresh private directory (mode 0700 on Unix), never in a
        // shared, guessable one that another user could have filled with a repository whose
        // config runs programs. `GIT_DIR` names a path that does not exist inside it, git may
        // not look above it, and the system config is not read (the snapshot carries it).
        let dir = tempfile::Builder::new()
            .prefix("asc-credential-")
            .tempdir()?;
        cmd.env("GIT_DIR", dir.path().join("no-repository"))
            .env("GIT_CEILING_DIRECTORIES", dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .current_dir(dir.path());
        private = Some(dir);
    }
    cmd.args(["credential", verb])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if never {
            Stdio::null()
        } else {
            Stdio::inherit()
        });
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A helper that exits early closes the pipe; the exit status decides.
        let _ = stdin.write_all(input.as_bytes());
    }
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout.as_mut() {
            let _ = out.take(MAX_ANSWER).read_to_end(&mut buf);
        }
        buf
    });
    let deadline = never.then(|| Instant::now() + NEVER_TIMEOUT);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CredentialError::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = reader.join().unwrap_or_default();
    drop(private);
    Ok(status
        .success()
        .then(|| String::from_utf8_lossy(&out).into_owned()))
}

/// Asks the credential helpers (and, under [`Prompt::Allowed`], the prompt) for `request`.
/// `Ok(None)` when git finds nothing, fails or is not allowed to ask; an expired answer
/// counts as nothing.
pub fn fill(
    real: &RealGit,
    request: &CredentialRequest,
    session: &Session,
) -> Result<Option<Credential>, CredentialError> {
    let input = request.encode(None)?;
    let Some(output) = run(real, "fill", &input, session)? else {
        return Ok(None);
    };
    Ok(parse_answer(&output)?.filter(|c| !c.is_expired()))
}

/// Tells the helpers `credential` worked (`git credential approve`), so `store`, `cache`,
/// the keychain and Git Credential Manager keep a freshly prompted one.
pub fn approve(
    real: &RealGit,
    request: &CredentialRequest,
    credential: &Credential,
    session: &Session,
) -> Result<(), CredentialError> {
    report(real, "approve", request, credential, session)
}

/// Tells the helpers `credential` was refused (`git credential reject`), so they forget it.
pub fn reject(
    real: &RealGit,
    request: &CredentialRequest,
    credential: &Credential,
    session: &Session,
) -> Result<(), CredentialError> {
    report(real, "reject", request, credential, session)
}

fn report(
    real: &RealGit,
    verb: &'static str,
    request: &CredentialRequest,
    credential: &Credential,
    session: &Session,
) -> Result<(), CredentialError> {
    // Reporting never prompts, whatever the lookup was allowed to do.
    let quiet = Session {
        prompt: Prompt::Never,
        config: session.config.clone(),
    };
    let mut request = request.clone();
    request.wwwauth.clear();
    let input = request.encode(Some(credential))?;
    match run(real, verb, &input, &quiet)? {
        Some(_) => Ok(()),
        None => Err(CredentialError::Failed(verb)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> CredentialRequest {
        CredentialRequest::new("https", "git.example.com")
    }

    #[test]
    fn encodes_protocol_host_and_capability() {
        assert_eq!(
            req().encode(None).unwrap(),
            "protocol=https\nhost=git.example.com\ncapability[]=authtype\n\n"
        );
    }

    #[test]
    fn encodes_every_attribute_in_order() {
        let r = CredentialRequest {
            protocol: "https".into(),
            host: "h".into(),
            port: Some(8443),
            path: Some("org/repo.git".into()),
            username: Some("alice".into()),
            wwwauth: vec!["Basic realm=\"x\"".into(), "Bearer".into()],
        };
        assert_eq!(
            r.encode(None).unwrap(),
            "protocol=https\nhost=h:8443\npath=org/repo.git\nusername=alice\n\
             wwwauth[]=Basic realm=\"x\"\nwwwauth[]=Bearer\ncapability[]=authtype\n\n"
        );
    }

    #[test]
    fn approve_input_carries_the_answer_back() {
        let mut c = Credential::new("alice", "pw");
        c.state.push("opaque".into());
        c.password_expiry_utc = Some(4_000_000_000);
        let text = req().encode(Some(&c)).unwrap();
        assert!(text.contains("username=alice\npassword=pw\n"), "{text}");
        assert!(
            text.contains("password_expiry_utc=4000000000\nstate[]=opaque\n"),
            "{text}"
        );
    }

    #[test]
    fn refuses_nul_cr_and_lf_in_a_request() {
        for bad in ["a\nb", "a\rb", "a\0b"] {
            assert!(
                CredentialRequest::new("https", bad).encode(None).is_err(),
                "host {bad:?}"
            );
            assert!(
                CredentialRequest::new(bad, "h").encode(None).is_err(),
                "protocol {bad:?}"
            );
            let mut r = req();
            r.path = Some(bad.into());
            assert!(r.encode(None).is_err(), "path {bad:?}");
            let mut r = req();
            r.username = Some(bad.into());
            assert!(r.encode(None).is_err(), "username {bad:?}");
            let mut r = req();
            r.wwwauth = vec![bad.into()];
            assert!(r.encode(None).is_err(), "wwwauth {bad:?}");
            assert!(
                req().encode(Some(&Credential::new("u", bad))).is_err(),
                "password {bad:?}"
            );
        }
        assert!(CredentialRequest::new("", "h").encode(None).is_err());
        assert!(CredentialRequest::new("s3", "").encode(None).is_err());
    }

    #[test]
    fn parses_username_and_password() {
        let c = parse_answer("protocol=s3\nusername=AK\npassword=se=cret\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            (c.username.as_str(), c.password.as_str()),
            ("AK", "se=cret")
        );
    }

    #[test]
    fn parses_authtype_credential_and_extras() {
        let c = parse_answer(
            "capability[]=authtype\nauthtype=Bearer\ncredential=tok\nstate[]=a\nstate[]=b\npassword_expiry_utc=4000000000\nephemeral=1\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.authtype.as_deref(), Some("Bearer"));
        assert_eq!(c.credential.as_deref(), Some("tok"));
        assert_eq!(c.state, ["a", "b"]);
        assert!(c.ephemeral && !c.is_expired());
    }

    #[test]
    fn incomplete_answers_are_nothing() {
        for text in [
            "",
            "username=AK\n",
            "password=x\n",
            "username=\npassword=x\n",
            "username=AK\npassword=\n",
            "authtype=Bearer\n",
        ] {
            assert!(parse_answer(text).unwrap().is_none(), "{text:?}");
        }
    }

    #[test]
    fn dirty_answers_are_refused() {
        for text in [
            "username=A\0K\npassword=x\n",
            "username=AK\npassword=a\rb\n",
            "username=AK\r\npassword=x\r\n",
        ] {
            assert!(
                matches!(parse_answer(text), Err(CredentialError::Invalid(_))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn expired_credentials_are_expired() {
        let mut c = Credential::new("u", "p");
        c.password_expiry_utc = Some(1);
        assert!(c.is_expired());
        c.password_expiry_utc = None;
        assert!(!c.is_expired());
    }

    #[test]
    fn debug_redacts() {
        let mut c = parse_answer("username=AK\npassword=hunter2\n")
            .unwrap()
            .unwrap();
        c.authtype = Some("Bearer".into());
        c.credential = Some("tok3n".into());
        let d = format!("{c:?}");
        assert!(
            !["AK", "hunter2", "tok3n"].iter().any(|s| d.contains(s)),
            "{d}"
        );
    }

    fn file(text: &str, trust: Trust) -> File {
        let meta = gix_config::file::Metadata {
            path: None,
            source: gix_config::Source::Local,
            level: 0,
            trust,
        };
        File::from_bytes_no_includes(text.as_bytes(), meta, Default::default()).unwrap()
    }

    #[test]
    fn helper_config_keeps_trusted_sections_only() {
        let text = "[credential]\n\thelper = evil\n[core]\n\taskPass = /evil\n";
        let reduced = HelperConfig::from_file(&file(text, Trust::Reduced));
        assert_eq!(reduced, HelperConfig::default());
        assert_eq!(reduced.git_pairs().len(), 1);
        let full = HelperConfig::from_file(&file(text, Trust::Full));
        assert_eq!(full.ask_pass(), Some("/evil"));
        assert_eq!(
            full.git_pairs()[1],
            ("credential.helper".to_owned(), "evil".to_owned())
        );
    }

    #[test]
    fn settings_use_the_most_specific_url_section() {
        let text = "[credential]\n\tuseHttpPath = true\n\tusername = anyone\n\
            [credential \"https://git.example.com\"]\n\tuseHttpPath = false\n\
            [credential \"https://git.example.com/org\"]\n\tusername = org-bot\n\
            [credential \"https://other.example\"]\n\tuseHttpPath = true\n\tusername = nobody\n";
        let cfg = HelperConfig::from_file(&file(text, Trust::Full));
        let a = cfg.settings_for("https://git.example.com/org/repo");
        assert_eq!(
            (a.use_http_path, a.username.as_deref()),
            (false, Some("org-bot"))
        );
        let b = cfg.settings_for("https://git.example.com/elsewhere");
        assert_eq!(
            (b.use_http_path, b.username.as_deref()),
            (false, Some("anyone"))
        );
        let c = cfg.settings_for("https://third.example/x");
        assert_eq!(
            (c.use_http_path, c.username.as_deref()),
            (true, Some("anyone"))
        );
    }

    /// The real `git credential-store` helper through `fill`, `approve` and `reject`.
    #[test]
    fn real_git_helper_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir
            .path()
            .join("creds")
            .display()
            .to_string()
            .replace('\\', "/");
        let cfg = HelperConfig::from_file(&file(
            &format!(
                "[credential]
	helper = store --file {store}
"
            ),
            Trust::Full,
        ));
        let session = Session {
            prompt: Prompt::Never,
            config: Some(cfg),
        };
        let real = RealGit::resolve(None, None).expect("real git");
        let request = CredentialRequest::new("https", "git.example.com");
        assert_eq!(fill(&real, &request, &session).unwrap(), None);
        approve(
            &real,
            &request,
            &Credential::new("alice", "s3cret"),
            &session,
        )
        .unwrap();
        let found = fill(&real, &request, &session).unwrap().unwrap();
        assert_eq!(
            (found.username.as_str(), found.password.as_str()),
            ("alice", "s3cret")
        );
        reject(&real, &request, &found, &session).unwrap();
        assert_eq!(fill(&real, &request, &session).unwrap(), None);
    }

    #[test]
    fn git_pairs_reset_then_replay_in_order() {
        let text = "[credential]\n\thelper = one\n\thelper = two\n\
            [credential \"https://h\"]\n\thelper = three\n";
        let pairs = HelperConfig::from_file(&file(text, Trust::Full)).git_pairs();
        let keys: Vec<_> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(
            keys,
            [
                "credential.helper=",
                "credential.helper=one",
                "credential.helper=two",
                "credential.https://h.helper=three"
            ]
        );
    }
}
