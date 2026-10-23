//! Harness for U79: sandboxes, helper recorders, the two clients (real git and asc's
//! resolver plus HTTP layer) and the comparisons between them.
//!
//! Every assertion message goes through [`mask`] or [`redact_call`], so a failing run never
//! prints a credential.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use asc_git::realgit::RealGit;
use gix_config::File as Config;
use gix_config::file::Metadata;
use gix_sec::Trust;
use reqwest::header::{LOCATION, WWW_AUTHENTICATE};
use sha2::{Digest, Sha256};
use url::Url;

use crate::credential::{
    MAX_REDIRECTS, Prompt, RedirectAction, Resolved, Resolver, failure_message, redirect_action,
    redirect_policy, report_result,
};
use crate::fake_host::FakeHost;

/// A secret that must never reach a message, a log or a second host.
pub const CANARY: &str = "canary-S3CR3T-do-not-leak";

pub fn real() -> RealGit {
    RealGit::resolve(None, None).expect("real git")
}

pub fn cfg(text: &str) -> Config {
    Config::try_from(text).expect("config parses")
}

/// `text` as one config section source with the given trust.
pub fn config_with_trust(text: &str, trust: Trust) -> Config {
    let meta = Metadata {
        path: None,
        source: gix_config::Source::Local,
        level: 0,
        trust,
    };
    Config::from_bytes_no_includes(text.as_bytes(), meta, Default::default()).expect("config")
}

/// `path` as git config and sh want it.
pub fn sh_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// `<scheme> #<first 8 hex of sha256>` for an `Authorization` value, `-` for none: enough to
/// compare two credentials in a message without showing either.
pub fn mask(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "-".to_owned();
    };
    let scheme = value.split(' ').next().unwrap_or("");
    let digest = Sha256::digest(value.as_bytes());
    format!(
        "{scheme} #{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

// ---------------------------------------------------------------- helper recorders

/// A credential helper that logs every call with the attributes git sent, and answers `get`
/// with `answer`.
pub struct Recorder {
    script: PathBuf,
    log: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Call {
    pub op: String,
    pub attrs: Vec<String>,
}

impl Recorder {
    pub fn new(dir: &Path, name: &str, answer: &str) -> Self {
        let script = dir.join(format!("{name}.sh"));
        let log = dir.join(format!("{name}.log"));
        let body = format!(
            "#!/bin/sh\n{{ echo \"== $1\"; cat; echo '--'; }} >> '{}'\ncase \"$1\" in get)\ncat <<'EOF'\n{answer}\nEOF\n;; esac\n",
            sh_path(&log)
        );
        std::fs::write(&script, body).expect("write helper");
        Self { script, log }
    }

    /// Answers `get` with a user name and password.
    pub fn plain(dir: &Path, name: &str, user: &str, pass: &str) -> Self {
        Self::new(dir, name, &format!("username={user}\npassword={pass}"))
    }

    /// Answers `get` as Git Credential Manager does for a Bearer challenge.
    pub fn authtype(dir: &Path, name: &str, authtype: &str, credential: &str) -> Self {
        Self::new(
            dir,
            name,
            &format!("capability[]=authtype\nauthtype={authtype}\ncredential={credential}"),
        )
    }

    /// The `credential.helper` value.
    pub fn config(&self) -> String {
        format!("!sh '{}'", sh_path(&self.script))
    }

    pub fn calls(&self) -> Vec<Call> {
        let Ok(text) = std::fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        text.split("--\n")
            .filter(|b| !b.trim().is_empty())
            .map(|b| {
                let mut lines = b.lines();
                let op = lines
                    .next()
                    .unwrap_or_default()
                    .trim_start_matches("== ")
                    .to_owned();
                Call {
                    op,
                    attrs: lines.filter(|l| !l.is_empty()).map(str::to_owned).collect(),
                }
            })
            .collect()
    }

    pub fn ops(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.op).collect()
    }
}

impl Call {
    pub fn has(&self, line: &str) -> bool {
        self.attrs.iter().any(|a| a == line)
    }

    /// The attributes with the host's port replaced, sorted: two runs against two hosts
    /// compare equal when git and asc sent the same things. Left out, because they differ and
    /// carry no credential: `capability[]=state` (git offers it for multi-round helpers, asc
    /// has none; one test pins that), and on `store` and `erase` the `capability[]=authtype`
    /// asc adds and the `wwwauth[]` lines git adds.
    pub fn normalized(&self, port: u16) -> Vec<String> {
        let approve_like = matches!(self.op.as_str(), "store" | "erase");
        let mut attrs: Vec<String> = self
            .attrs
            .iter()
            .filter(|a| *a != "capability[]=state")
            .filter(|a| {
                !(approve_like && (*a == "capability[]=authtype" || a.starts_with("wwwauth[]=")))
            })
            .map(|a| a.replace(&format!(":{port}"), ":PORT"))
            .collect();
        attrs.sort();
        attrs
    }
}

/// A call for a failure message: the keys, with secret values hidden.
pub fn redact_call(call: &Call) -> String {
    let attrs: Vec<String> = call
        .attrs
        .iter()
        .map(|a| match a.split_once('=') {
            Some((k @ ("password" | "credential" | "username" | "state[]"), _)) => {
                format!("{k}=<hidden>")
            }
            _ => a.clone(),
        })
        .collect();
    format!("{} {attrs:?}", call.op)
}

pub fn redact_calls(calls: &[Call]) -> String {
    calls
        .iter()
        .map(redact_call)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// git and asc called the helper the same way: same operations, same attributes.
pub fn assert_same_calls(git: &[Call], asc: &[Call], ports: (u16, u16), what: &str) {
    let ops = |c: &[Call]| c.iter().map(|c| c.op.clone()).collect::<Vec<_>>();
    assert_eq!(
        ops(git),
        ops(asc),
        "{what}: helper operations differ\n git: {}\n asc:  {}",
        redact_calls(git),
        redact_calls(asc)
    );
    for (g, c) in git.iter().zip(asc) {
        assert_eq!(
            g.normalized(ports.0),
            c.normalized(ports.1),
            "{what}: attributes of `{}` differ\n git: {}\n asc:  {}",
            g.op,
            redact_call(g),
            redact_call(c)
        );
    }
}

// ---------------------------------------------------------------- sandboxes

/// What one run is configured with. `other` is git config that is not `credential.*`
/// (`http.*`, `core.askPass`); `credential` is the `credential.*` text. asc's anonymous first
/// request is resolved from `other` alone, so a helper is asked only after the host's challenge,
/// as git asks it.
#[derive(Default)]
pub struct Spec {
    pub other: String,
    pub credential: String,
    /// Written as `~/.netrc` and `~/_netrc`.
    pub netrc: Option<String>,
    /// `ASC_TOKEN` and friends for asc's resolver.
    pub asc_env: Vec<(String, String)>,
    /// The `credential.*` text lives in the repository's own config (for git), and asc reads it
    /// with this trust: `Reduced` is a repo config another user owns.
    pub repo_credential: Option<Trust>,
}

impl Spec {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn other(mut self, text: impl AsRef<str>) -> Self {
        self.other.push_str(text.as_ref());
        self
    }

    #[must_use]
    pub fn credential(mut self, text: impl AsRef<str>) -> Self {
        self.credential.push_str(text.as_ref());
        self
    }

    #[must_use]
    pub fn in_repo(mut self, trust: Trust) -> Self {
        self.repo_credential = Some(trust);
        self
    }

    #[must_use]
    pub fn netrc(mut self, text: impl Into<String>) -> Self {
        self.netrc = Some(text.into());
        self
    }
}

/// One host, one isolated home and one global git config.
pub struct Side {
    pub host: FakeHost,
    dir: tempfile::TempDir,
    spec: Spec,
}

impl Side {
    pub async fn new(setup: impl Fn(&FakeHost)) -> Self {
        let host = FakeHost::start().await;
        setup(&host);
        Self {
            host,
            dir: tempfile::tempdir().expect("tempdir"),
            spec: Spec::new(),
        }
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn home(&self) -> PathBuf {
        self.dir().join("home")
    }

    pub fn port(&self) -> u16 {
        self.host.addr.port()
    }

    pub fn url(&self) -> String {
        self.host.url()
    }

    /// The remote URL with `userinfo@` in front of the host.
    pub fn url_with_userinfo(&self, userinfo: &str) -> String {
        format!(
            "http://{userinfo}@{}{}",
            self.host.addr,
            crate::fake_host::REPO_PATH
        )
    }

    /// Writes the config and netrc files.
    pub fn configure(&mut self, spec: Spec) {
        std::fs::create_dir_all(self.home()).expect("home");
        if spec.repo_credential.is_some() {
            // A bare-bones repository whose own config carries the credential text.
            let git_dir = self.repo().join(".git");
            std::fs::create_dir_all(git_dir.join("objects")).expect("repo");
            std::fs::create_dir_all(git_dir.join("refs")).expect("repo");
            std::fs::write(
                git_dir.join("HEAD"),
                "ref: refs/heads/main
",
            )
            .expect("HEAD");
            std::fs::write(
                git_dir.join("config"),
                format!(
                    "[core]
	repositoryformatversion = 0
{}",
                    spec.credential
                ),
            )
            .expect("repo config");
            std::fs::write(self.gitconfig(), &spec.other).expect("gitconfig");
        } else {
            std::fs::write(
                self.gitconfig(),
                format!("{}{}", spec.other, spec.credential),
            )
            .expect("gitconfig");
        }
        if let Some(netrc) = &spec.netrc {
            for name in [".netrc", "_netrc"] {
                std::fs::write(self.home().join(name), netrc).expect("netrc");
            }
        }
        self.spec = spec;
    }

    fn repo(&self) -> PathBuf {
        self.dir().join("repo")
    }

    fn gitconfig(&self) -> PathBuf {
        self.dir().join("gitconfig")
    }

    /// Real `git ls-remote <url>` in this sandbox: `HOME`, global config and netrc are the
    /// sandbox's, the system config and every inherited `GIT_*` variable are gone (except the
    /// prompt controls when a test re-ran itself with them set).
    pub fn git(&self, url: &str) -> GitRun {
        let mut cmd = real().command().expect("git command");
        let inner = is_inner();
        for (key, _) in std::env::vars_os() {
            let upper = key.to_string_lossy().to_ascii_uppercase();
            let keep = inner && matches!(upper.as_str(), "GIT_ASKPASS" | "GIT_TERMINAL_PROMPT");
            let drop = upper.starts_with("GIT_")
                || upper.starts_with("GCM_")
                || upper.starts_with("ASC_")
                || upper == "SSH_ASKPASS"
                || upper.ends_with("_PROXY");
            if drop && !keep {
                cmd.env_remove(key);
            }
        }
        cmd.env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.gitconfig())
            .env("NO_PROXY", "*")
            .current_dir(if self.spec.repo_credential.is_some() {
                self.repo()
            } else {
                self.dir().to_owned()
            });
        if !inner {
            cmd.env("GIT_TERMINAL_PROMPT", "0");
        }
        let out = tokio::task::block_in_place(|| {
            cmd.args(["ls-remote", url])
                .stdin(std::process::Stdio::null())
                .output()
                .expect("run git")
        });
        GitRun {
            ok: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// What asc's resolver gives for `url` after a Basic challenge (helpers included), with
    /// `Prompt::Never`: the credential an `RpcClient` call would carry.
    pub fn resolved(&self, url: &str) -> Resolved {
        let url = Url::parse(url).expect("url");
        let mut full = cfg(&self.spec.other);
        full.append(cfg(&self.spec.credential)).expect("append");
        let resolver = Resolver::new(&full)
            .with_env(|_| None)
            .with_home(self.home())
            .with_real_git(real());
        let challenge = ["Basic realm=\"asc\"".to_owned()];
        tokio::task::block_in_place(|| {
            resolver.resolve_with_challenge(&url, Prompt::Never, &challenge)
        })
        .expect("resolve")
    }

    /// asc's side of the same request: the resolver and the HTTP layer.
    pub async fn asc(&self, url: &str, prompt: Prompt) -> AscRun {
        let url = Url::parse(url).expect("url");
        let mut full = cfg(&self.spec.other);
        let credential = match self.spec.repo_credential {
            Some(trust) => config_with_trust(&self.spec.credential, trust),
            None => cfg(&self.spec.credential),
        };
        full.append(credential).expect("append credential section");
        let probe = cfg(&self.spec.other);
        let env: HashMap<String, String> = self.spec.asc_env.iter().cloned().collect();
        let make = |config| {
            let env = env.clone();
            Resolver::new(config)
                .with_env(move |k| env.get(k).cloned())
                .with_home(self.home())
                .with_real_git(real())
        };
        let (probe_resolver, full_resolver) = (make(&probe), make(&full));
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(redirect_policy())
            .build()
            .expect("client");

        let mut target = url;
        for hop in 0..=MAX_REDIRECTS {
            match round(&probe_resolver, &full_resolver, &client, &target, prompt).await {
                Step::Done(status) => {
                    return AscRun {
                        ok: status == 200,
                        status,
                        message: String::new(),
                    };
                }
                Step::Failed(message) => {
                    return AscRun {
                        ok: false,
                        status: 0,
                        message,
                    };
                }
                Step::Redirect(to) => match redirect_action(&target, &to, hop) {
                    RedirectAction::Follow | RedirectAction::ReAsk => target = to,
                    RedirectAction::Refuse(why) => {
                        return AscRun {
                            ok: false,
                            status: 0,
                            message: why.to_owned(),
                        };
                    }
                },
            }
        }
        AscRun {
            ok: false,
            status: 0,
            message: "too many redirects".into(),
        }
    }
}

pub struct GitRun {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitRun {
    /// git's `fatal:` line, URL userinfo left as git printed it.
    pub fn fatal(&self) -> String {
        self.stderr
            .lines()
            .rev()
            .find(|l| l.starts_with("fatal:"))
            .unwrap_or("")
            .to_owned()
    }
}

pub struct AscRun {
    pub ok: bool,
    pub status: u16,
    pub message: String,
}

enum Step {
    Done(u16),
    Redirect(Url),
    Failed(String),
}

async fn send(client: &reqwest::Client, resolved: &Resolved, url: &Url) -> reqwest::Response {
    resolved
        .apply(client.get(url.clone()), url)
        .header("Git-Protocol", "version=2")
        .send()
        .await
        .expect("request")
}

fn challenge_lines(response: &reqwest::Response) -> Vec<String> {
    response
        .headers()
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect()
}

/// One origin's exchange, in git's order: an anonymous first request (carrying only what
/// sources 1 to 4 give), then, after a 401, the helpers with the challenge; a refused
/// credential is rejected and the next source asked. This is the stand-in for the fetch
/// that is still a stub.
async fn round(
    probe: &Resolver<'_>,
    full: &Resolver<'_>,
    client: &reqwest::Client,
    url: &Url,
    prompt: Prompt,
) -> Step {
    let mut current = match tokio::task::block_in_place(|| probe.resolve(url, Prompt::Never)) {
        Ok(r) => r,
        Err(e) => return Step::Failed(e.to_string()),
    };
    let mut response = send(client, &current, url).await;
    for _ in 0..5 {
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let to = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok());
            return to.map_or_else(|| Step::Failed("bad redirect".into()), Step::Redirect);
        }
        if status != 401 {
            if status == 200 {
                report_result(&current, true);
            }
            return Step::Done(status);
        }
        let lines = challenge_lines(&response);
        let refused = !current.is_anonymous();
        let next = tokio::task::block_in_place(|| {
            if refused {
                full.resolve_after(url, prompt, &current, &lines)
            } else {
                full.resolve_with_challenge(url, prompt, &lines)
            }
        });
        match next {
            Err(e) => return Step::Failed(e.to_string()),
            Ok(n) if n.is_anonymous() => {
                let disabled = prompt == Prompt::Never
                    || std::env::var("GIT_TERMINAL_PROMPT").is_ok_and(|v| v == "0");
                return Step::Failed(failure_message(url, refused, disabled));
            }
            Ok(n) => {
                current = n;
                response = send(client, &current, url).await;
            }
        }
    }
    Step::Failed("too many attempts".into())
}

// ---------------------------------------------------------------- pairs

/// The same configuration run by real git against one fake host and by asc against another.
pub struct Pair<R> {
    pub git: Side,
    pub git_r: R,
    pub asc: Side,
    pub asc_r: R,
}

/// Starts two identical hosts (`setup` configures each) and builds each side's spec with
/// `build`, which also returns whatever the test needs to read back (recorders, paths).
pub async fn pair<R>(setup: impl Fn(&FakeHost), build: impl Fn(&Side) -> (Spec, R)) -> Pair<R> {
    let mut git = Side::new(&setup).await;
    let mut asc = Side::new(&setup).await;
    let (spec, git_r) = build(&git);
    git.configure(spec);
    let (spec, asc_r) = build(&asc);
    asc.configure(spec);
    Pair {
        git,
        git_r,
        asc,
        asc_r,
    }
}

impl<R> Pair<R> {
    pub fn ports(&self) -> (u16, u16) {
        (self.git.port(), self.asc.port())
    }

    /// The `Authorization` each host accepted is the same value.
    pub fn assert_same_accepted_authorization(&self, what: &str) {
        let (g, c) = (
            self.git.host.accepted_authorization(),
            self.asc.host.accepted_authorization(),
        );
        assert!(g.is_some(), "{what}: git was never accepted");
        assert_eq!(
            mask(g.as_deref()),
            mask(c.as_deref()),
            "{what}: Authorization on the wire differs (git, asc)"
        );
        assert_eq!(g, c, "{what}: Authorization on the wire differs");
    }
}

// ---------------------------------------------------------------- re-running with an environment

/// Marks a re-run of this test binary in a child with a controlled environment.
const INNER: &str = "AUTH_COMPAT_INNER";

pub fn is_inner() -> bool {
    std::env::var_os(INNER).is_some()
}

/// Re-runs one test of this binary in a child with `envs` set and every inherited `GIT_*`,
/// `GCM_*` and `SSH_ASKPASS` variable removed, so a test can set `GIT_ASKPASS` and
/// `GIT_TERMINAL_PROMPT` for real git and for asc's child gits alike without touching its
/// siblings. The child test returns at once unless [`INNER`] is set.
pub fn rerun(inner: &str, envs: &[(&str, &str)]) {
    let mut cmd = Command::new(std::env::current_exe().expect("exe"));
    cmd.args([inner, "--exact", "--nocapture", "--test-threads=1"]);
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_") || upper == "SSH_ASKPASS" || upper.starts_with("GCM_") {
            cmd.env_remove(key);
        }
    }
    cmd.env(INNER, "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("re-run");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && text.contains("1 passed"),
        "{inner} failed:\n{text}"
    );
}

/// A program that records each prompt it is asked and answers by prompt text:
/// `Username` gets `user`, anything else `pass`.
pub fn askpass(dir: &Path, name: &str, user: &str, pass: &str) -> (PathBuf, PathBuf) {
    let script = dir.join(format!("{name}.sh"));
    let ran = dir.join(format!("{name}.ran"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$1\" >> '{}'\ncase \"$1\" in Username*) echo '{user}';; *) echo '{pass}';; esac\n",
            sh_path(&ran)
        ),
    )
    .expect("write askpass");
    (script, ran)
}

/// git's `fatal:` line and asc's failure message say the same thing about the same URL.
///
/// Known wording differences, pinned here rather than hidden: for "could not read Username"
/// git names the origin only, asc the whole remote URL; for "Authentication failed" git adds a
/// trailing `/`. The words around the quoted URL and the URL's origin must agree.
pub fn assert_same_failure_words(git_fatal: &str, asc_message: &str, ports: (u16, u16)) {
    let split = |text: &str, port: u16| -> (String, String, String) {
        let text = text.replace(&format!(":{port}"), ":PORT");
        let open = text.find('\'').expect("quoted url") + 1;
        let close = text.rfind('\'').expect("closing quote");
        (
            text[..open].to_owned(),
            text[open..close].trim_end_matches('/').to_owned(),
            text[close..].to_owned(),
        )
    };
    let (gp, gu, gs) = split(git_fatal, ports.0);
    let (cp, cu, cs) = split(asc_message, ports.1);
    assert_eq!((&gp, &gs), (&cp, &cs), "failure words differ");
    assert!(
        cu.starts_with(&gu),
        "asc names a different URL: git {gu} asc {cu}"
    );
}
