//! U77 acceptance: the client credential resolver (SPEC §15.6.1, §15.6.4) against real git.
//!
//! `asc` is a binary crate, so the module is compiled into this test crate by path. Helpers
//! are shell scripts that log every invocation with the attributes git sent them; they run
//! through real git exactly as a user's helper would.
#![allow(dead_code, clippy::unwrap_used)]

#[path = "../src/credential.rs"]
mod credential;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use asc_git::credential::netrc::Netrc;
use asc_git::credential::{Credential, CredentialRequest, Session};
use asc_git::realgit::RealGit;
use credential::{
    Origin, Prompt, RedirectAction, Resolved, Resolver, Scheme, Source, mask_url, redirect_action,
    redirect_policy, report_result, same_origin,
};
use gix_config::File as Config;
use gix_config::file::Metadata;
use gix_sec::Trust;
use url::Url;

const CANARY: &str = "canary-S3CR3T-do-not-leak";

fn real() -> RealGit {
    RealGit::resolve(None, None).expect("real git")
}

fn cfg(text: &str) -> Config {
    Config::try_from(text).expect("config parses")
}

fn cfg_trust(text: &str, trust: Trust) -> Config {
    let meta = Metadata {
        path: None,
        source: gix_config::Source::Local,
        level: 0,
        trust,
    };
    Config::from_bytes_no_includes(text.as_bytes(), meta, Default::default()).unwrap()
}

fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

/// `path` as git config and sh want it.
fn sh_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// A resolver that sees `env` instead of the process environment and `home` as the home.
fn resolver<'a>(config: &'a Config, env: &[(&str, &str)], home: &Path) -> Resolver<'a> {
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Resolver::new(config)
        .with_env(move |k| env.get(k).cloned())
        .with_home(home.to_owned())
        .with_real_git(real())
}

/// A logging helper: every call appends `== <op>`, the attributes git sent and `--`;
/// `answer` is what `get` prints.
struct Helper {
    script: PathBuf,
    log: PathBuf,
}

#[derive(Debug)]
struct Call {
    op: String,
    attrs: Vec<String>,
}

impl Helper {
    fn new(dir: &Path, name: &str, answer: &str) -> Self {
        let script = dir.join(format!("{name}.sh"));
        let log = dir.join(format!("{name}.log"));
        let body = format!(
            "#!/bin/sh\n{{ echo \"== $1\"; cat; echo '--'; }} >> '{}'\ncase \"$1\" in get)\ncat <<'EOF'\n{answer}\nEOF\n;; esac\n",
            sh_path(&log)
        );
        std::fs::write(&script, body).unwrap();
        Self { script, log }
    }

    fn plain(dir: &Path, name: &str, user: &str, pass: &str) -> Self {
        Self::new(dir, name, &format!("username={user}\npassword={pass}"))
    }

    /// The `credential.helper` config line value.
    fn config(&self) -> String {
        format!("!sh '{}'", sh_path(&self.script))
    }

    fn calls(&self) -> Vec<Call> {
        let Ok(text) = std::fs::read_to_string(&self.log) else {
            return Vec::new();
        };
        text.split("--\n")
            .filter(|b| !b.trim().is_empty())
            .map(|b| {
                let mut lines = b.lines();
                let op = lines.next().unwrap().trim_start_matches("== ").to_owned();
                Call {
                    op,
                    attrs: lines.filter(|l| !l.is_empty()).map(str::to_owned).collect(),
                }
            })
            .collect()
    }

    fn ops(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.op).collect()
    }
}

fn has(call: &Call, line: &str) -> bool {
    call.attrs.iter().any(|a| a == line)
}

fn auth(resolved: &Resolved, to: &Url) -> Option<String> {
    resolved
        .headers_for(to)
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.expose().to_owned())
}

const REMOTE: &str = "https://git.example.com/org/repo.git";

// ---------------------------------------------------------------- the order

/// Every source configured at once; each refusal moves to the next, in SPEC order.
#[test]
fn sources_are_tried_in_spec_order_and_a_refusal_moves_on() {
    let dir = tempfile::tempdir().unwrap();
    let helper = Helper::plain(dir.path(), "h", "helper-user", "helper-pass");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        Netrc::candidates(&home)[0].clone(),
        "machine git.example.com login urluser password netrc-pass\n",
    )
    .unwrap();
    let config = cfg(&format!(
        "[http \"https://git.example.com/\"]\n\textraHeader = Authorization: Bearer from-header\n\
         [credential]\n\thelper = {}\n",
        helper.config()
    ));
    let target = url("https://urluser:urlpass@git.example.com/org/repo.git");
    let r = resolver(&config, &[("ASC_TOKEN", "env-token")], &home).allow_asc_token(&target);

    let first = r.resolve(&target, Prompt::Never).unwrap();
    assert_eq!(first.source(), "http.extraHeader");
    assert_eq!(first.scheme(), Some(&Scheme::Bearer));
    assert!(
        helper.calls().is_empty(),
        "no helper before the sources above it"
    );

    let second = r
        .resolve_after(&target, Prompt::Never, &first, &[])
        .unwrap();
    assert_eq!(second.source(), "url");
    assert_eq!(
        (second.user(), second.scheme()),
        (Some("urluser"), Some(&Scheme::Basic))
    );
    // The refused header no longer goes out.
    assert!(second.headers_for(&target).iter().all(|(n, v)| {
        !(n.eq_ignore_ascii_case("authorization") && v.expose().contains("from-header"))
    }));

    let third = r
        .resolve_after(&target, Prompt::Never, &second, &[])
        .unwrap();
    assert_eq!(third.source(), "ASC_TOKEN");
    assert_eq!(auth(&third, &target).as_deref(), Some("Bearer env-token"));

    let fourth = r
        .resolve_after(&target, Prompt::Never, &third, &[])
        .unwrap();
    assert_eq!(fourth.source(), "netrc");
    assert_eq!(fourth.secret().unwrap().expose(), "netrc-pass");
    assert!(helper.calls().is_empty());

    let fifth = r
        .resolve_after(&target, Prompt::Never, &fourth, &[])
        .unwrap();
    assert_eq!(fifth.source(), "credential helper");
    assert_eq!(fifth.user(), Some("helper-user"));
    assert_eq!(helper.ops(), ["get"]);
}

#[test]
fn each_source_alone() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let target = url(REMOTE);

    // 1. extraHeader, scoped by URL; `basic` header decodes to its parts.
    let config = cfg(
        "[http \"https://git.example.com/\"]\n\textraHeader = AUTHORIZATION: basic YWxpY2U6c2VjcmV0\n",
    );
    let got = resolver(&config, &[], &home)
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert_eq!(
        (got.source(), got.user()),
        ("http.extraHeader", Some("alice"))
    );
    assert_eq!(got.secret().unwrap().expose(), "secret");
    assert_eq!(
        auth(&got, &target).as_deref(),
        Some("basic YWxpY2U6c2VjcmV0")
    );

    // 2. userinfo: a lone token is Basic `TOKEN:`.
    let config = cfg("");
    let got = resolver(&config, &[], &home)
        .resolve(
            &url("https://TOKEN@git.example.com/org/repo.git"),
            Prompt::Never,
        )
        .unwrap();
    assert_eq!(got.source(), "url");
    assert_eq!(
        auth(&got, &url("https://git.example.com/x")).as_deref(),
        Some("Basic VE9LRU46")
    );
    let got = resolver(&config, &[], &home)
        .resolve(
            &url("https://al%40ice:p%3Aw@git.example.com/"),
            Prompt::Never,
        )
        .unwrap();
    assert_eq!(
        (got.user(), got.secret().unwrap().expose()),
        (Some("al@ice"), "p:w")
    );

    // 3. ASC_TOKEN.
    let got = resolver(&config, &[("ASC_TOKEN", "tok")], &home)
        .allow_asc_token(&target)
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert_eq!(
        (got.source(), got.scheme()),
        ("ASC_TOKEN", Some(&Scheme::Bearer))
    );

    // 4. netrc, by machine and by default entry.
    std::fs::write(
        Netrc::candidates(&home)[0].clone(),
        "machine other.example login x password y\ndefault login anon password anonpw\n",
    )
    .unwrap();
    let got = resolver(&config, &[], &home)
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert_eq!((got.source(), got.user()), ("netrc", Some("anon")));
    assert_eq!(
        auth(&got, &target).as_deref(),
        Some("Basic YW5vbjphbm9ucHc=")
    );

    // 5. a helper.
    let helper = Helper::plain(dir.path(), "h", "hu", "hp");
    let config = cfg(&format!("[credential]\n\thelper = {}\n", helper.config()));
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let got = resolver(&config, &[], &empty)
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert_eq!(
        (got.source(), got.user()),
        ("credential helper", Some("hu"))
    );

    // Nothing at all.
    let got = resolver(&cfg(""), &[], &empty)
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert!(got.is_anonymous());
    assert_eq!(got.source(), "none");
}

#[test]
fn extra_header_matching_follows_git() {
    let dir = tempfile::tempdir().unwrap();
    let config = cfg("[http]\n\textraHeader = X-Plain: one\n\
         [http \"https://*.example.com/\"]\n\textraHeader = X-Wild: two\n\
         [http \"https://git.example.com/org\"]\n\textraHeader = X-Org: three\n\
         [http \"https://other.example.com/\"]\n\textraHeader = X-Other: four\n\
         [http \"https://git.example.com\"]\n\textraHeader = X-Host: five\n");
    let r = resolver(&config, &[], dir.path());
    let got = r
        .resolve(&url("https://git.example.com/org/repo"), Prompt::Never)
        .unwrap();
    let names: Vec<_> = got
        .headers_for(&url("https://git.example.com/org/repo"))
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    // The plain entry always applies; of the URL entries, only those at least as specific
    // as the best so far, in config order: the wildcard one is superseded by the org one.
    assert_eq!(names, ["X-Plain", "X-Wild", "X-Org"]);

    // An empty value resets the list.
    let config = cfg("[http]\n\textraHeader = X-A: 1\n\textraHeader =\n\textraHeader = X-B: 2\n");
    let got = resolver(&config, &[], dir.path())
        .resolve(&url(REMOTE), Prompt::Never)
        .unwrap();
    let names: Vec<_> = got
        .headers_for(&url(REMOTE))
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["X-B"]);

    // A header without an Authorization is not a credential: later sources still run.
    let config = cfg("[http]\n\textraHeader = X-Trace: abc\n");
    let got = resolver(&config, &[("ASC_TOKEN", "t")], dir.path())
        .allow_asc_token(&url(REMOTE))
        .resolve(&url(REMOTE), Prompt::Never)
        .unwrap();
    assert_eq!(got.source(), "ASC_TOKEN");
    let headers = got.headers_for(&url(REMOTE));
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].0, "X-Trace");
}

#[test]
fn asc_token_goes_to_listed_origins_only() {
    let dir = tempfile::tempdir().unwrap();
    let config = cfg("");
    let remote = url("https://asc.example.com/api");
    let objects = url("https://objects.example.com/o");
    let stranger = url("https://elsewhere.example.net/");
    let r = resolver(&config, &[("ASC_TOKEN", "tok")], dir.path())
        .allow_asc_token(&remote)
        .allow_asc_token(&objects);
    assert_eq!(
        r.resolve(&remote, Prompt::Never).unwrap().source(),
        "ASC_TOKEN"
    );
    assert_eq!(
        r.resolve(&objects, Prompt::Never).unwrap().source(),
        "ASC_TOKEN"
    );
    assert!(r.resolve(&stranger, Prompt::Never).unwrap().is_anonymous());
    // Same host, other port or scheme: another origin.
    assert!(
        r.resolve(&url("https://asc.example.com:8443/"), Prompt::Never)
            .unwrap()
            .is_anonymous()
    );
    // No origin named: nothing gets it.
    let bare = resolver(&config, &[("ASC_TOKEN", "tok")], dir.path());
    assert!(bare.resolve(&remote, Prompt::Never).unwrap().is_anonymous());
}

// ---------------------------------------------------------------- helpers

#[test]
fn use_http_path_controls_the_path_attribute() {
    let dir = tempfile::tempdir().unwrap();
    let h = Helper::plain(dir.path(), "h", "u", "p");
    let target = url(REMOTE);
    let ask = |extra: &str| {
        let config = cfg(&format!("[credential]\n\thelper = {}\n{extra}", h.config()));
        resolver(&config, &[], dir.path())
            .resolve(&target, Prompt::Never)
            .unwrap();
        h.calls().pop().unwrap()
    };
    let off = ask("");
    assert!(
        has(&off, "protocol=https") && has(&off, "host=git.example.com"),
        "{off:?}"
    );
    assert!(!off.attrs.iter().any(|a| a.starts_with("path=")), "{off:?}");
    let on = ask("\tuseHttpPath = true\n");
    assert!(has(&on, "path=org/repo.git"), "{on:?}");
    // Per URL: on for this host, off for the rest.
    let scoped = ask("[credential \"https://git.example.com\"]\n\tuseHttpPath = true\n");
    assert!(has(&scoped, "path=org/repo.git"), "{scoped:?}");
    let other = ask("[credential \"https://elsewhere.example\"]\n\tuseHttpPath = true\n");
    assert!(
        !other.attrs.iter().any(|a| a.starts_with("path=")),
        "{other:?}"
    );
}

#[test]
fn username_comes_from_the_url_else_credential_username() {
    let dir = tempfile::tempdir().unwrap();
    let h = Helper::plain(dir.path(), "h", "u", "p");
    let config = cfg(&format!(
        "[credential]\n\thelper = {}\n\tusername = cfguser\n\
         [credential \"https://special.example\"]\n\tusername = special\n",
        h.config()
    ));
    let r = resolver(&config, &[], dir.path());
    r.resolve(&url(REMOTE), Prompt::Never).unwrap();
    r.resolve(&url("https://special.example/x"), Prompt::Never)
        .unwrap();
    let calls = h.calls();
    assert!(has(&calls[0], "username=cfguser"), "{calls:?}");
    assert!(has(&calls[1], "username=special"), "{calls:?}");
    // A user in the URL is source 2, so it never reaches a helper; with netrc absent and
    // no userinfo the helper sees the configured one only.
}

#[test]
fn port_is_part_of_host_only_when_not_default() {
    let dir = tempfile::tempdir().unwrap();
    let h = Helper::plain(dir.path(), "h", "u", "p");
    let config = cfg(&format!("[credential]\n\thelper = {}\n", h.config()));
    let r = resolver(&config, &[], dir.path());
    r.resolve(&url("https://git.example.com:443/x"), Prompt::Never)
        .unwrap();
    r.resolve(&url("https://git.example.com:8443/x"), Prompt::Never)
        .unwrap();
    let calls = h.calls();
    assert!(has(&calls[0], "host=git.example.com"), "{calls:?}");
    assert!(has(&calls[1], "host=git.example.com:8443"), "{calls:?}");
}

#[test]
fn helper_lists_resets_and_url_scoped_helpers_follow_git() {
    let dir = tempfile::tempdir().unwrap();
    let a = Helper::new(dir.path(), "a", "");
    let b = Helper::plain(dir.path(), "b", "bu", "bp");
    let scoped = Helper::plain(dir.path(), "s", "su", "sp");
    let target = url(REMOTE);

    // A helper that answers nothing hands over to the next one in the list.
    let config = cfg(&format!(
        "[credential]\n\thelper = {}\n\thelper = {}\n",
        a.config(),
        b.config()
    ));
    let got = resolver(&config, &[], dir.path())
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert_eq!(got.user(), Some("bu"));
    assert_eq!(
        (a.ops(), b.ops()),
        (vec!["get".to_owned()], vec!["get".to_owned()])
    );

    // An empty value resets: only what follows runs.
    let (a2, b2) = (
        Helper::plain(dir.path(), "a2", "au", "ap"),
        Helper::plain(dir.path(), "b2", "bu2", "bp2"),
    );
    let config = cfg(&format!(
        "[credential]\n\thelper = {}\n\thelper =\n\thelper = {}\n",
        a2.config(),
        b2.config()
    ));
    let got = resolver(&config, &[], dir.path())
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert_eq!(got.user(), Some("bu2"));
    assert!(a2.calls().is_empty());

    // `credential.<url>.helper` sits in the list where it is written: before the generic
    // one it wins for its URL, and the generic one still serves every other URL.
    let config = cfg(&format!(
        "[credential \"https://git.example.com\"]\n\thelper = {}\n[credential]\n\thelper = {}\n",
        scoped.config(),
        b.config()
    ));
    let r = resolver(&config, &[], dir.path());
    assert_eq!(
        r.resolve(&target, Prompt::Never).unwrap().user(),
        Some("su")
    );
    assert_eq!(
        r.resolve(&url("https://elsewhere.example/x"), Prompt::Never)
            .unwrap()
            .user(),
        Some("bu")
    );
    assert_eq!(scoped.ops().len(), 1);
}

#[test]
fn authtype_challenge_and_capability_reach_the_helper() {
    let dir = tempfile::tempdir().unwrap();
    let h = Helper::new(
        dir.path(),
        "h",
        "capability[]=authtype\nauthtype=Bearer\ncredential=helper-bearer-token",
    );
    let config = cfg(&format!("[credential]\n\thelper = {}\n", h.config()));
    let r = resolver(&config, &[], dir.path());
    let target = url(REMOTE);
    let challenge = vec![
        "Bearer realm=\"asc\"".to_owned(),
        "Basic realm=\"asc\"".to_owned(),
    ];
    let got = r
        .resolve_with_challenge(&target, Prompt::Never, &challenge)
        .unwrap();
    let call = h.calls().pop().unwrap();
    assert!(has(&call, "wwwauth[]=Bearer realm=\"asc\""), "{call:?}");
    assert!(has(&call, "wwwauth[]=Basic realm=\"asc\""), "{call:?}");
    assert!(has(&call, "capability[]=authtype"), "{call:?}");
    assert_eq!(got.scheme(), Some(&Scheme::Bearer));
    assert_eq!(
        auth(&got, &target).as_deref(),
        Some("Bearer helper-bearer-token")
    );
}

// ------------------------------------------------- approve and reject

#[test]
fn approve_once_on_success_reject_on_401_only_for_helper_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let h = Helper::plain(dir.path(), "h", "alice", "pw");
    let config = cfg(&format!("[credential]\n\thelper = {}\n", h.config()));
    let r = resolver(&config, &[], dir.path());
    let target = url(REMOTE);

    let got = r.resolve(&target, Prompt::Never).unwrap();
    report_result(&got, true);
    report_result(&got, true);
    assert_eq!(h.ops(), ["get", "store"]);
    let store = &h.calls()[1];
    assert!(
        has(store, "username=alice") && has(store, "password=pw"),
        "{store:?}"
    );
    assert!(has(store, "host=git.example.com"), "{store:?}");

    let got = r.resolve(&target, Prompt::Never).unwrap();
    report_result(&got, false);
    report_result(&got, true);
    assert_eq!(h.ops(), ["get", "store", "get", "erase"]);

    // A 401 through `resolve_after` rejects, then asks again (once without a prompt).
    let first = r.resolve(&target, Prompt::Never).unwrap();
    let second = r
        .resolve_after(&target, Prompt::Never, &first, &[])
        .unwrap();
    assert!(!second.is_anonymous());
    assert_eq!(&h.ops()[4..], ["get", "erase", "get"]);
    let third = r
        .resolve_after(&target, Prompt::Never, &second, &[])
        .unwrap();
    assert!(third.is_anonymous(), "one re-ask, then failure");
    assert_eq!(&h.ops()[7..], ["erase"]);

    // Sources 1 to 4 are never reported.
    let quiet = Helper::plain(dir.path(), "q", "u", "p");
    let config = cfg(&format!("[credential]\n\thelper = {}\n", quiet.config()));
    let got = resolver(&config, &[], dir.path())
        .resolve(&url("https://tok@git.example.com/x"), Prompt::Never)
        .unwrap();
    report_result(&got, true);
    report_result(&got, false);
    assert!(quiet.calls().is_empty());
}

/// The real `git credential-store` helper: approve stores, reject erases, fill finds it.
#[test]
fn real_git_store_helper_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("creds");
    let config = cfg(&format!(
        "[credential]\n\thelper = store --file {}\n",
        sh_path(&store)
    ));
    let r = resolver(&config, &[], dir.path());
    let target = url(REMOTE);
    assert!(r.resolve(&target, Prompt::Never).unwrap().is_anonymous());

    let session = Session {
        prompt: Prompt::Never,
        config: Some(asc_git::credential::HelperConfig::from_file(&config)),
    };
    let request = CredentialRequest::new("https", "git.example.com");
    asc_git::credential::approve(
        &real(),
        &request,
        &Credential::new("alice", "s3cret"),
        &session,
    )
    .unwrap();
    assert!(
        std::fs::read_to_string(&store)
            .unwrap()
            .contains("alice:s3cret@git.example.com")
    );

    let got = r.resolve(&target, Prompt::Never).unwrap();
    assert_eq!(
        (got.source(), got.user()),
        ("credential helper", Some("alice"))
    );
    assert_eq!(got.secret().unwrap().expose(), "s3cret");

    report_result(&got, false);
    assert!(!std::fs::read_to_string(&store).unwrap().contains("alice"));
    assert!(r.resolve(&target, Prompt::Never).unwrap().is_anonymous());
}

#[test]
fn control_characters_in_a_helper_answer_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = Helper::new(dir.path(), "h", "username=u\npassword=a\rb");
    let config = cfg(&format!("[credential]\n\thelper = {}\n", h.config()));
    // Git for Windows already drops a CR inside a value; where git passes it on, the
    // resolver refuses the answer. Either way no CR reaches a header.
    match resolver(&config, &[], dir.path()).resolve(&url(REMOTE), Prompt::Never) {
        Err(err) => assert!(err.to_string().contains("invalid helper answer"), "{err}"),
        Ok(got) => assert!(!got.secret().unwrap().expose().contains('\r')),
    }

    let config = cfg("");
    let err = resolver(&config, &[("ASC_TOKEN", "a\nb")], dir.path())
        .allow_asc_token(&url(REMOTE))
        .resolve(&url(REMOTE), Prompt::Never)
        .unwrap_err();
    assert!(err.to_string().contains("ASC_TOKEN"), "{err}");
}

// ------------------------------------------------- trusted config

#[test]
fn credential_keys_from_reduced_trust_sections_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let h = Helper::plain(dir.path(), "h", "u", "p");
    let text = format!(
        "[credential]\n\thelper = {}\n\tuseHttpPath = true\n\tusername = evil\n",
        h.config()
    );
    let target = url(REMOTE);
    let got = resolver(&cfg_trust(&text, Trust::Reduced), &[], dir.path())
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert!(got.is_anonymous());
    assert!(h.calls().is_empty(), "a foreign repo's helper never runs");

    let got = resolver(&cfg_trust(&text, Trust::Full), &[], dir.path())
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert!(!got.is_anonymous());
    let call = &h.calls()[0];
    assert!(
        has(call, "username=evil") && has(call, "path=org/repo.git"),
        "{call:?}"
    );
}

// ------------------------------------------------- prompts

/// Re-runs one test of this file in a child process with a controlled environment, so a test
/// can set `GIT_ASKPASS` and friends without touching its siblings. The child test returns
/// at once unless `U77_INNER` is set.
fn rerun(inner: &str, envs: &[(&str, &str)], cwd: Option<&Path>) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([inner, "--exact", "--nocapture", "--test-threads=1"]);
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_") || upper == "SSH_ASKPASS" || upper.starts_with("GCM_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("U77_INNER", "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().unwrap();
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

fn inner_dir() -> Option<PathBuf> {
    std::env::var_os("U77_INNER")?;
    std::env::var_os("U77_DIR").map(PathBuf::from)
}

/// A program that records that it ran and answers every prompt with `answer`.
fn marker(dir: &Path, name: &str, answer: &str) -> (PathBuf, PathBuf) {
    let script = dir.join(format!("{name}.sh"));
    let ran = dir.join(format!("{name}.ran"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$1\" >> '{}'\necho '{answer}'\n",
            sh_path(&ran)
        ),
    )
    .unwrap();
    (script, ran)
}

#[test]
fn never_starts_no_askpass_and_allowed_does() {
    let dir = tempfile::tempdir().unwrap();
    let (env_ask, env_ran) = marker(dir.path(), "env", "from-env-askpass");
    let (ssh_ask, ssh_ran) = marker(dir.path(), "ssh", "from-ssh-askpass");
    let (cfg_ask, cfg_ran) = marker(dir.path(), "cfg", "from-config-askpass");
    let d = sh_path(dir.path());
    rerun(
        "askpass_inner",
        &[
            ("U77_DIR", &d),
            ("GIT_ASKPASS", &sh_path(&env_ask)),
            ("SSH_ASKPASS", &sh_path(&ssh_ask)),
            ("GIT_TERMINAL_PROMPT", "0"),
        ],
        None,
    );
    assert!(env_ran.exists(), "Allowed runs GIT_ASKPASS");
    assert!(
        !ssh_ran.exists() && !cfg_ran.exists(),
        "git's order: GIT_ASKPASS first"
    );
    let _ = (cfg_ask,);
}

#[test]
fn askpass_inner() {
    let Some(dir) = inner_dir() else { return };
    let (cfg_ask, cfg_ran) = (dir.join("cfg.sh"), dir.join("cfg.ran"));
    let (env_ran, ssh_ran) = (dir.join("env.ran"), dir.join("ssh.ran"));
    let config = cfg(&format!("[core]\n\taskPass = {}\n", sh_path(&cfg_ask)));
    let r = resolver(&config, &[], &dir);
    let target = url(REMOTE);

    let got = r.resolve(&target, Prompt::Never).unwrap();
    assert!(got.is_anonymous(), "{got:?}");
    assert!(
        !env_ran.exists() && !ssh_ran.exists() && !cfg_ran.exists(),
        "Never ran an askpass"
    );

    let got = r.resolve(&target, Prompt::Allowed).unwrap();
    assert_eq!(
        got.secret().map(|s| s.expose().to_owned()).as_deref(),
        Some("from-env-askpass")
    );
    assert!(env_ran.exists());
}

#[test]
fn terminal_prompts_disabled_is_honoured_under_allowed() {
    let dir = tempfile::tempdir().unwrap();
    rerun(
        "no_terminal_inner",
        &[
            ("U77_DIR", &sh_path(dir.path())),
            ("GIT_TERMINAL_PROMPT", "0"),
        ],
        None,
    );
}

#[test]
fn no_terminal_inner() {
    let Some(dir) = inner_dir() else { return };
    let config = cfg("");
    let got = resolver(&config, &[], &dir)
        .resolve(&url(REMOTE), Prompt::Allowed)
        .unwrap();
    assert!(got.is_anonymous());
}

#[test]
fn trusted_core_askpass_is_used_under_allowed_and_a_reduced_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let (script, ran) = marker(dir.path(), "cfg", "from-config-askpass");
    rerun(
        "config_askpass_inner",
        &[
            ("U77_DIR", &sh_path(dir.path())),
            ("GIT_TERMINAL_PROMPT", "0"),
        ],
        None,
    );
    assert!(ran.exists());
    let _ = script;
}

#[test]
fn config_askpass_inner() {
    let Some(dir) = inner_dir() else { return };
    let path = sh_path(&dir.join("cfg.sh"));
    let text = format!("[core]\n\taskPass = {path}\n");
    let target = url(REMOTE);
    let reduced = cfg_trust(&text, Trust::Reduced);
    let got = resolver(&reduced, &[], &dir)
        .resolve(&target, Prompt::Allowed)
        .unwrap();
    assert!(got.is_anonymous());
    assert!(
        !dir.join("cfg.ran").exists(),
        "a foreign repo's core.askPass ran"
    );
    let full = cfg_trust(&text, Trust::Full);
    let got = resolver(&full, &[], &dir)
        .resolve(&target, Prompt::Allowed)
        .unwrap();
    assert_eq!(
        got.secret().map(|s| s.expose().to_owned()).as_deref(),
        Some("from-config-askpass")
    );
}

#[test]
fn a_repository_config_is_never_consulted_by_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let init = Command::new(real().path())
        .args(["init", "-q"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(init.status.success());
    let h = Helper::plain(dir.path(), "repo-helper", "repo-user", "repo-pass");
    let mut config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
    config.push_str(&format!("[credential]\n\thelper = {}\n", h.config()));
    std::fs::write(repo.join(".git/config"), config).unwrap();
    rerun(
        "repo_config_inner",
        &[("U77_DIR", &sh_path(dir.path()))],
        Some(&repo),
    );
    // Only the control call in the child reached the helper.
    assert_eq!(h.ops(), ["get"]);
}

#[test]
fn repo_config_inner() {
    let Some(dir) = inner_dir() else { return };
    let config = cfg("");
    let target = url(REMOTE);
    let got = resolver(&config, &[], &dir)
        .resolve(&target, Prompt::Never)
        .unwrap();
    assert!(
        got.is_anonymous(),
        "the cwd repository's helper was consulted: {got:?}"
    );
    assert!(!dir.join("repo-helper.log").exists());
    // Control: git itself, without asc's isolation, does read the repository.
    let request = CredentialRequest::new("https", "git.example.com");
    let found = asc_git::credential::fill(&real(), &request, &Session::never()).unwrap();
    assert!(found.is_some(), "the control must see the repo helper");
}

// ------------------------------------------------- host binding

#[test]
fn plain_http_gets_no_credential_except_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let h = Helper::plain(dir.path(), "h", "u", "p");
    let config = cfg(&format!("[credential]\n\thelper = {}\n", h.config()));
    let remote = url("http://git.example.com/x");
    let r = resolver(&config, &[], dir.path());
    assert!(r.resolve(&remote, Prompt::Never).unwrap().is_anonymous());
    assert!(
        h.calls().is_empty(),
        "helpers are not asked about a plain http origin"
    );
    let err = resolver(&config, &[("ASC_TOKEN", "t")], dir.path())
        .allow_asc_token(&remote)
        .resolve(&remote, Prompt::Never)
        .unwrap_err();
    assert!(err.to_string().contains("plain http"), "{err}");
    let err = resolver(&cfg(""), &[], dir.path())
        .resolve(&url("http://u:p@git.example.com/x"), Prompt::Never)
        .unwrap_err();
    assert!(err.to_string().contains("plain http"), "{err}");

    let local = url("http://127.0.0.1:8080/x");
    let got = resolver(&cfg(""), &[], dir.path())
        .resolve(&url("http://u:p@127.0.0.1:8080/x"), Prompt::Never)
        .unwrap();
    assert_eq!(got.source(), "url");
    assert!(auth(&got, &local).is_some());
    assert!(auth(&got, &url("http://[::1]:8080/x")).is_none());
}

#[test]
fn origins_and_redirect_rules() {
    assert!(same_origin(&url("https://h/a"), &url("https://H:443/b")));
    assert!(!same_origin(&url("https://h/"), &url("https://h:8443/")));
    assert!(!same_origin(&url("https://h/"), &url("http://h/")));
    assert!(!same_origin(&url("https://h/"), &url("https://g/")));
    let from = url("https://h/a");
    assert_eq!(
        redirect_action(&from, &url("https://h/b"), 0),
        RedirectAction::Follow
    );
    assert_eq!(
        redirect_action(&from, &url("https://s3.example/b"), 0),
        RedirectAction::ReAsk
    );
    assert_eq!(
        redirect_action(&from, &url("https://h:444/b"), 0),
        RedirectAction::ReAsk
    );
    assert!(matches!(
        redirect_action(&from, &url("http://h/b"), 0),
        RedirectAction::Refuse(_)
    ));
    assert_eq!(
        redirect_action(&from, &url("https://h/b"), 4),
        RedirectAction::Follow
    );
    assert!(matches!(
        redirect_action(&from, &url("https://h/b"), 5),
        RedirectAction::Refuse(_)
    ));
    assert_eq!(Origin::of(&from).unwrap().to_string(), "https://h:443");
}

mod wire {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    type Seen = Arc<Mutex<Vec<String>>>;

    /// Serves forever: every request is recorded, and answered by `reply(request line)`.
    async fn serve(reply: impl Fn(&str) -> String + Send + Sync + 'static) -> (u16, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Seen = Arc::default();
        let log = seen.clone();
        let reply = Arc::new(reply);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (log, reply) = (log.clone(), reply.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut got = Vec::new();
                    loop {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        got.extend_from_slice(&buf[..n]);
                        if got.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&got).into_owned();
                    let first = text.lines().next().unwrap_or("").to_owned();
                    log.lock().unwrap().push(text);
                    let _ = sock.write_all(reply(&first).as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (port, seen)
    }

    fn redirect(to: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(redirect_policy())
            .build()
            .unwrap()
    }

    fn lowered(seen: &Seen) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|r| r.to_ascii_lowercase())
            .collect()
    }

    #[tokio::test]
    async fn a_credential_never_follows_a_redirect_to_another_origin() {
        let (port_b, seen_b) = serve(|_| OK.to_owned()).await;
        let location = format!("http://127.0.0.1:{port_b}/landing");
        let (port_a, seen_a) = serve(move |_| redirect(&location)).await;
        let dir = tempfile::tempdir().unwrap();
        let config = cfg(&format!(
            "[http \"http://127.0.0.1:{port_a}/\"]\n\textraHeader = Authorization: Bearer {CANARY}\n\
             \textraHeader = X-Extra: {CANARY}-extra\n"
        ));
        let origin_a = url(&format!("http://127.0.0.1:{port_a}/start"));
        let resolved = resolver(&config, &[], dir.path())
            .resolve(&origin_a, Prompt::Never)
            .unwrap();
        assert_eq!(resolved.source(), "http.extraHeader");

        let http = client();
        let response = resolved
            .apply(http.get(origin_a.clone()), &origin_a)
            .send()
            .await
            .unwrap();
        // The policy hands the cross-origin 302 back instead of following it.
        assert_eq!(response.status().as_u16(), 302);
        let landing = url(response.headers()["location"].to_str().unwrap());
        assert!(!same_origin(&origin_a, &landing));
        assert!(
            seen_b.lock().unwrap().is_empty(),
            "nothing reached the second origin"
        );
        assert!(lowered(&seen_a)[0].contains(&CANARY.to_ascii_lowercase()));

        // The caller follows by hand: the resolver binding gives the new origin nothing.
        assert!(resolved.headers_for(&landing).is_empty());
        let response = resolved
            .apply(http.get(landing.clone()), &landing)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let b = lowered(&seen_b);
        assert_eq!(b.len(), 1);
        assert!(
            !b[0].contains("authorization") && !b[0].contains("x-extra"),
            "{}",
            b[0]
        );
        assert!(!b[0].contains(&CANARY.to_ascii_lowercase()));
    }

    #[tokio::test]
    async fn a_same_origin_redirect_keeps_the_credential_and_is_bounded() {
        let (port, seen) = serve(|line| {
            if line.contains("/hop/") {
                let n: usize = line
                    .split("/hop/")
                    .nth(1)
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                redirect(&format!("/hop/{}", n + 1))
            } else if line.contains("/once") {
                redirect("/final")
            } else if line.contains("/final") {
                OK.to_owned()
            } else {
                redirect("/hop/1")
            }
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let config = cfg("");
        let start = url(&format!("http://tok@127.0.0.1:{port}/once"));
        let resolved = resolver(&config, &[], dir.path())
            .resolve(&start, Prompt::Never)
            .unwrap();
        let http = client();
        let plain = url(&format!("http://127.0.0.1:{port}/once"));
        let response = resolved
            .apply(http.get(plain.clone()), &plain)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let seen_now = lowered(&seen);
        assert_eq!(seen_now.len(), 2);
        assert!(
            seen_now.iter().all(|r| r.contains("authorization: basic")),
            "{seen_now:?}"
        );

        // An endless same-origin chain stops with an error after five redirects.
        let endless = url(&format!("http://127.0.0.1:{port}/start"));
        let err = resolved
            .apply(http.get(endless.clone()), &endless)
            .send()
            .await
            .unwrap_err();
        assert!(err.is_redirect(), "{err}");
    }
}

// ------------------------------------------------- secrets stay secret

#[test]
fn a_canary_secret_never_appears_in_debug_display_or_errors() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        Netrc::candidates(&home)[0].clone(),
        format!("machine git.example.com login {CANARY}-login password {CANARY}-netrc\n"),
    )
    .unwrap();
    let h = Helper::plain(
        dir.path(),
        "h",
        &format!("{CANARY}-hu"),
        &format!("{CANARY}-hp"),
    );
    let helper_cfg = cfg(&format!("[credential]\n\thelper = {}\n", h.config()));
    let header_cfg = cfg(&format!(
        "[http]\n\textraHeader = Authorization: Bearer {CANARY}-header\n"
    ));
    let target = url(REMOTE);
    let with_user = url(&format!(
        "https://{CANARY}-user:{CANARY}-pass@git.example.com/org/repo.git"
    ));
    let empty_home = dir.path().join("empty");
    std::fs::create_dir_all(&empty_home).unwrap();

    let results = [
        resolver(&header_cfg, &[], &empty_home)
            .resolve(&target, Prompt::Never)
            .unwrap(),
        resolver(&cfg(""), &[], &empty_home)
            .resolve(&with_user, Prompt::Never)
            .unwrap(),
        resolver(&cfg(""), &[("ASC_TOKEN", CANARY)], &empty_home)
            .allow_asc_token(&target)
            .resolve(&target, Prompt::Never)
            .unwrap(),
        resolver(&cfg(""), &[], &home)
            .resolve(&target, Prompt::Never)
            .unwrap(),
        resolver(&helper_cfg, &[], &empty_home)
            .resolve(&target, Prompt::Never)
            .unwrap(),
    ];
    for r in &results {
        assert!(!r.is_anonymous(), "{:?}", r.source());
        let shown = format!("{r:?} {:?} {:?} {}", r.presented(), r.secret(), r.source());
        assert!(!shown.contains(CANARY), "{shown}");
        assert!(!shown.contains(&base64_of(CANARY)), "{shown}");
    }
    let err = resolver(&cfg(""), &[], &empty_home)
        .resolve(
            &url(&format!("http://{CANARY}@git.example.com/")),
            Prompt::Never,
        )
        .unwrap_err();
    assert!(!format!("{err} {err:?}").contains(CANARY));
    assert!(!mask_url(&with_user).contains(CANARY));
    assert!(!credential::failure_message(&with_user, true, false).contains(CANARY));
    let cred = Credential::new(format!("{CANARY}-u"), format!("{CANARY}-p"));
    assert!(!format!("{cred:?}").contains(CANARY));
}

fn base64_of(text: &str) -> String {
    // The canary as the first 12 bytes of a Basic payload are enough to catch a leak of an
    // encoded value without re-implementing base64 here: the resolver's own encoder.
    let p = credential::Presented {
        scheme: Scheme::Basic,
        user: text.to_owned(),
        secret: credential::Secret::new(""),
    };
    p.authorization()
        .expose()
        .trim_start_matches("Basic ")
        .chars()
        .take(24)
        .collect()
}

#[test]
fn failure_messages_follow_git() {
    let target = url("https://u:p@git.example.com/org/repo.git");
    assert_eq!(
        credential::failure_message(&target, false, true),
        "fatal: could not read Username for 'https://***@git.example.com/org/repo.git': terminal prompts disabled"
    );
    assert_eq!(
        credential::failure_message(&target, true, true),
        "fatal: Authentication failed for 'https://***@git.example.com/org/repo.git'"
    );
}

#[test]
fn source_ordering_matches_the_spec_list() {
    assert!(Source::ExtraHeader < Source::UrlUserinfo);
    assert!(Source::UrlUserinfo < Source::AscToken);
    assert!(Source::AscToken < Source::Netrc);
    assert!(Source::Netrc < Source::Helper);
}
