//! U79: asc authenticates as git does, proven by running both (SPEC §15.6, PLAN U79).
//!
//! A test-only fake git host (`fake_host.rs`) issues git's exact flow and records every
//! request. For each configuration the same host setup is run twice, once with real git 2.40+
//! (`git ls-remote`, through `RealGit`) and once with asc, and the `Authorization` header on the
//! wire and the helper calls (`get`, `store`, `erase`, with their attributes) are compared.
//!
//! asc's side is the resolver (`asc::credential`, U77) plus its HTTP layer: `asc fetch`, `clone`
//! and `push` are still stubs, so `Side::asc` is the stand-in request loop (an anonymous first
//! request, the helpers after the 401 with its challenge, `approve` after a 200, `reject`
//! after a 401, cross-origin redirects handed back to the resolver). The asc edge section runs
//! real git, the resolver and `RpcClient` against an in-process `asc serve` router
//! (`asc_serve::build_routes`) with a static token table.
//!
//! `asc` is a binary crate, so the resolver is compiled into this test crate by path.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

#[path = "../../src/credential.rs"]
mod credential;
mod fake_host;
mod helpers;

use fake_host::{FakeHost, REPO_PATH};
use gix_sec::Trust;
use helpers::{
    CANARY, Recorder, Spec, askpass, assert_same_calls, assert_same_failure_words, is_inner, mask,
    pair, redact_calls, rerun, sh_path,
};

use credential::Prompt;

const USER: &str = "alice";
const PASS: &str = CANARY;
const MULTI: &str = "multi_thread";

fn accept_alice(host: &FakeHost) {
    host.accept_basic(USER, PASS);
}

fn helper_section(recorder: &Recorder) -> String {
    format!("[credential]\n\thelper = {}\n", recorder.config())
}

// ====================================================================== helpers: get, store, erase

/// The success round trip: the first request is anonymous, the 401's challenge reaches the
/// helper as `wwwauth[]`, the credential goes out as Basic, and `store` follows the 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn helper_get_then_store_after_a_401_challenge() {
    let p = pair(accept_alice, |s| {
        let r = Recorder::plain(s.dir(), "h", USER, PASS);
        (Spec::new().credential(helper_section(&r)), r)
    })
    .await;
    let g = p.git.git(&p.git.url());
    assert!(g.ok, "git failed: {}", g.stderr);
    let c = p.asc.asc(&p.asc.url(), Prompt::Never).await;
    assert!(c.ok, "asc failed: {} (status {})", c.message, c.status);

    p.assert_same_accepted_authorization("helper success");
    let (gc, cc) = (p.git_r.calls(), p.asc_r.calls());
    assert_eq!(p.git_r.ops(), ["get", "store"], "{}", redact_calls(&gc));
    assert_same_calls(&gc, &cc, p.ports(), "helper success");

    // Challenge-driven: neither client sent a credential before the 401.
    for (who, side) in [("git", &p.git), ("asc", &p.asc)] {
        let seen = side.host.requests();
        assert_eq!(seen[0].status, 401, "{who}: first request");
        assert!(
            seen[0].authorization().is_none(),
            "{who}: first request was anonymous"
        );
    }
    // git also offers `capability[]=state` (multi-round helpers); asc does not: the one
    // attribute on which the two differ.
    assert!(gc[0].has("capability[]=state") && !cc[0].has("capability[]=state"));
    // The Basic challenge reached the helper, with the capability that lets it answer in kind.
    for call in [&gc[0], &cc[0]] {
        assert!(
            call.has("wwwauth[]=Basic realm=\"fake git host\""),
            "{}",
            redact_calls(&gc)
        );
        assert!(call.has("capability[]=authtype"), "{}", redact_calls(&gc));
    }
}

/// Every `WWW-Authenticate` line reaches the helper, in order, as git passes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_challenge_line_reaches_the_helper() {
    let p = pair(
        |h| {
            accept_alice(h);
            h.challenge(&["Basic realm=\"one\"", "Bearer realm=\"two\", scope=\"x\""]);
        },
        |s| {
            let r = Recorder::plain(s.dir(), "h", USER, PASS);
            (Spec::new().credential(helper_section(&r)), r)
        },
    )
    .await;
    assert!(p.git.git(&p.git.url()).ok);
    assert!(p.asc.asc(&p.asc.url(), Prompt::Never).await.ok);
    let (gc, cc) = (p.git_r.calls(), p.asc_r.calls());
    assert_same_calls(&gc, &cc, p.ports(), "challenge lines");
    let www: Vec<_> = cc[0]
        .attrs
        .iter()
        .filter(|a| a.starts_with("wwwauth[]="))
        .collect();
    assert_eq!(www.len(), 2, "{}", redact_calls(&cc));
}

/// A Git Credential Manager style helper answers `authtype=Bearer`; the credential is sent as
/// given and the same bytes reach the host from git and from asc.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gcm_style_helper_answers_with_authtype_and_credential() {
    let p = pair(
        |h| {
            h.accept(&format!("Bearer {PASS}"));
            h.challenge(&["Basic realm=\"x\"", "Bearer realm=\"x\""]);
        },
        |s| {
            let r = Recorder::authtype(s.dir(), "gcm", "Bearer", PASS);
            (Spec::new().credential(helper_section(&r)), r)
        },
    )
    .await;
    let g = p.git.git(&p.git.url());
    assert!(g.ok, "git failed: {}", g.stderr);
    let c = p.asc.asc(&p.asc.url(), Prompt::Never).await;
    assert!(c.ok, "asc failed: {}", c.message);
    p.assert_same_accepted_authorization("authtype");
    let (gc, cc) = (p.git_r.calls(), p.asc_r.calls());
    assert_same_calls(&gc, &cc, p.ports(), "authtype");
    assert_eq!(p.asc_r.ops(), ["get", "store"]);
    assert!(
        cc[0].has("capability[]=authtype")
            && cc[0]
                .attrs
                .iter()
                .any(|a| a.starts_with("wwwauth[]=Bearer"))
    );
}

/// A credential the host refuses is erased. git then stops: `Authentication failed`. asc asks
/// once more without a prompt (SPEC §15.6.1: non-interactive, one re-ask), erases that too,
/// and ends with git's own message. That one extra `get` is the documented difference.
// fidelity: D-4
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_credential_is_erased_and_the_failure_reads_like_gits() {
    let p = pair(
        |h| {
            h.accept_basic(USER, "some-other-password");
        },
        |s| {
            let r = Recorder::plain(s.dir(), "h", USER, PASS);
            (Spec::new().credential(helper_section(&r)), r)
        },
    )
    .await;
    let g = p.git.git(&p.git.url());
    assert!(!g.ok);
    assert_eq!(g.code, Some(128));
    let c = p.asc.asc(&p.asc.url(), Prompt::Never).await;
    assert!(!c.ok);

    let (gc, cc) = (p.git_r.calls(), p.asc_r.calls());
    assert_eq!(p.git_r.ops(), ["get", "erase"], "{}", redact_calls(&gc));
    assert_same_calls(&gc[..2], &cc[..2], p.ports(), "first get and erase");
    assert_eq!(
        p.asc_r.ops(),
        ["get", "erase", "get", "erase"],
        "{}",
        redact_calls(&cc)
    );
    // Same words, same URL.
    assert_same_failure_words(&g.fatal(), &c.message, p.ports());
    // The erased credential was never retried without the helper: no request carried the
    // refused value twice in a row beyond the one re-ask.
    assert!(p.asc.host.requests().iter().all(|r| r.status != 200));
}

/// The real `store` helper: a stored credential authenticates, and when the host revokes it
/// the entry is erased from the file by git and by asc alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_store_helper_round_trips_and_erases_on_401() {
    let p = pair(accept_alice, |s| {
        let file = s.dir().join("credentials");
        std::fs::write(
            &file,
            format!("http://{USER}:{PASS}@127.0.0.1:{}\n", s.port()),
        )
        .unwrap();
        let cred = format!("[credential]\n\thelper = store --file {}\n", sh_path(&file));
        (Spec::new().credential(cred), file)
    })
    .await;
    assert!(p.git.git(&p.git.url()).ok);
    assert!(p.asc.asc(&p.asc.url(), Prompt::Never).await.ok);
    p.assert_same_accepted_authorization("store helper");
    for file in [&p.git_r, &p.asc_r] {
        assert!(
            std::fs::read_to_string(file).unwrap().contains(USER),
            "entry kept after success"
        );
    }

    p.git.host.revoke_all();
    p.asc.host.revoke_all();
    assert!(!p.git.git(&p.git.url()).ok);
    assert!(!p.asc.asc(&p.asc.url(), Prompt::Never).await.ok);
    for (who, file) in [("git", &p.git_r), ("asc", &p.asc_r)] {
        let left = std::fs::read_to_string(file).unwrap();
        assert!(!left.contains(USER), "{who}: the refused entry was erased");
    }
}

/// The `cache` helper keeps a credential in a daemon socket (Unix only: git ships no cache
/// helper on Windows). A credential approved through asc's helper path is found by git and by asc.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cache_helper_serves_an_approved_credential() {
    let mut side = helpers::Side::new(accept_alice).await;
    // git's cache daemon refuses a socket directory that anyone else can enter, and a
    // temp dir is made with the umask (0755), so give the socket a private directory.
    let socket_dir = side.dir().join("cache");
    std::fs::create_dir(&socket_dir).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket = socket_dir.join("cache.sock");
    let cred = format!(
        "[credential]
	helper = cache --timeout 60 --socket {}
",
        sh_path(&socket)
    );
    let session = asc_git::credential::Session {
        prompt: Prompt::Never,
        config: Some(asc_git::credential::HelperConfig::from_file(&helpers::cfg(
            &cred,
        ))),
    };
    side.configure(Spec::new().credential(cred));
    let request =
        asc_git::credential::CredentialRequest::new("http", format!("127.0.0.1:{}", side.port()));
    let stored = asc_git::credential::Credential::new(USER, PASS);
    asc_git::credential::approve(&helpers::real(), &request, &stored, &session).unwrap();
    let g = side.git(&side.url());
    assert!(g.ok, "git read the cached credential: {}", g.stderr);
    assert!(
        side.asc(&side.url(), Prompt::Never).await.ok,
        "asc read it too"
    );
}

/// `credential.<url>.helper` is asked for its URL only, an empty value resets the list, and a
/// URL-scoped helper for another host is never run: the same three helpers, the same calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn helper_lists_resets_and_url_scoped_helpers_match_git() {
    let p = pair(accept_alice, |s| {
        let dir = s.dir();
        let global = Recorder::plain(dir, "global", "mallory", "wrong");
        let reset = Recorder::plain(dir, "afterreset", USER, PASS);
        let scoped = Recorder::new(dir, "scoped", ""); // answers nothing: git moves on
        let other = Recorder::plain(dir, "other", "bob", "nope");
        let cred = format!(
            "[credential]\n\thelper = {}\n\thelper =\n\thelper = {}\n[credential \"http://127.0.0.1:{}\"]\n\thelper = {}\n[credential \"http://other.example\"]\n\thelper = {}\n",
            global.config(),
            reset.config(),
            s.port(),
            scoped.config(),
            other.config()
        );
        (Spec::new().credential(cred), [global, reset, scoped, other])
    })
    .await;
    let g = p.git.git(&p.git.url());
    assert!(g.ok, "git failed: {}", g.stderr);
    let c = p.asc.asc(&p.asc.url(), Prompt::Never).await;
    assert!(c.ok, "asc failed: {}", c.message);
    p.assert_same_accepted_authorization("helper lists");
    for ((gr, cr), name) in
        p.git_r
            .iter()
            .zip(&p.asc_r)
            .zip(["global", "afterreset", "scoped", "other"])
    {
        assert_same_calls(&gr.calls(), &cr.calls(), p.ports(), name);
    }
    assert!(
        p.asc_r[0].calls().is_empty(),
        "the reset dropped the earlier helper"
    );
    assert!(
        p.asc_r[3].calls().is_empty(),
        "another host's helper never runs"
    );
    assert_eq!(p.asc_r[1].ops(), ["get", "store"]);
}

// ====================================================================== helper request attributes

/// `credential.useHttpPath` puts `path` in the request; without it git and asc send none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn use_http_path_controls_the_path_attribute() {
    for (config, expect_path) in [
        ("[credential]\n\tuseHttpPath = true\n", true),
        ("", false),
        (
            "[credential \"http://elsewhere.example\"]\n\tuseHttpPath = true\n",
            false,
        ),
    ] {
        let p = pair(accept_alice, |s| {
            let r = Recorder::plain(s.dir(), "h", USER, PASS);
            (
                Spec::new().credential(format!("{config}{}", helper_section(&r))),
                r,
            )
        })
        .await;
        assert!(p.git.git(&p.git.url()).ok);
        assert!(p.asc.asc(&p.asc.url(), Prompt::Never).await.ok);
        let (gc, cc) = (p.git_r.calls(), p.asc_r.calls());
        assert_same_calls(&gc, &cc, p.ports(), "useHttpPath");
        for call in &cc {
            let has_path = call.has(&format!("path={}", REPO_PATH.trim_start_matches('/')));
            assert_eq!(has_path, expect_path, "{config:?}: {}", redact_calls(&cc));
        }
    }
}

/// `credential.username` is sent as the `username` attribute of `get`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credential_username_reaches_the_helper() {
    let p = pair(accept_alice, |s| {
        let r = Recorder::new(s.dir(), "h", &format!("password={PASS}"));
        let cred = format!(
            "[credential]\n\tusername = {USER}\n\thelper = {}\n",
            r.config()
        );
        (Spec::new().credential(cred), r)
    })
    .await;
    let g = p.git.git(&p.git.url());
    assert!(g.ok, "git failed: {}", g.stderr);
    assert!(p.asc.asc(&p.asc.url(), Prompt::Never).await.ok);
    let (gc, cc) = (p.git_r.calls(), p.asc_r.calls());
    assert_same_calls(&gc, &cc, p.ports(), "credential.username");
    assert!(
        cc[0].has(&format!("username={USER}")),
        "{}",
        redact_calls(&cc)
    );
    p.assert_same_accepted_authorization("credential.username");
}

// ====================================================================== netrc

/// `~/.netrc`: libcurl reads it for git, asc reads it itself; the same Basic goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn netrc_authenticates_both() {
    let p = pair(accept_alice, |s| {
        let netrc = format!("machine 127.0.0.1\n  login {USER}\n  password {PASS}\n");
        let r = Recorder::new(s.dir(), "h", "");
        (Spec::new().credential(helper_section(&r)).netrc(netrc), r)
    })
    .await;
    let g = p.git.git(&p.git.url());
    assert!(g.ok, "git failed: {}", g.stderr);
    let c = p.asc.asc(&p.asc.url(), Prompt::Never).await;
    assert!(c.ok, "asc failed: {}", c.message);
    p.assert_same_accepted_authorization("netrc");
    // A netrc credential is never reported to a helper.
    assert!(
        p.asc_r
            .calls()
            .iter()
            .all(|c| c.op != "store" && c.op != "erase")
    );
}

// ====================================================================== http.extraHeader

fn basic_value(user: &str, pass: &str) -> String {
    fake_host::base64(format!("{user}:{pass}").as_bytes())
}

/// `http.extraHeader`, global and URL-scoped, in the forms `actions/checkout` and Azure DevOps
/// write: the value goes out verbatim on the very first request and no helper is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extra_header_forms_go_out_verbatim_on_the_first_request() {
    let checkout = format!("basic {}", basic_value("x-access-token", PASS));
    let azure = format!("bearer {PASS}");
    let cases: Vec<(&str, String, String)> = vec![
        (
            "global",
            checkout.clone(),
            "[http]\n\textraheader = AUTHORIZATION: {v}\n".into(),
        ),
        (
            "actions/checkout",
            checkout,
            "[http \"http://127.0.0.1:{port}/\"]\n\textraheader = AUTHORIZATION: {v}\n".into(),
        ),
        (
            "azure bearer",
            azure,
            "[http \"http://127.0.0.1:{port}/\"]\n\textraheader = AUTHORIZATION: {v}\n".into(),
        ),
    ];
    for (name, value, template) in cases {
        let p = pair(
            |h| {
                h.accept(&value);
            },
            |s| {
                let r = Recorder::plain(s.dir(), "h", "helper-user", "helper-pass");
                let other = template
                    .replace("{port}", &s.port().to_string())
                    .replace("{v}", &value);
                (Spec::new().other(other).credential(helper_section(&r)), r)
            },
        )
        .await;
        let g = p.git.git(&p.git.url());
        assert!(g.ok, "{name}: git failed: {}", g.stderr);
        let c = p.asc.asc(&p.asc.url(), Prompt::Never).await;
        assert!(c.ok, "{name}: asc failed: {}", c.message);
        p.assert_same_accepted_authorization(name);
        for (who, side) in [("git", &p.git), ("asc", &p.asc)] {
            let first = &side.host.requests()[0];
            assert_eq!(
                first.status, 200,
                "{name}/{who}: the first request authenticated"
            );
            assert_eq!(
                mask(first.authorization()),
                mask(Some(&value)),
                "{name}/{who}: verbatim"
            );
        }
        assert!(
            p.git_r.calls().is_empty() && p.asc_r.calls().is_empty(),
            "{name}: no helper call"
        );
    }
}

/// An `http.<url>.extraHeader` for another URL is not sent to this host by either client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extra_header_for_another_url_is_not_sent() {
    let p = pair(accept_alice, |_| {
        (
            Spec::new().other(format!(
                "[http \"http://elsewhere.example/\"]\n\textraheader = AUTHORIZATION: bearer {PASS}\n"
            )),
            (),
        )
    })
    .await;
    assert!(!p.git.git(&p.git.url()).ok);
    assert!(!p.asc.asc(&p.asc.url(), Prompt::Never).await.ok);
    for (who, side) in [("git", &p.git), ("asc", &p.asc)] {
        for r in side.host.requests() {
            assert!(r.authorization().is_none(), "{who}: nothing was sent");
        }
    }
}

// ====================================================================== URL userinfo

/// `https://user:token@host/...`: Basic on the wire from both. git also reports it to the
/// helpers (`store`); asc never passes URL userinfo to `approve` (SPEC §15.6.1 step 2).
// fidelity: Q-6
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn url_userinfo_is_basic_and_asc_never_approves_it() {
    let p = pair(accept_alice, |s| {
        let r = Recorder::new(s.dir(), "h", "");
        (Spec::new().credential(helper_section(&r)), r)
    })
    .await;
    let g = p
        .git
        .git(&p.git.url_with_userinfo(&format!("{USER}:{PASS}")));
    assert!(g.ok, "git failed: {}", g.stderr);
    let c =
        p.asc.asc(
            &p.asc.url_with_userinfo(&format!("{USER}:{PASS}")),
            Prompt::Never,
        )
        .await;
    assert!(c.ok, "asc failed: {}", c.message);
    p.assert_same_accepted_authorization("userinfo");
    assert!(
        p.asc_r.calls().is_empty(),
        "documented difference: asc never approves URL userinfo"
    );
    let git_ops = p.git_r.ops();
    assert!(
        git_ops.iter().all(|o| o == "store"),
        "git only ever reports it: {git_ops:?}"
    );
}

/// `https://TOKEN@host`: both send Basic `TOKEN:`, a user name and an empty password, which
/// the server reads as the token (SPEC §15.6.2). git does not prompt for the missing password.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_as_user_name_is_basic_with_an_empty_password() {
    let p = pair(
        |h| {
            h.accept(&format!("Basic {}", basic_value(CANARY, "")));
        },
        |_| (Spec::new(), ()),
    )
    .await;
    let g = p.git.git(&p.git.url_with_userinfo(CANARY));
    assert!(g.ok, "git failed: {}", g.stderr.replace(CANARY, "***"));
    let c =
        p.asc.asc(&p.asc.url_with_userinfo(CANARY), Prompt::Never)
            .await;
    assert!(c.ok, "asc failed: {}", c.message);
    p.assert_same_accepted_authorization("TOKEN@host");
}

// ====================================================================== prompts (env)

/// `GIT_ASKPASS`: with prompts allowed git and asc both run it (Username, then Password), both
/// authenticate, and the helper's `store` receives the prompted credential. asc with
/// `Prompt::Never` runs no askpass at all. Runs in a child process that has `GIT_ASKPASS` set.
#[test]
fn git_askpass_is_run_by_git_and_by_asc_only_when_prompts_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let (script, _) = askpass(dir.path(), "ask", USER, PASS);
    rerun(
        "askpass_inner",
        &[
            ("AUTH_COMPAT_DIR", &sh_path(dir.path())),
            ("GIT_ASKPASS", &sh_path(&script)),
        ],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn askpass_inner() {
    if !is_inner() {
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var("AUTH_COMPAT_DIR").unwrap());
    let ran = dir.join("ask.ran");
    let prompts = || {
        std::fs::read_to_string(&ran)
            .unwrap_or_default()
            .lines()
            .count()
    };

    let p = pair(accept_alice, |s| {
        let r = Recorder::new(s.dir(), "h", "");
        (Spec::new().credential(helper_section(&r)), r)
    })
    .await;
    let g = p.git.git(&p.git.url());
    assert!(g.ok, "git failed: {}", g.stderr);
    assert_eq!(prompts(), 2, "git asked for a user name and a password");
    let c = p.asc.asc(&p.asc.url(), Prompt::Allowed).await;
    assert!(c.ok, "asc failed: {}", c.message);
    assert_eq!(prompts(), 4, "asc ran the same askpass");
    p.assert_same_accepted_authorization("GIT_ASKPASS");
    // The prompted credential is offered to the helper once, as `store`, by both.
    assert_eq!(p.git_r.ops(), ["get", "store"]);
    assert_same_calls(&p.git_r.calls(), &p.asc_r.calls(), p.ports(), "askpass");

    // Libraries never prompt.
    let n = pair(accept_alice, |_| (Spec::new(), ())).await;
    let c = n.asc.asc(&n.asc.url(), Prompt::Never).await;
    assert!(!c.ok);
    assert_eq!(prompts(), 4, "Prompt::Never ran no askpass");
    assert!(
        c.message.contains("terminal prompts disabled"),
        "{}",
        c.message
    );
}

/// `core.askPass` from trusted config drives both clients too (no environment involved).
#[test]
fn core_askpass_is_run_by_git_and_by_asc_when_prompts_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    rerun(
        "config_askpass_inner",
        &[("AUTH_COMPAT_DIR", &sh_path(dir.path()))],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_askpass_inner() {
    if !is_inner() {
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var("AUTH_COMPAT_DIR").unwrap());
    let (script, _) = askpass(&dir, "cfgask", USER, PASS);
    let ran = dir.join("cfgask.ran");
    let p = pair(accept_alice, |_| {
        (
            Spec::new().other(format!("[core]\n\taskPass = {}\n", sh_path(&script))),
            (),
        )
    })
    .await;
    let g = p.git.git(&p.git.url());
    assert!(g.ok, "git failed: {}", g.stderr);
    assert_eq!(std::fs::read_to_string(&ran).unwrap().lines().count(), 2);
    let c = p.asc.asc(&p.asc.url(), Prompt::Allowed).await;
    assert!(c.ok, "asc failed: {}", c.message);
    assert_eq!(std::fs::read_to_string(&ran).unwrap().lines().count(), 4);
    p.assert_same_accepted_authorization("core.askPass");
    let n = pair(accept_alice, |_| {
        (
            Spec::new().other(format!("[core]\n\taskPass = {}\n", sh_path(&script))),
            (),
        )
    })
    .await;
    assert!(!n.asc.asc(&n.asc.url(), Prompt::Never).await.ok);
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap().lines().count(),
        4,
        "Never ran no askpass"
    );
}

/// A prompt test can never block on a terminal. git asks `GIT_ASKPASS`, then the terminal, so
/// an askpass that git cannot run (here a plain file with no execute bit) would send git to
/// `/dev/tty`: an hour-long hang in a developer's shell, and in CI a different failure. `rerun`
/// sets `GIT_TERMINAL_PROMPT=0` for its child, so both clients fail at once with git's words.
/// Unix only: a script without the execute bit still runs under Git for Windows' `sh`.
#[cfg(unix)]
#[test]
fn an_askpass_that_cannot_run_never_reaches_a_terminal() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("not-executable.sh");
    std::fs::write(&broken, "#!/bin/sh\necho nobody\n").unwrap();
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o644)).unwrap();
    rerun(
        "broken_askpass_inner",
        &[("GIT_ASKPASS", &sh_path(&broken))],
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broken_askpass_inner() {
    if !is_inner() {
        return;
    }
    assert_eq!(
        std::env::var("GIT_TERMINAL_PROMPT").as_deref(),
        Ok("0"),
        "rerun leaves the terminal on"
    );
    let p = pair(accept_alice, |_| (Spec::new(), ())).await;
    let g = p.git.git(&p.git.url());
    assert!(!g.ok);
    assert!(
        g.fatal().contains("terminal prompts disabled"),
        "git: {}",
        g.stderr
    );
    let c = p.asc.asc(&p.asc.url(), Prompt::Allowed).await;
    assert!(!c.ok);
    assert!(
        c.message.contains("terminal prompts disabled"),
        "asc: {}",
        c.message
    );
}

/// `GIT_TERMINAL_PROMPT=0` disables the terminal for both, even when prompts are allowed, and
/// both fail with git's words.
#[test]
fn git_terminal_prompt_zero_disables_prompting_in_both() {
    rerun("no_terminal_inner", &[("GIT_TERMINAL_PROMPT", "0")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_terminal_inner() {
    if !is_inner() {
        return;
    }
    let p = pair(accept_alice, |_| (Spec::new(), ())).await;
    let g = p.git.git(&p.git.url());
    assert!(!g.ok);
    assert_eq!(g.code, Some(128));
    let c = p.asc.asc(&p.asc.url(), Prompt::Allowed).await;
    assert!(!c.ok);
    assert_same_failure_words(&g.fatal(), &c.message, p.ports());
    assert!(g.fatal().contains("terminal prompts disabled"));
}

// ====================================================================== redirects

/// A 302 to a second host receives no `Authorization` from real git or from asc. libcurl still
/// forwards a non-sensitive `http.extraHeader` to the new host; asc forwards none (SPEC
/// §15.6.4 item 3): the documented difference.
// fidelity: U-1
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redirect_to_another_host_carries_no_credential() {
    let mut results = Vec::new();
    for client in ["git", "asc"] {
        let mut a = helpers::Side::new(|_| {}).await;
        let b = FakeHost::start().await;
        b.open();
        a.host
            .redirect_to(&format!("{}{}", b.origin(), fake_host::INFO_REFS));
        let spec = Spec::new().other(format!(
            "[http \"http://127.0.0.1:{}/\"]\n\textraheader = Authorization: Bearer {CANARY}\n\textraheader = X-Extra: {CANARY}-extra\n",
            a.port()
        ));
        a.configure(spec);
        let url = a.url();
        if client == "git" {
            let _ = a.git(&url);
        } else {
            let run = a.asc(&url, Prompt::Never).await;
            assert!(
                run.ok,
                "asc followed the redirect by hand: {} ({})",
                run.message, run.status
            );
        }
        let first = a.host.requests();
        assert!(
            first[0].authorization().is_some(),
            "{client}: the origin host got its credential"
        );
        let landed = b.requests();
        assert!(!landed.is_empty(), "{client}: the redirect was followed");
        for r in &landed {
            assert!(
                r.authorization().is_none(),
                "{client}: Authorization crossed hosts ({})",
                mask(r.authorization())
            );
            let leaked: Vec<&str> = r
                .headers
                .iter()
                .filter(|(_, v)| v.contains(CANARY))
                .map(|(n, _)| n.as_str())
                .collect();
            assert!(
                leaked.iter().all(|n| *n == "x-extra"),
                "{client}: a credential header crossed hosts: {leaked:?}"
            );
        }
        results.push((client, landed.iter().any(|r| r.header("x-extra").is_some())));
    }
    let leaked_by = |who: &str| results.iter().find(|(c, _)| *c == who).unwrap().1;
    assert!(
        !leaked_by("asc"),
        "asc forwards no extra header to another origin"
    );
    // The documented difference (SPEC §15.6.4 item 3): libcurl still forwards a
    // non-sensitive extra header, which is why asc is stricter.
    assert!(
        leaked_by("git"),
        "git (libcurl) forwards a non-sensitive extra header"
    );
}

/// A credential from URL userinfo does not follow a redirect either, and asc re-asks the
/// resolver for the new origin: a helper is consulted for the second host only after its 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redirect_re_asks_the_resolver_for_the_new_origin() {
    let mut a = helpers::Side::new(|_| {}).await;
    let b = FakeHost::start().await;
    b.accept_basic("bob", "b-secret");
    a.host
        .redirect_to(&format!("{}{}", b.origin(), fake_host::INFO_REFS));
    let rec = Recorder::plain(a.dir(), "h", "bob", "b-secret");
    a.configure(Spec::new().credential(helper_section(&rec)));
    let url = a.url_with_userinfo(&format!("{USER}:{PASS}"));
    let run = a.asc(&url, Prompt::Never).await;
    assert!(run.ok, "{} ({})", run.message, run.status);
    let seen = b.requests();
    assert!(
        seen[0].authorization().is_none(),
        "the userinfo credential did not cross"
    );
    assert_eq!(seen.last().unwrap().status, 200);
    let calls = rec.calls();
    assert_eq!(rec.ops(), ["get", "store"]);
    let b_host = format!("host=127.0.0.1:{}", b.addr.port());
    assert!(
        calls[0].has(&b_host),
        "the helper was asked for the new origin: {}",
        redact_calls(&calls)
    );
}

// ====================================================================== trusted config

/// `credential.helper` is honoured from a repo config the current user owns, by git and by asc;
/// from a repo config another user owns (reduced trust) asc drops it and never runs it, so a
/// foreign repository cannot make asc run a program or collect a credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credential_helper_from_a_foreign_repo_config_is_ignored() {
    for (trust, runs) in [(Trust::Full, true), (Trust::Reduced, false)] {
        let p = pair(accept_alice, |s| {
            let r = Recorder::plain(s.dir(), "h", USER, PASS);
            (Spec::new().credential(helper_section(&r)).in_repo(trust), r)
        })
        .await;
        if runs {
            let g = p.git.git(&p.git.url());
            assert!(g.ok, "git ran the owner's repo helper: {}", g.stderr);
        }
        let c = p.asc.asc(&p.asc.url(), Prompt::Never).await;
        assert_eq!(c.ok, runs, "{trust:?}: {}", c.message);
        assert_eq!(!p.asc_r.calls().is_empty(), runs, "{trust:?}");
        if runs {
            assert_same_calls(
                &p.git_r.calls(),
                &p.asc_r.calls(),
                p.ports(),
                "owner's repo config",
            );
        } else {
            assert!(
                p.asc.host
                    .requests()
                    .iter()
                    .all(|r| r.authorization().is_none())
            );
        }
    }
}

// ====================================================================== the asc edge

/// Real git, the resolver and `RpcClient` against an in-process `asc serve` router with a static
/// token table (the token store, M30, is not built yet). The git routes answer the challenge
/// and then the disabled stub; `[git] enabled = false` and the live layer are U81/U83.
#[cfg(feature = "serve")]
mod edge {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use asc_rpc::auth::{
        Access, AuthError, Authorizer, BearerToken, Credential, Principal, Scheme as WireScheme,
        StaticTokens,
    };
    use asc_rpc::client::{CallError, PushMode, RpcClient};
    use asc_rpc::server::ServerInfo;
    use asc_serve::registry::Extensions;
    use tonic::Code;

    use super::*;
    use credential::{Resolved, Scheme};
    use helpers::Side;

    /// One `authorize_credential` call as the edge made it: the audit facts.
    #[derive(Clone, Debug)]
    struct Decision {
        scheme: WireScheme,
        user: Option<String>,
        repo: String,
        access: Access,
        principal: Result<String, String>,
    }

    struct Spy {
        inner: StaticTokens,
        decisions: Mutex<Vec<Decision>>,
    }

    #[async_trait]
    impl Authorizer for Spy {
        async fn authorize(
            &self,
            token: Option<&BearerToken>,
            repo: &str,
            access: Access,
        ) -> Result<Principal, AuthError> {
            self.inner.authorize(token, repo, access).await
        }

        async fn authorize_credential(
            &self,
            credential: Option<&Credential>,
            repo: &str,
            access: Access,
        ) -> Result<Principal, AuthError> {
            let result = self
                .authorize(credential.map(Credential::secret), repo, access)
                .await;
            if let Some(c) = credential {
                self.decisions.lock().unwrap().push(Decision {
                    scheme: c.scheme(),
                    user: c.user().map(str::to_owned),
                    repo: repo.to_owned(),
                    access,
                    principal: match &result {
                        Ok(p) => Ok(p.id.clone()),
                        Err(e) => Err(format!("{e:?}")),
                    },
                });
            }
            result
        }
    }

    struct Edge {
        addr: std::net::SocketAddr,
        spy: Arc<Spy>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Edge {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Edge {
        async fn start(tokens: &[(&str, &str, Access)]) -> Self {
            let table = tokens
                .iter()
                .fold(StaticTokens::new(), |t, (tok, who, max)| {
                    t.with_token(tok, who, *max)
                });
            let spy = Arc::new(Spy {
                inner: table,
                decisions: Mutex::default(),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let routes =
                asc_serve::build_routes(&Extensions::new(), spy.clone(), ServerInfo::default());
            let incoming = futures::stream::unfold(listener, |l| async move {
                let next = l.accept().await.map(|(s, _)| s);
                Some((next, l))
            });
            let task = tokio::spawn(async move {
                let _ = tonic::transport::Server::builder()
                    .accept_http1(true)
                    .add_routes(routes)
                    .serve_with_incoming(incoming)
                    .await;
            });
            Self { addr, spy, task }
        }

        fn url(&self) -> String {
            format!("http://{}/acme/web.git", self.addr)
        }

        fn decisions(&self) -> Vec<Decision> {
            self.spy.decisions.lock().unwrap().clone()
        }
    }

    fn code(e: &CallError) -> Option<Code> {
        match e {
            CallError::Status { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// The resolver's credential as the credential an `RpcClient` sends.
    fn rpc_credential(resolved: &Resolved) -> Credential {
        let p = resolved.presented().expect("a credential");
        let secret = p.secret.expose();
        match &p.scheme {
            Scheme::Basic => Credential::basic(&p.user, secret)
                .or_else(|_| Credential::basic("", &p.user))
                .expect("basic"),
            Scheme::Bearer => Credential::bearer(BearerToken::new(secret).unwrap()),
            Scheme::Other(word) if word.eq_ignore_ascii_case("token") => {
                Credential::token(BearerToken::new(secret).unwrap())
            }
            Scheme::Other(word) => panic!("no RPC form for scheme {word}"),
        }
    }

    fn rpc(edge: &Edge, resolved: &Resolved) -> RpcClient {
        RpcClient::connect(
            &format!("http://{}", edge.addr),
            "acme/web",
            Some(rpc_credential(resolved)),
        )
        .unwrap()
    }

    async fn write_call(client: &RpcClient) -> Result<(), CallError> {
        client
            .update_refs_with(&[], PushMode::Force, uuid::Uuid::now_v7())
            .await
    }

    /// What the edge answers an authenticated `info/refs` with: status, `X-Asc-Git`, body.
    async fn stub_answer(edge: &Edge, authorization: &str) -> (u16, Option<String>, String) {
        let response = reqwest::Client::new()
            .get(format!("{}/info/refs?service=git-upload-pack", edge.url()))
            .header("Authorization", authorization)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let flag = response
            .headers()
            .get("x-asc-git")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (status, flag, response.text().await.unwrap())
    }

    fn basic_header(user: &str, secret: &str) -> String {
        format!(
            "Basic {}",
            fake_host::base64(format!("{user}:{secret}").as_bytes())
        )
    }

    const RW: &str = "tok-write-S3CR3T";
    const RO: &str = "tok-read-S3CR3T";

    /// `git ls-remote` at the edge: the anonymous request meets the Basic challenge, git runs the
    /// helper's `get` (with the challenge as `wwwauth[]`), retries with Basic, is authorised
    /// as the token's principal, and then meets the git stub, like any host that does not
    /// serve git.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn real_git_gets_the_challenge_runs_the_helper_and_is_authorised() {
        let edge = Edge::start(&[(RW, "ci-write", Access::Write)]).await;
        let mut side = Side::new(|_| {}).await;
        let helper = Recorder::plain(side.dir(), "h", "x-access-token", RW);
        side.configure(Spec::new().credential(helper_section(&helper)));

        let run = side.git(&edge.url());
        assert!(
            !run.ok,
            "the git routes answer a stub, not an advertisement"
        );
        let calls = helper.calls();
        assert_eq!(calls[0].op, "get", "{}", redact_calls(&calls));
        assert!(
            calls[0]
                .attrs
                .iter()
                .any(|a| a.starts_with("wwwauth[]=Basic realm=\"asc 127.0.0.1")),
            "the helper got the edge's challenge: {}",
            redact_calls(&calls)
        );
        let decisions = edge.decisions();
        let last = decisions
            .last()
            .expect("the edge decided on the credential");
        assert_eq!(last.scheme, WireScheme::Basic);
        assert_eq!(last.user.as_deref(), Some("x-access-token"));
        assert_eq!(
            last.principal,
            Ok("ci-write".to_owned()),
            "the principal is known"
        );
        assert_eq!(
            (last.repo.as_str(), last.access),
            ("acme/web", Access::Read)
        );
        assert!(!run.stderr.contains(RW), "no secret on git's stderr");

        let (status, flag, body) = stub_answer(&edge, &basic_header("x-access-token", RW)).await;
        // SPEC §25.9 (v1.12), U81: the disabled stub is `403` with `X-Asc-Git: disabled`.
        assert_eq!(status, 403, "{body}");
        assert_eq!(flag.as_deref(), Some("disabled"));
        assert!(body.contains("git protocol disabled"), "{body}");
    }

    /// The v1.12 stub contract (PLAN U78/U81): `403`, `X-Asc-Git: disabled`, and the text.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_disabled_git_stub_is_403_with_x_asc_git() {
        let edge = Edge::start(&[(RW, "ci-write", Access::Write)]).await;
        let (status, flag, body) = stub_answer(&edge, &basic_header("git", RW)).await;
        assert_eq!(status, 403);
        assert_eq!(flag.as_deref(), Some("disabled"));
        assert_eq!(body.trim(), "asc: git protocol disabled on this server");
    }

    /// A revoked (here: unknown) token gives `401` again, and git tells the helper to `erase`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_revoked_token_is_401_and_the_helper_is_told_to_erase() {
        let edge = Edge::start(&[(RW, "ci-write", Access::Write)]).await;
        let mut side = Side::new(|_| {}).await;
        let helper = Recorder::plain(side.dir(), "h", "x-access-token", "tok-revoked");
        side.configure(Spec::new().credential(helper_section(&helper)));

        let run = side.git(&edge.url());
        assert!(!run.ok);
        assert!(
            run.fatal().contains("Authentication failed"),
            "{}",
            run.fatal()
        );
        assert_eq!(
            helper.ops(),
            ["get", "erase"],
            "{}",
            redact_calls(&helper.calls())
        );
        let decisions = edge.decisions();
        assert!(!decisions.is_empty());
        assert!(
            decisions.iter().all(|d| d.principal.is_err()),
            "no principal for a revoked token"
        );

        // asc's resolver does the same against the same edge: the stand-in loop erases too.
        let helper = Recorder::plain(side.dir(), "h2", "x-access-token", "tok-revoked");
        side.configure(Spec::new().credential(helper_section(&helper)));
        let discovery = format!("{}/info/refs?service=git-upload-pack", edge.url());
        let run = side.asc(&discovery, Prompt::Never).await;
        assert!(!run.ok);
        assert_eq!(&helper.ops()[..2], ["get", "erase"]);
    }

    /// A read deploy token and a write deploy token are told apart on the RPC, with the
    /// credential the helper gave the resolver, whatever the user name.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn read_and_write_deploy_tokens_are_told_apart_on_the_rpc() {
        let edge = Edge::start(&[
            (RO, "deploy-read", Access::Read),
            (RW, "deploy-write", Access::Write),
        ])
        .await;
        for (token, may_write) in [(RO, false), (RW, true)] {
            let mut side = Side::new(|_| {}).await;
            let helper = Recorder::plain(side.dir(), "h", "gitlab-ci-token", token);
            side.configure(Spec::new().credential(helper_section(&helper)));
            let resolved = side.resolved(&edge.url());
            assert_eq!(resolved.source(), "credential helper");
            let client = rpc(&edge, &resolved);

            // Read reaches the (unregistered) handler: authorised, then UNIMPLEMENTED.
            let read = client.list_refs_all().await.unwrap_err();
            assert_eq!(
                code(&read),
                Some(Code::Unimplemented),
                "read with {token}: {read}"
            );
            let write = write_call(&client).await.unwrap_err();
            let expected = if may_write {
                Code::Unimplemented
            } else {
                Code::PermissionDenied
            };
            assert_eq!(code(&write), Some(expected), "write with {token}: {write}");
        }
        // The wrong token is UNAUTHENTICATED.
        let mut side = Side::new(|_| {}).await;
        let helper = Recorder::plain(side.dir(), "h", "x", "tok-unknown");
        side.configure(Spec::new().credential(helper_section(&helper)));
        let client = rpc(&edge, &side.resolved(&edge.url()));
        assert_eq!(
            code(&client.list_refs_all().await.unwrap_err()),
            Some(Code::Unauthenticated)
        );

        let decisions = edge.decisions();
        assert!(
            decisions
                .iter()
                .any(|d| d.principal == Ok("deploy-read".into()))
        );
        assert!(
            decisions
                .iter()
                .any(|d| d.principal == Ok("deploy-write".into()) && d.access == Access::Write)
        );
    }

    /// CI: a repo config with `http.<url>.extraheader` written as `actions/checkout` writes it
    /// authenticates real git and asc to the asc edge with no other setup, on the first request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn actions_checkout_extraheader_authenticates_git_and_asc_with_no_other_setup() {
        let edge = Edge::start(&[(RW, "ci-write", Access::Write)]).await;
        let mut side = Side::new(|_| {}).await;
        let header = format!(
            "basic {}",
            fake_host::base64(format!("x-access-token:{RW}").as_bytes())
        );
        side.configure(Spec::new().other(format!(
            "[http \"http://{}/\"]\n\textraheader = AUTHORIZATION: {header}\n",
            edge.addr
        )));

        let run = side.git(&edge.url());
        assert!(!run.ok, "the stub, not an advertisement");
        let resolved = side.resolved(&edge.url());
        assert_eq!(resolved.source(), "http.extraHeader");
        let client = rpc(&edge, &resolved);
        let read = client.list_refs_all().await.unwrap_err();
        assert_eq!(
            code(&read),
            Some(Code::Unimplemented),
            "authorised through the header: {read}"
        );
        write_call(&client).await.unwrap_err();

        let decisions = edge.decisions();
        assert!(decisions.len() >= 3, "git, then two RPCs: {decisions:?}");
        for d in &decisions {
            assert_eq!(d.principal, Ok("ci-write".to_owned()));
            assert_eq!(
                (d.scheme, d.user.as_deref()),
                (WireScheme::Basic, Some("x-access-token"))
            );
        }
        assert!(
            !format!("{decisions:?}").contains(RW),
            "no secret in what the edge records"
        );
    }

    /// `https://TOKEN@host`: Basic `TOKEN:`; the edge reads the user name as the secret.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_token_in_the_url_user_name_is_the_secret() {
        let edge = Edge::start(&[(RW, "ci-write", Access::Write)]).await;
        let mut side = Side::new(|_| {}).await;
        side.configure(Spec::new());
        let url = format!("http://{RW}@{}/acme/web.git", edge.addr);
        let resolved = side.resolved(&url);
        assert_eq!(resolved.source(), "url");
        let plain: url::Url = format!("{}/info/refs?service=git-upload-pack", edge.url())
            .parse()
            .unwrap();
        let response = resolved
            .apply(reqwest::Client::new().get(plain.clone()), &plain)
            .send()
            .await
            .unwrap();
        assert_ne!(response.status().as_u16(), 401);
        let decisions = edge.decisions();
        let d = decisions.last().unwrap();
        assert_eq!(d.principal, Ok("ci-write".to_owned()));
        assert_eq!(d.scheme, WireScheme::Basic);
        assert_eq!(d.user, None, "the user name was the secret, not a hint");
    }
}
